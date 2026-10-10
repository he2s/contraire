//! Live updates pushed to the browser: one Server-Sent Events stream per tab.
//!
//! Each Docker stream (events, plus stats and logs for the container open in the UI)
//! is relayed by its own small thread into a bounded channel; the connection thread
//! batches what arrives and writes it out. No locks: shutdown is an `AtomicBool` that
//! producers check whenever their socket read times out.

use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::time::Duration;

use serde_json::{Value, json};

use crate::docker::{Demux, Docker, Tick};

/// Socket read timeout for Docker streams and the SSE heartbeat interval.
const IDLE: Duration = Duration::from_secs(15);
/// Only events that change what the UI shows (health-check `exec_*` events fire every few
/// seconds per container and are left out by Docker itself):
/// `filters={"type":["container","image"],"event":["create","start","restart","die","oom","pause","unpause","destroy","health_status","pull","delete","untag","tag"]}`
pub const EVENTS: &str = "/events?filters=%7B%22type%22%3A%5B%22container%22%2C%22image%22%5D%2C%22event%22%3A%5B%22create%22%2C%22start%22%2C%22restart%22%2C%22die%22%2C%22oom%22%2C%22pause%22%2C%22unpause%22%2C%22destroy%22%2C%22health_status%22%2C%22pull%22%2C%22delete%22%2C%22untag%22%2C%22tag%22%5D%7D";
/// Upper bound for one write to the browser, so a log burst can't balloon memory.
const MAX_BATCH: usize = 256 * 1024;

