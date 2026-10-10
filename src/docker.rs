//! Minimal Docker Engine API client. One connection per call with `Connection: close`.
//! Transports: unix socket (Linux/macOS), TCP (`tcp://host:port`, plain HTTP only) and
//! Windows named pipes (`npipe://`). Buffered calls read to EOF; streaming calls
//! (events, stats, logs) decode chunked transfer-encoding incrementally.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[derive(Clone)]
pub enum Endpoint {
    #[cfg(unix)]
    Unix(String),
    Tcp(String),
    #[cfg(windows)]
    Pipe(String),
}

/// Also used in client mode to talk to a remote contraire server over `tcp://`.
#[derive(Clone)]
pub struct Docker {
    endpoint: Endpoint,
    /// Extra request header lines (each ending in CRLF), e.g. `Authorization`.
    headers: String,
}

pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

trait Conn: Read + Write + Send {
    fn set_timeout(&self, d: Duration) -> io::Result<()>;
}

#[cfg(unix)]
impl Conn for std::os::unix::net::UnixStream {
    fn set_timeout(&self, d: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(d))?;
        self.set_write_timeout(Some(Duration::from_secs(10)))
    }
}

impl Conn for TcpStream {
    fn set_timeout(&self, d: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(d))?;
        self.set_write_timeout(Some(Duration::from_secs(10)))
    }
}

/// Named pipes opened as files have no timeouts; idle streams on Windows only notice a
/// closed client on their next event.
#[cfg(windows)]
impl Conn for std::fs::File {
    fn set_timeout(&self, _: Duration) -> io::Result<()> {
        Ok(())
    }
}

pub const DEFAULT_HOST: &str = if cfg!(windows) { "npipe:////./pipe/docker_engine" } else { "unix:///var/run/docker.sock" };

impl Docker {
    /// Accepts `unix:///path`, a bare socket path, `tcp://host:port` or `npipe:////./pipe/name`.
    pub fn parse(host: &str) -> Result<Self, String> {
        let endpoint = match host.split_once("://") {
            Some(("tcp", addr)) if !addr.is_empty() => Endpoint::Tcp(addr.trim_end_matches('/').to_string()),
            #[cfg(unix)]
            Some(("unix", path)) if !path.is_empty() => Endpoint::Unix(path.to_string()),
            #[cfg(unix)]
            None if !host.is_empty() => Endpoint::Unix(host.to_string()),
            #[cfg(windows)]
            Some(("npipe", path)) if !path.is_empty() => Endpoint::Pipe(path.replace('/', "\\")),
            #[cfg(windows)]
            None if host.starts_with(r"\\") => Endpoint::Pipe(host.to_string()),
            _ => return Err(format!("unsupported docker host '{host}'")),
        };
        Ok(Docker { endpoint, headers: String::new() })
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push_str(&format!("{name}: {value}\r\n"));
        self
    }

    pub fn describe(&self) -> String {
        match &self.endpoint {
            #[cfg(unix)]
            Endpoint::Unix(p) => p.clone(),
            Endpoint::Tcp(a) => format!("tcp://{a}"),
            #[cfg(windows)]
            Endpoint::Pipe(p) => p.clone(),
        }
    }

    fn connect(&self, timeout: Duration) -> io::Result<Box<dyn Conn>> {
        let c: Box<dyn Conn> = match &self.endpoint {
            #[cfg(unix)]
            Endpoint::Unix(p) => Box::new(std::os::unix::net::UnixStream::connect(p)?),
            Endpoint::Tcp(a) => Box::new(TcpStream::connect(a)?),
            #[cfg(windows)]
            Endpoint::Pipe(p) => Box::new(std::fs::OpenOptions::new().read(true).write(true).open(p)?),
        };
        c.set_timeout(timeout)?;
        Ok(c)
    }

