#[cfg(test)]
mod bench;
mod audit;
mod cli;
mod client;
mod compose;
mod docker;
mod http;
mod live;
mod logwatch;
mod metrics;
mod otlp;
mod tokens;

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use docker::Docker;
use http::{Request, Response};
use serde_json::{Value, json};

/// Human-facing name of the tool. The crate, binary and repository stay `contraire`.
pub const DISPLAY_NAME: &str = "mopsfraugibtdentonan";

/// The web page, gzip-compressed at build time when possible (see build.rs).
const INDEX_PAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.bin"));
const PAGE_ENCODING: &str = env!("PAGE_ENCODING");

struct App {
    docker: Docker,
    /// Expected `Authorization` header value, if a password is configured.
    auth: Option<String>,
    audit: audit::Audit,
    /// The Basic-auth user, when a password is configured.
    user: Option<String>,
    tokens: tokens::Store,
    metrics: Arc<metrics::Store>,
    logs: Arc<logwatch::Watch>,
    otlp: Arc<otlp::Otlp>,
    /// Base URL the Learn tab fetches its content from (never embedded in the binary).
    docs_url: String,
    listen: String,
    started: std::time::Instant,
    started_unix: u64,
    active: Arc<AtomicUsize>,
    requests: std::sync::atomic::AtomicU64,
}

fn main() {
    // One binary, two roles: `contraire` serves; `contraire <command>` is the client.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| client::COMMANDS.contains(&a.as_str())) {
        std::process::exit(client::run(&args));
    }
    let cfg = match cli::parse(std::env::args_os(), |k| std::env::var(k).ok()) {
        Ok(cli::Cli::Run(cfg)) => cfg,
        Ok(cli::Cli::Done) => return,
        Err(e) => {
            eprintln!("contraire: {e}\nTry 'contraire --help' for more information.");
            std::process::exit(2);
        }
    };
    let auth = cfg
        .password
        .as_ref()
        .map(|pw| format!("Basic {}", http::base64(format!("{}:{pw}", cfg.user).as_bytes())));

    let listener = TcpListener::bind(&cfg.listen).unwrap_or_else(|e| {
        eprintln!("contraire: cannot listen on {}: {e}", cfg.listen);
        std::process::exit(1);
    });
    let docker = Docker::parse(&cfg.docker).unwrap_or_else(|e| {
        eprintln!("contraire: {e}\nTry 'contraire --help' for more information.");
        std::process::exit(2);
    });
    let active = Arc::new(AtomicUsize::new(0));
    let audit = audit::Audit::open(&cfg.audit, |k| std::env::var(k).ok());
    let state_dir = audit.path().and_then(|p| p.parent()).map(Path::to_path_buf);
    let token_store = tokens::Store::open(state_dir.as_ref().map(|d| d.join("tokens.json")));
    let metrics_store = metrics::start("/sys/fs/cgroup", state_dir.as_deref());
    let watch = logwatch::Watch::start(docker.clone(), metrics_store.clone(), state_dir);
    let app = Arc::new(App {
        docker: docker.clone(),
        auth,
        docs_url: cfg.docs_url,
        audit,
        user: cfg.password.is_some().then(|| cfg.user.clone()),
        tokens: token_store,
        otlp: otlp::Otlp::start(watch.clone(), cfg.otlp_forward.clone()),
        logs: watch,
        metrics: metrics_store,
        listen: cfg.listen.clone(),
        started: std::time::Instant::now(),
        started_unix: unix_now(),
        active: active.clone(),
        requests: std::sync::atomic::AtomicU64::new(0),
    });

    eprintln!("contraire: {DISPLAY_NAME} listening on http://{} (docker: {})", cfg.listen, app.docker.describe());
    match app.audit.path() {
        Some(p) => eprintln!("contraire: audit log: {}", p.display()),
        None => eprintln!("contraire: audit log: off (API tokens need it too)"),
    }
    if !app.metrics.available() {
        eprintln!("contraire: metrics: no container cgroups found under /sys/fs/cgroup (host metrics only)");
    }
    if let Some(p) = app.tokens.path() {
        eprintln!("contraire: API tokens: {} ({} defined)", p.display(), app.tokens.len());
    }
    let loopback = listener.local_addr().is_ok_and(|a| a.ip().is_loopback());
    if !loopback && app.auth.is_none() {
        eprintln!(
            "contraire: WARNING: reachable from the network without a password; anyone on it can control Docker \
             (use --password-file or CONTRAIRE_PASSWORD)"
        );
    }

    for stream in listener.incoming().flatten() {
        if active.fetch_add(1, Ordering::Relaxed) >= cfg.max_connections {
            active.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        let slot = Slot(active.clone());
        let app = app.clone();
        // If spawning fails the closure (and the slot) is dropped, releasing the slot.
        let _ = std::thread::Builder::new().stack_size(256 * 1024).spawn(move || {
            let _slot = slot;
            http::serve(stream, |req| app.handle(req));
        });
    }
}

