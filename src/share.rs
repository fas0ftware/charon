//! What is being shared, under which secret link, until when; and the rules for names on disk.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};

pub const MAX_WRONG: u32 = 30; // requests with a wrong link before the share closes itself

#[derive(Debug, Clone)]
pub struct SharedFile {
    pub path: PathBuf,
    pub name: String, // shown and offered as the download name
    pub size: u64,
    pub started: u32, // downloads in progress or finished
    pub finished: u32,
    /// A note shared with --text: kept in memory only, never written to disk.
    pub text: Option<std::sync::Arc<String>>,
}

pub const MAX_TEXT: usize = 64 * 1024;

#[derive(Debug)]
pub enum Mode {
    Send(Vec<SharedFile>),
    Receive { dir: PathBuf, max_size: u64, max_uploads: u32, received: Vec<(String, u64)> },
}

#[derive(Debug)]
pub struct Share {
    pub token: String,
    pub mode: Mode,
    pub expires: Instant,
    pub max_downloads: u32,
    pub wrong: u32,
    pub closed: Option<String>, // why the share ended
}

pub fn new_token() -> String {
    let mut b = [0u8; 16]; // 128 bits: not guessable, even with every device on the network trying
    SystemRandom::new().fill(&mut b).expect("the system random generator failed");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Constant-time comparison, so response timing says nothing about how much of a guess was right.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A file name that is safe to show, to put in a Content-Disposition header and to create on disk:
/// no directories, no control or quote characters, no leading dot, at most 100 characters.
pub fn safe_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let mut s: String = base
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ' | '(' | ')' | '+' | ','))
        .take(100)
        .collect();
    s = s.trim().trim_start_matches('.').trim().to_string();
    if s.is_empty() {
        "file".into()
    } else {
        s
    }
}

/// ASCII fallback for the Content-Disposition `filename=` (the real name goes in `filename*`).
pub fn ascii_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || ".-_ ()+,".contains(c) { c } else { '_' }).collect();
    if s.trim_matches('_').is_empty() {
        "file".into()
    } else {
        s
    }
}

pub fn rfc5987(name: &str) -> String {
    name.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Create `dir/name`, or `name (1)`, `name (2)`… if it exists; never follows or replaces anything.
pub fn create_unique(dir: &Path, name: &str) -> io::Result<(File, PathBuf)> {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 0..1000 {
        let candidate = if n == 0 { name.to_string() } else { format!("{stem} ({n}){ext}") };
        let path = dir.join(&candidate);
        // create_new is O_CREAT|O_EXCL: it fails on anything already there, a symlink included.
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "too many files with this name"))
}

pub fn human(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i < units.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

/// "2G", "500M", "10k" or bytes; decimal units, like the sizes Charon prints.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mul) = match s.char_indices().last()? {
        (i, 'k' | 'K') => (&s[..i], 1_000u64),
        (i, 'm' | 'M') => (&s[..i], 1_000_000),
        (i, 'g' | 'G') => (&s[..i], 1_000_000_000),
        _ => (s, 1),
    };
    num.trim().parse::<u64>().ok()?.checked_mul(mul)
}

impl Share {
    pub fn send(paths: &[PathBuf], minutes: u64, max_downloads: u32) -> io::Result<Share> {
        let mut files = vec![];
        for p in paths {
            let meta = fs::metadata(p)?;
            if !meta.is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a file", p.display())));
            }
            let name = safe_name(&p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            files.push(SharedFile { path: p.clone(), name, size: meta.len(), started: 0, finished: 0, text: None });
        }
        Ok(Share::with(Mode::Send(files), minutes, max_downloads))
    }

