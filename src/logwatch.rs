//! Log patterns and the crash flight recorder.
//!
//! One small thread per running container follows its log stream. Every line is masked
//! into a reused buffer (`GET /api/items 42 1.2ms` → `GET /api/items <*> <*>`), so a
//! million lines become a few dozen counted patterns; lines matching a known pattern cost
//! no allocation. Unmatched lines merge into a similar pattern (same length and first
//! word, at least 70% of words equal) or start a new one, up to `MAX_PATTERNS`.
//!
//! The same thread keeps the last `RING` bytes of output in a fixed circular buffer. When
//! the container stops with an error or an OOM kill, it writes a postmortem (exit code,
//! last lines, new and error patterns, the 15 minutes of metrics before the end) to
//! `<state>/incidents/`. Repeated crashes of one container within 10 minutes update the
//! same incident instead of filling the disk; at most `MAX_INCIDENTS` files are kept.
//!
//! Followers publish a pattern snapshot every few seconds through a `Mutex<Arc<_>>` cell:
//! the lock is held only to swap or clone a pointer (the same deliberate exception as the
//! token table). The registry of followers changes only when containers start or stop.

use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::docker::{Demux, Docker, Tick};
use crate::metrics;

const MAX_PATTERNS: usize = 128;
const MAX_PATTERN_LEN: usize = 240;
const RING: usize = 32 * 1024;
const WARMUP: u64 = 120;
const MAX_INCIDENTS: usize = 100;
const PUBLISH_EVERY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Other,
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Other => "",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

#[derive(Clone)]
pub struct Pattern {
    pub text: String,
    pub count: u64,
    pub first: u64,
    pub last: u64,
    pub level: Level,
    pub example: String,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub since: u64,
    pub lines: u64,
    pub other: u64,
    pub patterns: Vec<Pattern>,
}

impl Snapshot {
    pub fn to_json(&self, only_new: bool) -> Value {
        let warm = self.since + WARMUP;
        let mut ps: Vec<&Pattern> = self.patterns.iter().filter(|p| !only_new || p.first > warm).collect();
        ps.sort_by(|a, b| b.level.cmp(&a.level).then(b.count.cmp(&a.count)));
        json!({
            "since": self.since,
            "lines": self.lines,
            "unclassified": self.other,
            "patterns": ps.iter().map(|p| json!({
                "pattern": p.text, "count": p.count, "first": p.first, "last": p.last,
                "level": p.level.as_str(), "new": p.first > warm, "example": p.example,
            })).collect::<Vec<_>>(),
        })
    }
}

struct Cell {
    snap: Mutex<Arc<Snapshot>>,
    name: Mutex<String>,
    /// Set by the supervisor on `health_status: unhealthy`; the follower records a postmortem.
    unhealthy: AtomicBool,
}

pub struct Watch {
    docker: Docker,
    metrics: Arc<metrics::Store>,
    incidents: Option<PathBuf>,
    followers: Mutex<HashMap<String, Arc<Cell>>>,
    /// Container ID → time of the last `kill` event (`docker stop`/`restart`/`kill`), so a
    /// deliberate stop that ends in SIGKILL (137) isn't mistaken for a crash.
    killed: Mutex<HashMap<String, u64>>,
    /// Log records received over OTLP, mined per service (not per container).
    services: Mutex<HashMap<String, Miner>>,
}

/* ---------- masking and matching (the hot path: no allocation for known patterns) ---------- */

fn has_digit(t: &str) -> bool {
    t.bytes().any(|b| b.is_ascii_digit())
}

