//! Accept connections (TLS or plain), route the few URLs Charon has, stop when the share is over.

use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::http::{self, html_escape, Request};
use crate::share::{self, human, Mode, Share};

const PAGE_CSS: &str = include_str!("../assets/style.css");
const PAGE_JS: &str = include_str!("../assets/app.js");
const CHUNK: usize = 64 * 1024;

pub trait Conn: Read + Write + Send {}
impl<T: Read + Write + Send> Conn for T {}

pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

pub struct Server {
    pub share: Arc<Mutex<Share>>,
    pub tls: Option<Arc<rustls::ServerConfig>>,
    pub log: Log,
    pub stop: Arc<AtomicBool>,
}

fn lock(s: &Mutex<Share>) -> std::sync::MutexGuard<'_, Share> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

impl Server {
    /// Serve until the share is over (expired, used up, closed) or `stop` is set.
    pub fn run(&self, listener: TcpListener) -> io::Result<String> {
        listener.set_nonblocking(true)?;
        let mut workers = vec![];
        loop {
            if self.stop.load(Ordering::SeqCst) {
                lock(&self.share).closed.get_or_insert_with(|| "stopped".into());
                break;
            }
            if lock(&self.share).check_done(Instant::now()) {
                break;
            }
            match listener.accept() {
                Ok((stream, peer)) => {
                    let (share, tls, log) = (self.share.clone(), self.tls.clone(), self.log.clone());
                    workers.push(std::thread::spawn(move || {
                        let _ = handle(stream, peer, &share, tls, &log);
                    }));
                    workers.retain(|w| !w.is_finished());
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(40)),
                Err(e) => return Err(e),
            }
        }
        drop(listener); // no new connections; let transfers in progress finish
        for w in workers {
            let _ = w.join();
        }
        Ok(lock(&self.share).closed.clone().unwrap_or_default())
    }
}

fn handle(stream: TcpStream, peer: SocketAddr, share: &Mutex<Share>, tls: Option<Arc<rustls::ServerConfig>>, log: &Log) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let conn: Box<dyn Conn> = match tls {
        Some(cfg) => {
            let tc = rustls::ServerConnection::new(cfg).map_err(io::Error::other)?;
            Box::new(rustls::StreamOwned::new(tc, stream))
        }
        None => Box::new(stream),
    };
    let mut r = BufReader::with_capacity(CHUNK, conn);
    let req = match http::read_head(&mut r) {
        Ok(req) => req,
        Err(http::HeadError::Closed) => return Ok(()),
        Err(_) => return http::respond(r.get_mut(), 400, "Bad Request", "text/plain", b"bad request\n"),
    };
    route(&req, &mut r, peer, share, log)
}

fn route(req: &Request, r: &mut BufReader<Box<dyn Conn>>, peer: SocketAddr, share: &Mutex<Share>, log: &Log) -> io::Result<()> {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/s/style.css") => return http::respond(r.get_mut(), 200, "OK", "text/css; charset=utf-8", PAGE_CSS.as_bytes()),
        ("GET", "/s/app.js") => return http::respond(r.get_mut(), 200, "OK", "text/javascript; charset=utf-8", PAGE_JS.as_bytes()),
        ("GET", "/favicon.ico") => return http::respond(r.get_mut(), 404, "Not Found", "text/plain", b""),
        _ => {}
    }
    // Every other URL starts with the secret: /<token>/...
    let mut parts = req.path.trim_start_matches('/').splitn(2, '/');
    let (given, rest) = (parts.next().unwrap_or(""), parts.next());
    {
        let mut s = lock(share);
        if !share::same(given, &s.token) {
            s.wrong += 1;
            drop(s);
            return http::respond(r.get_mut(), 404, "Not Found", "text/plain", b"not found\n");
        }
        if s.check_done(Instant::now()) {
            return http::respond(r.get_mut(), 410, "Gone", "text/html; charset=utf-8", &page("Link used up", "<p>This link has expired or was already used.</p>"));
        }
    }
    match (req.method.as_str(), rest) {
        ("GET", None) => {
            let to = format!("/{given}/");
            http::write_head(r.get_mut(), 302, "Found", &[("Location", to)], 0)
        }
        ("GET", Some("")) => {
            let body = index(&lock(share));
            http::respond(r.get_mut(), 200, "OK", "text/html; charset=utf-8", &body)
        }
        ("GET", Some(f)) if f.starts_with("f/") => download(f[2..].parse().ok(), r, peer, share, log),
        ("PUT", Some("up")) => upload(req, r, peer, share, log),
        _ => http::respond(r.get_mut(), 404, "Not Found", "text/plain", b"not found\n"),
    }
}

