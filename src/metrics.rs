//! Container and host metrics, sampled straight from cgroup v2 and /proc every 10 s.
//!
//! Docker's stats API costs about a second of daemon work per container; reading a
//! handful of small files under /sys/fs/cgroup costs microseconds. Containers are found by
//! scanning the cgroup tree (systemd driver: `system.slice/docker-<id>.scope`, cgroupfs:
//! `docker/<id>`), so the sampler never calls the Docker API.
//!
//! History lives in fixed rings of `AtomicU32` (f32 bits): one sampler thread writes, any
//! number of request threads read, with no locks and no per-sample allocation. Each
//! container gets 1 h at 10 s and 24 h at 1 min, about 50 KB, allocated on first sight.
//! Slots outlive their container (useful for postmortems) and are reused least-recently-seen.
//!
//! The minute averages are also appended to `<state>/metrics.bin` (40 bytes per container
//! per minute, rotated at 2 MB, so ~2 MB/day of SD-card writes for 30 containers) and
//! replayed on start, so the 24 h history survives restarts and reboots.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub const INTERVAL: u64 = 10;
pub const FINE: usize = 360; // 1 h of 10 s samples
pub const COARSE: usize = 1440; // 24 h of 1 min samples
const PER_COARSE: u64 = 6;
const SLOTS: usize = 96;

/// Fields stored per sample, in this order.
pub const FIELDS: [&str; 7] = ["cpu", "mem", "rx", "tx", "read", "write", "pids"];
const NF: usize = FIELDS.len();
pub const HOST_FIELDS: [&str; 7] = ["cpu", "mem", "load1", "rx", "tx", "mem_total", "cpus"];

struct Ring {
    written: AtomicU64,
    cells: Box<[AtomicU32]>,
    len: usize,
}

impl Ring {
    fn new(len: usize) -> Ring {
        Ring { written: AtomicU64::new(0), cells: (0..len * NF).map(|_| AtomicU32::new(0)).collect(), len }
    }

    fn push(&self, v: &[f32; NF]) {
        let w = self.written.load(Ordering::Relaxed);
        let base = (w as usize % self.len) * NF;
        for (i, x) in v.iter().enumerate() {
            self.cells[base + i].store(x.to_bits(), Ordering::Relaxed);
        }
        self.written.store(w + 1, Ordering::Release);
    }

    /// The last `n` values of `field`, oldest first. Stays one sample clear of the writer.
    fn series(&self, field: usize, n: usize) -> Vec<f32> {
        let w = self.written.load(Ordering::Acquire) as usize;
        let n = n.min(w).min(self.len - 1);
        (w - n..w).map(|i| f32::from_bits(self.cells[(i % self.len) * NF + field].load(Ordering::Relaxed))).collect()
    }

    fn last(&self, field: usize) -> Option<f32> {
        self.series(field, 1).first().copied()
    }
}

struct Rings {
    fine: Ring,
    coarse: Ring,
}

impl Rings {
    fn new() -> Rings {
        Rings { fine: Ring::new(FINE), coarse: Ring::new(COARSE) }
    }
}

struct Slot {
    /// First 16 hex digits of the container ID; 0 = free.
    key: AtomicU64,
    last_seen: AtomicU64,
    rings: OnceLock<Box<Rings>>,
    /// Set when the slot is reused for another container, so readers ignore stale data.
    generation: AtomicU64,
}

/// (container key, latest values, CPU sparkline, memory sparkline)
pub type Latest = (u64, [f32; NF], Vec<f32>, Vec<f32>);

pub struct Store {
    slots: Box<[Slot]>,
    host: Rings,
    pub root: Option<PathBuf>,
    samples: AtomicU64,
    /// Nanoseconds the last sampling pass took (self-telemetry).
    pub last_pass_ns: AtomicU64,
}

pub fn key_of(id: &str) -> Option<u64> {
    id.get(..16).and_then(|h| u64::from_str_radix(h, 16).ok())
}