/// Releases a connection slot when the handler thread ends.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `/proc/self/status` fields in kB (or counts), Linux only.
fn proc_status(key: &str) -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with(key))?;
    line[key.len()..].trim_start_matches(':').split_whitespace().next()?.parse().ok()
}

/// User + system CPU seconds of this process. Assumes USER_HZ = 100, which Linux fixes for
/// the /proc ABI on every architecture contraire targets.
fn proc_cpu_seconds() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the ")" that closes the command name; utime and stime are 14 and 15.
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}

struct Identity {
    name: Option<String>,
    /// "token", "password" or "open".
    via: &'static str,
    scope: tokens::Scope,
}

/// Least privilege: reading needs `read`; day-to-day operations (start, stop, restart,
/// pause, stack actions) need `operate`; anything destructive or about access needs `admin`.
fn required_scope(req: &Request) -> tokens::Scope {
    use tokens::Scope::*;
    let segs: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    match (req.method.as_str(), segs.as_slice()) {
        (_, ["api", "tokens", ..]) | (_, ["api", "audit"]) => Admin,
        ("POST", ["v1", "traces" | "logs" | "metrics"]) => Read,
        ("GET" | "HEAD", _) => Read,
        ("POST", ["api", "containers", _, "start" | "stop" | "restart" | "pause" | "unpause"]) => Operate,
        ("POST", ["api", "projects", _, _]) => Operate,
        _ => Admin,
    }
}

/// `%20` and friends in one path segment (service names may contain spaces or slashes).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Rows as JSON lines (default) or CSV with a header from the first row's keys.
fn export(req: &Request, rows: Vec<Value>) -> Response {
    if req.param("format") == Some("csv") {
        let cols: Vec<String> = rows.first().and_then(Value::as_object).map(|o| o.keys().cloned().collect()).unwrap_or_default();
        let mut out = cols.join(",") + "\n";
        for r in &rows {
            let cells: Vec<String> = cols
                .iter()
                .map(|c| match &r[c] {
                    Value::String(s) => format!("\"{}\"", s.replace('"', "\"\"")),
                    Value::Null => String::new(),
                    v => v.to_string(),
                })
                .collect();
            out += &(cells.join(",") + "\n");
        }
        return Response::new(200, "text/csv; charset=utf-8", out);
    }
    let mut out = String::new();
    for r in rows {
        out += &(r.to_string() + "\n");
    }
    Response::new(200, "application/x-ndjson", out)
}

/// What a mutating request does, for the audit log: (`kind.verb`, target).
fn audit_subject(req: &Request) -> Option<(String, String)> {
    if matches!(req.method.as_str(), "GET" | "HEAD") {
        return None;
    }
    let segs: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    Some(match (req.method.as_str(), segs.as_slice()) {
        ("POST", ["api", "containers", "prune"]) => ("container.prune".into(), "stopped".into()),
        ("POST", ["api", "containers", id, verb]) => (format!("container.{verb}"), id.to_string()),
        ("DELETE", ["api", "containers", id]) => ("container.remove".into(), id.to_string()),
        ("POST", ["api", "images", "pull"]) => {
            let image = serde_json::from_slice::<Value>(&req.body)
                .ok()
                .and_then(|v| v.get("image")?.as_str().map(|s| s.chars().take(256).collect()))
                .unwrap_or_default();
            ("image.pull".into(), image)
        }
        ("POST", ["api", "images", "prune"]) => ("image.prune".into(), "dangling".into()),
        ("DELETE", ["api", "images", id]) => ("image.remove".into(), id.to_string()),
        ("POST", ["api", "projects", name, verb]) => (format!("project.{verb}"), name.to_string()),
        ("POST", ["api", "tokens"]) => {
            let name = serde_json::from_slice::<Value>(&req.body)
                .ok()
                .and_then(|v| v.get("name")?.as_str().map(|s| s.chars().take(64).collect()))
                .unwrap_or_default();
            ("token.create".into(), name)
        }
        ("DELETE", ["api", "tokens", id]) => ("token.revoke".into(), id.to_string()),
        _ => return None,
    })
}

