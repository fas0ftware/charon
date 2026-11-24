//! `charon app`: the local window's API, end to end with a real share on 127.0.0.1.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpStream};

use charon::app::App;
use charon::http;

fn call(app: &App, raw: &[u8]) -> (u16, String) {
    let mut r = raw;
    let req = http::read_head(&mut r).unwrap();
    let (code, _, body) = app.route(&req, &mut r);
    (code, String::from_utf8_lossy(&body).into_owned())
}

fn req(method: &str, path: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut v = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:8766\r\n{headers}Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    v.extend_from_slice(body);
    v
}

fn app() -> App {
    std::env::set_var("XDG_DATA_HOME", std::env::temp_dir().join("charon-app-test-data"));
    App::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, false).unwrap()
}

#[test]
fn local_guards() {
    let a = app();
    assert_eq!(call(&a, b"GET / HTTP/1.1\r\nHost: evil.example:8766\r\n\r\n").0, 400, "DNS rebinding");
    assert_eq!(call(&a, &req("POST", "/api/clear", "", b"")).0, 403, "no X-Charon header: refused");
    assert_eq!(call(&a, &req("PUT", "/api/stage?name=x", "", b"abc")).0, 403);
    let (code, page) = call(&a, b"GET / HTTP/1.1\r\nHost: 127.0.0.1:8766\r\n\r\n");
    assert!(code == 200 && page.contains("<title>Charon</title>"));
    assert_eq!(call(&a, &req("POST", "/api/share", "X-Charon: 1\r\n", b"{}")).0, 400, "nothing to share");
}

#[test]
fn stage_share_download_and_finish() {
    let a = app();
    let staging = a.staging.clone();
    let (code, body) = call(&a, &req("PUT", "/api/stage?name=..%2F..%2Frapor%20%C5%9Eubat.pdf", "X-Charon: 1\r\n", b"PDFDATA"));
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("rapor Şubat.pdf"));
    assert!(staging.join("rapor Şubat.pdf").exists(), "names are cleaned, the file stays in the staging folder");
    let (code, st) = call(&a, &req("POST", "/api/share", "X-Charon: 1\r\n",
                                   r#"{"text": "wifi: \"kıyı\"", "minutes": 5, "downloads": 1}"#.as_bytes()));
    assert_eq!(code, 200, "{st}");
    assert!(st.contains("\"url\":\"http://127.0.0.1:") && !st.contains("<svg") && st.contains("\\u003csvg"));
    assert_eq!(call(&a, &req("POST", "/api/share", "X-Charon: 1\r\n", b"{}")).0, 409, "one share at a time");
    let url = st.split("\"url\":\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    let (hostport, path) = url.trim_start_matches("http://").split_once('/').unwrap();
    let get = |p: String| {
        let mut s = TcpStream::connect(hostport).unwrap();
        s.write_all(format!("GET /{p} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = vec![];
        s.read_to_end(&mut out).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    };
    assert!(get(format!("{path}f/0")).ends_with("wifi: \"kıyı\""), "the note, unescaped");
    assert!(get(format!("{path}f/1")).ends_with("PDFDATA"));
    for _ in 0..50 {
        if call(&a, b"GET /api/status HTTP/1.1\r\nHost: localhost\r\n\r\n").1.contains("everything was downloaded") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let (_, st) = call(&a, b"GET /api/status HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(st.contains("\"done\":\"everything was downloaded\""), "{st}");
    assert!(st.contains("was shown the note"));
    assert_eq!(call(&a, &req("POST", "/api/clear", "X-Charon: 1\r\n", b"")).0, 200);
    assert!(!staging.join("rapor Şubat.pdf").exists());
    drop(a);
    assert!(!staging.exists(), "the staging folder goes away with the app");
}

#[test]
fn stop_ends_a_running_share() {
    let a = app();
    call(&a, &req("PUT", "/api/stage?name=a.txt", "X-Charon: 1\r\n", b"a"));
    assert_eq!(call(&a, &req("POST", "/api/share", "X-Charon: 1\r\n", b"{}")).0, 200);
    assert_eq!(call(&a, &req("POST", "/api/stop", "X-Charon: 1\r\n", b"")).0, 200);
    for _ in 0..50 {
        if call(&a, b"GET /api/status HTTP/1.1\r\nHost: localhost\r\n\r\n").1.contains("\"done\":\"stopped\"") {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the share did not stop");
}

#[test]
fn qr_is_svg() {
    assert!(charon::app::qr_svg("https://192.0.2.1:8765/x/").starts_with("<?xml") || charon::app::qr_svg("x").contains("<svg"));
}
