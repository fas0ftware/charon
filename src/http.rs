//! A deliberately small HTTP/1.1: one request per connection, Content-Length bodies only,
//! bounded headers. Enough for a browser to fetch a page, download a file and upload one.

use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};

pub const MAX_HEAD: usize = 16 * 1024;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|s| s.as_str())
    }
    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")?.trim().parse().ok()
    }
}

#[derive(Debug, PartialEq)]
pub enum HeadError {
    Closed,
    TooLarge,
    Bad,
}

pub fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Read the request line and headers (never more than MAX_HEAD bytes).
pub fn read_head<R: BufRead>(r: &mut R) -> Result<Request, HeadError> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        let n = r.by_ref().take((MAX_HEAD - head.len() + 1) as u64).read_until(b'\n', &mut head).map_err(|_| HeadError::Closed)?;
        if n == 0 {
            return Err(if head.is_empty() { HeadError::Closed } else { HeadError::Bad });
        }
        if head.len() > MAX_HEAD {
            return Err(HeadError::TooLarge);
        }
        let line = &head[before..];
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    let text = std::str::from_utf8(&head).map_err(|_| HeadError::Bad)?;
    let mut lines = text.split("\r\n").flat_map(|l| l.split('\n'));
    let first = lines.next().ok_or(HeadError::Bad)?;
    let mut parts = first.split(' ');
    let (method, target, version) = (parts.next().ok_or(HeadError::Bad)?, parts.next().ok_or(HeadError::Bad)?, parts.next().ok_or(HeadError::Bad)?);
    if !version.starts_with("HTTP/1.") || !target.starts_with('/') || parts.next().is_some() {
        return Err(HeadError::Bad);
    }
    let mut headers = HashMap::new();
    for l in lines.filter(|l| !l.is_empty()) {
        let (k, v) = l.split_once(':').ok_or(HeadError::Bad)?;
        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let query = q
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            Some((percent_decode(k)?, percent_decode(v)?))
        })
        .collect();
    Ok(Request { method: method.to_string(), path: percent_decode(path).ok_or(HeadError::Bad)?, query, headers })
}

pub const SECURITY_HEADERS: &[(&str, &str)] = &[
    ("Content-Security-Policy", "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'"),
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("Referrer-Policy", "no-referrer"), // the link is the secret: never leak it in a Referer
    ("Cache-Control", "no-store"),
    ("Connection", "close"),
];

pub fn write_head<W: Write>(w: &mut W, status: u16, reason: &str, headers: &[(&str, String)], len: u64) -> io::Result<()> {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {len}\r\n");
    for (k, v) in SECURITY_HEADERS {
        out += &format!("{k}: {v}\r\n");
    }
    for (k, v) in headers {
        out += &format!("{k}: {v}\r\n");
    }
    out += "\r\n";
    w.write_all(out.as_bytes())
}

pub fn respond<W: Write>(w: &mut W, status: u16, reason: &str, ctype: &str, body: &[u8]) -> io::Result<()> {
    write_head(w, status, reason, &[("Content-Type", ctype.to_string())], body.len() as u64)?;
    w.write_all(body)?;
    w.flush()
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out += "&amp;",
            '<' => out += "&lt;",
            '>' => out += "&gt;",
            '"' => out += "&quot;",
            '\'' => out += "&#39;",
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_parsing() {
        let raw = b"PUT /abc/up?name=a%20b.txt&x HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello";
        let mut r = &raw[..];
        let req = read_head(&mut r).unwrap();
        assert_eq!((req.method.as_str(), req.path.as_str()), ("PUT", "/abc/up"));
        assert_eq!(req.query["name"], "a b.txt");
        assert_eq!(req.content_length(), Some(5));
        assert_eq!(r, b"hello", "the body is left unread");
    }

    #[test]
    fn head_limits_and_garbage() {
        let mut big = b"GET / HTTP/1.1\r\nX: ".to_vec();
        big.extend(vec![b'a'; MAX_HEAD]);
        big.extend(b"\r\n\r\n");
        assert_eq!(read_head(&mut &big[..]).unwrap_err(), HeadError::TooLarge);
        assert_eq!(read_head(&mut &b""[..]).unwrap_err(), HeadError::Closed);
        assert_eq!(read_head(&mut &b"GET / HTTP/1.1\r\nHost"[..]).unwrap_err(), HeadError::Bad);
        assert_eq!(read_head(&mut &b"GET http://x/ HTTP/1.1\r\n\r\n"[..]).unwrap_err(), HeadError::Bad);
        assert_eq!(read_head(&mut &b"GET /%zz HTTP/1.1\r\n\r\n"[..]).unwrap_err(), HeadError::Bad);
    }

    #[test]
    fn escaping() {
        assert_eq!(html_escape("<img src=x onerror='a'>&"), "&lt;img src=x onerror=&#39;a&#39;&gt;&amp;");
        assert_eq!(percent_decode("%E2%9C%93+ok"), Some("✓ ok".into()));
        assert_eq!(percent_decode("%4"), None);
    }
}