/// `https://host:port/some/path` → `https://host:port`
fn origin(url: &str) -> &str {
    let rest = url.find("://").map_or(0, |i| i + 3);
    url[rest..].find('/').map_or(url, |i| &url[..rest + i])
}

/// Container names, IDs and image IDs (`sha256:…`). Rejects anything that could escape the path.
fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-:".contains(&b))
}

impl App {
    fn handle(&self, req: &Request) -> Response {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let bearer = req.header("authorization").and_then(|h| h.strip_prefix("Bearer ")).map(str::trim);
        let who = match bearer {
            Some(t) => match self.tokens.check(t, unix_now()) {
                tokens::Check::Valid { name, scope } => Identity { name: Some(name), via: "token", scope },
                tokens::Check::Invalid => return Response::error(401, "invalid, revoked or expired API token"),
            },
            None => {
                if let Some(expected) = &self.auth {
                    let given = req.header("authorization").unwrap_or("");
                    if !http::ct_eq(given.as_bytes(), expected.as_bytes()) {
                        return Response::error(401, "authentication required")
                            .with_header("WWW-Authenticate", "Basic realm=\"contraire\", charset=\"UTF-8\"");
                    }
                }
                let via = if self.auth.is_some() { "password" } else { "open" };
                Identity { name: self.user.clone(), via, scope: tokens::Scope::Admin }
            }
        };
        // CSRF guard for browser sessions: a custom header can't be sent cross-origin without
        // a CORS preflight, which we never answer. Bearer tokens are never sent implicitly.
        // OTLP exports are exempt: their content types (protobuf, JSON) need a preflight too.
        let otlp = req.path.starts_with("/v1/")
            && req.header("content-type").is_some_and(|t| t.starts_with("application/x-protobuf") || t.starts_with("application/json"));
        if bearer.is_none() && !otlp && !matches!(req.method.as_str(), "GET" | "HEAD") && req.header("x-contraire").is_none() {
            return Response::error(403, "missing X-Contraire header");
        }
        let need = required_scope(req);
        if who.scope < need {
            let res = Response::error(
                403,
                &format!("token scope '{}' does not allow this (needs '{}')", who.scope.as_str(), need.as_str()),
            );
            // Denied attempts belong on the record too.
            if let Some((action, target)) = audit_subject(req) {
                self.audit.record(&self.audit_entry(req, &who, &action, &target, &res, Duration::ZERO));
            }
            return res;
        }
        let started = std::time::Instant::now();
        let res = self.route(req).unwrap_or_else(|e| Response::error(502, &format!("docker: {e}")));
        if let Some((action, target)) = audit_subject(req) {
            self.audit.record(&self.audit_entry(req, &who, &action, &target, &res, started.elapsed()));
        }
        res
    }

    fn audit_entry(&self, req: &Request, who: &Identity, action: &str, target: &str, res: &Response, took: Duration) -> Value {
        let body: Value = serde_json::from_slice(&res.body).unwrap_or_default();
        let failed = body.get("failed").and_then(Value::as_array).filter(|f| !f.is_empty());
        let message = body.get("message").and_then(Value::as_str).map(str::to_string).or_else(|| {
            failed.map(|f| {
                f.iter()
                    .filter_map(|x| Some(format!("{}: {}", x.get("name")?.as_str()?, x.get("message")?.as_str()?)))
                    .collect::<Vec<_>>()
                    .join("; ")
            })
        });
        json!({
            "ts": unix_now(),
            "action": action,
            "target": target,
            "ok": res.status < 300 && failed.is_none(),
            "status": res.status,
            "message": message,
            "ms": took.as_millis() as u64,
            // Token name, the Basic-auth user, or nobody when the dashboard is open.
            "user": who.name,
            "via": who.via,
            "ip": req.peer.map(|p| p.to_string()),
            "client": audit::client_kind(req.header("user-agent")),
        })
    }