pub fn stream(docker: Docker, container: Option<String>, tail: u32) -> impl FnOnce(&mut TcpStream) -> io::Result<()> {
    move |w| {
        let (tx, rx) = sync_channel::<String>(32);
        let closed = Arc::new(AtomicBool::new(false));
        let _close = CloseOnDrop(closed.clone());

        relay(&docker, EVENTS.to_string(), &tx, &closed, events());
        if let Some(id) = container {
            relay(&docker, format!("/containers/{id}/stats?stream=true"), &tx, &closed, stats());
            let path = format!("/containers/{id}/logs?follow=1&stdout=1&stderr=1&timestamps=1&tail={tail}");
            relay(&docker, path, &tx, &closed, logs());
        }
        drop(tx);

        w.write_all(b"retry: 2000\n\n")?;
        let mut batch = String::new();
        loop {
            match rx.recv_timeout(IDLE) {
                Ok(msg) => {
                    batch.push_str(&msg);
                    while batch.len() < MAX_BATCH {
                        match rx.try_recv() {
                            Ok(m) => batch.push_str(&m),
                            Err(_) => break,
                        }
                    }
                    w.write_all(batch.as_bytes())?;
                    batch.clear();
                }
                Err(RecvTimeoutError::Timeout) => w.write_all(b": ping\n\n")?,
                // Every producer has ended (e.g. Docker went away); the browser reconnects.
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }
}

struct CloseOnDrop(Arc<AtomicBool>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Turns decoded body bytes into SSE frames appended to `out`. May keep partial input in `buf`.
type Handler = Box<dyn FnMut(&mut Vec<u8>, Tick, &mut String) + Send>;

fn relay(docker: &Docker, path: String, tx: &SyncSender<String>, closed: &Arc<AtomicBool>, mut handle: Handler) {
    let (docker, tx, closed) = (docker.clone(), tx.clone(), closed.clone());
    let spawned = std::thread::Builder::new().stack_size(128 * 1024).spawn(move || {
        let mut s = match docker.stream(&path, IDLE) {
            Ok(Ok(s)) => s,
            Ok(Err(reply)) => {
                let msg = serde_json::from_slice::<Value>(&reply.body)
                    .ok()
                    .and_then(|v| v.get("message")?.as_str().map(str::to_string))
                    .unwrap_or_else(|| format!("status {}", reply.status));
                let _ = tx.send(frame("fail", &json!({ "message": msg }).to_string()));
                return;
            }
            Err(e) => {
                let _ = tx.send(frame("fail", &json!({ "message": e.to_string() }).to_string()));
                return;
            }
        };
        let (mut buf, mut out) = (Vec::with_capacity(8192), String::new());
        while !closed.load(Ordering::Relaxed) {
            let tick = s.read_into(&mut buf).unwrap_or(Tick::Eof);
            handle(&mut buf, tick, &mut out);
            if !out.is_empty() && tx.send(std::mem::take(&mut out)).is_err() {
                return;
            }
            if matches!(tick, Tick::Eof) {
                return;
            }
        }
    });
    drop(spawned);
}

fn frame(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Calls `f` for each complete line in `buf` (all of it when `flush`) and removes them.
fn drain_lines(buf: &mut Vec<u8>, flush: bool, mut f: impl FnMut(&[u8])) {
    let end = if flush {
        buf.len()
    } else {
        match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => return,
        }
    };
    let body = buf[..end].strip_suffix(b"\n").unwrap_or(&buf[..end]);
    if end > 0 {
        body.split(|&b| b == b'\n').for_each(|l| f(l.strip_suffix(b"\r").unwrap_or(l)));
    }
    buf.drain(..end);
}

fn events() -> Handler {
    Box::new(|buf, tick, out| {
        drain_lines(buf, matches!(tick, Tick::Eof), |line| {
            let Ok(v) = serde_json::from_slice::<Value>(line) else { return };
            let action = v.get("Action").and_then(Value::as_str).unwrap_or("");
            // Health checks fire exec_* every few seconds; they never change what the UI shows.
            if action.starts_with("exec_") || action == "top" || action.is_empty() {
                return;
            }
            let attrs = v.pointer("/Actor/Attributes");
            let attr = |k: &str| attrs.and_then(|a| a.get(k)).cloned().unwrap_or(Value::Null);
            let ev = json!({
                "t": v.get("Type"),
                "a": action,
                "id": v.pointer("/Actor/ID"),
                "name": attr("name"),
                "project": attr("com.docker.compose.project"),
                "exit": attr("exitCode"),
                "time": v.get("time"),
            });
            out.push_str(&frame("docker", &ev.to_string()));
        });
    })
}

fn stats() -> Handler {
    Box::new(|buf, tick, out| {
        drain_lines(buf, matches!(tick, Tick::Eof), |line| {
            if let Ok(v) = serde_json::from_slice::<Value>(line) {
                out.push_str(&frame("stats", &summarize_stats(&v).to_string()));
            }
        });
    })
}

fn logs() -> Handler {
    let (mut demux, mut text) = (Demux::default(), Vec::with_capacity(8192));
    Box::new(move |buf, tick, out| {
        demux.feed(buf, &mut text);
        buf.clear();
        let eof = matches!(tick, Tick::Eof);
        if eof {
            demux.finish(&mut text);
        }
        // Flush partial lines when idle too, so prompts without a newline still show up.
        let mut started = false;
        drain_lines(&mut text, eof || matches!(tick, Tick::Idle), |line| {
            if !started {
                out.push_str("event: logs\n");
                started = true;
            }
            out.push_str("data: ");
            // Fast path: valid UTF-8 without a bare \r (which would end the SSE line early).
            match std::str::from_utf8(line) {
                Ok(t) if !line.contains(&b'\r') => out.push_str(t),
                _ => out.extend(String::from_utf8_lossy(line).chars().filter(|&c| c != '\r')),
            }
            out.push('\n');
        });
        if started {
            out.push('\n');
        }
        if eof {
            out.push_str(&frame("logend", ""));
        }
    })
}

/// Reduce Docker's ~2 KB stats sample to what the UI shows, computed like `docker stats`.
pub fn summarize_stats(s: &Value) -> Value {
    let n = |p: &str| s.pointer(p).and_then(Value::as_f64);
    let cpu = (|| {
        let cpu_delta = n("/cpu_stats/cpu_usage/total_usage")? - n("/precpu_stats/cpu_usage/total_usage")?;
        let sys_delta = n("/cpu_stats/system_cpu_usage")? - n("/precpu_stats/system_cpu_usage")?;
        let cpus = n("/cpu_stats/online_cpus")
            .or_else(|| s.pointer("/cpu_stats/cpu_usage/percpu_usage")?.as_array().map(|a| a.len() as f64))
            .unwrap_or(1.0);
        (sys_delta > 0.0).then(|| (cpu_delta / sys_delta * cpus * 100.0).max(0.0))
    })();
    // Subtract page cache (cgroup v2: inactive_file, v1: cache).
    let mem = n("/memory_stats/usage")
        .map(|u| u - n("/memory_stats/stats/inactive_file").or(n("/memory_stats/stats/cache")).unwrap_or(0.0));
    let (mut rx, mut tx) = (0.0, 0.0);
    if let Some(nets) = s.get("networks").and_then(Value::as_object) {
        for net in nets.values() {
            rx += net.get("rx_bytes").and_then(Value::as_f64).unwrap_or(0.0);
            tx += net.get("tx_bytes").and_then(Value::as_f64).unwrap_or(0.0);
        }
    }
    json!({
        "cpu": cpu,
        "mem": mem,
        "mem_limit": n("/memory_stats/limit"),
        "rx": rx,
        "tx": tx,
        "pids": n("/pids_stats/current"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn bench_live_logs_to_sse() {
        let line = b"2026-10-10T09:50:03.701695991Z GET /api/items 200 1.2ms user=42 trace=4bf92f3577b34da6a3ce929d0e0e47\n";
        let text: Vec<u8> = line.iter().copied().cycle().take(1 << 20).collect();
        let mut h = logs();
        let (mut buf, mut out) = (Vec::with_capacity(16384), String::with_capacity(32768));
        crate::bench::run("live: logs 1 MB -> SSE", text.len(), || {
            for piece in text.chunks(8192) {
                buf.extend_from_slice(piece);
                h(&mut buf, Tick::Data, &mut out);
                out.clear();
            }
        });
    }

    #[test]
    #[ignore]
    fn bench_live_stats_sample() {
        let sample = include_str!("../tests/fixtures/stats.json").replace('\n', "");
        let mut buf = Vec::with_capacity(8192);
        let mut out = String::with_capacity(1024);
        let mut h = stats();
        crate::bench::run("live: stats sample -> SSE", sample.len(), || {
            buf.extend_from_slice(sample.as_bytes());
            buf.push(b'\n');
            h(&mut buf, Tick::Data, &mut out);
            out.clear();
        });
    }

    #[test]
    #[ignore]
    fn bench_live_event() {
        let ev = br#"{"status":"start","id":"558a276b005d","from":"busybox","Type":"container","Action":"start","Actor":{"ID":"558a276b005d41a7ae92515cad48edd20fbb3ab436104b21f383f00d0c54b62a","Attributes":{"com.docker.compose.project":"shop","image":"busybox","name":"ct-live"}},"scope":"local","time":1791625803,"timeNano":1791625803701695991}"#;
        let mut buf = Vec::with_capacity(4096);
        let mut out = String::with_capacity(1024);
        let mut h = events();
        crate::bench::run("live: docker event -> SSE", ev.len(), || {
            buf.extend_from_slice(ev);
            buf.push(b'\n');
            h(&mut buf, Tick::Data, &mut out);
            out.clear();
        });
    }

    #[test]
    fn drains_complete_lines_only() {
        let mut buf = b"a\nbb\r\ncc".to_vec();
        let mut got = Vec::new();
        drain_lines(&mut buf, false, |l| got.push(l.to_vec()));
        assert_eq!(got, [b"a".to_vec(), b"bb".to_vec()]);
        assert_eq!(buf, b"cc");
        drain_lines(&mut buf, true, |l| got.push(l.to_vec()));
        assert_eq!(got.last().unwrap(), b"cc");
        assert!(buf.is_empty());
    }

    #[test]
    fn log_lines_become_one_sse_frame() {
        let mut h = logs();
        let (mut buf, mut out) = (b"one\ntwo\r\nthr".to_vec(), String::new());
        h(&mut buf, Tick::Data, &mut out);
        assert_eq!(out, "event: logs\ndata: one\ndata: two\n\n");
        out.clear();
        h(&mut Vec::new(), Tick::Eof, &mut out);
        assert_eq!(out, "event: logs\ndata: thr\n\nevent: logend\ndata: \n\n");
    }
}