/// Is this word a value that changes from line to line? Numbers, times, dates, IP
/// addresses, hex IDs, UUIDs and numbers with a short unit (`12ms`, `4kB`, `95%`) are;
/// words, code locations and versions (`collector@v0.160.0`) are not.
fn is_variable(raw: &str) -> bool {
    let t = raw.trim_matches(|c: char| "\"'()[]{},;<>".contains(c));
    // Very long words are usually blobs, except paths, which `mask` handles per segment.
    let long = t.len() > 40 && !t.contains('/');
    if t.is_empty() || !has_digit(t) {
        return long;
    }
    let b = t.as_bytes();
    if b.iter().all(|c| c.is_ascii_digit() || b".:-+/_TZ".contains(c)) {
        return true; // 42, 1.2, 10.0.0.1:8080, 2026-10-10T12:51:13.282Z
    }
    if t.len() >= 8 && b.iter().all(|c| c.is_ascii_hexdigit() || *c == b'-') {
        return true; // hashes, trace IDs, UUIDs
    }
    let digits = b.iter().take_while(|c| c.is_ascii_digit() || **c == b'.').count();
    if digits > 0 && b.len() - digits <= 3 && b[digits..].iter().all(|c| c.is_ascii_alphabetic() || *c == b'%') {
        return true; // 12ms, 1.5s, 4kB, 95%
    }
    long
}

/// Append one piece of a word, masked if it is a value (`key=value` keeps the key).
fn push_piece(piece: &str, out: &mut String) {
    if let Some((k, v)) = piece.split_once('=')
        && !k.is_empty()
        && !has_digit(k)
    {
        out.push_str(k);
        out.push('=');
        out.push_str(if is_variable(v) || v.len() > 24 { "<*>" } else { v });
    } else if is_variable(piece) {
        out.push_str("<*>");
    } else {
        out.push_str(piece);
    }
}

/// Append the masked form of `line` to `out`. Words are split at `/ [ ] ( ) , ;` so values
/// glued to text (`/api/items/42`, `[2026-10-10 09:53:28][8][info]`) are masked on their
/// own; runs of masked words collapse into one `<*>`. Returns the word count.
fn mask(line: &str, out: &mut String) -> usize {
    out.clear();
    let (mut n, mut last_var) = (0, false);
    for tok in line.split_whitespace() {
        if out.len() >= MAX_PATTERN_LEN {
            break;
        }
        let before = out.len();
        if n > 0 {
            out.push(' ');
        }
        let start = out.len();
        let mut piece_start = 0;
        for (i, c) in tok.char_indices() {
            if "/[](),;".contains(c) {
                push_piece(&tok[piece_start..i], out);
                out.push(c);
                piece_start = i + c.len_utf8();
            }
        }
        push_piece(&tok[piece_start..], out);
        // "<*> <*> <*>" → "<*>": variable runs shouldn't split one message into several patterns.
        let var = &out[start..] == "<*>";
        if var && last_var {
            out.truncate(before);
            continue;
        }
        last_var = var;
        n += 1;
    }
    n
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

fn level_of(line: &str) -> Level {
    if ["error", "fatal", "panic", "exception", "crit", "failed", "fail:", "refused", "denied"].iter().any(|w| contains_ci(line, w)) {
        Level::Error
    } else if contains_ci(line, "warn") {
        Level::Warn
    } else if contains_ci(line, "info") {
        Level::Info
    } else {
        Level::Other
    }
}

fn hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Same length, same first word, ≥70% of words equal → merge (differences become `<*>`).
fn similar(a: &str, b: &str) -> Option<String> {
    let (ta, tb): (Vec<&str>, Vec<&str>) = (a.split(' ').collect(), b.split(' ').collect());
    if ta.len() != tb.len() || ta.len() < 2 || ta[0] != tb[0] {
        return None;
    }
    let same = ta.iter().zip(&tb).filter(|(x, y)| x == y).count();
    (same * 10 >= ta.len() * 7).then(|| ta.iter().zip(&tb).map(|(x, y)| if x == y { *x } else { "<*>" }).collect::<Vec<_>>().join(" "))
}

struct Miner {
    snap: Snapshot,
    index: HashMap<u64, usize>,
    key: String,
}

impl Miner {
    fn new(since: u64) -> Miner {
        Miner { snap: Snapshot { since, ..Default::default() }, index: HashMap::new(), key: String::with_capacity(MAX_PATTERN_LEN + 8) }
    }

    fn add(&mut self, line: &str, now: u64) {
        if line.trim().is_empty() {
            return;
        }
        self.snap.lines += 1;
        let mut key = std::mem::take(&mut self.key);
        mask(line, &mut key);
        let level = level_of(line);
        let h = hash(&key);
        let found = self.index.get(&h).copied().or_else(|| {
            // Unknown masked form: fold into a similar pattern if there is one.
            let i = self.snap.patterns.iter().position(|p| similar(&p.text, &key).is_some())?;
            let merged = similar(&self.snap.patterns[i].text, &key)?;
            self.index.insert(h, i);
            self.index.insert(hash(&merged), i);
            self.snap.patterns[i].text = merged;
            Some(i)
        });
        match found {
            Some(i) => {
                let p = &mut self.snap.patterns[i];
                p.count += 1;
                p.last = now;
                p.level = p.level.max(level);
            }
            None if self.snap.patterns.len() < MAX_PATTERNS => {
                self.index.insert(h, self.snap.patterns.len());
                self.snap.patterns.push(Pattern {
                    text: key.clone(),
                    count: 1,
                    first: now,
                    last: now,
                    level,
                    example: line.chars().take(300).collect(),
                });
            }
            None => self.snap.other += 1,
        }
        self.key = key;
    }
}

/* ---------- the flight recorder's fixed circular buffer ---------- */

struct Ring {
    buf: Vec<u8>,
    pos: usize,
    full: bool,
}

impl Ring {
    fn new() -> Ring {
        Ring { buf: vec![0; RING], pos: 0, full: false }
    }

    fn push(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let n = (RING - self.pos).min(data.len());
            self.buf[self.pos..self.pos + n].copy_from_slice(&data[..n]);
            self.pos = (self.pos + n) % RING;
            self.full |= self.pos == 0 || n < data.len();
            data = &data[n..];
        }
    }

    /// The buffered text as lines, oldest first, dropping a partial first line.
    fn lines(&self, max: usize) -> Vec<String> {
        let mut bytes = if self.full { [&self.buf[self.pos..], &self.buf[..self.pos]].concat() } else { self.buf[..self.pos].to_vec() };
        if self.full && let Some(i) = bytes.iter().position(|&b| b == b'\n') {
            bytes.drain(..=i);
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(max)..].iter().map(|l| l.to_string()).collect()
    }
}