fn page(title: &str, body: &str) -> Vec<u8> {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"referrer\" content=\"no-referrer\"><title>{} · Charon</title><link rel=\"stylesheet\" href=\"/s/style.css\">\
         <script src=\"/s/app.js\" defer></script></head><body><main><h1><span class=\"logo\">⛵</span> Charon</h1>{body}\
         <p class=\"foot\">Charon carries files across your own network: nothing goes through the internet.</p></main></body></html>",
        html_escape(title)
    )
    .into_bytes()
}

fn left(s: &Share) -> String {
    let secs = s.expires.saturating_duration_since(Instant::now()).as_secs();
    if secs >= 60 {
        format!("{} min", secs.div_ceil(60))
    } else {
        format!("{secs} s")
    }
}

fn index(s: &Share) -> Vec<u8> {
    match &s.mode {
        Mode::Send(files) => {
            let total: u64 = files.iter().filter(|f| f.text.is_none()).map(|f| f.size).sum();
            let real = files.iter().filter(|f| f.text.is_none()).count();
            let what = match (files.len() - real, real) {
                (0, n) => format!("{n} file(s), {}", human(total)),
                (_, 0) => "A note".to_string(),
                (_, n) => format!("A note and {n} file(s), {}", human(total)),
            };
            let mut list = String::new();
            for (i, f) in files.iter().enumerate() {
                let remaining = s.max_downloads.saturating_sub(f.started);
                if f.text.is_some() {
                    // The note is never in the page: it is fetched only when "Show" is pressed, so a
                    // link preview or a crawler that opens the page cannot use it up.
                    let action = if remaining > 0 {
                        format!("<button class=\"btn reveal\" type=\"button\" data-i=\"{i}\">Show</button>")
                    } else {
                        "<span class=\"muted\">already shown</span>".into()
                    };
                    list += &format!("<li class=\"note\"><span class=\"name\">Note</span><span class=\"muted\">{} character(s)</span>{action}\
                                      <pre class=\"secret\" id=\"note-{i}\" hidden></pre></li>", f.text.as_ref().map(|t| t.chars().count()).unwrap_or(0));
                    continue;
                }
                let link = if remaining > 0 {
                    format!("<a class=\"btn\" href=\"f/{i}\" download>Download</a>")
                } else {
                    "<span class=\"muted\">downloaded</span>".into()
                };
                list += &format!("<li><span class=\"name\">{}</span><span class=\"muted\">{}</span>{link}</li>", html_escape(&f.name), human(f.size));
            }
            let each = if s.max_downloads == 1 { "once".to_string() } else { format!("{} times", s.max_downloads) };
            page("Download", &format!("<p class=\"lead\">{what}. Each can be opened {each}; the link ends in {}.</p><ul class=\"files\">{list}</ul>", left(s)))
        }
        Mode::Receive { max_size, received, .. } => page(
            "Send files",
            &format!(
                "<p class=\"lead\">Pick files to send to this computer (up to {} each). The link ends in {}.</p>\
                 <form id=\"up\"><input id=\"pick\" type=\"file\" multiple><button type=\"submit\">Send</button></form>\
                 <ul id=\"progress\" class=\"files\" data-max=\"{max_size}\"></ul><p class=\"muted\">{} file(s) received so far.</p>",
                human(*max_size),
                left(s),
                received.len()
            ),
        ),
    }
}

