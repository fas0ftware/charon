//! A self-signed certificate for this machine, kept between runs so a phone only has to accept
//! it once, and made again when the machine's address is not in it any more.

use std::fs;
use std::io::{self, Write};
use std::net::IpAddr;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("charon")
}

pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    pub created: bool,
}

pub fn fingerprint(cert: &[u8]) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, cert);
    d.as_ref().iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    fs::rename(tmp, path)
}

/// Load the stored certificate if it names every address in `ips`, else make a new one.
pub fn identity(dir: &Path, ips: &[IpAddr]) -> io::Result<Identity> {
    let (cert_path, key_path, ips_path) = (dir.join("cert.pem"), dir.join("key.pem"), dir.join("cert.ips"));
    let mut want: Vec<String> = ips.iter().map(|i| i.to_string()).collect();
    want.sort();
    let stored: Vec<String> = fs::read_to_string(&ips_path).map(|t| t.lines().map(String::from).collect()).unwrap_or_default();
    if want.iter().all(|i| stored.contains(i)) {
        if let (Ok(c), Ok(k)) = (fs::read(&cert_path), fs::read(&key_path)) {
            let cert = rustls_pemfile::certs(&mut &c[..]).next();
            let key = rustls_pemfile::private_key(&mut &k[..]);
            if let (Some(Ok(cert)), Ok(Some(key))) = (cert, key) {
                return Ok(Identity { cert, key, created: false });
            }
        }
    }
    // The new certificate names every address seen so far (a laptop moves between networks).
    let mut all = stored;
    all.extend(want);
    all.sort();
    all.dedup();
    all.retain(|a| a.parse::<IpAddr>().is_ok());
    let mut names = vec!["localhost".to_string()];
    names.extend(all.iter().cloned());
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(names).map_err(|e| io::Error::other(e.to_string()))?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    write_private(&key_path, key_pair.serialize_pem().as_bytes())?;
    write_private(&cert_path, cert.pem().as_bytes())?;
    write_private(&ips_path, all.join("\n").as_bytes())?;
    let key = PrivateKeyDer::try_from(key_pair.serialize_der()).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(Identity { cert: cert.der().clone(), key, created: true })
}

pub fn server_config(id: &Identity) -> io::Result<Arc<rustls::ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| io::Error::other(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(vec![id.cert.clone()], id.key.clone_key())
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept_between_runs_and_renewed_for_a_new_address() {
        let d = tempfile::tempdir().unwrap();
        let a: IpAddr = "192.0.2.5".parse().unwrap();
        let b: IpAddr = "198.51.100.7".parse().unwrap();
        let first = identity(d.path(), &[a]).unwrap();
        assert!(first.created);
        let again = identity(d.path(), &[a]).unwrap();
        assert!(!again.created);
        assert_eq!(fingerprint(&first.cert), fingerprint(&again.cert));
        let moved = identity(d.path(), &[b]).unwrap();
        assert!(moved.created && fingerprint(&moved.cert) != fingerprint(&first.cert));
        assert!(!identity(d.path(), &[a]).unwrap().created, "both addresses are remembered");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(d.path().join("key.pem")).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(server_config(&moved).is_ok());
        assert_eq!(fingerprint(&moved.cert).len(), 32 * 3 - 1);
    }
}