    fn route(&self, req: &Request) -> std::io::Result<Response> {
        let segs: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        let method = if req.method == "HEAD" { "GET" } else { req.method.as_str() };

        Ok(match (method, segs.as_slice()) {
            ("GET", [""]) => {
                let gzip_ok = req.header("accept-encoding").is_some_and(|e| e.contains("gzip"));
                if PAGE_ENCODING == "gzip" && !gzip_ok {
                    // Every browser accepts gzip; this is for e.g. plain curl.
                    return Ok(Response::new(406, "text/plain; charset=utf-8", "Open this page in a web browser (or curl --compressed).\n"));
                }
                let page = Response::new(200, "text/html; charset=utf-8", INDEX_PAGE)
                    .with_header("Vary", "Accept-Encoding")
                    .with_header(
                        "Content-Security-Policy",
                        // The Learn tab fetches text and images from the docs origin (images shown via blob: URLs);
                        // fonts come from Google Fonts, with system fallbacks offline.
                        format!(
                            "default-src 'self'; script-src 'unsafe-inline'; \
                             style-src 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; \
                             img-src 'self' data: blob:; connect-src 'self' {}",
                            origin(&self.docs_url)
                        ),
                    );
                if PAGE_ENCODING == "gzip" { page.with_header("Content-Encoding", "gzip") } else { page }
            }
            ("GET", ["api", "info"]) => self.info()?,
            ("GET", ["api", "config"]) => {
                Response::json(200, json!({ "name": DISPLAY_NAME, "docs": self.docs_url, "version": env!("CARGO_PKG_VERSION") }).to_string())
            }
            ("GET", ["api", "audit"]) => {
                let limit = req.param("limit").and_then(|t| t.parse::<usize>().ok()).unwrap_or(200).clamp(1, 2000);
                let target = req.param("target").filter(|t| valid_id(t));
                let body = json!({
                    "path": self.audit.path().map(|p| p.display().to_string()),
                    "entries": self.audit.recent(limit, target),
                });
                Response::json(200, body.to_string()).with_header("Cache-Control", "no-store")
            }
            ("GET", ["api", "tokens"]) => Response::json(
                200,
                json!({ "enabled": self.tokens.path().is_some(), "tokens": self.tokens.list() }).to_string(),
            )
            .with_header("Cache-Control", "no-store"),
            ("POST", ["api", "tokens"]) => self.create_token(&req.body),
            ("DELETE", ["api", "tokens", id]) => match self.tokens.revoke(id) {
                Ok(true) => Response::new(204, "application/json", ""),
                Ok(false) => Response::error(404, "no such token"),
                Err(e) => Response::error(500, &e),
            },
            ("POST", ["v1", signal @ ("traces" | "logs" | "metrics")]) => {
                let (status, ctype, body) = self.otlp.ingest(
                    signal,
                    req.header("content-type").unwrap_or(""),
                    req.header("content-encoding"),
                    &req.body,
                );
                Response::new(status, ctype, body)
            }
            ("GET", ["api", "telemetry"]) => Response::json(200, self.otlp.overview().to_string()).with_header("Cache-Control", "no-store"),
            ("GET", ["api", "telemetry", "export"]) => export(req, self.otlp.export()),
            ("GET", ["api", "telemetry", name]) => match self.otlp.service(&percent_decode(name)) {
                Some(v) => Response::json(200, v.to_string()).with_header("Cache-Control", "no-store"),
                None => Response::error(404, "no telemetry from this service yet"),
            },
            ("GET", ["api", "metrics", "export"]) => export(req, self.metrics_rows()),
            ("GET", ["api", "logs", "patterns"]) => Response::json(200, self.logs.overview().to_string()),
            ("GET", ["api", "logs", "patterns", id]) if valid_id(id) => match self.logs.patterns(id) {
                Some(s) => Response::json(200, s.to_json(req.param("new") == Some("1")).to_string()).with_header("Cache-Control", "no-store"),
                None => Response::error(404, "not following this container (is it running?)"),
            },
            ("GET", ["api", "incidents"]) => Response::json(
                200,
                json!({ "enabled": self.logs.incidents_dir().is_some(), "incidents": self.logs.incident_list(200) }).to_string(),
            )
            .with_header("Cache-Control", "no-store"),
            ("GET", ["api", "incidents", id]) if valid_id(id) => match self.logs.incident(id) {
                Some(v) => Response::json(200, v.to_string()),
                None => Response::error(404, "no such incident"),
            },
            ("GET", ["api", "metrics"]) => self.metrics_overview(req),
            ("GET", ["api", "metrics", id]) if valid_id(id) => self.metrics_container(req, id)?,
            ("GET", ["metrics"]) => self.prometheus()?,
            ("GET", ["api", "self"]) => Response::json(200, self.telemetry().to_string()).with_header("Cache-Control", "no-store"),
            ("GET", ["api", "events"]) => {
                // Recent history for the events panel; the live tail comes over /api/stream.
                let secs = req.param("since").and_then(|t| t.parse::<u64>().ok()).unwrap_or(3600).min(86_400);
                let now = unix_now();
                let path = format!("{}&since={}&until={now}", live::EVENTS, now.saturating_sub(secs));
                let r = self.docker.get(&path)?;
                Response::new(r.status, "application/x-ndjson", r.body).with_header("Cache-Control", "no-store")
            }
            ("GET", ["api", "stream"]) => {
                let container = req.param("c").filter(|c| valid_id(c)).map(str::to_string);
                let tail = req.param("tail").and_then(|t| t.parse::<u32>().ok()).unwrap_or(300).clamp(1, 10_000);
                Response::sse(live::stream(self.docker.clone(), container, tail))
            }
            ("GET", ["api", "containers"]) => self.proxy("GET", "/containers/json?all=1")?,
            ("GET", ["api", "images"]) => self.proxy("GET", "/images/json")?,
            ("POST", ["api", "images", "pull"]) => self.pull(&req.body)?,
            ("POST", ["api", "images", "prune"]) => {
                self.proxy("POST", "/images/prune?filters=%7B%22dangling%22%3A%5B%22true%22%5D%7D")?
            }
            ("POST", ["api", "containers", "prune"]) => self.proxy("POST", "/containers/prune")?,

            (_, ["api", _, id, ..]) if !valid_id(id) => Response::error(400, "invalid id"),

            ("DELETE", ["api", "images", id]) => self.proxy("DELETE", &format!("/images/{id}"))?,
            ("GET", ["api", "containers", id]) => self.proxy("GET", &format!("/containers/{id}/json"))?,
            ("DELETE", ["api", "containers", id]) => {
                let force = if req.param("force") == Some("1") { "?force=1" } else { "" };
                self.proxy("DELETE", &format!("/containers/{id}{force}"))?
            }
            ("GET", ["api", "containers", id, "logs"]) => {
                let tail = req.param("tail").and_then(|t| t.parse::<u32>().ok()).unwrap_or(300).clamp(1, 10_000);
                let r = self.docker.get(&format!(
                    "/containers/{id}/logs?stdout=1&stderr=1&timestamps=1&tail={tail}"
                ))?;
                if r.status != 200 {
                    Response::json(r.status, r.body)
                } else {
                    Response::new(200, "text/plain; charset=utf-8", docker::demux_logs(&r.body))
                        .with_header("Cache-Control", "no-store")
                }
            }
            ("GET", ["api", "containers", id, "stats"]) => self.stats(id)?,
            ("POST", ["api", "projects", name, action @ ("start" | "stop" | "restart")]) => {
                compose::project_action(&self.docker, name, action)?
            }
            ("POST", ["api", "containers", id, action @ ("start" | "stop" | "restart" | "pause" | "unpause" | "kill")]) => {
                let r = self.docker.call("POST", &format!("/containers/{id}/{action}"), None, Duration::from_secs(90))?;
                // 304 = already in the requested state; not worth an error toast.
                let status = if r.status == 304 { 204 } else { r.status };
                Response::json(status, r.body)
            }
            ("GET", _) => Response::error(404, "not found"),
            _ => Response::error(405, "method not allowed"),
        })
    }