    fn send(&self, method: &str, path: &str, body: &[u8], timeout: Duration) -> io::Result<Box<dyn Conn>> {
        let mut c = self.connect(timeout)?;
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n{}\r\n",
            body.len(),
            self.headers
        );
        c.write_all(head.as_bytes())?;
        c.write_all(body)?;
        Ok(c)
    }

    pub fn call(&self, method: &str, path: &str, body: Option<&[u8]>, timeout: Duration) -> io::Result<Reply> {
        let mut c = self.send(method, path, body.unwrap_or_default(), timeout)?;
        let mut raw = Vec::new();
        c.read_to_end(&mut raw)?;
        parse_response(raw)
    }

    pub fn get(&self, path: &str) -> io::Result<Reply> {
        self.call("GET", path, None, Duration::from_secs(30))
    }

    /// Open a streaming GET. Reads time out after `idle` so callers can do housekeeping.
    /// Non-200 replies are read fully and returned as `Err(Reply)`.
    pub fn stream(&self, path: &str, idle: Duration) -> io::Result<Result<Stream, Reply>> {
        let mut c = self.send("GET", path, b"", Duration::from_secs(30))?;
        let mut raw = Vec::with_capacity(4096);
        let mut tmp = [0u8; 4096];
        let split = loop {
            if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            if raw.len() > 16 * 1024 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "docker headers too large"));
            }
            let n = c.read(&mut tmp)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "docker closed connection"));
            }
            raw.extend_from_slice(&tmp[..n]);
        };
        let (status, chunked) = parse_head(&raw[..split])?;
        if status != 200 {
            c.read_to_end(&mut raw)?;
            return parse_response(raw).map(Err);
        }
        c.set_timeout(idle)?;
        raw.drain(..split + 4);
        let state = if chunked { Chunk::Size } else { Chunk::Raw };
        Ok(Ok(Stream { conn: c, raw, state }))
    }
}

fn parse_head(head: &[u8]) -> io::Result<(u16, bool)> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed docker response");
    let head = std::str::from_utf8(head).map_err(|_| bad())?;
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
    Ok((status, chunked))
}

fn parse_response(raw: Vec<u8>) -> io::Result<Reply> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed docker response");
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(bad)?;
    let (status, chunked) = parse_head(&raw[..split])?;
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

enum Chunk {
    Raw,
    Size,
    Data(usize),
    Crlf,
    Done,
}

/// An open streaming response. `read_into` appends decoded body bytes to `out`.
pub struct Stream {
    conn: Box<dyn Conn>,
    raw: Vec<u8>,
    state: Chunk,
}

#[derive(Clone, Copy)]
pub enum Tick {
    Data,
    Idle,
    Eof,
}

