//! Command line, the machine's LAN address and the QR code.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use qrcode::render::unicode::Dense1x2;
use qrcode::QrCode;

use crate::server::{Log, Server};
use crate::share::{self, human, Share};
use crate::{tls, VERSION};

const HELP: &str = "charon: send files between devices on your own network. No cloud, no account.

usage:
  charon send FILE... [--minutes 10] [--downloads 1]
  charon receive [DIR] [--minutes 10] [--max-size 2G] [--uploads N]
  charon fingerprint
  common: [--port 8765] [--http] [--bind ADDRESS]

  send         share files; the link works --downloads times per file, for --minutes, then Charon exits
  receive      open a page where a phone or another computer can send files here
               (default DIR: ~/Downloads/Charon; --uploads N exits after N files)
  fingerprint  show this machine's certificate fingerprint (to compare with the browser's)
  --http       no TLS: only on a network you trust completely";

#[derive(Debug, PartialEq)]
pub struct Args {
    pub cmd: String,
    pub files: Vec<PathBuf>,
    pub dir: Option<PathBuf>,
    pub minutes: u64,
    pub downloads: u32,
    pub max_size: u64,
    pub uploads: u32,
    pub port: u16,
    pub http: bool,
    pub bind: Option<IpAddr>,
}

pub fn parse(argv: &[String]) -> Result<Args, String> {
    let mut a = Args {
        cmd: String::new(),
        files: vec![],
        dir: None,
        minutes: 10,
        downloads: 1,
        max_size: 2 << 30,
        uploads: 0,
        port: 8765,
        http: false,
        bind: None,
    };
    let mut it = argv.iter();
    let num = |v: Option<&String>, name: &str| -> Result<u64, String> {
        v.and_then(|s| s.parse::<u64>().ok()).filter(|n| *n > 0).ok_or(format!("{name} needs a positive number"))
    };
    while let Some(x) = it.next() {
        match x.as_str() {
            "send" | "receive" | "fingerprint" if a.cmd.is_empty() => a.cmd = x.clone(),
            "-h" | "--help" => a.cmd = "help".into(),
            "-V" | "--version" => a.cmd = "version".into(),
            "--minutes" => a.minutes = num(it.next(), "--minutes")?.min(24 * 60),
            "--downloads" => a.downloads = num(it.next(), "--downloads")?.min(1000) as u32,
            "--uploads" => a.uploads = num(it.next(), "--uploads")?.min(100_000) as u32,
            "--port" => a.port = it.next().and_then(|s| s.parse().ok()).ok_or("--port needs a port number")?,
            "--max-size" => a.max_size = it.next().and_then(|s| share::parse_size(s)).filter(|n| *n > 0).ok_or("--max-size needs a size like 500M or 2G")?,
            "--bind" => a.bind = Some(it.next().and_then(|s| s.parse().ok()).ok_or("--bind needs an IP address")?),
            "--http" => a.http = true,
            s if s.starts_with('-') => return Err(format!("unknown option {s:?} (see charon --help)")),
            path if a.cmd == "send" => a.files.push(PathBuf::from(path)),
            path if a.cmd == "receive" && a.dir.is_none() => a.dir = Some(PathBuf::from(path)),
            other => return Err(format!("unexpected {other:?} (see charon --help)")),
        }
    }
    if a.cmd.is_empty() {
        a.cmd = "help".into();
    }
    if a.cmd == "send" && a.files.is_empty() {
        return Err("send needs at least one file".into());
    }
    Ok(a)
}

/// The address other devices reach this one at: the source address of the default route.
/// Connecting a UDP socket sends nothing; it only asks the kernel which interface it would use.
pub fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect("192.0.2.1:9").ok()?; // TEST-NET-1: never actually contacted
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

pub fn qr(text: &str) -> String {
    match QrCode::new(text.as_bytes()) {
        Ok(code) => code.render::<Dense1x2>().dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).quiet_zone(true).build(),
        Err(_) => String::new(),
    }
}

fn downloads_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("Downloads").join("Charon")
}

fn now_hm() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // UTC: reading the local time zone would need a time-zone database or libc.
    format!("{:02}:{:02}:{:02} UTC", secs % 86400 / 3600, secs % 3600 / 60, secs % 60)
}

pub fn run(argv: &[String]) -> i32 {
    let a = match parse(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("charon: {e}");
            return 2;
        }
    };
    match a.cmd.as_str() {
        "help" => {
            println!("{HELP}");
            return 0;
        }
        "version" => {
            println!("charon {VERSION}");
            return 0;
        }
        _ => {}
    }
    let ip = a.bind.filter(|b| !b.is_unspecified()).or_else(lan_ip);
    let ips: Vec<IpAddr> = ip.into_iter().collect();
    if a.cmd == "fingerprint" {
        return match tls::identity(&tls::data_dir(), &ips) {
            Ok(id) => {
                println!("{}", tls::fingerprint(&id.cert));
                0
            }
            Err(e) => {
                eprintln!("charon: {e}");
                1
            }
        };
    }
    let share = match a.cmd.as_str() {
        "send" => match Share::send(&a.files, a.minutes, a.downloads) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("charon: {e}");
                return 1;
            }
        },
        _ => Share::receive(a.dir.clone().unwrap_or_else(downloads_dir), a.minutes, a.max_size, a.uploads),
    };
    let tls_cfg = if a.http {
        None
    } else {
        match tls::identity(&tls::data_dir(), &ips).and_then(|id| Ok((tls::fingerprint(&id.cert), id.created, tls::server_config(&id)?))) {
            Ok((fp, created, cfg)) => {
                println!("TLS certificate {}: SHA-256 {fp}", if created { "(new)" } else { "(this machine's)" });
                Some(cfg)
            }
            Err(e) => {
                eprintln!("charon: cannot set up TLS: {e} (or use --http)");
                return 1;
            }
        }
    };
    let bind_ip = a.bind.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let listener = match TcpListener::bind(SocketAddr::new(bind_ip, a.port)).or_else(|_| TcpListener::bind(SocketAddr::new(bind_ip, 0))) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("charon: cannot listen: {e}");
            return 1;
        }
    };
    let port = listener.local_addr().map(|s| s.port()).unwrap_or(a.port);
    let host = ip.map(|i| if i.is_ipv6() { format!("[{i}]") } else { i.to_string() }).unwrap_or_else(|| "localhost".into());
    let url = format!("{}://{host}:{port}/{}/", if a.http { "http" } else { "https" }, share.token);
    match &share.mode {
        share::Mode::Send(files) => {
            let total: u64 = files.iter().map(|f| f.size).sum();
            println!("Sharing {} file(s), {}, for {} min; each can be downloaded {} time(s).", files.len(), human(total), a.minutes, a.downloads);
        }
        share::Mode::Receive { dir, max_size, .. } => {
            println!("Receiving into {} (up to {} per file) for {} min.", dir.display(), human(*max_size), a.minutes);
        }
    }
    println!("\n{}\n{url}\n", qr(&url));
    if !a.http {
        println!("The browser will warn once about the self-signed certificate: compare its SHA-256 with the one above.");
    }
    println!("Anyone with this link can use it until it ends; Ctrl+C stops sharing.\n");
    let log: Log = Arc::new(|line: &str| println!("{}  {line}", now_hm()));
    let server = Server { share: Arc::new(Mutex::new(share)), tls: tls_cfg, log, stop: Arc::new(AtomicBool::new(false)) };
    match server.run(listener) {
        Ok(reason) => {
            println!("Done: {reason}.");
            0
        }
        Err(e) => {
            eprintln!("charon: {e}");
            1
        }
    }
}