    /// What this process itself costs: shown in the UI so the footprint claims stay checkable.
    fn telemetry(&self) -> Value {
        let exe = std::env::current_exe().ok();
        let kb = |k: &str| proc_status(k).map(|v| v * 1024);
        json!({
            "name": DISPLAY_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "exe": exe.as_ref().map(|p| p.display().to_string()),
            "exe_bytes": exe.and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "listen": self.listen,
            "docker": self.docker.describe(),
            "auth": self.auth.is_some(),
            "audit": self.audit.path().map(|p| p.display().to_string()),
            "tokens": self.tokens.path().map(|_| self.tokens.len()),
            "otlp": { "batches": self.otlp.received.load(Ordering::Relaxed), "rejected": self.otlp.rejected.load(Ordering::Relaxed), "forward": self.otlp.forward_to() },
            "logs": { "following": self.logs.following(), "incidents": self.logs.incidents_dir().map(|p| p.display().to_string()) },
            "metrics": {
                "available": self.metrics.available(),
                "samples": self.metrics.samples(),
                "last_pass_ms": self.metrics.last_pass_ns.load(Ordering::Relaxed) as f64 / 1e6,
            },
            "started": self.started_unix,
            "uptime": self.started.elapsed().as_secs(),
            "cpu_seconds": proc_cpu_seconds(),
            "rss": kb("VmRSS"),
            "rss_peak": kb("VmHWM"),
            "threads": proc_status("Threads"),
            "fds": std::fs::read_dir("/proc/self/fd").ok().map(|d| d.count()),
            "connections": self.active.load(Ordering::Relaxed),
            "requests": self.requests.load(Ordering::Relaxed),
        })
    }