fn download(i: Option<usize>, r: &mut BufReader<Box<dyn Conn>>, peer: SocketAddr, share: &Mutex<Share>, log: &Log) -> io::Result<()> {
    let Some(i) = i else { return http::respond(r.get_mut(), 404, "Not Found", "text/plain", b"not found\n") };
    let Some(f) = lock(share).start_download(i) else {
        return http::respond(r.get_mut(), 410, "Gone", "text/html; charset=utf-8", &page("Already downloaded", "<p>This file was already downloaded.</p>"));
    };
    if let Some(text) = &f.text {
        let w = r.get_mut();
        let result = http::write_head(w, 200, "OK", &[("Content-Type", "text/plain; charset=utf-8".into())], text.len() as u64)
            .and_then(|_| w.write_all(text.as_bytes()))
            .and_then(|_| w.flush());
        lock(share).end_download(i, result.is_ok());
        match &result {
            Ok(()) => log(&format!("{} was shown the note", peer.ip())),
            Err(e) => log(&format!("{}: showing the note failed ({e}); it can be tried again", peer.ip())),
        }
        return result;
    }
    let result = (|| -> io::Result<()> {
        let mut file = std::fs::File::open(&f.path)?;
        let size = file.metadata()?.len();
        let disposition = format!("attachment; filename=\"{}\"; filename*=UTF-8''{}", share::ascii_name(&f.name), share::rfc5987(&f.name));
        let w = r.get_mut();
        http::write_head(w, 200, "OK", &[("Content-Type", "application/octet-stream".into()), ("Content-Disposition", disposition)], size)?;
        let sent = io::copy(&mut (&mut file).take(size), w)?;
        w.flush()?;
        if sent != size {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the file shrank while it was sent"));
        }
        Ok(())
    })();
    lock(share).end_download(i, result.is_ok());
    match &result {
        Ok(()) => log(&format!("{} downloaded {} ({})", peer.ip(), f.name, human(f.size))),
        Err(e) => log(&format!("{}: download of {} stopped ({e}); it can be tried again", peer.ip(), f.name)),
    }
    result
}

fn upload(req: &Request, r: &mut BufReader<Box<dyn Conn>>, peer: SocketAddr, share: &Mutex<Share>, log: &Log) -> io::Result<()> {
    let (dir, max_size) = match &lock(share).mode {
        Mode::Receive { dir, max_size, .. } => (dir.clone(), *max_size),
        _ => return http::respond(r.get_mut(), 404, "Not Found", "text/plain", b"not found\n"),
    };
    let Some(len) = req.content_length() else {
        return http::respond(r.get_mut(), 411, "Length Required", "application/json", br#"{"ok":false,"error":"length required"}"#);
    };
    if len > max_size {
        return http::respond(r.get_mut(), 413, "Payload Too Large", "application/json",
                             format!(r#"{{"ok":false,"error":"larger than {}"}}"#, human(max_size)).as_bytes());
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return http::respond(r.get_mut(), 500, "Error", "application/json", format!(r#"{{"ok":false,"error":"{}"}}"#, json_str(&e.to_string())).as_bytes());
    }
    let name = share::safe_name(req.query.get("name").map(|s| s.as_str()).unwrap_or(""));
    let part = format!(".{name}.charon-part");
    let (mut file, tmp) = share::create_unique(&dir, &part)?;
    let got = io::copy(&mut r.by_ref().take(len), &mut file);
    let complete = matches!(got, Ok(n) if n == len) && file.sync_all().is_ok();
    drop(file);
    if !complete {
        let _ = std::fs::remove_file(&tmp);
        log(&format!("{}: upload of {name} was interrupted", peer.ip()));
        return http::respond(r.get_mut(), 400, "Bad Request", "application/json", br#"{"ok":false,"error":"upload interrupted"}"#);
    }
    // Move the finished file to its final, unique name (never replacing anything).
    let (placeholder, final_path) = share::create_unique(&dir, &name)?;
    drop(placeholder);
    std::fs::rename(&tmp, &final_path)?;
    let saved = final_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if let Mode::Receive { received, .. } = &mut lock(share).mode {
        received.push((saved.clone(), len));
    }
    log(&format!("{} sent {} ({}) -> {}", peer.ip(), saved, human(len), final_path.display()));
    http::respond(r.get_mut(), 200, "OK", "application/json", format!(r#"{{"ok":true,"name":"{}"}}"#, json_str(&saved)).as_bytes())
}

fn json_str(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\\' => "\\\\".chars().collect(),
            c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32).chars().collect(),
            c => vec![c],
        })
        .collect()
}