impl Store {
    fn new(root: Option<PathBuf>) -> Store {
        Store {
            slots: (0..SLOTS)
                .map(|_| Slot { key: AtomicU64::new(0), last_seen: AtomicU64::new(0), rings: OnceLock::new(), generation: AtomicU64::new(0) })
                .collect(),
            host: Rings::new(),
            root,
            samples: AtomicU64::new(0),
            last_pass_ns: AtomicU64::new(0),
        }
    }

    fn find(&self, key: u64) -> Option<&Slot> {
        self.slots.iter().find(|s| s.key.load(Ordering::Acquire) == key)
    }

    /// Sampler only: the slot for `key`, claiming a free or least-recently-seen one.
    fn claim(&self, key: u64, now: u64) -> &Slot {
        if let Some(s) = self.find(key) {
            return s;
        }
        let s = self
            .slots
            .iter()
            .min_by_key(|s| if s.key.load(Ordering::Relaxed) == 0 { 0 } else { s.last_seen.load(Ordering::Relaxed) + 1 })
            .expect("slots");
        s.generation.fetch_add(1, Ordering::Relaxed);
        if let Some(r) = s.rings.get() {
            r.fine.written.store(0, Ordering::Release);
            r.coarse.written.store(0, Ordering::Release);
        }
        s.last_seen.store(now, Ordering::Relaxed);
        s.key.store(key, Ordering::Release);
        s
    }

    pub fn available(&self) -> bool {
        self.root.is_some()
    }

    pub fn samples(&self) -> u64 {
        self.samples.load(Ordering::Relaxed)
    }

    /// Series of one container: `fine` (10 s) or `coarse` (1 min), last `n` points.
    pub fn container(&self, id: &str, coarse: bool, n: usize) -> Option<Vec<Vec<f32>>> {
        let slot = self.find(key_of(id)?)?;
        let r = slot.rings.get()?;
        let ring = if coarse { &r.coarse } else { &r.fine };
        Some((0..NF).map(|f| ring.series(f, n)).collect())
    }

    /// Latest sample of every container seen in the last two intervals, keyed by ID prefix.
    pub fn latest(&self, now: u64, spark: usize) -> Vec<Latest> {
        self.slots
            .iter()
            .filter(|s| s.key.load(Ordering::Acquire) != 0 && now.saturating_sub(s.last_seen.load(Ordering::Relaxed)) <= 2 * INTERVAL + 2)
            .filter_map(|s| {
                let r = s.rings.get()?;
                let mut v = [0f32; NF];
                for (i, x) in v.iter_mut().enumerate() {
                    *x = r.fine.last(i)?;
                }
                Some((s.key.load(Ordering::Relaxed), v, r.fine.series(0, spark), r.fine.series(1, spark)))
            })
            .collect()
    }

    pub fn host(&self, coarse: bool, n: usize) -> Vec<Vec<f32>> {
        let ring = if coarse { &self.host.coarse } else { &self.host.fine };
        (0..NF).map(|f| ring.series(f, n)).collect()
    }

    pub fn host_latest(&self) -> Option<[f32; NF]> {
        let mut v = [0f32; NF];
        for (i, x) in v.iter_mut().enumerate() {
            *x = self.host.fine.last(i)?;
        }
        Some(v)
    }
}

/// Where Docker's container cgroups live on this host, if anywhere we can read.
fn detect_root(base: &Path) -> Option<PathBuf> {
    [base.join("system.slice"), base.join("docker")].into_iter().find(|p| {
        fs::read_dir(p).is_ok_and(|mut d| {
            d.any(|e| e.is_ok_and(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                (n.starts_with("docker-") && n.ends_with(".scope")) || (n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit()))
            }))
        })
    })
}