    fn create_token(&self, body: &[u8]) -> Response {
        let v: Value = serde_json::from_slice(body).unwrap_or_default();
        let name: String = v.get("name").and_then(Value::as_str).unwrap_or("").trim().chars().take(64).collect();
        if name.is_empty() || name.chars().any(char::is_control) {
            return Response::error(400, "a token needs a name, e.g. 'claude-laptop'");
        }
        let Some(scope) = v.get("scope").and_then(Value::as_str).and_then(tokens::Scope::parse) else {
            return Response::error(400, "scope must be read, operate or admin");
        };
        let now = unix_now();
        let expires = v.get("days").and_then(Value::as_u64).filter(|d| *d > 0).map(|d| now + d.min(3650) * 86_400);
        match self.tokens.create(&name, scope, expires, now) {
            // The secret appears in this response only; it is never stored or logged.
            Ok((mut record, secret)) => {
                record["token"] = json!(secret);
                Response::json(201, record.to_string()).with_header("Cache-Control", "no-store")
            }
            Err(e) => Response::error(409, &e),
        }
    }

    /// 24 h of 1-minute container metrics as rows, for `contraire export metrics`.
    fn metrics_rows(&self) -> Vec<Value> {
        let names: HashMap<u64, String> = self
            .docker
            .get("/containers/json?all=1")
            .ok()
            .and_then(|r| serde_json::from_slice::<Vec<Value>>(&r.body).ok())
            .unwrap_or_default()
            .iter()
            .filter_map(|c| Some((metrics::key_of(c["Id"].as_str()?)?, c["Names"][0].as_str()?.trim_start_matches('/').to_string())))
            .collect();
        let now = unix_now() / 60 * 60;
        let mut rows = Vec::new();
        for (key, ..) in self.metrics.latest(unix_now(), 0) {
            let Some(name) = names.get(&key) else { continue };
            let Some(series) = self.metrics.container(&format!("{key:016x}"), true, metrics::COARSE) else { continue };
            let n = series[0].len() as u64;
            for i in 0..n as usize {
                let mut row = serde_json::Map::new();
                row.insert("container".into(), json!(name));
                row.insert("minute".into(), json!(now - (n - 1 - i as u64) * 60));
                for (f, s) in metrics::FIELDS.iter().zip(&series) {
                    row.insert((*f).into(), json!(s[i]));
                }
                rows.push(Value::Object(row));
            }
        }
        rows
    }

    fn metrics_overview(&self, req: &Request) -> Response {
        let spark = req.param("spark").and_then(|n| n.parse().ok()).unwrap_or(30).min(metrics::FINE - 1);
        let mut containers = serde_json::Map::new();
        for (key, v, cpu, mem) in self.metrics.latest(unix_now(), spark) {
            let mut o = serde_json::Map::new();
            for (name, x) in metrics::FIELDS.iter().zip(v) {
                o.insert((*name).into(), json!(x));
            }
            o.insert("spark_cpu".into(), json!(cpu));
            o.insert("spark_mem".into(), json!(mem));
            containers.insert(format!("{key:016x}"), Value::Object(o));
        }
        // `hostm=N`: the host's last N minute averages (restored after a restart) instead of 10 s samples.
        let host = match req.param("hostm").and_then(|n| n.parse::<usize>().ok()) {
            Some(m) => self.metrics.host(true, m.min(metrics::COARSE - 1)),
            None => self.metrics.host(false, spark),
        };
        let latest = self.metrics.host_latest().map(|v| {
            metrics::HOST_FIELDS.iter().zip(v).map(|(k, x)| ((*k).to_string(), json!(x))).collect::<serde_json::Map<_, _>>()
        });
        let body = json!({
            "interval": metrics::INTERVAL,
            "available": self.metrics.available(),
            "host": { "latest": latest, "spark_cpu": host[0], "spark_mem": host[1] },
            "containers": containers,
        });
        Response::json(200, body.to_string()).with_header("Cache-Control", "no-store")
    }

    fn metrics_container(&self, req: &Request, id: &str) -> std::io::Result<Response> {
        let coarse = req.param("range") == Some("24h");
        // Names and short IDs resolve to the full ID through Docker (cheap, one call).
        let full = if id.len() == 64 {
            id.to_string()
        } else {
            let r = self.docker.get(&format!("/containers/{id}/json"))?;
            serde_json::from_slice::<Value>(&r.body).ok().and_then(|v| v["Id"].as_str().map(str::to_string)).unwrap_or_default()
        };
        let n = if coarse { metrics::COARSE } else { metrics::FINE };
        let Some(series) = self.metrics.container(&full, coarse, n) else {
            return Ok(Response::error(404, "no metrics for this container yet"));
        };
        let fields: serde_json::Map<String, Value> =
            metrics::FIELDS.iter().zip(series).map(|(k, v)| ((*k).to_string(), json!(v))).collect();
        let body = json!({ "interval": if coarse { 60 } else { metrics::INTERVAL }, "fields": fields });
        Ok(Response::json(200, body.to_string()).with_header("Cache-Control", "no-store"))
    }

