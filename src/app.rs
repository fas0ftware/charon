//! `charon app`: a window on this machine (127.0.0.1) to pick files, write a note and start a share,
//! with the QR code and the progress on screen. The share itself is the same one-time TLS server
//! as `charon send`, on the network.
//!
//! Local API (every request that changes something needs the `X-Charon: 1` header: a web page on
//! another site cannot send it without a CORS preflight, which is never answered):
//!   GET  /                    the page
//!   PUT  /api/stage?name=     add a file (raw body)          POST /api/clear   forget the staged files
//!   POST /api/share           {"text", "minutes", "downloads"} -> {"url", "svg", "fingerprint"}
//!   GET  /api/status          staged files, the running share and its log
//!   POST /api/stop            end the share now

use std::fs;
use std::io::{self, BufReader, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use qrcode::render::svg;
use qrcode::QrCode;

use crate::http::{self, Request};
use crate::server::{Log, Server};
use crate::share::{self, Mode, Share};
use crate::tls;

const PAGE: &str = include_str!("../assets/app.html");
const PAGE_JS: &str = include_str!("../assets/app-page.js");
const CSS: &str = include_str!("../assets/style.css");
const MAX_STAGE: u64 = 2_000_000_000; // per file

pub struct Running {
    pub share: Arc<Mutex<Share>>,
    pub stop: Arc<AtomicBool>,
    pub url: String,
    pub svg: String,
    pub fingerprint: String,
    pub log: Arc<Mutex<Vec<String>>>,
    pub done: Arc<Mutex<Option<String>>>,
}

pub struct App {
    pub staging: PathBuf,
    pub staged: Mutex<Vec<(String, PathBuf, u64)>>,
    pub running: Mutex<Option<Running>>,
    /// Where the network share listens; tests use 127.0.0.1.
    pub bind: IpAddr,
    pub share_port: u16,
    pub tls_dir: PathBuf,
    pub use_tls: bool,
}

fn json_str(s: &str) -> String {
    serde_like_escape(s)
}

/// Minimal JSON string escaping (no serde dependency in this crate).
fn serde_like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '<' => out.push_str("\\u003c"), // keeps "</script>" and markup inert wherever the JSON ends up
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The few fields /api/share accepts, from a flat JSON object of strings and numbers.
fn field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let at = body.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = body[at..].trim_start().strip_prefix(':')?.trim_start();
    if let Some(s) = rest.strip_prefix('"') {
        // a string: up to the first unescaped quote
        let mut esc = false;
        for (i, c) in s.char_indices() {
            match c {
                '\\' if !esc => esc = true,
                '"' if !esc => return Some(&s[..i]),
                _ => esc = false,
            }
        }
        None
    } else {
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        Some(rest[..end].trim())
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

pub fn qr_svg(text: &str) -> String {
    QrCode::new(text.as_bytes())
        .map(|c| c.render::<svg::Color>().min_dimensions(240, 240).quiet_zone(true).build())
        .unwrap_or_default()
}

impl App {
    pub fn new(bind: IpAddr, share_port: u16, use_tls: bool) -> io::Result<App> {
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let staging = base.join(format!("charon-app-{}-{}", std::process::id(), &share::new_token()[..8]));
        fs::DirBuilder::new().mode(0o700).create(&staging)?;
        Ok(App {
            staging,
            staged: Mutex::new(vec![]),
            running: Mutex::new(None),
            bind,
            share_port,
            tls_dir: tls::data_dir(),
            use_tls,
        })
    }

    fn status_json(&self) -> String {
        let staged = self.staged.lock().unwrap_or_else(|e| e.into_inner());
        let files: Vec<String> = staged
            .iter()
            .map(|(n, _, s)| format!("{{\"name\":{},\"size\":{s},\"human\":{}}}", json_str(n), json_str(&share::human(*s))))
            .collect();
        let run = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let share = match run.as_ref() {
            None => "null".to_string(),
            Some(r) => {
                let s = r.share.lock().unwrap_or_else(|e| e.into_inner());
                let items: Vec<String> = match &s.mode {
                    Mode::Send(fs) => fs
                        .iter()
                        .map(|f| format!("{{\"name\":{},\"done\":{},\"note\":{}}}", json_str(&f.name), f.finished, f.text.is_some()))
                        .collect(),
                    _ => vec![],
                };
                let left = s.expires.saturating_duration_since(Instant::now()).as_secs();
                let log: Vec<String> = r.log.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|l| json_str(l)).collect();
                let done = r.done.lock().unwrap_or_else(|e| e.into_inner()).clone();
                format!(
                    "{{\"url\":{},\"svg\":{},\"fingerprint\":{},\"each\":{},\"left\":{left},\"items\":[{}],\"log\":[{}],\"done\":{}}}",
                    json_str(&r.url),
                    json_str(&r.svg),
                    json_str(&r.fingerprint),
                    s.max_downloads,
                    items.join(","),
                    log.join(","),
                    done.map(|d| json_str(&d)).unwrap_or_else(|| "null".into())
                )
            }
        };
        format!("{{\"staged\":[{}],\"share\":{share}}}", files.join(","))
    }

    fn stage(&self, req: &Request, body: &mut impl Read) -> Result<String, (u16, String)> {
        let len = req.content_length().ok_or((411, "length required".to_string()))?;
        if len > MAX_STAGE {
            return Err((413, format!("larger than {}", share::human(MAX_STAGE))));
        }
        let name = share::safe_name(req.query.get("name").map(|s| s.as_str()).unwrap_or(""));
        let (mut file, path) = share::create_unique(&self.staging, &name).map_err(|e| (500, e.to_string()))?;
        let got = io::copy(&mut body.take(len), &mut file).map_err(|e| (400, e.to_string()))?;
        if got != len {
            let _ = fs::remove_file(&path);
            return Err((400, "upload interrupted".into()));
        }
        let shown = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);
        self.staged.lock().unwrap_or_else(|e| e.into_inner()).push((shown.clone(), path, len));
        Ok(shown)
    }

    fn clear(&self) {
        let mut staged = self.staged.lock().unwrap_or_else(|e| e.into_inner());
        for (_, p, _) in staged.drain(..) {
            let _ = fs::remove_file(p);
        }
    }

    fn start(&self, body: &str) -> Result<(), (u16, String)> {
        let mut run = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = run.as_ref() {
            if r.done.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
                return Err((409, "a share is already running: stop it first".into()));
            }
        }
        let num = |k: &str, d: u64, max: u64| field(body, k).and_then(|v| v.parse::<u64>().ok()).filter(|n| *n > 0).unwrap_or(d).min(max);
        let (minutes, downloads) = (num("minutes", 10, 24 * 60), num("downloads", 1, 1000) as u32);
        let text = field(body, "text").map(unescape).unwrap_or_default();
        let paths: Vec<PathBuf> = self.staged.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(_, p, _)| p.clone()).collect();
        if paths.is_empty() && text.is_empty() {
            return Err((400, "pick a file or write a note first".into()));
        }
        let share = if text.is_empty() { Share::send(&paths, minutes, downloads) } else { Share::send_text(text, &paths, minutes, downloads) }
            .map_err(|e| (400, e.to_string()))?;
        let ip = if self.bind.is_unspecified() { crate::cli::lan_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)) } else { self.bind };
        let (tls_cfg, fingerprint) = if self.use_tls {
            let id = tls::identity(&self.tls_dir, &[ip]).map_err(|e| (500, e.to_string()))?;
            (Some(tls::server_config(&id).map_err(|e| (500, e.to_string()))?), tls::fingerprint(&id.cert))
        } else {
            (None, String::new())
        };
        let listener = TcpListener::bind(SocketAddr::new(self.bind, self.share_port))
            .or_else(|_| TcpListener::bind(SocketAddr::new(self.bind, 0)))
            .map_err(|e| (500, e.to_string()))?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(self.share_port);
        let host = if ip.is_ipv6() { format!("[{ip}]") } else { ip.to_string() };
        let url = format!("{}://{host}:{port}/{}/", if self.use_tls { "https" } else { "http" }, share.token);
        let lines = Arc::new(Mutex::new(Vec::new()));
        let l2 = lines.clone();
        let log: Log = Arc::new(move |s: &str| l2.lock().unwrap_or_else(|e| e.into_inner()).push(s.to_string()));
        let stop = Arc::new(AtomicBool::new(false));
        let share = Arc::new(Mutex::new(share));
        let done = Arc::new(Mutex::new(None));
        let server = Server { share: share.clone(), tls: tls_cfg, log, stop: stop.clone() };
        let d2 = done.clone();
        std::thread::spawn(move || {
            let reason = server.run(listener).unwrap_or_else(|e| e.to_string());
            *d2.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
        });
        *run = Some(Running { share, stop, svg: qr_svg(&url), url, fingerprint, log: lines, done });
        Ok(())
    }

    fn stop(&self) {
        if let Some(r) = self.running.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            r.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Answer one local request. Separate from the socket so tests can call it directly.
    pub fn route(&self, req: &Request, body: &mut impl Read) -> (u16, &'static str, Vec<u8>) {
        let host = req.header("host").unwrap_or("");
        let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
        if !["127.0.0.1", "localhost", "[::1]"].contains(&host) {
            return (400, "text/plain", b"bad host".to_vec()); // DNS rebinding
        }
        let json = |code: u16, s: String| (code, "application/json", s.into_bytes());
        let err = |(code, msg): (u16, String)| json(code, format!("{{\"ok\":false,\"error\":{}}}", json_str(&msg)));
        if req.method != "GET" && req.header("x-charon") != Some("1") {
            return err((403, "missing X-Charon header".into())); // a page on another site cannot set it
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") => (200, "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
            ("GET", "/s/app.js") => (200, "text/javascript; charset=utf-8", PAGE_JS.as_bytes().to_vec()),
            ("GET", "/s/style.css") => (200, "text/css; charset=utf-8", CSS.as_bytes().to_vec()),
            ("GET", "/api/status") => json(200, self.status_json()),
            ("PUT", "/api/stage") => match self.stage(req, body) {
                Ok(name) => json(200, format!("{{\"ok\":true,\"name\":{}}}", json_str(&name))),
                Err(e) => err(e),
            },
            ("POST", "/api/clear") => {
                self.clear();
                json(200, "{\"ok\":true}".into())
            }
            ("POST", "/api/share") => {
                let len = req.content_length().unwrap_or(0).min(256 * 1024);
                let mut s = String::new();
                if body.take(len).read_to_string(&mut s).is_err() {
                    return err((400, "send UTF-8 JSON".into()));
                }
                match self.start(&s) {
                    Ok(()) => json(200, self.status_json()),
                    Err(e) => err(e),
                }
            }
            ("POST", "/api/stop") => {
                self.stop();
                json(200, "{\"ok\":true}".into())
            }
            _ => err((404, "not found".into())),
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.staging);
    }
}

fn handle(app: &App, stream: TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut r = BufReader::new(stream);
    let req = match http::read_head(&mut r) {
        Ok(req) => req,
        Err(_) => return Ok(()),
    };
    let (code, ctype, body) = app.route(&req, &mut r);
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        _ => "Error",
    };
    http::respond(r.get_mut(), code, reason, ctype, &body)
}

/// Serve the window on 127.0.0.1 until the process is stopped.
pub fn serve(app: Arc<App>, port: u16) -> io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    println!("Charon on http://127.0.0.1:{}/", listener.local_addr()?.port());
    for stream in listener.incoming().flatten() {
        let app = app.clone();
        std::thread::spawn(move || {
            let _ = handle(&app, stream);
        });
    }
    Ok(())
}
