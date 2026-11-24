//! The real server on 127.0.0.1: plain HTTP and TLS, send and receive, wrong links, limits.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use charon::server::{Log, Server};
use charon::share::Share;

/// port, token, stop switch, the server thread (returns why it ended), log lines
type Running = (u16, String, Arc<AtomicBool>, JoinHandle<String>, Arc<Mutex<Vec<String>>>);

fn start(share: Share, tls: Option<Arc<rustls::ServerConfig>>) -> Running {
    let token = share.token.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let lines = Arc::new(Mutex::new(vec![]));
    let l2 = lines.clone();
    let log: Log = Arc::new(move |s: &str| l2.lock().unwrap().push(s.to_string()));
    let stop = Arc::new(AtomicBool::new(false));
    let server = Server { share: Arc::new(Mutex::new(share)), tls, log, stop: stop.clone() };
    let h = std::thread::spawn(move || server.run(listener).unwrap());
    (port, token, stop, h, lines)
}

fn raw(port: u16, req: &[u8]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(req).unwrap();
    s.shutdown(std::net::Shutdown::Write).unwrap(); // the request is complete (a short body ends here)
    let mut out = vec![];
    s.read_to_end(&mut out).unwrap();
    split(&out)
}

fn split(out: &[u8]) -> (u16, String, Vec<u8>) {
    let end = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&out[..end]).to_string();
    let status = head[9..12].parse().unwrap();
    (status, head, out[end + 4..].to_vec())
}

fn get(port: u16, path: &str) -> (u16, String, Vec<u8>) {
    raw(port, format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
}

#[test]
fn send_once_then_gone() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("<b>report.txt");
    std::fs::write(&f, b"secret contents").unwrap();
    let (port, token, _stop, h, lines) = start(Share::send(&[f], 5, 1).unwrap(), None);

    let (st, head, _) = get(port, &format!("/{token}"));
    assert_eq!(st, 302);
    assert!(head.contains(&format!("Location: /{token}/")));
    let (st, head, body) = get(port, &format!("/{token}/"));
    assert_eq!(st, 200);
    let page = String::from_utf8(body).unwrap();
    assert!(page.contains("breport.txt"), "the name is cleaned: {page}");
    assert!(!page.contains("<b>report"));
    assert!(head.contains("Referrer-Policy: no-referrer") && head.contains("Content-Security-Policy: default-src 'none'"));
    let (st, head, body) = get(port, &format!("/{token}/f/0"));
    assert_eq!((st, body.as_slice()), (200, &b"secret contents"[..]));
    assert!(head.contains("filename=\"breport.txt\""));
    assert_eq!(h.join().unwrap(), "everything was downloaded");
    assert!(lines.lock().unwrap()[0].contains("downloaded breport.txt"));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "the server is gone");
}

#[test]
fn wrong_links_close_the_share() {
    let d = tempfile::tempdir().unwrap();
    let (port, token, _stop, h, _) = start(Share::receive(d.path().into(), 5, 10, 0), None);
    assert_eq!(get(port, "/s/app.js").0, 200);
    assert_eq!(get(port, &format!("/{}/", "0".repeat(32))).0, 404);
    assert_eq!(raw(port, b"garbage\r\n\r\n").0, 400);
    for _ in 0..30 {
        // the share closes itself during this loop: later connections may be refused or reset
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.write_all(b"GET /guess/ HTTP/1.1\r\n\r\n");
            let _ = s.read_to_end(&mut vec![]);
        }
    }
    assert!(h.join().unwrap().contains("guessing"));
    let _ = token;
}

#[test]
fn receive_cleans_names_and_limits_size() {
    let d = tempfile::tempdir().unwrap();
    let (port, token, stop, h, _) = start(Share::receive(d.path().into(), 5, 10, 0), None);
    let put = |name: &str, body: &[u8], claimed: usize| {
        let mut req = format!("PUT /{token}/up?name={name} HTTP/1.1\r\nHost: x\r\nContent-Length: {claimed}\r\n\r\n").into_bytes();
        req.extend_from_slice(body);
        raw(port, &req)
    };
    let (st, _, body) = put("..%2F..%2Fevil%3C%3E.txt", b"hello", 5);
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(std::fs::read(d.path().join("evil.txt")).unwrap(), b"hello");
    assert_eq!(put("evil.txt", b"again", 5).0, 200);
    assert!(d.path().join("evil (1).txt").exists(), "never replaces a file");
    assert_eq!(put("big.bin", b"", 11).0, 413);
    assert_eq!(raw(port, format!("PUT /{token}/up?name=x HTTP/1.1\r\n\r\n").as_bytes()).0, 411);
    assert_eq!(put("cut.bin", b"abc", 10).0, 400, "a short body is an interrupted upload");
    let names: Vec<String> = std::fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(!names.iter().any(|n| n.contains("charon-part") || n.starts_with("cut")), "{names:?}");
    stop.store(true, Ordering::SeqCst);
    assert_eq!(h.join().unwrap(), "stopped");
}

#[test]
fn over_tls() {
    let d = tempfile::tempdir().unwrap();
    let id = charon::tls::identity(d.path(), &["127.0.0.1".parse().unwrap()]).unwrap();
    let cfg = charon::tls::server_config(&id).unwrap();
    let f = d.path().join("a.txt");
    std::fs::write(&f, b"over tls").unwrap();
    let (port, token, _stop, h, _) = start(Share::send(&[f], 5, 1).unwrap(), Some(cfg));

    let mut roots = rustls::RootCertStore::empty();
    roots.add(id.cert.clone()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(client), name).unwrap();
    let mut tls = rustls::StreamOwned::new(conn, TcpStream::connect(("127.0.0.1", port)).unwrap());
    tls.write_all(format!("GET /{token}/f/0 HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).unwrap();
    let mut out = vec![];
    let _ = tls.read_to_end(&mut out);
    let (st, _, body) = split(&out);
    assert_eq!((st, body.as_slice()), (200, &b"over tls"[..]));
    assert_eq!(h.join().unwrap(), "everything was downloaded");
}

#[test]
fn a_note_is_shown_once_and_never_in_the_page() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("a.txt");
    std::fs::write(&f, b"file").unwrap();
    let share = Share::send_text("hunter2 <b>&".into(), &[f], 5, 1).unwrap();
    let (port, token, _stop, h, lines) = start(share, None);
    let (st, _, body) = get(port, &format!("/{token}/"));
    let page = String::from_utf8(body).unwrap();
    assert_eq!(st, 200);
    assert!(!page.contains("hunter2"), "the note is fetched on demand only");
    assert!(page.contains("A note and 1 file(s)") && page.contains("data-i=\"0\""));
    let (st, head, body) = get(port, &format!("/{token}/f/0"));
    assert_eq!((st, body.as_slice()), (200, &b"hunter2 <b>&"[..]));
    assert!(head.contains("text/plain; charset=utf-8") && head.contains("Cache-Control: no-store"));
    assert_eq!(get(port, &format!("/{token}/f/0")).0, 410, "shown once");
    let (st, _, page) = get(port, &format!("/{token}/"));
    assert!(st == 200 && String::from_utf8(page).unwrap().contains("already shown"));
    assert_eq!(get(port, &format!("/{token}/f/1")).0, 200);
    assert_eq!(h.join().unwrap(), "everything was downloaded");
    assert!(lines.lock().unwrap()[0].contains("was shown the note"));
    assert!(Share::send_text(String::new(), &[], 5, 1).is_err());
    assert!(Share::send_text("x".repeat(charon::share::MAX_TEXT + 1), &[], 5, 1).is_err());
}
