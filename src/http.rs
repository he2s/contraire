//! A deliberately small HTTP/1.1 server: one thread per connection, keep-alive,
//! Content-Length bodies only. Enough for a LAN dashboard, nothing more.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const MAX_LINE: u64 = 8 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_BODY: usize = 64 * 1024;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
}

impl Response {
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Response { status, content_type, body: body.into(), headers: Vec::new() }
    }

    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::new(status, "application/json", body)
    }

    pub fn error(status: u16, msg: &str) -> Self {
        Self::json(status, serde_json::json!({ "message": msg }).to_string())
    }

    pub fn with_header(mut self, k: &'static str, v: impl Into<String>) -> Self {
        self.headers.push((k, v.into()));
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn read_line(r: &mut impl BufRead, buf: &mut String) -> io::Result<usize> {
    buf.clear();
    let n = r.take(MAX_LINE).read_line(buf)?;
    if n > 0 && !buf.ends_with('\n') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    Ok(n)
}

/// Returns Ok(None) when the client closed the connection cleanly.
fn read_request(r: &mut impl BufRead) -> io::Result<Option<Request>> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let mut line = String::new();
    if read_line(r, &mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target), Some(_version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("malformed request line"));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut req = Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers: Vec::new(),
        body: Vec::new(),
    };

    loop {
        if read_line(r, &mut line)? == 0 {
            return Err(bad("unexpected eof"));
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if req.headers.len() >= MAX_HEADERS {
            return Err(bad("too many headers"));
        }
        let (k, v) = l.split_once(':').ok_or_else(|| bad("malformed header"))?;
        req.headers.push((k.trim().to_string(), v.trim().to_string()));
    }

    if req.header("transfer-encoding").is_some() {
        return Err(bad("chunked requests unsupported"));
    }
    let len: usize = req
        .header("content-length")
        .map(|v| v.parse().map_err(|_| bad("bad content-length")))
        .transpose()?
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err(bad("body too large"));
    }
    req.body.resize(len, 0);
    r.read_exact(&mut req.body)?;
    Ok(Some(req))
}

fn write_response(w: &mut impl Write, res: &Response, head_only: bool, keep_alive: bool) -> io::Result<()> {
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n",
        res.status,
        reason(res.status),
        res.content_type,
        res.body.len(),
        if keep_alive { "keep-alive" } else { "close" },
    );
    for (k, v) in &res.headers {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    w.write_all(out.as_bytes())?;
    if !head_only {
        w.write_all(&res.body)?;
    }
    w.flush()
}

pub fn serve(stream: TcpStream, handler: impl Fn(&Request) -> Response) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);

    loop {
        let req = match read_request(&mut reader) {
            Ok(Some(req)) => req,
            Ok(None) => return,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let _ = write_response(&mut writer, &Response::error(400, &e.to_string()), false, false);
                return;
            }
            Err(_) => return,
        };
        let keep_alive = !req
            .header("connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"));
        let res = handler(&req);
        if write_response(&mut writer, &res, req.method == "HEAD", keep_alive).is_err() || !keep_alive {
            return;
        }
    }
}

pub fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Constant-time comparison so the password check doesn't leak via timing.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"admin:secret"), "YWRtaW46c2VjcmV0");
    }

    #[test]
    fn parses_request_with_body() {
        let raw = b"POST /api/x?a=1&b=two HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\n\r\nbodyGET";
        let req = read_request(&mut &raw[..]).unwrap().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api/x");
        assert_eq!(req.param("b"), Some("two"));
        assert_eq!(req.header("HOST"), Some("h"));
        assert_eq!(req.body, b"body");
    }
}