fn container_id(name: &str) -> Option<&str> {
    let id = name.strip_prefix("docker-").and_then(|n| n.strip_suffix(".scope")).unwrap_or(name);
    (id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

/// Reads small files into one reusable buffer.
struct Reader {
    buf: String,
}

impl Reader {
    fn read(&mut self, p: &Path) -> Option<&str> {
        self.buf.clear();
        fs::File::open(p).ok()?.read_to_string(&mut self.buf).ok()?;
        Some(&self.buf)
    }

    fn number(&mut self, p: &Path) -> Option<f64> {
        self.read(p)?.trim().parse().ok()
    }

    fn keyed(&mut self, p: &Path, key: &str) -> Option<f64> {
        // "usage_usec 42" (cgroup) and "MemTotal:   16052968 kB" (/proc/meminfo) alike.
        self.read(p)?.lines().find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse().ok())
    }

    fn io(&mut self, p: &Path) -> (f64, f64) {
        let Some(t) = self.read(p) else { return (0.0, 0.0) };
        let (mut r, mut w) = (0.0, 0.0);
        for kv in t.split_whitespace() {
            if let Some(v) = kv.strip_prefix("rbytes=") {
                r += v.parse::<f64>().unwrap_or(0.0);
            } else if let Some(v) = kv.strip_prefix("wbytes=") {
                w += v.parse::<f64>().unwrap_or(0.0);
            }
        }
        (r, w)
    }

    /// Received/sent bytes on all non-loopback interfaces of a network namespace.
    fn net(&mut self, p: &Path) -> (f64, f64) {
        let Some(t) = self.read(p) else { return (0.0, 0.0) };
        let (mut rx, mut tx) = (0.0, 0.0);
        for l in t.lines().skip(2) {
            let Some((iface, rest)) = l.split_once(':') else { continue };
            if iface.trim() == "lo" {
                continue;
            }
            let f: Vec<&str> = rest.split_whitespace().collect();
            rx += f.first().and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
            tx += f.get(8).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
        }
        (rx, tx)
    }
}

/// Cumulative counters from the previous pass, for rates. Sampler-private.
#[derive(Default, Clone, Copy)]
struct Prev {
    cpu_us: f64,
    rx: f64,
    tx: f64,
    rd: f64,
    wr: f64,
    at: Option<Instant>,
}

/// Append-only minute records: u32 minute (unix s), u64 key (`HOST_KEY` = host), 7 × f32, LE.
const REC: usize = 4 + 8 + NF * 4;
const HOST_KEY: u64 = u64::MAX;
const HISTORY_MAX: u64 = 2 << 20;

struct History {
    path: PathBuf,
    buf: Vec<u8>,
}

impl History {
    fn add(&mut self, now: u64, key: u64, v: &[f32; NF]) {
        self.buf.extend_from_slice(&((now / 60 * 60) as u32).to_le_bytes());
        self.buf.extend_from_slice(&key.to_le_bytes());
        for x in v {
            self.buf.extend_from_slice(&x.to_le_bytes());
        }
    }

    /// One `O_APPEND` write per pass that completed a minute; past 2 MB the file moves to `.1`.
    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        if fs::metadata(&self.path).is_ok_and(|m| m.len() > HISTORY_MAX) {
            let _ = fs::rename(&self.path, self.path.with_extension("bin.1"));
        }
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = f.write_all(&self.buf);
        }
        self.buf.clear();
    }
}