    /// Prometheus text exposition: containers (labelled by name, stack, service), host, self.
    fn prometheus(&self) -> std::io::Result<Response> {
        use std::fmt::Write as _;
        let list: Vec<Value> = serde_json::from_slice(&self.docker.get("/containers/json?all=1")?.body).unwrap_or_default();
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
        let labels: HashMap<u64, String> = list
            .iter()
            .filter_map(|c| {
                let id = c["Id"].as_str()?;
                let l = &c["Labels"];
                let name = c["Names"][0].as_str().unwrap_or("").trim_start_matches('/');
                Some((
                    metrics::key_of(id)?,
                    format!(
                        "name=\"{}\",id=\"{}\",stack=\"{}\",service=\"{}\"",
                        esc(name),
                        &id[..12],
                        esc(l["com.docker.compose.project"].as_str().unwrap_or("")),
                        esc(l["com.docker.compose.service"].as_str().unwrap_or(""))
                    ),
                ))
            })
            .collect();
        let latest = self.metrics.latest(unix_now(), 0);
        let mut out = String::with_capacity(16 * 1024);
        let defs = [
            ("cpu_percent", "CPU use in percent of one core"),
            ("memory_bytes", "Memory in use, excluding reclaimable page cache"),
            ("network_receive_bytes_per_second", "Network receive rate"),
            ("network_transmit_bytes_per_second", "Network transmit rate"),
            ("io_read_bytes_per_second", "Block IO read rate"),
            ("io_write_bytes_per_second", "Block IO write rate"),
            ("pids", "Processes and threads"),
        ];
        for (i, (name, help)) in defs.iter().enumerate() {
            let _ = writeln!(out, "# HELP contraire_container_{name} {help}\n# TYPE contraire_container_{name} gauge");
            for (key, v, ..) in &latest {
                if let Some(l) = labels.get(key) {
                    let _ = writeln!(out, "contraire_container_{name}{{{l}}} {}", v[i]);
                }
            }
        }
        let _ = writeln!(out, "# HELP contraire_container_running 1 if the container is running\n# TYPE contraire_container_running gauge");
        let _ = writeln!(out, "# HELP contraire_container_healthy 1 healthy, 0 unhealthy (only containers with a health check)\n# TYPE contraire_container_healthy gauge");
        for c in &list {
            let Some(l) = c["Id"].as_str().and_then(metrics::key_of).and_then(|k| labels.get(&k)) else { continue };
            let _ = writeln!(out, "contraire_container_running{{{l}}} {}", u8::from(c["State"] == "running"));
            let status = c["Status"].as_str().unwrap_or("");
            if status.contains("(healthy)") || status.contains("(unhealthy)") {
                let _ = writeln!(out, "contraire_container_healthy{{{l}}} {}", u8::from(status.contains("(healthy)")));
            }
        }
        if let Some(h) = self.metrics.host_latest() {
            for (i, name, help) in [
                (0, "cpu_percent", "Host CPU use in percent of all cores"),
                (1, "memory_used_bytes", "Host memory in use"),
                (5, "memory_total_bytes", "Host memory"),
                (2, "load1", "1-minute load average"),
                (3, "network_receive_bytes_per_second", "Host network receive rate"),
                (4, "network_transmit_bytes_per_second", "Host network transmit rate"),
            ] {
                let _ = writeln!(out, "# HELP contraire_host_{name} {help}\n# TYPE contraire_host_{name} gauge\ncontraire_host_{name} {}", h[i]);
            }
        }
        let rss = proc_status("VmRSS").unwrap_or(0) * 1024;
        let _ = writeln!(
            out,
            "# HELP contraire_self_resident_bytes Memory of the dashboard itself\n# TYPE contraire_self_resident_bytes gauge\ncontraire_self_resident_bytes {rss}\n\
             # HELP contraire_self_cpu_seconds_total CPU time of the dashboard itself\n# TYPE contraire_self_cpu_seconds_total counter\ncontraire_self_cpu_seconds_total {}\n\
             # HELP contraire_self_requests_total HTTP requests served\n# TYPE contraire_self_requests_total counter\ncontraire_self_requests_total {}",
            proc_cpu_seconds().unwrap_or(0.0),
            self.requests.load(Ordering::Relaxed)
        );
        Ok(Response::new(200, "text/plain; version=0.0.4; charset=utf-8", out).with_header("Cache-Control", "no-store"))
    }