    /// A note (a password, an address) shown once on the page, optionally next to files.
    pub fn send_text(text: String, paths: &[PathBuf], minutes: u64, max_downloads: u32) -> io::Result<Share> {
        if text.is_empty() || text.len() > MAX_TEXT {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("the text must be 1 byte to {} KB", MAX_TEXT / 1024)));
        }
        let mut share = Share::send(paths, minutes, max_downloads)?;
        if let Mode::Send(files) = &mut share.mode {
            let note = SharedFile { path: PathBuf::new(), name: "Note".into(), size: text.len() as u64, started: 0, finished: 0,
                                    text: Some(std::sync::Arc::new(text)) };
            files.insert(0, note);
        }
        Ok(share)
    }

    pub fn receive(dir: PathBuf, minutes: u64, max_size: u64, max_uploads: u32) -> Share {
        Share::with(Mode::Receive { dir, max_size, max_uploads, received: vec![] }, minutes, 0)
    }

    fn with(mode: Mode, minutes: u64, max_downloads: u32) -> Share {
        Share {
            token: new_token(),
            mode,
            expires: Instant::now() + Duration::from_secs(minutes.max(1) * 60),
            max_downloads: max_downloads.max(1),
            wrong: 0,
            closed: None,
        }
    }

    /// Is the share over? Sets `closed` with the reason the first time.
    pub fn check_done(&mut self, now: Instant) -> bool {
        if self.closed.is_some() {
            return true;
        }
        let reason = if now >= self.expires {
            Some("the link expired".to_string())
        } else if self.wrong >= MAX_WRONG {
            Some(format!("{MAX_WRONG} requests with a wrong link: someone may be guessing, so the share was closed"))
        } else {
            match &self.mode {
                Mode::Send(files) if files.iter().all(|f| f.finished >= self.max_downloads) => {
                    Some("everything was downloaded".to_string())
                }
                Mode::Receive { max_uploads, received, .. } if *max_uploads > 0 && received.len() as u32 >= *max_uploads => {
                    Some("the files arrived".to_string())
                }
                _ => None,
            }
        };
        self.closed = reason;
        self.closed.is_some()
    }

    /// Reserve one download of file `i`; false if it is used up.
    pub fn start_download(&mut self, i: usize) -> Option<SharedFile> {
        let max = self.max_downloads;
        match &mut self.mode {
            Mode::Send(files) => {
                let f = files.get_mut(i)?;
                if f.started >= max {
                    return None;
                }
                f.started += 1;
                Some(f.clone())
            }
            _ => None,
        }
    }

    pub fn end_download(&mut self, i: usize, ok: bool) {
        if let Mode::Send(files) = &mut self.mode {
            if let Some(f) = files.get_mut(i) {
                if ok {
                    f.finished += 1;
                } else {
                    f.started = f.started.saturating_sub(1); // an aborted download does not use up the link
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_comparison() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(same(&a, &a.clone()) && !same(&a, &b) && !same(&a, &a[..31]));
    }

    #[test]
    fn names() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\Users\\x\\Şirket raporu.pdf"), "Şirket raporu.pdf");
        assert_eq!(safe_name(".bashrc"), "bashrc");
        assert_eq!(safe_name("a\"b\r\nContent-Type: x<script>.txt"), "abContent-Type xscript.txt");
        assert_eq!(safe_name("..."), "file");
        assert_eq!(safe_name(&"x".repeat(300)).len(), 100);
        assert_eq!(ascii_name("Şirket raporu.pdf"), "_irket raporu.pdf");
        assert_eq!(rfc5987("Ş a.pdf"), "%C5%9E%20a.pdf");
    }

    #[test]
    fn unique_files_never_replace() {
        let d = tempfile::tempdir().unwrap();
        let (_, p1) = create_unique(d.path(), "photo.jpg").unwrap();
        let (_, p2) = create_unique(d.path(), "photo.jpg").unwrap();
        let (_, p3) = create_unique(d.path(), "notes").unwrap();
        let (_, p4) = create_unique(d.path(), "notes").unwrap();
        assert_eq!(p1.file_name().unwrap(), "photo.jpg");
        assert_eq!(p2.file_name().unwrap(), "photo (1).jpg");
        assert_eq!(p4.file_name().unwrap(), "notes (1)");
        assert!(p3.exists());
        std::os::unix::fs::symlink("/etc/hostname", d.path().join("link.txt")).unwrap();
        let (_, p5) = create_unique(d.path(), "link.txt").unwrap();
        assert_eq!(p5.file_name().unwrap(), "link (1).txt", "an existing symlink is left alone");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(p1).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("2G"), Some(2_000_000_000));
        assert_eq!(parse_size("500m"), Some(500_000_000));
        assert_eq!(parse_size("1234"), Some(1234));
        assert_eq!(parse_size("x"), None);
        assert_eq!(human(3_100_000), "3.1 MB");
    }

    #[test]
    fn download_counting_and_closing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        fs::write(&p, "hi").unwrap();
        let mut s = Share::send(&[p], 10, 1).unwrap();
        assert!(s.start_download(0).is_some());
        assert!(s.start_download(0).is_none(), "one download at a time on a one-time link");
        s.end_download(0, false);
        assert!(!s.check_done(Instant::now()));
        assert!(s.start_download(0).is_some(), "an aborted download can be retried");
        s.end_download(0, true);
        assert!(s.check_done(Instant::now()));
        assert_eq!(s.closed.as_deref(), Some("everything was downloaded"));
        assert!(s.start_download(5).is_none());

        let mut r = Share::receive(d.path().into(), 1, 10, 0);
        assert!(!r.check_done(Instant::now()));
        r.wrong = MAX_WRONG;
        assert!(r.check_done(Instant::now()) && r.closed.unwrap().contains("guessing"));
        let mut e = Share::receive(d.path().into(), 1, 10, 0);
        assert!(e.check_done(Instant::now() + Duration::from_secs(61)));
        assert!(Share::send(&[d.path().into()], 1, 1).is_err(), "a directory is refused");
    }
}
