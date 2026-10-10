//! OpenTelemetry (OTLP/HTTP) receiver: summarise traces, mine logs, count metrics, forward.
//!
//! Applications or a collector send to `/v1/traces`, `/v1/logs`, `/v1/metrics` on the
//! dashboard's port (protobuf or JSON, uncompressed). Nothing is stored raw. Per service
//! we keep the rate, error rate and latency histogram (p50/p95/p99) of its entry spans in
//! a 60-minute ring of 1-minute buckets, per-operation figures in 5-minute buckets, and a
//! few example traces (slowest, latest errors). Log records feed the same pattern miner
//! as container logs. `--otlp-forward` hands every batch on, untouched, to a real backend.
//!
//! Request threads decode and pre-aggregate a batch, then send one small summary to a
//! single aggregator thread over a bounded channel; queries use the same channel. No
//! locks, bounded memory, and a full queue answers 503 so exporters back off and retry.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use serde_json::{Value, json};

use crate::docker::Docker;
use crate::logwatch::Watch;

const BUCKETS: usize = 28; // log2 µs: [1 µs, 2 µs), … [2^27 µs ≈ 134 s, ∞)
const MINUTES: usize = 60;
const OP_SLOTS: usize = 12; // 12 × 5 min = 1 h
const MAX_SERVICES: usize = 64;
const MAX_OPS: usize = 32;
const EXEMPLARS: usize = 10;

/* ---------- protobuf wire format, read in place ---------- */

enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32,
}

struct Pb<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Pb<'a> {
    fn new(b: &'a [u8]) -> Self {
        Pb { b, i: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.b.get(self.i)?;
            self.i += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn next(&mut self) -> Option<(u32, Wire<'a>)> {
        if self.i >= self.b.len() {
            return None;
        }
        let key = self.varint()?;
        let w = match key & 7 {
            0 => Wire::Varint(self.varint()?),
            1 => {
                let v = u64::from_le_bytes(self.b.get(self.i..self.i + 8)?.try_into().ok()?);
                self.i += 8;
                Wire::Fixed64(v)
            }
            2 => {
                let n = self.varint()? as usize;
                let s = self.b.get(self.i..self.i.checked_add(n)?)?;
                self.i += n;
                Wire::Bytes(s)
            }
            5 => {
                self.i += 4;
                Wire::Fixed32
            }
            _ => return None,
        };
        Some(((key >> 3) as u32, w))
    }
}

fn utf8(b: &[u8]) -> &str {
    std::str::from_utf8(b).unwrap_or("")
}

/// AnyValue → string (string, bool, int, double).
fn any_str(b: &[u8]) -> String {
    let mut p = Pb::new(b);
    while let Some((f, w)) = p.next() {
        match (f, w) {
            (1, Wire::Bytes(s)) => return utf8(s).to_string(),
            (2 | 3, Wire::Varint(v)) => return v.to_string(),
            (4, Wire::Fixed64(v)) => return f64::from_bits(v).to_string(),
            _ => {}
        }
    }
    String::new()
}

/// Calls `f(key, value_bytes)` for each attribute KeyValue in `b`'s field `field`.
fn attrs<'a>(b: &'a [u8], field: u32, mut f: impl FnMut(&'a str, &'a [u8])) {
    let mut p = Pb::new(b);
    while let Some((n, w)) = p.next() {
        if n == field
            && let Wire::Bytes(kv) = w
        {
            let (mut k, mut v) = ("", &[][..]);
            let mut q = Pb::new(kv);
            while let Some((m, x)) = q.next() {
                match (m, x) {
                    (1, Wire::Bytes(s)) => k = utf8(s),
                    (2, Wire::Bytes(s)) => v = s,
                    _ => {}
                }
            }
            f(k, v);
        }
    }
}