/* ---------- followers ---------- */

impl Watch {
    pub fn start(docker: Docker, metrics: Arc<metrics::Store>, state_dir: Option<PathBuf>) -> Arc<Watch> {
        let incidents = state_dir.map(|d| d.join("incidents")).filter(|d| fs::create_dir_all(d).is_ok());
        let w = Arc::new(Watch { docker, metrics, incidents, followers: Mutex::new(HashMap::new()), killed: Mutex::new(HashMap::new()), services: Mutex::new(HashMap::new()) });
        let me = w.clone();
        let _ = std::thread::Builder::new().name("logwatch".into()).stack_size(128 * 1024).spawn(move || me.supervise());
        w
    }

    pub fn incidents_dir(&self) -> Option<&PathBuf> {
        self.incidents.as_ref()
    }

    /// Follow every running container now, then every container that starts later.
    fn supervise(self: Arc<Self>) {
        loop {
            if let Ok(r) = self.docker.get("/containers/json") {
                for c in serde_json::from_slice::<Vec<Value>>(&r.body).unwrap_or_default() {
                    if let Some(id) = c["Id"].as_str() {
                        self.follow(id, c["Names"][0].as_str().unwrap_or("").trim_start_matches('/'));
                    }
                }
            }
            // filters={"type":["container"],"event":["start","kill","health_status"]}
            let path = "/events?filters=%7B%22type%22%3A%5B%22container%22%5D%2C%22event%22%3A%5B%22start%22%2C%22kill%22%2C%22health_status%22%5D%7D";
            if let Ok(Ok(mut s)) = self.docker.stream(path, Duration::from_secs(60)) {
                let mut buf = Vec::new();
                loop {
                    match s.read_into(&mut buf) {
                        Ok(Tick::Eof) | Err(_) => break,
                        Ok(Tick::Idle) => continue,
                        Ok(Tick::Data) => {}
                    }
                    while let Some(i) = buf.iter().position(|&b| b == b'\n') {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[..i])
                            && let Some(id) = v.pointer("/Actor/ID").and_then(Value::as_str)
                        {
                            if v["Action"] == "health_status: unhealthy" {
                                if let Some(c) = self.followers.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
                                    c.unhealthy.store(true, Ordering::Relaxed);
                                }
                            } else if v["Action"].as_str().is_some_and(|a| a.starts_with("health_status")) {
                                // healthy again, or starting: nothing to record
                            } else if v["Action"] == "kill" {
                                let mut k = self.killed.lock().unwrap_or_else(|e| e.into_inner());
                                let now = crate::unix_now();
                                k.retain(|_, t| now.saturating_sub(*t) < 300);
                                k.insert(id.to_string(), now);
                            } else {
                                self.follow(id, v.pointer("/Actor/Attributes/name").and_then(Value::as_str).unwrap_or(""));
                            }
                        }
                        buf.drain(..=i);
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(5)); // Docker went away; retry
        }
    }

