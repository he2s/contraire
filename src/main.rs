#[cfg(test)]
mod bench;
mod audit;
mod cli;
mod compose;
mod docker;
mod http;
mod live;

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use docker::Docker;
use http::{Request, Response};
use serde_json::{Value, json};

/// Human-facing name of the tool. The crate, binary and repository stay `contraire`.
pub const DISPLAY_NAME: &str = "mopsfraugibtdentonan";

const INDEX_HTML: &str = include_str!("../static/index.html");

struct App {
    docker: Docker,
    /// Expected `Authorization` header value, if a password is configured.
    auth: Option<String>,
    audit: audit::Audit,
    /// The Basic-auth user, when a password is configured.
    user: Option<String>,
    /// Base URL the Learn tab fetches its content from (never embedded in the binary).
    docs_url: String,
    listen: String,
    started: std::time::Instant,
    started_unix: u64,
    active: Arc<AtomicUsize>,
    requests: std::sync::atomic::AtomicU64,
}

fn main() {
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
    let app = Arc::new(App {
        docker,
        auth,
        docs_url: cfg.docs_url,
        audit: audit::Audit::open(&cfg.audit, |k| std::env::var(k).ok()),
        user: cfg.password.is_some().then(|| cfg.user.clone()),
        listen: cfg.listen.clone(),
        started: std::time::Instant::now(),
        started_unix: unix_now(),
        active: active.clone(),
        requests: std::sync::atomic::AtomicU64::new(0),
    });

    eprintln!("contraire: {DISPLAY_NAME} listening on http://{} (docker: {})", cfg.listen, app.docker.describe());
    match app.audit.path() {
        Some(p) => eprintln!("contraire: audit log: {}", p.display()),
        None => eprintln!("contraire: audit log: off"),
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

fn unix_now() -> u64 {
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
        if let Some(expected) = &self.auth {
            let given = req.header("authorization").unwrap_or("");
            if !http::ct_eq(given.as_bytes(), expected.as_bytes()) {
                return Response::error(401, "authentication required")
                    .with_header("WWW-Authenticate", "Basic realm=\"contraire\", charset=\"UTF-8\"");
            }
        }
        // CSRF guard: a custom header can't be sent cross-origin without a CORS preflight,
        // which we never answer.
        if !matches!(req.method.as_str(), "GET" | "HEAD") && req.header("x-contraire").is_none() {
            return Response::error(403, "missing X-Contraire header");
        }
        let started = std::time::Instant::now();
        let res = self.route(req).unwrap_or_else(|e| Response::error(502, &format!("docker: {e}")));
        if let Some((action, target)) = audit_subject(req) {
            self.audit.record(&self.audit_entry(req, &action, &target, &res, started.elapsed()));
        }
        res
    }

    fn audit_entry(&self, req: &Request, action: &str, target: &str, res: &Response, took: Duration) -> Value {
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
            // Basic auth has exactly one account; without a password nobody is identified.
            "user": self.user,
            "ip": req.peer.map(|p| p.to_string()),
            "client": audit::client_kind(req.header("user-agent")),
        })
    }

    fn route(&self, req: &Request) -> std::io::Result<Response> {
        let segs: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        let method = if req.method == "HEAD" { "GET" } else { req.method.as_str() };

        Ok(match (method, segs.as_slice()) {
            ("GET", [""]) => Response::new(200, "text/html; charset=utf-8", INDEX_HTML).with_header(
                "Content-Security-Policy",
                // The Learn tab fetches text and images from the docs origin (images shown via blob: URLs);
                // fonts come from Google Fonts, with system fallbacks offline.
                format!(
                    "default-src 'self'; script-src 'unsafe-inline'; \
                     style-src 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; \
                     img-src 'self' data: blob:; connect-src 'self' {}",
                    origin(&self.docs_url)
                ),
            ),
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