fn service_of(resource: &[u8]) -> String {
    let mut name = String::new();
    attrs(resource, 1, |k, v| {
        if k == "service.name" {
            name = any_str(v);
        }
    });
    if name.is_empty() { "unknown".into() } else { name }
}

/* ---------- one decoded span, before aggregation ---------- */

#[derive(Clone)]
pub struct Exemplar {
    trace_id: String,
    op: String,
    dur_us: u64,
    at: u64,
    error: bool,
    message: String,
    detail: String,
}

impl Exemplar {
    fn json(&self) -> Value {
        json!({ "trace_id": self.trace_id, "operation": self.op, "ms": self.dur_us as f64 / 1000.0, "at": self.at,
                "error": self.error, "message": self.message, "detail": self.detail })
    }
}

struct SpanLite<'a> {
    trace: &'a [u8],
    name: &'a str,
    kind: u64,
    root: bool,
    start: u64,
    end: u64,
    error: bool,
    message: &'a str,
    span: &'a [u8],
}

fn decode_span(b: &[u8]) -> SpanLite<'_> {
    let mut s = SpanLite { trace: &[], name: "", kind: 0, root: true, start: 0, end: 0, error: false, message: "", span: b };
    let mut p = Pb::new(b);
    while let Some((f, w)) = p.next() {
        match (f, w) {
            (1, Wire::Bytes(x)) => s.trace = x,
            (4, Wire::Bytes(x)) => s.root = x.is_empty(),
            (5, Wire::Bytes(x)) => s.name = utf8(x),
            (6, Wire::Varint(v)) => s.kind = v,
            (7, Wire::Fixed64(v)) => s.start = v,
            (8, Wire::Fixed64(v)) => s.end = v,
            (15, Wire::Bytes(x)) => {
                let mut q = Pb::new(x);
                while let Some((g, y)) = q.next() {
                    match (g, y) {
                        (2, Wire::Bytes(m)) => s.message = utf8(m),
                        (3, Wire::Varint(c)) => s.error = c == 2,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    s
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A few attributes that make an exemplar meaningful at a glance.
fn detail_of(span: &[u8]) -> String {
    let mut parts = Vec::new();
    attrs(span, 9, |k, v| {
        if matches!(k, "http.request.method" | "http.method" | "http.route" | "url.path" | "http.response.status_code" | "http.status_code" | "rpc.method" | "db.system" | "messaging.destination.name") {
            parts.push(format!("{k}={}", any_str(v)));
        }
    });
    parts.join(" ")
}

fn bucket(us: u64) -> usize {
    (64 - us.max(1).leading_zeros() as usize - 1).min(BUCKETS - 1)
}

/* ---------- per-batch summary sent to the aggregator ---------- */

#[derive(Default)]
struct OpBatch {
    count: u32,
    errors: u32,
    hist: [u32; BUCKETS],
}

#[derive(Default)]
struct SvcBatch {
    spans: u64,
    entry: OpBatch,
    ops: HashMap<String, OpBatch>,
    exemplars: Vec<Exemplar>,
    logs: u64,
    metrics: u64,
}

enum Msg {
    Batch(HashMap<String, SvcBatch>),
    Overview(SyncSender<Value>),
    Service(String, SyncSender<Option<Value>>),
    Export(SyncSender<Vec<Value>>),
}

/* ---------- aggregator state (owned by one thread) ---------- */

#[derive(Clone, Copy)]
struct Bucket {
    t: u64,
    count: u32,
    errors: u32,
    hist: [u32; BUCKETS],
}

impl Default for Bucket {
    fn default() -> Self {
        Bucket { t: u64::MAX, count: 0, errors: 0, hist: [0; BUCKETS] }
    }
}

struct Ring<const N: usize> {
    b: [Bucket; N],
    width: u64,
}

impl<const N: usize> Ring<N> {
    fn new(width: u64) -> Self {
        Ring { b: [Bucket::default(); N], width }
    }

    fn add(&mut self, now: u64, o: &OpBatch) {
        let t = now / self.width;
        let slot = &mut self.b[(t % N as u64) as usize];
        if slot.t != t {
            *slot = Bucket { t, ..Bucket::default() };
        }
        slot.count += o.count;
        slot.errors += o.errors;
        for (a, b) in slot.hist.iter_mut().zip(o.hist) {
            *a += b;
        }
    }

    /// Totals over the last `secs` seconds.
    fn window(&self, now: u64, secs: u64) -> (u64, u64, [u64; BUCKETS]) {
        let from = now.saturating_sub(secs) / self.width;
        let (mut c, mut e, mut h) = (0, 0, [0u64; BUCKETS]);
        for b in self.b.iter().filter(|b| b.t != u64::MAX && b.t >= from && b.t <= now / self.width) {
            c += u64::from(b.count);
            e += u64::from(b.errors);
            for (a, x) in h.iter_mut().zip(b.hist) {
                *a += u64::from(x);
            }
        }
        (c, e, h)
    }

    /// Per-bucket counts, oldest first, for sparklines.
    fn series(&self, now: u64) -> Vec<u32> {
        let cur = now / self.width;
        (0..N as u64).rev().map(|k| {
            let t = cur.saturating_sub(k);
            let b = &self.b[(t % N as u64) as usize];
            if b.t == t { b.count } else { 0 }
        }).collect()
    }
}

/// Percentile (ms) from a log2-µs histogram, interpolating geometrically inside the
/// bucket by rank (bucket i spans [2^i, 2^(i+1)) µs), so values don't snap to powers of 2.
fn pct(h: &[u64; BUCKETS], p: f64) -> Option<f64> {
    let total: u64 = h.iter().sum();
    if total == 0 {
        return None;
    }
    let target = (total as f64 * p).max(1.0);
    let mut seen = 0.0;
    for (i, &n) in h.iter().enumerate() {
        if n == 0 {
            continue;
        }
        if seen + n as f64 >= target {
            let frac = (target - seen) / n as f64;
            return Some((1u64 << i) as f64 * 2f64.powf(frac) / 1000.0);
        }
        seen += n as f64;
    }
    None
}

struct Service {
    entry: Ring<MINUTES>,
    ops: HashMap<String, Ring<OP_SLOTS>>,
    slow: Vec<Exemplar>,
    errors: Vec<Exemplar>,
    spans: u64,
    logs: u64,
    metrics: u64,
    last: u64,
}

impl Service {
    fn new() -> Self {
        Service { entry: Ring::new(60), ops: HashMap::new(), slow: Vec::new(), errors: Vec::new(), spans: 0, logs: 0, metrics: 0, last: 0 }
    }

    fn summary(&self, name: &str, now: u64) -> Value {
        let (c5, e5, h5) = self.entry.window(now, 300);
        let (c60, e60, _) = self.entry.window(now, 3600);
        json!({
            "service": name,
            "rpm": c5 as f64 / 5.0,
            "error_rate": if c5 > 0 { e5 as f64 / c5 as f64 } else { 0.0 },
            "requests_1h": c60, "errors_1h": e60,
            "p50": pct(&h5, 0.50), "p95": pct(&h5, 0.95), "p99": pct(&h5, 0.99),
            "spark": self.entry.series(now),
            "spans": self.spans, "logs": self.logs, "metric_points": self.metrics, "last": self.last,
        })
    }
}

pub struct Otlp {
    tx: SyncSender<Msg>,
    forward: Option<SyncSender<(String, String, Vec<u8>)>>,
    forward_to: Option<String>,
    pub received: AtomicU64,
    pub rejected: AtomicU64,
    pub forwarded: AtomicU64,
    pub forward_dropped: AtomicU64,
    logs: Arc<Watch>,
}

impl Otlp {
    pub fn start(logs: Arc<Watch>, forward_to: Option<String>) -> Arc<Otlp> {
        let (tx, rx) = sync_channel::<Msg>(256);
        let _ = std::thread::Builder::new().name("otlp".into()).stack_size(256 * 1024).spawn(move || aggregate(rx));
        let forward = forward_to.as_ref().and_then(|url| {
            let target = url.trim_end_matches('/').strip_prefix("http://")?.to_string();
            let docker = Docker::parse(&format!("tcp://{target}")).ok()?;
            let (ftx, frx) = sync_channel::<(String, String, Vec<u8>)>(64);
            let _ = std::thread::Builder::new().name("otlp-forward".into()).stack_size(128 * 1024).spawn(move || {
                for (path, ctype, body) in frx {
                    let _ = docker.clone().with_header("X-Forwarded-By", "contraire").call_typed("POST", &path, &body, &ctype, Duration::from_secs(10));
                }
            });
            Some(ftx)
        });
        Arc::new(Otlp {
            tx,
            forward,
            forward_to,
            received: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
            forward_dropped: AtomicU64::new(0),
            logs,
        })
    }

    pub fn forward_to(&self) -> Option<&str> {
        self.forward_to.as_deref()
    }

    /// Handle one OTLP/HTTP export. Returns (status, content type, body).
    pub fn ingest(&self, signal: &str, ctype: &str, encoding: Option<&str>, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
        let json_in = ctype.starts_with("application/json");
        let reply_type = if json_in { "application/json" } else { "application/x-protobuf" };
        if encoding.is_some_and(|e| !e.eq_ignore_ascii_case("identity")) {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return (415, "application/json", br#"{"message":"compressed OTLP is not supported: set compression to none"}"#.to_vec());
        }
        if !json_in && !ctype.starts_with("application/x-protobuf") {
            return (415, "application/json", br#"{"message":"OTLP needs application/x-protobuf or application/json"}"#.to_vec());
        }
        let now = crate::unix_now();
        let mut batch: HashMap<String, SvcBatch> = HashMap::new();
        let ok = if json_in {
            serde_json::from_slice::<Value>(body).ok().map(|v| self.decode_json(signal, &v, now, &mut batch)).is_some()
        } else {
            self.decode_pb(signal, body, now, &mut batch)
        };
        if !ok {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return (400, "application/json", br#"{"message":"could not decode OTLP payload"}"#.to_vec());
        }
        if let Some(f) = &self.forward {
            match f.try_send((format!("/v1/{signal}"), ctype.to_string(), body.to_vec())) {
                Ok(()) => self.forwarded.fetch_add(1, Ordering::Relaxed),
                Err(_) => self.forward_dropped.fetch_add(1, Ordering::Relaxed),
            };
        }
        match self.tx.try_send(Msg::Batch(batch)) {
            Ok(()) => {
                self.received.fetch_add(1, Ordering::Relaxed);
                (200, reply_type, if json_in { b"{}".to_vec() } else { Vec::new() })
            }
            // Full: let the exporter back off and retry rather than growing memory.
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                (503, "application/json", br#"{"message":"busy, retry later"}"#.to_vec())
            }
        }
    }

    fn add_span(batch: &mut HashMap<String, SvcBatch>, svc: &str, s: &SpanLite, now: u64) {
        // Look up before inserting: owned keys are allocated only for new services/operations.
        if !batch.contains_key(svc) {
            if batch.len() >= MAX_SERVICES {
                return;
            }
            batch.insert(svc.to_string(), SvcBatch::default());
        }
        let Some(b) = batch.get_mut(svc) else { return };
        b.spans += 1;
        let dur = s.end.saturating_sub(s.start) / 1000; // ns → µs
        let i = bucket(dur);
        // Entry spans (server/consumer, or roots) describe what the service's callers see.
        if matches!(s.kind, 2 | 5) || s.root {
            b.entry.count += 1;
            b.entry.errors += u32::from(s.error);
            b.entry.hist[i] += 1;
        }
        if !b.ops.contains_key(s.name) && b.ops.len() < MAX_OPS {
            b.ops.insert(s.name.to_string(), OpBatch::default());
        }
        if let Some(o) = b.ops.get_mut(s.name) {
            o.count += 1;
            o.errors += u32::from(s.error);
            o.hist[i] += 1;
        }
        // Candidate exemplars: errors and anything slower than 100 ms.
        if (s.error || dur > 100_000) && b.exemplars.len() < 2 * EXEMPLARS {
            b.exemplars.push(Exemplar {
                trace_id: hex(s.trace),
                op: s.name.chars().take(120).collect(),
                dur_us: dur,
                at: now,
                error: s.error,
                message: s.message.chars().take(200).collect(),
                detail: if s.span.is_empty() { String::new() } else { detail_of(s.span) },
            });
        }
    }

    fn decode_pb(&self, signal: &str, body: &[u8], now: u64, batch: &mut HashMap<String, SvcBatch>) -> bool {
        let mut top = Pb::new(body);
        while let Some((f, w)) = top.next() {
            let (1, Wire::Bytes(res_block)) = (f, w) else { continue };
            let (mut svc, mut scopes) = (String::from("unknown"), Vec::new());
            let mut r = Pb::new(res_block);
            while let Some((g, x)) = r.next() {
                match (g, x) {
                    (1, Wire::Bytes(resource)) => svc = service_of(resource),
                    (2, Wire::Bytes(scope)) => scopes.push(scope),
                    _ => {}
                }
            }
            for scope in scopes {
                let mut sc = Pb::new(scope);
                while let Some((h, y)) = sc.next() {
                    let (2, Wire::Bytes(item)) = (h, y) else { continue };
                    match signal {
                        "traces" => Self::add_span(batch, &svc, &decode_span(item), now),
                        "logs" => self.add_log_pb(batch, &svc, item),
                        _ => {
                            // A Metric: count its data points (any of gauge/sum/histogram/…).
                            let points = count_points(item);
                            if batch.len() < MAX_SERVICES || batch.contains_key(&svc) {
                                batch.entry(svc.clone()).or_default().metrics += points;
                            }
                        }
                    }
                }
            }
        }
        top.i == body.len()
    }

    fn add_log_pb(&self, batch: &mut HashMap<String, SvcBatch>, svc: &str, rec: &[u8]) {
        let (mut sev, mut body) = (String::new(), String::new());
        let mut p = Pb::new(rec);
        while let Some((f, w)) = p.next() {
            match (f, w) {
                (3, Wire::Bytes(t)) => sev = utf8(t).to_string(),
                (5, Wire::Bytes(v)) => body = any_str(v),
                _ => {}
            }
        }
        if batch.len() < MAX_SERVICES || batch.contains_key(svc) {
            batch.entry(svc.to_string()).or_default().logs += 1;
        }
        self.logs.ingest(svc, &sev, &body);
    }

    fn decode_json(&self, signal: &str, v: &Value, now: u64, batch: &mut HashMap<String, SvcBatch>) {
        let (outer, inner, items) = match signal {
            "traces" => ("resourceSpans", "scopeSpans", "spans"),
            "logs" => ("resourceLogs", "scopeLogs", "logRecords"),
            _ => ("resourceMetrics", "scopeMetrics", "metrics"),
        };
        for rs in v[outer].as_array().into_iter().flatten() {
            let svc = rs["resource"]["attributes"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| a["key"] == "service.name")
                .and_then(|a| a["value"]["stringValue"].as_str())
                .unwrap_or("unknown")
                .to_string();
            for ss in rs[inner].as_array().into_iter().flatten() {
                for it in ss[items].as_array().into_iter().flatten() {
                    match signal {
                        "traces" => {
                            let num = |k: &str| it[k].as_str().and_then(|s| s.parse().ok()).or(it[k].as_u64()).unwrap_or(0);
                            let trace = it["traceId"].as_str().unwrap_or("");
                            let s = SpanLite {
                                trace: &[],
                                name: it["name"].as_str().unwrap_or(""),
                                kind: it["kind"].as_u64().unwrap_or(0),
                                root: it["parentSpanId"].as_str().is_none_or(str::is_empty),
                                start: num("startTimeUnixNano"),
                                end: num("endTimeUnixNano"),
                                error: it["status"]["code"].as_u64() == Some(2),
                                message: it["status"]["message"].as_str().unwrap_or(""),
                                span: &[],
                            };
                            Self::add_span(batch, &svc, &s, now);
                            if let Some(e) = batch.get_mut(&svc).and_then(|b| b.exemplars.last_mut()).filter(|e| e.trace_id.is_empty()) {
                                e.trace_id = trace.to_string();
                            }
                        }
                        "logs" => {
                            batch.entry(svc.clone()).or_default().logs += 1;
                            let body = it["body"]["stringValue"].as_str().map(str::to_string).unwrap_or_else(|| it["body"].to_string());
                            self.logs.ingest(&svc, it["severityText"].as_str().unwrap_or(""), &body);
                        }
                        _ => batch.entry(svc.clone()).or_default().metrics += 1,
                    }
                }
            }
        }
    }

    fn ask<T>(&self, make: impl FnOnce(SyncSender<T>) -> Msg) -> Option<T> {
        let (tx, rx) = sync_channel(1);
        self.tx.send(make(tx)).ok()?;
        rx.recv_timeout(Duration::from_secs(5)).ok()
    }

    pub fn overview(&self) -> Value {
        let mut v = self.ask(Msg::Overview).unwrap_or(json!({}));
        v["received_batches"] = json!(self.received.load(Ordering::Relaxed));
        v["rejected_batches"] = json!(self.rejected.load(Ordering::Relaxed));
        v["forward"] = json!({ "to": self.forward_to, "sent": self.forwarded.load(Ordering::Relaxed), "dropped": self.forward_dropped.load(Ordering::Relaxed) });
        v
    }

    pub fn service(&self, name: &str) -> Option<Value> {
        let mut v = self.ask(|tx| Msg::Service(name.to_string(), tx)).flatten()?;
        v["log_patterns"] = self.logs.service_patterns(name).map(|s| s.to_json(false)).unwrap_or(Value::Null);
        Some(v)
    }

    /// Per service and minute of the last hour: requests, errors, p50/p95/p99. For export.
    pub fn export(&self) -> Vec<Value> {
        self.ask(Msg::Export).unwrap_or_default()
    }
}

/// Data points in one Metric message (gauge=5, sum=7, histogram=9, exp. histogram=10, summary=11).
fn count_points(metric: &[u8]) -> u64 {
    let mut n = 0;
    let mut p = Pb::new(metric);
    while let Some((f, w)) = p.next() {
        if let (5 | 7 | 9 | 10 | 11, Wire::Bytes(data)) = (f, w) {
            let mut q = Pb::new(data);
            while let Some((g, _)) = q.next() {
                n += u64::from(g == 1);
            }
        }
    }
    n
}

fn aggregate(rx: Receiver<Msg>) {
    let mut services: HashMap<String, Service> = HashMap::new();
    for msg in rx {
        let now = crate::unix_now();
        match msg {
            Msg::Batch(batch) => {
                for (name, b) in batch {
                    if services.len() >= MAX_SERVICES && !services.contains_key(&name) {
                        continue;
                    }
                    let s = services.entry(name).or_insert_with(Service::new);
                    s.spans += b.spans;
                    s.logs += b.logs;
                    s.metrics += b.metrics;
                    s.last = now;
                    if b.entry.count > 0 {
                        s.entry.add(now, &b.entry);
                    }
                    for (op, o) in b.ops {
                        if s.ops.len() < MAX_OPS || s.ops.contains_key(&op) {
                            s.ops.entry(op).or_insert_with(|| Ring::new(300)).add(now, &o);
                        }
                    }
                    for e in b.exemplars {
                        if e.error {
                            s.errors.push(e.clone());
                            if s.errors.len() > EXEMPLARS {
                                s.errors.remove(0);
                            }
                        }
                        if e.dur_us > 100_000 {
                            s.slow.retain(|x| now.saturating_sub(x.at) < 900);
                            s.slow.push(e);
                            s.slow.sort_by_key(|x| std::cmp::Reverse(x.dur_us));
                            s.slow.truncate(EXEMPLARS);
                        }
                    }
                }
            }
            Msg::Overview(reply) => {
                let mut list: Vec<Value> = services.iter().map(|(n, s)| s.summary(n, now)).collect();
                list.sort_by(|a, b| b["requests_1h"].as_u64().cmp(&a["requests_1h"].as_u64()));
                let _ = reply.send(json!({ "services": list }));
            }
            Msg::Service(name, reply) => {
                let _ = reply.send(services.get(&name).map(|s| {
                    let mut ops: Vec<Value> = s
                        .ops
                        .iter()
                        .map(|(op, r)| {
                            let (c, e, h) = r.window(now, 3600);
                            json!({ "operation": op, "count_1h": c, "errors_1h": e, "p50": pct(&h, 0.5), "p95": pct(&h, 0.95), "p99": pct(&h, 0.99) })
                        })
                        .collect();
                    ops.sort_by(|a, b| b["count_1h"].as_u64().cmp(&a["count_1h"].as_u64()));
                    let mut v = s.summary(&name, now);
                    v["operations"] = json!(ops);
                    v["slowest"] = json!(s.slow.iter().map(Exemplar::json).collect::<Vec<_>>());
                    v["errors"] = json!(s.errors.iter().rev().map(Exemplar::json).collect::<Vec<_>>());
                    v
                }));
            }
            Msg::Export(reply) => {
                let mut rows = Vec::new();
                for (name, s) in &services {
                    for b in s.entry.b.iter().filter(|b| b.t != u64::MAX) {
                        let h: [u64; BUCKETS] = std::array::from_fn(|i| u64::from(b.hist[i]));
                        rows.push(json!({ "service": name, "minute": b.t * 60, "requests": b.count, "errors": b.errors,
                                          "p50_ms": pct(&h, 0.5), "p95_ms": pct(&h, 0.95), "p99_ms": pct(&h, 0.99) }));
                    }
                }
                rows.sort_by(|a, b| a["minute"].as_u64().cmp(&b["minute"].as_u64()));
                let _ = reply.send(rows);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }
    fn field_bytes(f: u32, b: &[u8], out: &mut Vec<u8>) {
        varint(u64::from(f) << 3 | 2, out);
        varint(b.len() as u64, out);
        out.extend_from_slice(b);
    }
    fn field_fixed64(f: u32, v: u64, out: &mut Vec<u8>) {
        varint(u64::from(f) << 3 | 1, out);
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn field_varint(f: u32, v: u64, out: &mut Vec<u8>) {
        varint(u64::from(f) << 3, out);
        varint(v, out);
    }

    /// A minimal ExportTraceServiceRequest: one service, `n` server spans of `ms` each.
    fn request(service: &str, n: usize, ms: u64, error: bool) -> Vec<u8> {
        let mut any = Vec::new();
        field_bytes(1, service.as_bytes(), &mut any);
        let mut kv = Vec::new();
        field_bytes(1, b"service.name", &mut kv);
        field_bytes(2, &any, &mut kv);
        let mut resource = Vec::new();
        field_bytes(1, &kv, &mut resource);
        let mut scope = Vec::new();
        for i in 0..n {
            let mut span = Vec::new();
            field_bytes(1, &[i as u8; 16], &mut span);
            field_bytes(5, b"GET /api/items", &mut span);
            field_varint(6, 2, &mut span);
            field_fixed64(7, 1_000_000_000, &mut span);
            field_fixed64(8, 1_000_000_000 + ms * 1_000_000, &mut span);
            if error {
                let mut st = Vec::new();
                field_varint(3, 2, &mut st);
                field_bytes(15, &st, &mut span);
            }
            field_bytes(2, &span, &mut scope);
        }
        let mut rs = Vec::new();
        field_bytes(1, &resource, &mut rs);
        field_bytes(2, &scope, &mut rs);
        let mut req = Vec::new();
        field_bytes(1, &rs, &mut req);
        req
    }

    #[test]
    fn histogram_percentiles() {
        let mut h = [0u64; BUCKETS];
        h[bucket(1000)] = 90; // ~1 ms
        h[bucket(500_000)] = 10; // ~0.5 s
        let p50 = pct(&h, 0.5).unwrap();
        let p99 = pct(&h, 0.99).unwrap();
        assert!((0.7..2.0).contains(&p50), "{p50}");
        assert!((300.0..800.0).contains(&p99), "{p99}");
    }

    #[test]
    fn decodes_and_aggregates_protobuf_traces() {
        let watch_less = |sig: &str, body: &[u8]| {
            let mut batch = HashMap::new();
            let mut top = Pb::new(body);
            while let Some((_, Wire::Bytes(rs))) = top.next() {
                let mut r = Pb::new(rs);
                let (mut svc, mut scopes) = (String::new(), vec![]);
                while let Some((g, x)) = r.next() {
                    match (g, x) {
                        (1, Wire::Bytes(res)) => svc = service_of(res),
                        (2, Wire::Bytes(s)) => scopes.push(s),
                        _ => {}
                    }
                }
                for s in scopes {
                    let mut sc = Pb::new(s);
                    while let Some((2, Wire::Bytes(item))) = sc.next() {
                        assert_eq!(sig, "traces");
                        Otlp::add_span(&mut batch, &svc, &decode_span(item), 100);
                    }
                }
            }
            batch
        };
        let b = watch_less("traces", &request("checkout", 5, 250, true));
        let c = &b["checkout"];
        assert_eq!(c.spans, 5);
        assert_eq!(c.entry.count, 5);
        assert_eq!(c.entry.errors, 5);
        assert_eq!(c.ops["GET /api/items"].count, 5);
        assert_eq!(c.exemplars.len(), 5);
        assert!(c.exemplars[0].error && c.exemplars[0].dur_us == 250_000);
    }

    #[test]
    fn ring_windows_and_sparkline() {
        let mut r: Ring<MINUTES> = Ring::new(60);
        let o = OpBatch { count: 3, errors: 1, hist: [0; BUCKETS] };
        r.add(6000, &o);
        r.add(6060, &o);
        let (c, e, _) = r.window(6060, 300);
        assert_eq!((c, e), (6, 2));
        let s = r.series(6060);
        assert_eq!(s.len(), MINUTES);
        assert_eq!(&s[MINUTES - 2..], &[3, 3]);
    }

    #[test]
    #[ignore]
    fn bench_otlp_decode_1000_spans() {
        let body = request("checkout", 1000, 12, false);
        crate::bench::run("otlp: decode+aggregate 1000 spans", body.len(), || {
            let mut batch = HashMap::new();
            let mut top = Pb::new(&body);
            while let Some((_, Wire::Bytes(rs))) = top.next() {
                let mut r = Pb::new(rs);
                while let Some((g, x)) = r.next() {
                    if let (2, Wire::Bytes(s)) = (g, x) {
                        let mut sc = Pb::new(s);
                        while let Some((2, Wire::Bytes(item))) = sc.next() {
                            Otlp::add_span(&mut batch, "checkout", &decode_span(item), 1);
                        }
                    }
                }
            }
            std::hint::black_box(batch);
        });
    }
}