    fn follow(self: &Arc<Self>, id: &str, name: &str) {
        let cell = {
            let mut f = self.followers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = f.get(id) {
                *c.name.lock().unwrap_or_else(|e| e.into_inner()) = name.to_string();
                if Arc::strong_count(c) > 1 {
                    return; // already followed
                }
            }
            let cell = Arc::new(Cell { snap: Mutex::new(Arc::new(Snapshot::default())), name: Mutex::new(name.to_string()), unhealthy: AtomicBool::new(false) });
            f.insert(id.to_string(), cell.clone());
            cell
        };
        let (me, id) = (self.clone(), id.to_string());
        let _ = std::thread::Builder::new().name("logfollow".into()).stack_size(128 * 1024).spawn(move || me.run(&id, &cell));
    }

    fn run(&self, id: &str, cell: &Arc<Cell>) {
        let started = crate::unix_now();
        let path = format!("/containers/{id}/logs?follow=1&stdout=1&stderr=1&tail=200");
        let Ok(Ok(mut s)) = self.docker.stream(&path, Duration::from_secs(5)) else { return };
        let (mut miner, mut ring, mut demux) = (Miner::new(started), Ring::new(), Demux::default());
        let (mut raw, mut text) = (Vec::with_capacity(8192), Vec::with_capacity(8192));
        let mut published = Instant::now();
        let mut dirty = false;
        *cell.snap.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(miner.snap.clone());
        loop {
            let tick = s.read_into(&mut raw).unwrap_or(Tick::Eof);
            demux.feed(&raw, &mut text);
            raw.clear();
            if matches!(tick, Tick::Eof) {
                demux.finish(&mut text);
            }
            let end = if matches!(tick, Tick::Data) { text.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) } else { text.len() };
            if end > 0 {
                let now = crate::unix_now();
                ring.push(&text[..end]);
                for line in String::from_utf8_lossy(&text[..end]).lines() {
                    miner.add(line, now);
                }
                text.drain(..end);
                dirty = true;
            }
            if dirty && (published.elapsed() >= PUBLISH_EVERY || matches!(tick, Tick::Eof)) {
                *cell.snap.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(miner.snap.clone());
                published = Instant::now();
                dirty = false;
            }
            if cell.unhealthy.swap(false, Ordering::Relaxed)
                && let Some(dir) = &self.incidents
            {
                self.record(dir, id, &miner, &ring, true);
            }
            if matches!(tick, Tick::Eof) {
                break;
            }
        }
        // The container stopped. Record it if it died badly.
        if let Some(dir) = &self.incidents {
            self.record(dir, id, &miner, &ring, false);
        }
    }

    /// Writes a postmortem: for an exit (`unhealthy == false`, skipped when the exit was clean or
    /// deliberate), or for a container whose health check just started failing while it runs.
    fn record(&self, dir: &PathBuf, id: &str, miner: &Miner, ring: &Ring, unhealthy: bool) {
        let Ok(r) = self.docker.get(&format!("/containers/{id}/json")) else { return };
        let d: Value = serde_json::from_slice(&r.body).unwrap_or_default();
        let st = &d["State"];
        // A restart policy may already have started it again; ExitCode still holds the last exit.
        let (code, oom) = (st["ExitCode"].as_i64().unwrap_or(0), st["OOMKilled"].as_bool().unwrap_or(false));
        let now = crate::unix_now();
        let deliberate = self.killed.lock().unwrap_or_else(|e| e.into_inner()).get(id).is_some_and(|t| now.saturating_sub(*t) < 30);
        // Clean exits (0), polite stops (143 = SIGTERM) and stop-then-SIGKILL (137 after a
        // kill event) are not incidents; an OOM kill always is.
        if !unhealthy && !oom && (code == 0 || code == 143 || (code == 137 && deliberate)) {
            return;
        }
        let reason = if unhealthy { "unhealthy" } else if oom { "oom" } else { "exit" };
        // The health check's own last outputs usually say exactly what failed.
        let health = unhealthy.then(|| {
            let log: Vec<Value> = st["Health"]["Log"]
                .as_array()
                .into_iter()
                .flatten()
                .rev()
                .take(3)
                .map(|l| json!({ "exit_code": l["ExitCode"], "output": l["Output"].as_str().unwrap_or("").trim().chars().take(400).collect::<String>(), "at": l["End"] }))
                .collect();
            json!({ "failing_streak": st["Health"]["FailingStreak"], "checks": log })
        });
        let name = d["Name"].as_str().unwrap_or("").trim_start_matches('/').to_string();
        let series = self.metrics.container(id, false, 90); // 15 minutes at 10 s
        let field = |i: usize| series.as_ref().map(|s| json!(s[i])).unwrap_or(json!([]));
        let snap = miner.snap.to_json(false);
        let errors: Vec<&Value> =
            snap["patterns"].as_array().into_iter().flatten().filter(|p| p["level"] == "error").take(10).collect();
        let new: Vec<&Value> = snap["patterns"].as_array().into_iter().flatten().filter(|p| p["new"] == true).take(10).collect();
        let labels = &d["Config"]["Labels"];

        // A restart loop updates the latest incident of the same container and exit code.
        let mut entries: Vec<(PathBuf, u64)> = fs::read_dir(dir)
            .map(|rd| rd.flatten().filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs()))).collect())
            .unwrap_or_default();
        entries.sort_by_key(|e| std::cmp::Reverse(e.1));
        let previous = entries.iter().find_map(|(p, _)| {
            let v: Value = serde_json::from_str(&fs::read_to_string(p).ok()?).ok()?;
            let same = if unhealthy { v["reason"] == "unhealthy" } else { v["reason"] != "unhealthy" && v["exit_code"] == code };
            (v["container"]["id"] == id && same && now.saturating_sub(v["last"].as_u64()?) < 600).then(|| (p.clone(), v))
        });
        let (path, first, repeats) = match previous {
            Some((p, v)) => (p, v["first"].as_u64().unwrap_or(now), v["repeats"].as_u64().unwrap_or(1) + 1),
            None => (dir.join(format!("{now}-{}.json", name.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_"))), now, 1),
        };
        let incident = json!({
            "id": path.file_stem().map(|s| s.to_string_lossy().into_owned()),
            "first": first,
            "last": now,
            "repeats": repeats,
            "container": {
                "id": id, "name": name, "image": d["Config"]["Image"],
                "stack": labels["com.docker.compose.project"], "service": labels["com.docker.compose.service"],
            },
            "reason": reason,
            "exit_code": if unhealthy { Value::Null } else { json!(code) },
            "oom_killed": oom && !unhealthy,
            "health": health,
            "error": st["Error"],
            "restarts": d["RestartCount"],
            "started_at": st["StartedAt"],
            "finished_at": st["FinishedAt"],
            "memory_limit": d["HostConfig"]["Memory"],
            "last_lines": ring.lines(150),
            "error_patterns": errors,
            "new_patterns": new,
            "metrics": { "interval": metrics::INTERVAL, "cpu": field(0), "mem": field(1) },
        });
        let _ = fs::write(&path, format!("{incident:#}"));
        // Keep the folder bounded.
        if entries.len() >= MAX_INCIDENTS {
            for (p, _) in entries.iter().skip(MAX_INCIDENTS - 1) {
                if *p != path {
                    let _ = fs::remove_file(p);
                }
            }
        }
    }

    /// One OTLP log record of `service`: mined like a container log line.
    pub fn ingest(&self, service: &str, severity: &str, body: &str) {
        let mut m = self.services.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= 64 && !m.contains_key(service) {
            return;
        }
        let now = crate::unix_now();
        let miner = m.entry(service.to_string()).or_insert_with(|| Miner::new(now));
        for line in body.lines().take(20) {
            if severity.is_empty() {
                miner.add(line, now);
            } else {
                miner.add(&format!("{severity} {line}"), now);
            }
        }
    }

    pub fn service_patterns(&self, service: &str) -> Option<Snapshot> {
        self.services.lock().unwrap_or_else(|e| e.into_inner()).get(service).map(|m| m.snap.clone())
    }

    /// Pattern snapshot of one container (by full ID or name).
    pub fn patterns(&self, id_or_name: &str) -> Option<Arc<Snapshot>> {
        let f = self.followers.lock().unwrap_or_else(|e| e.into_inner());
        let cell = f.get(id_or_name).or_else(|| {
            f.iter().find(|(id, c)| id.starts_with(id_or_name) || *c.name.lock().unwrap_or_else(|e| e.into_inner()) == id_or_name).map(|(_, c)| c)
        })?;
        Some(cell.snap.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// Per container: lines seen, pattern count, and the error patterns that are new since it started.
    pub fn overview(&self) -> Value {
        let f = self.followers.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = serde_json::Map::new();
        for (id, c) in f.iter() {
            let s = c.snap.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let warm = s.since + WARMUP;
            let new_errors = s.patterns.iter().filter(|p| p.level == Level::Error && p.first > warm).count();
            let errors = s.patterns.iter().filter(|p| p.level == Level::Error).map(|p| p.count).sum::<u64>();
            out.insert(
                id[..16.min(id.len())].to_string(),
                json!({ "lines": s.lines, "patterns": s.patterns.len(), "error_lines": errors, "new_error_patterns": new_errors, "following": Arc::strong_count(c) > 1 }),
            );
        }
        Value::Object(out)
    }

    pub fn following(&self) -> usize {
        self.followers.lock().map(|f| f.values().filter(|c| Arc::strong_count(c) > 1).count()).unwrap_or(0)
    }

    pub fn incident_list(&self, limit: usize) -> Vec<Value> {
        let Some(dir) = &self.incidents else { return Vec::new() };
        let mut v: Vec<Value> = fs::read_dir(dir)
            .map(|rd| rd.flatten().filter_map(|e| serde_json::from_str::<Value>(&fs::read_to_string(e.path()).ok()?).ok()).collect())
            .unwrap_or_default();
        v.sort_by_key(|i| std::cmp::Reverse(i["last"].as_u64().unwrap_or(0)));
        v.truncate(limit);
        v.into_iter()
            .map(|i| {
                json!({
                    "id": i["id"], "first": i["first"], "last": i["last"], "repeats": i["repeats"],
                    "container": i["container"], "reason": i["reason"], "exit_code": i["exit_code"], "oom_killed": i["oom_killed"],
                    "headline": i["health"]["checks"][0]["output"].as_str().filter(|s| !s.is_empty())
                        .or_else(|| i["error_patterns"][0]["example"].as_str())
                        .or_else(|| i["last_lines"].as_array()?.last()?.as_str()),
                })
            })
            .collect()
    }

    pub fn incident(&self, id: &str) -> Option<Value> {
        if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return None;
        }
        serde_json::from_str(&fs::read_to_string(self.incidents.as_ref()?.join(format!("{id}.json"))).ok()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_variables() {
        let mut k = String::new();
        mask("2026-10-10T12:51:13.282Z GET /api/items/42 200 1.2ms user=17 trace=4bf92f3577b34da6a3ce929d0e0e4736 from 10.0.0.7:5432 ok", &mut k);
        assert_eq!(k, "<*> GET /api/items/<*> <*> user=<*> trace=<*> from <*> ok");
        mask("[2026-10-10 09:53:28.377][8][warning][main] [source/server/server.cc:1038] There is no limit", &mut k);
        assert_eq!(k, "[<*> <*>][<*>][warning][main] [source/server/server.cc:1038] There is no limit");
        mask("\tgo.opentelemetry.io/collector/service@v0.160.0/telemetry/logger.go:69", &mut k);
        assert_eq!(k, "go.opentelemetry.io/collector/service@v0.160.0/telemetry/logger.go:69", "code locations stay readable");
    }

    #[test]
    fn groups_lines_into_patterns() {
        let mut m = Miner::new(0);
        for i in 0..100 {
            m.add(&format!("GET /api/items/{i} 200 {}ms", i % 7), 1);
        }
        m.add("ERROR connection refused to db:5432", 2);
        m.add("ERROR connection refused to cache:6379", 3);
        m.add("listening on port", 3);
        m.add("listening at port", 4); // same length, same first word, 2/3 equal → merged? (66% < 70%: new)
        let s = &m.snap;
        assert_eq!(s.lines, 104);
        assert_eq!(s.patterns[0].count, 100);
        let err = s.patterns.iter().find(|p| p.text.starts_with("ERROR")).unwrap();
        assert_eq!(err.count, 2, "{}", err.text);
        assert!(err.level == Level::Error);
    }

    #[test]
    fn similar_patterns_merge() {
        assert_eq!(similar("user alice logged in from web", "user bob logged in from web").as_deref(), Some("user <*> logged in from web"));
        assert_eq!(similar("a b c", "x b c"), None);
    }

    #[test]
    fn pattern_table_is_bounded() {
        let mut m = Miner::new(0);
        for i in 0..1000 {
            m.add(&format!("unique{} word{} other{}", "a".repeat(i % 50), "b".repeat(i % 37), "c".repeat(i % 11)), 1);
        }
        assert!(m.snap.patterns.len() <= MAX_PATTERNS);
    }

    #[test]
    fn ring_keeps_the_tail() {
        let mut r = Ring::new();
        for i in 0..5000 {
            r.push(format!("line {i}\n").as_bytes());
        }
        let l = r.lines(3);
        assert_eq!(l, ["line 4997", "line 4998", "line 4999"]);
        let mut small = Ring::new();
        small.push(b"one\ntwo\n");
        assert_eq!(small.lines(10), ["one", "two"]);
    }

    #[test]
    #[ignore]
    fn bench_logwatch_mine_1mb() {
        let lines: Vec<String> = (0..10_000).map(|i| format!("2026-10-10T09:50:03Z GET /api/items/{i} 200 {}ms user={} trace=4bf92f3577b34da6a3ce929d0e0e4736", i % 9, i % 13)).collect();
        let bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
        let mut m = Miner::new(0);
        crate::bench::run("logwatch: mine 10k lines", bytes, || {
            for l in &lines {
                m.add(l, 1);
            }
        });
    }
}