/// Replays the last 24 h of minute records for the host and the containers in `keys` (running
/// now: history of long-gone containers would only cost memory). Missing minutes, e.g. while
/// the server was down, become NaN (`null` in JSON), so the series stays aligned to the clock.
fn restore(store: &Store, path: &Path, keys: &[u64], now: u64) -> usize {
    let mut last: HashMap<u64, u64> = HashMap::new();
    let mut n = 0;
    let gap = [f32::NAN; NF];
    let ring_of = |key: u64, ts: u64| -> &Ring {
        if key == HOST_KEY {
            &store.host.coarse
        } else {
            let slot = store.claim(key, ts);
            &slot.rings.get_or_init(|| Box::new(Rings::new())).coarse
        }
    };
    for p in [path.with_extension("bin.1"), path.to_path_buf()] {
        let Ok(bytes) = fs::read(&p) else { continue };
        #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)] // as_chunks needs Rust 1.88; Pi distros ship older
        for r in bytes.chunks_exact(REC) {
            let ts = u32::from_le_bytes(r[0..4].try_into().unwrap()) as u64;
            let key = u64::from_le_bytes(r[4..12].try_into().unwrap());
            if ts + 86_400 < now || ts > now || (key != HOST_KEY && !keys.contains(&key)) {
                continue;
            }
            let mut v = [0f32; NF];
            for (i, x) in v.iter_mut().enumerate() {
                *x = f32::from_le_bytes(r[12 + i * 4..16 + i * 4].try_into().unwrap());
            }
            let ring = ring_of(key, ts);
            match last.get(&key) {
                Some(&l) if ts <= l => continue,
                Some(&l) => (0..((ts - l) / 60).saturating_sub(1).min(COARSE as u64)).for_each(|_| ring.push(&gap)),
                None => {}
            }
            ring.push(&v);
            last.insert(key, ts);
            n += 1;
        }
    }
    for (key, l) in last {
        let ring = ring_of(key, l);
        (0..((now / 60 * 60).saturating_sub(l) / 60).saturating_sub(1).min(COARSE as u64)).for_each(|_| ring.push(&gap));
    }
    n
}

#[derive(Default)]
struct Acc {
    sum: [f32; NF],
    n: u64,
}

impl Acc {
    /// Adds a sample; returns the minute average when one completes.
    fn add(&mut self, v: &[f32; NF], ring: &Ring) -> Option<[f32; NF]> {
        for (s, x) in self.sum.iter_mut().zip(v) {
            *s += x;
        }
        self.n += 1;
        if self.n == PER_COARSE {
            let mut avg = [0f32; NF];
            for (a, s) in avg.iter_mut().zip(self.sum) {
                *a = s / PER_COARSE as f32;
            }
            ring.push(&avg);
            *self = Acc::default();
            return Some(avg);
        }
        None
    }
}

pub fn start(cgroup_base: &str, state_dir: Option<&Path>) -> Arc<Store> {
    let store = Arc::new(Store::new(detect_root(Path::new(cgroup_base))));
    let history = state_dir.map(|d| History { path: d.join("metrics.bin"), buf: Vec::with_capacity(64 * REC) });
    if let Some(h) = &history {
        let running: Vec<u64> = store
            .root
            .as_ref()
            .and_then(|r| fs::read_dir(r).ok())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| container_id(&e.file_name().to_string_lossy()).and_then(key_of))
            .collect();
        restore(&store, &h.path, &running, crate::unix_now());
    }
    let s = store.clone();
    let _ = std::thread::Builder::new().name("metrics".into()).stack_size(128 * 1024).spawn(move || sampler(&s, history));
    store
}

fn rate(now: f64, before: f64, secs: f64) -> f32 {
    if secs <= 0.0 || now < before { 0.0 } else { ((now - before) / secs) as f32 }
}

