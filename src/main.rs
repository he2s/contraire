#[cfg(test)]
mod bench;
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
    /// Base URL the Learn tab fetches its content from (never embedded in the binary).
    docs_url: String,
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
    let app = Arc::new(App { docker, auth, docs_url: cfg.docs_url });

    eprintln!("contraire: {DISPLAY_NAME} listening on http://{} (docker: {})", cfg.listen, app.docker.describe());
    let loopback = listener.local_addr().is_ok_and(|a| a.ip().is_loopback());
    if !loopback && app.auth.is_none() {
        eprintln!(
            "contraire: WARNING: reachable from the network without a password; anyone on it can control Docker \
             (use --password-file or CONTRAIRE_PASSWORD)"
        );
    }

    let active = Arc::new(AtomicUsize::new(0));
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
        self.route(req).unwrap_or_else(|e| Response::error(502, &format!("docker: {e}")))
    }

    fn route(&self, req: &Request) -> std::io::Result<Response> {
        let segs: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        let method = if req.method == "HEAD" { "GET" } else { req.method.as_str() };

        Ok(match (method, segs.as_slice()) {
            ("GET", [""]) => Response::new(200, "text/html; charset=utf-8", INDEX_HTML).with_header(
                "Content-Security-Policy",
                // The Learn tab fetches text and images from the docs origin; images are shown via blob: URLs.
                format!(
                    "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
                     img-src 'self' data: blob:; connect-src 'self' {}",
                    origin(&self.docs_url)
                ),
            ),
            ("GET", ["api", "info"]) => self.info()?,
            ("GET", ["api", "config"]) => {
                Response::json(200, json!({ "name": DISPLAY_NAME, "docs": self.docs_url, "version": env!("CARGO_PKG_VERSION") }).to_string())
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