impl Stream {
    pub fn read_into(&mut self, out: &mut Vec<u8>) -> io::Result<Tick> {
        let mut tmp = [0u8; 8192];
        loop {
            let before = out.len();
            self.decode(out)?;
            if out.len() > before {
                return Ok(Tick::Data);
            }
            if matches!(self.state, Chunk::Done) {
                return Ok(Tick::Eof);
            }
            match self.conn.read(&mut tmp) {
                Ok(0) => return Ok(Tick::Eof),
                Ok(n) => self.raw.extend_from_slice(&tmp[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    return Ok(Tick::Idle);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn decode(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        let mut pos = 0;
        loop {
            let avail = &self.raw[pos..];
            match self.state {
                Chunk::Raw => {
                    out.extend_from_slice(avail);
                    pos = self.raw.len();
                    break;
                }
                Chunk::Size => {
                    let Some(eol) = avail.windows(2).position(|w| w == b"\r\n") else { break };
                    let size = std::str::from_utf8(&avail[..eol])
                        .ok()
                        .and_then(|s| usize::from_str_radix(s.split(';').next()?.trim(), 16).ok())
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
                    pos += eol + 2;
                    self.state = if size == 0 { Chunk::Done } else { Chunk::Data(size) };
                }
                Chunk::Data(n) => {
                    if avail.is_empty() {
                        break;
                    }
                    let take = n.min(avail.len());
                    out.extend_from_slice(&avail[..take]);
                    pos += take;
                    self.state = if take == n { Chunk::Crlf } else { Chunk::Data(n - take) };
                }
                Chunk::Crlf => {
                    if avail.len() < 2 {
                        break;
                    }
                    pos += 2;
                    self.state = Chunk::Size;
                }
                Chunk::Done => break,
            }
        }
        self.raw.drain(..pos);
        Ok(())
    }
}

/// Container logs without a TTY come multiplexed: 8-byte header [stream,0,0,0,len_be32] + payload.
/// TTY containers send raw bytes. Detect which one we got and flatten to plain text.
pub fn demux_logs(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut d = Demux::default();
    d.feed(data, &mut out);
    out
}

/// Incremental log demultiplexer; keeps partial frames between calls.
#[derive(Default)]
pub struct Demux {
    muxed: Option<bool>,
    pending: Vec<u8>,
    /// Payload bytes still owed by the current frame.
    left: usize,
}

impl Demux {
    pub fn feed(&mut self, mut data: &[u8], out: &mut Vec<u8>) {
        if self.muxed.is_none() {
            self.pending.extend_from_slice(data);
            if self.pending.len() < 8 {
                return;
            }
            let p = &self.pending;
            self.muxed = Some(p[0] <= 2 && p[1..4] == [0, 0, 0]);
            let buf = std::mem::take(&mut self.pending);
            return self.feed(&buf, out);
        }
        if self.muxed == Some(false) {
            out.extend_from_slice(data);
            return;
        }
        loop {
            if self.left > 0 {
                let take = self.left.min(data.len());
                out.extend_from_slice(&data[..take]);
                self.left -= take;
                data = &data[take..];
            }
            if data.is_empty() {
                return;
            }
            // Need a full 8-byte header; stash partial headers.
            let need = 8 - self.pending.len();
            let take = need.min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() < 8 {
                return;
            }
            let h = &self.pending;
            self.left = u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as usize;
            self.pending.clear();
        }
    }

    /// Flush bytes held back while sniffing the format (stream ended before 8 bytes).
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.muxed.is_none() {
            out.append(&mut self.pending);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ~1 MB of log text in 100-byte lines, as Docker's multiplexed frames.
    fn muxed_logs() -> Vec<u8> {
        let line = b"2026-10-10T09:50:03.701695991Z GET /api/items 200 1.2ms user=42 trace=4bf92f3577b34da6a3ce929d0e0e47\n";
        let mut out = Vec::new();
        while out.len() < 1 << 20 {
            out.extend_from_slice(&[1, 0, 0, 0]);
            out.extend_from_slice(&(line.len() as u32).to_be_bytes());
            out.extend_from_slice(line);
        }
        out
    }

    #[test]
    #[ignore]
    fn bench_docker_dechunk_stream() {
        let body = muxed_logs();
        let mut chunked = Vec::new();
        for c in body.chunks(4096) {
            chunked.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
            chunked.extend_from_slice(c);
            chunked.extend_from_slice(b"\r\n");
        }
        let mut s = Stream { conn: Box::new(Empty), raw: Vec::with_capacity(16384), state: Chunk::Size };
        let mut out = Vec::with_capacity(16384);
        crate::bench::run("docker: dechunk 1 MB (8K reads)", body.len(), || {
            s.state = Chunk::Size;
            for piece in chunked.chunks(8192) {
                s.raw.extend_from_slice(piece);
                s.decode(&mut out).unwrap();
                out.clear();
            }
        });
    }

    #[test]
    #[ignore]
    fn bench_docker_demux_logs() {
        let body = muxed_logs();
        let mut out = Vec::with_capacity(16384);
        crate::bench::run("docker: demux 1 MB (8K reads)", body.len(), || {
            let mut d = Demux::default();
            for piece in body.chunks(8192) {
                d.feed(piece, &mut out);
                out.clear();
            }
        });
    }

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

    #[test]
    fn demuxes_across_arbitrary_splits() {
        let mut d = vec![1, 0, 0, 0, 0, 0, 0, 6];
        d.extend_from_slice(b"hello\n");
        d.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 4]);
        d.extend_from_slice(b"err\n");
        for split in 1..d.len() {
            let (mut m, mut out) = (Demux::default(), Vec::new());
            for piece in d.chunks(split) {
                m.feed(piece, &mut out);
            }
            assert_eq!(out, b"hello\nerr\n", "split size {split}");
        }
    }

    #[test]
    fn parses_hosts() {
        assert!(Docker::parse("tcp://10.0.0.2:2375").is_ok());
        assert!(Docker::parse("ssh://pi").is_err());
        #[cfg(unix)]
        {
            assert_eq!(Docker::parse("unix:///run/d.sock").unwrap().describe(), "/run/d.sock");
            assert_eq!(Docker::parse("/run/d.sock").unwrap().describe(), "/run/d.sock");
        }
    }

    #[test]
    fn stream_decoder_handles_split_chunks() {
        let body = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        for split in 1..body.len() {
            let mut s = Stream { conn: Box::new(Empty), raw: Vec::new(), state: Chunk::Size };
            let mut out = Vec::new();
            for piece in body.chunks(split) {
                s.raw.extend_from_slice(piece);
                s.decode(&mut out).unwrap();
            }
            assert_eq!(out, b"hello world", "split size {split}");
            assert!(matches!(s.state, Chunk::Done));
        }
    }

    struct Empty;
    impl Read for Empty {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }
    impl Write for Empty {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Conn for Empty {
        fn set_timeout(&self, _: Duration) -> io::Result<()> {
            Ok(())
        }
    }
}