fn sampler(store: &Store, mut history: Option<History>) {
    let mut rd = Reader { buf: String::with_capacity(8192) };
    let mut prev: HashMap<u64, Prev> = HashMap::new();
    let mut acc: HashMap<u64, Acc> = HashMap::new();
    let (mut host_prev, mut host_acc) = ((0.0f64, 0.0f64, Prev::default()), Acc::default());
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f32;
    let mut path = PathBuf::with_capacity(256);
    let mut ids: Vec<String> = Vec::with_capacity(64);
    loop {
        let t0 = Instant::now();
        let now = crate::unix_now();

        // Host: CPU from /proc/stat deltas, memory from /proc/meminfo.
        if let Some(line) = rd.read(Path::new("/proc/stat")).and_then(|s| s.lines().next().map(str::to_string)) {
            let f: Vec<f64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            let total: f64 = f.iter().sum();
            let idle = f.get(3).copied().unwrap_or(0.0) + f.get(4).copied().unwrap_or(0.0);
            let (pt, pi, mut np) = host_prev;
            let cpu = if pt > 0.0 && total > pt { (1.0 - (idle - pi) / (total - pt)) * 100.0 } else { 0.0 };
            let mem_total = rd.keyed(Path::new("/proc/meminfo"), "MemTotal:").unwrap_or(0.0) * 1024.0;
            let mem_avail = rd.keyed(Path::new("/proc/meminfo"), "MemAvailable:").unwrap_or(0.0) * 1024.0;
            let load = rd.read(Path::new("/proc/loadavg")).and_then(|s| s.split_whitespace().next()?.parse::<f32>().ok()).unwrap_or(0.0);
            let (rx, tx) = rd.net(Path::new("/proc/net/dev"));
            let secs = np.at.map_or(0.0, |a| a.elapsed().as_secs_f64());
            let v = [cpu as f32, (mem_total - mem_avail) as f32, load, rate(rx, np.rx, secs), rate(tx, np.tx, secs), mem_total as f32, cpus];
            if np.at.is_some() {
                store.host.fine.push(&v);
                if let (Some(avg), Some(h)) = (host_acc.add(&v, &store.host.coarse), history.as_mut()) {
                    h.add(now, HOST_KEY, &avg);
                }
            }
            np.rx = rx;
            np.tx = tx;
            np.at = Some(Instant::now());
            host_prev = (total, idle, np);
        }

        // Containers.
        if let Some(root) = &store.root {
            ids.clear();
            if let Ok(dir) = fs::read_dir(root) {
                for e in dir.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if container_id(&name).is_some() {
                        ids.push(name);
                    }
                }
            }
            for name in &ids {
                let Some(id) = container_id(name) else { continue };
                let Some(key) = key_of(id) else { continue };
                path.clear();
                path.push(root);
                path.push(name);
                let file = |f: &str| {
                    let mut p = path.clone();
                    p.push(f);
                    p
                };
                let Some(cpu_us) = rd.keyed(&file("cpu.stat"), "usage_usec") else { continue };
                let mem = rd.number(&file("memory.current")).unwrap_or(0.0)
                    - rd.keyed(&file("memory.stat"), "inactive_file").unwrap_or(0.0);
                let pids = rd.number(&file("pids.current")).unwrap_or(0.0);
                let (r, w) = rd.io(&file("io.stat"));
                let pid = rd.read(&file("cgroup.procs")).and_then(|s| s.lines().next().map(str::to_string));
                let (rx, tx) = pid.map_or((0.0, 0.0), |p| rd.net(&PathBuf::from(format!("/proc/{p}/net/dev"))));

                let p = prev.entry(key).or_default();
                let secs = p.at.map_or(0.0, |a| a.elapsed().as_secs_f64());
                let v = [
                    rate(cpu_us, p.cpu_us, secs) / 10_000.0, // µs per s → % of one core
                    mem.max(0.0) as f32,
                    rate(rx, p.rx, secs),
                    rate(tx, p.tx, secs),
                    rate(r, p.rd, secs),
                    rate(w, p.wr, secs),
                    pids as f32,
                ];
                let first = p.at.is_none();
                *p = Prev { cpu_us, rx, tx, rd: r, wr: w, at: Some(Instant::now()) };
                let slot = store.claim(key, now);
                slot.last_seen.store(now, Ordering::Relaxed);
                if first {
                    continue; // rates need two samples
                }
                let rings = slot.rings.get_or_init(|| Box::new(Rings::new()));
                rings.fine.push(&v);
                if let (Some(avg), Some(h)) = (acc.entry(key).or_default().add(&v, &rings.coarse), history.as_mut()) {
                    h.add(now, key, &avg);
                }
            }
            // Forget counters of containers that are gone (their history stays in the slot).
            if prev.len() > ids.len() + 16 {
                let live: Vec<u64> = ids.iter().filter_map(|n| container_id(n).and_then(key_of)).collect();
                prev.retain(|k, _| live.contains(k));
                acc.retain(|k, _| live.contains(k));
            }
        }
        if let Some(h) = history.as_mut() {
            h.flush();
        }
        store.samples.fetch_add(1, Ordering::Relaxed);
        store.last_pass_ns.store(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        let spent = t0.elapsed();
        std::thread::sleep(Duration::from_secs(INTERVAL).saturating_sub(spent));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_latest_values_in_order() {
        let r = Ring::new(4);
        for i in 0..10 {
            r.push(&[i as f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        }
        assert_eq!(r.series(0, 10), vec![7.0, 8.0, 9.0]);
        assert_eq!(r.last(0), Some(9.0));
    }

    #[test]
    fn history_survives_a_restart_with_gaps() {
        let dir = std::env::temp_dir().join(format!("contraire-hist-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metrics.bin");
        let _ = fs::remove_file(&path);
        let now = 1_800_000_000 / 60 * 60;
        let mut h = History { path: path.clone(), buf: Vec::new() };
        // Container 7: minutes -10 and -9, then down until -3; the host only at -5.
        for (t, key, cpu) in [(now - 600, 7, 1.0), (now - 540, 7, 2.0), (now - 180, 7, 3.0), (now - 300, HOST_KEY, 50.0)] {
            h.add(t, key, &[cpu, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        }
        h.add(now - 60, 8, &[9.0; NF]); // not running any more: skipped
        h.flush();
        let store = Store::new(None);
        assert_eq!(restore(&store, &path, &[7], now), 4);
        let cpu = store.container(&format!("{:016x}", 7u64), true, COARSE).unwrap().remove(0);
        // -10, -9, five missing minutes, -3, then -2 and -1 missing up to now.
        assert_eq!(cpu.len(), 10);
        assert_eq!((cpu[0], cpu[1], cpu[7]), (1.0, 2.0, 3.0));
        assert!(cpu[2..7].iter().all(|x| x.is_nan()) && cpu[8..].iter().all(|x| x.is_nan()));
        assert_eq!(store.host(true, COARSE)[0].first(), Some(&50.0));
        assert!(store.find(8).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_and_keys() {
        let id = "0e5b3220552a0d2b5306b29695268dc01fa00349396fdf65002656d0b71a9b61";
        assert_eq!(container_id(&format!("docker-{id}.scope")), Some(id));
        assert_eq!(container_id(id), Some(id));
        assert_eq!(container_id("docker.service"), None);
        assert_eq!(key_of(id), Some(0x0e5b3220552a0d2b));
    }

    #[test]
    fn slots_are_reused_least_recently_seen() {
        let s = Store::new(None);
        for k in 1..=SLOTS as u64 {
            s.claim(k, k);
        }
        s.claim(9999, 10_000);
        assert!(s.find(1).is_none(), "oldest slot was reused");
        assert!(s.find(9999).is_some());
    }

    #[test]
    fn reader_parses_cgroup_files() {
        let dir = std::env::temp_dir().join(format!("contraire-metrics-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("io.stat"), "8:0 rbytes=100 wbytes=50 rios=1\n251:0 rbytes=1 wbytes=2\n").unwrap();
        fs::write(dir.join("net"), "h1\nh2\n    lo: 999 1 0 0 0 0 0 0 999 1\n  eth0: 10 1 0 0 0 0 0 0 20 1\n").unwrap();
        fs::write(dir.join("cpu.stat"), "usage_usec 42\nuser_usec 1\n").unwrap();
        let mut r = Reader { buf: String::new() };
        assert_eq!(r.io(&dir.join("io.stat")), (101.0, 52.0));
        assert_eq!(r.net(&dir.join("net")), (10.0, 20.0));
        assert_eq!(r.keyed(&dir.join("cpu.stat"), "usage_usec"), Some(42.0));
        let _ = fs::remove_dir_all(dir);
    }
}