    fn proxy(&self, method: &str, path: &str) -> std::io::Result<Response> {
        let r = self.docker.call(method, path, None, Duration::from_secs(60))?;
        Ok(Response::json(r.status, r.body).with_header("Cache-Control", "no-store"))
    }

    /// The full /info payload is several KB of mostly noise; send only what the header shows.
    fn info(&self) -> std::io::Result<Response> {
        let r = self.docker.get("/info")?;
        if r.status != 200 {
            return Ok(Response::json(r.status, r.body));
        }
        let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
        let pick = |k: &str| v.get(k).cloned().unwrap_or(Value::Null);
        let out = json!({
            "name": pick("Name"),
            "version": pick("ServerVersion"),
            "os": pick("OperatingSystem"),
            "arch": pick("Architecture"),
            "kernel": pick("KernelVersion"),
            "cpus": pick("NCPU"),
            "memory": pick("MemTotal"),
            "containers": pick("Containers"),
            "running": pick("ContainersRunning"),
            "paused": pick("ContainersPaused"),
            "stopped": pick("ContainersStopped"),
            "images": pick("Images"),
        });
        Ok(Response::json(200, out.to_string()).with_header("Cache-Control", "no-store"))
    }

    fn stats(&self, id: &str) -> std::io::Result<Response> {
        let r = self.docker.get(&format!("/containers/{id}/stats?stream=false"))?;
        if r.status != 200 {
            return Ok(Response::json(r.status, r.body));
        }
        let s: Value = serde_json::from_slice(&r.body).unwrap_or_default();
        let out = live::summarize_stats(&s);
        Ok(Response::json(200, out.to_string()).with_header("Cache-Control", "no-store"))
    }

    fn pull(&self, body: &[u8]) -> std::io::Result<Response> {
        let image = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| v.get("image")?.as_str().map(|s| s.trim().to_string()))
            .unwrap_or_default();
        if image.is_empty() || image.len() > 256 || image.contains(char::is_whitespace) {
            return Ok(Response::error(400, "invalid image reference"));
        }
        // An empty tag would make Docker pull *every* tag of the repository.
        let query = if image.contains('@') {
            format!("fromImage={}", http::url_encode(&image))
        } else {
            let last = image.rfind('/').map_or(0, |i| i + 1);
            let (repo, tag) = match image[last..].rfind(':') {
                Some(i) => (&image[..last + i], &image[last + i + 1..]),
                None => (image.as_str(), "latest"),
            };
            format!("fromImage={}&tag={}", http::url_encode(repo), http::url_encode(tag))
        };
        let r = self.docker.call("POST", &format!("/images/create?{query}"), None, Duration::from_secs(900))?;
        if r.status != 200 {
            return Ok(Response::json(r.status, r.body));
        }
        // Progress is streamed as JSON lines; errors arrive in-band with a 200 status.
        let error = r
            .body
            .split(|&b| b == b'\n')
            .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
            .find_map(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
        Ok(match error {
            Some(e) => Response::error(500, &e),
            None => Response::json(200, json!({ "image": image }).to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokens::Scope::*;

    fn req(method: &str, path: &str) -> Request {
        let raw = format!("{method} {path} HTTP/1.1\r\n\r\n");
        http::parse_for_test(raw.as_bytes())
    }

    #[test]
    fn scopes_follow_least_privilege() {
        assert_eq!(required_scope(&req("GET", "/api/containers")), Read);
        assert_eq!(required_scope(&req("GET", "/api/containers/x/logs")), Read);
        assert_eq!(required_scope(&req("POST", "/api/containers/x/restart")), Operate);
        assert_eq!(required_scope(&req("POST", "/api/projects/shop/stop")), Operate);
        assert_eq!(required_scope(&req("POST", "/api/containers/x/kill")), Admin);
        assert_eq!(required_scope(&req("DELETE", "/api/containers/x")), Admin);
        assert_eq!(required_scope(&req("POST", "/api/images/pull")), Admin);
        assert_eq!(required_scope(&req("POST", "/api/containers/prune")), Admin);
        assert_eq!(required_scope(&req("GET", "/api/tokens")), Admin);
        assert_eq!(required_scope(&req("GET", "/api/audit")), Admin);
    }
}
