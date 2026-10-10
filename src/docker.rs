//! Minimal Docker Engine API client over the unix socket.
//! One connection per call, `Connection: close`, so the whole response is read to EOF.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub struct Docker {
    socket: String,
}

pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Docker {
    pub fn new(socket: String) -> Self {
        Docker { socket }
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    pub fn call(&self, method: &str, path: &str, body: Option<&[u8]>, timeout: Duration) -> io::Result<Reply> {
        let mut s = UnixStream::connect(&self.socket)?;
        s.set_read_timeout(Some(timeout))?;
        s.set_write_timeout(Some(Duration::from_secs(10)))?;
        let body = body.unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes())?;
        s.write_all(body)?;

        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?;
        parse_response(raw)
    }

    pub fn get(&self, path: &str) -> io::Result<Reply> {
        self.call("GET", path, None, Duration::from_secs(30))
    }
}

fn parse_response(raw: Vec<u8>) -> io::Result<Reply> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed docker response");
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(bad)?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| bad())?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(bad)?;
    let chunked = lines.any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding") && v.trim().eq_ignore_ascii_case("chunked")
        })
    });
    let body = &raw[split + 4..];
    let body = if chunked { dechunk(body).ok_or_else(bad)? } else { body.to_vec() };
    Ok(Reply { status, body })
}

fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let eol = data.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&data[..eol]).ok()?;
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        data = &data[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(data.get(..size)?);
        data = data.get(size + 2..)?;
    }
}

/// Container logs without a TTY come multiplexed: 8-byte header [stream,0,0,0,len_be32] + payload.
/// TTY containers send raw bytes. Detect which one we got and flatten to plain text.
pub fn demux_logs(data: &[u8]) -> Vec<u8> {
    let looks_muxed = data.len() >= 8 && data[0] <= 2 && data[1..4] == [0, 0, 0];
    if !looks_muxed {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    let mut rest = data;
    while rest.len() >= 8 {
        let len = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
        let end = (8 + len).min(rest.len());
        out.extend_from_slice(&rest[8..end]);
        rest = &rest[end..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n".to_vec();
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"Wikipedia");
    }

    #[test]
    fn demuxes_log_frames() {
        let mut d = vec![1, 0, 0, 0, 0, 0, 0, 3];
        d.extend_from_slice(b"hi\n");
        d.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 4]);
        d.extend_from_slice(b"err\n");
        assert_eq!(demux_logs(&d), b"hi\nerr\n");
        assert_eq!(demux_logs(b"plain tty output\n"), b"plain tty output\n");
    }
}
