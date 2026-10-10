//! Client mode: the same binary, run on a workstation, talking to a contraire server.
//!
//!     contraire init <dir> | ps | top | inspect <c> | logs <c> [-f] | patterns <c> | incidents | incident <id>
//!     contraire start|stop|restart|pause|unpause <c> | stacks | stack <name> start|stop|restart
//!     contraire events | activity | info | services | service <name> | export metrics|telemetry
//!     contraire mcp      # MCP server over stdio for Claude Code and other agents
//!
//! `HELP` below is the authoritative list; keep this, `cli.rs`, README, the cheat sheet and SKILL.md in step.
//!
//! Connection: --host / CONTRAIRE_HOST (http://host:port) and --token / CONTRAIRE_TOKEN.
//! Plain HTTP only (no TLS in the binary): reach a remote Pi over an SSH tunnel or a VPN.
//! Every call goes through the server's API, so token scopes and the audit log apply.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::docker::{Docker, Tick};

pub const COMMANDS: &[&str] =
    &["init", "ps", "top", "patterns", "incidents", "incident", "inspect", "logs", "start", "stop", "restart", "pause", "unpause", "stacks", "stack", "events", "activity", "info", "services", "service", "export", "mcp"];

const HELP: &str = "\
Client mode: control a contraire server from this machine.

Usage: contraire <COMMAND> [ARGS] [--host URL] [--token TOKEN] [--json]

Commands:
  init <dir>                 Create a session folder for Claude Code: skill, reference, PROJECT.md, action log, journal
  ps                         List containers
  top                        CPU, memory, network and disk per container, busiest first
  patterns <container> [--new] [-n N]   Log lines grouped into patterns with counts, errors first (top N, default 40)
  incidents                  Incidents: crashes and failing health checks the flight recorder caught
  incident <id>              One postmortem: exit reason, last lines, error patterns, metrics
  inspect <container>        State, health, exit reason, ports, mounts, compose info
  logs <container> [-n N] [-f] [-t]   Last N lines (default 100); -f follows live, -t shows timestamps
  start|stop|restart|pause|unpause <container>
  stacks                     List Compose stacks
  stack <name> start|stop|restart     Whole stack, in depends_on order
  events [--since MIN]       Docker events of the last MIN minutes (default 60)
  activity [-n N]            The server's audit log (admin token)
  info                       Host and server facts
  services                   Telemetry: apps reporting over OpenTelemetry, with requests/min, errors, p50/p95/p99
  service <name>             One service: operations, failed and slowest requests, log patterns
  export metrics|telemetry [--csv|--parquet]   Metrics (24 h per minute) or telemetry (1 h per minute) as JSON lines, CSV or Parquet
  mcp                        Run an MCP server on stdio for Claude Code and other agents

Options:
  --host <URL>    Server, e.g. http://pi.local:8083 (-H) [env: CONTRAIRE_HOST] [default: http://127.0.0.1:8083]
  --token <TOKEN> API token from the Access page      [env: CONTRAIRE_TOKEN]
  --json          Print the raw JSON instead of a table
  --new           (patterns) only patterns that first appeared after the container started
  --csv           (export) CSV instead of JSON lines
  --parquet       (export) Parquet, for DuckDB, pandas or polars; redirect it to a file
  --skill-from <PATH>   (init) copy the skill from a local file instead of GitHub

Sessions: inside a folder made by `contraire init`, host and token come from .contraire/,
and every command (and MCP tool call) is appended to .contraire/actions.log.

Claude Code:
  claude mcp add contraire --env CONTRAIRE_TOKEN=ctr_... -- contraire mcp --host http://pi.local:8083
";

pub struct Remote {
    http: Docker,
    pub host: String,
    /// Session action log (`.contraire/actions.log`), when run inside a session folder.
    log: Option<PathBuf>,
}

impl Remote {
    pub fn connect(host: &str, token: Option<&str>) -> Result<Remote, String> {
        let host = host.trim_end_matches('/');
        let addr = host.strip_prefix("http://").ok_or_else(|| {
            format!("--host must look like http://host:port, got '{host}' (no TLS: use an SSH tunnel or VPN for remote access)")
        })?;
        if addr.is_empty() || addr.contains('/') {
            return Err(format!("--host must be http://host:port without a path, got '{host}'"));
        }
        let addr = if addr.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
            addr.to_string()
        } else {
            format!("{addr}:8083")
        };
        let mut http = Docker::parse(&format!("tcp://{addr}"))?
            .with_header("X-Contraire", "1")
            .with_header("User-Agent", concat!("contraire-cli/", env!("CARGO_PKG_VERSION")));
        if let Some(t) = token.filter(|t| !t.is_empty()) {
            http = http.with_header("Authorization", &format!("Bearer {t}"));
        }
        Ok(Remote { http, host: format!("http://{addr}"), log: None })
    }

    /// Append one line per command to the session log: time, result, read/change, command.
    fn record(&self, what: &str, change: bool, result: &Result<String, String>) {
        let Some(path) = &self.log else { return };
        let (status, detail) = match result {
            Ok(_) => ("ok  ", String::new()),
            Err(e) => ("FAIL", format!("  -- {}", e.replace('\n', " "))),
        };
        let line = format!("{}  {status}  {}  {what}{detail}\n", rfc3339(now()), if change { "change" } else { "read  " });
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| f.write_all(line.as_bytes()));
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>, timeout: u64) -> Result<Vec<u8>, String> {
        let body = body.map(|b| b.to_string());
        let r = self
            .http
            .call(method, path, body.as_deref().map(str::as_bytes), Duration::from_secs(timeout))
            .map_err(|e| format!("cannot reach {}: {e}", self.host))?;
        if r.status >= 300 {
            let msg = serde_json::from_slice::<Value>(&r.body)
                .ok()
                .and_then(|v| v.get("message")?.as_str().map(str::to_string))
                .unwrap_or_else(|| String::from_utf8_lossy(&r.body).trim().to_string());
            return Err(match r.status {
                401 => format!("not authorized ({msg}); pass --token or set CONTRAIRE_TOKEN"),
                _ => format!("{} (HTTP {})", msg, r.status),
            });
        }
        Ok(r.body)
    }

    pub fn get(&self, path: &str) -> Result<Value, String> {
        let b = self.call("GET", path, None, 30)?;
        serde_json::from_slice(&b).map_err(|e| format!("bad JSON from server: {e}"))
    }

    pub fn get_text(&self, path: &str) -> Result<String, String> {
        Ok(String::from_utf8_lossy(&self.call("GET", path, None, 30)?).into_owned())
    }

    pub fn post(&self, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let b = self.call("POST", path, body, 120)?;
        Ok(serde_json::from_slice(&b).unwrap_or(Value::Null))
    }
}

fn enc(s: &str) -> String {
    crate::http::url_encode(s)
}

/* ---------- formatting shared by the CLI and MCP tools ---------- */

fn s<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or("")
}

fn label<'a>(c: &'a Value, k: &str) -> Option<&'a str> {
    c.get("Labels")?.get(k)?.as_str().filter(|x| !x.is_empty())
}

fn name(c: &Value) -> String {
    s(c, "/Names/0").trim_start_matches('/').to_string()
}

/// The same plain-language status vocabulary as the web UI.
fn status(c: &Value) -> String {
    let st = s(c, "/State");
    let text = s(c, "/Status");
    if st == "running" && text.contains("(unhealthy)") {
        return "unhealthy".into();
    }
    if st == "exited" && !text.contains("(0)") {
        let code = text.split('(').nth(1).and_then(|x| x.split(')').next()).unwrap_or("?");
        return format!("failed ({code})");
    }
    st.to_string()
}

fn ports(c: &Value) -> String {
    let mut out: Vec<String> = c
        .get("Ports")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| Some(format!("{}->{}", p.get("PublicPort")?.as_u64()?, p.get("PrivatePort")?.as_u64()?)))
        .collect();
    out.sort();
    out.dedup();
    out.join(",")
}

fn ago(ts: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let d = now.saturating_sub(ts);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.first().map_or(0, Vec::len);
    let width: Vec<usize> =
        (0..cols).map(|i| rows.iter().map(|r| r.get(i).map_or(0, |c| c.chars().count())).max().unwrap_or(0)).collect();
    let mut out = String::new();
    for r in rows {
        let line: Vec<String> = r.iter().enumerate().map(|(i, c)| format!("{c:<w$}", w = width[i])).collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

fn ps_text(list: &Value) -> String {
    let mut cs: Vec<&Value> = list.as_array().map(|a| a.iter().collect()).unwrap_or_default();
    cs.sort_by_key(|c| (label(c, "com.docker.compose.project").unwrap_or("~").to_string(), name(c)));
    let mut rows = vec![vec!["NAME".into(), "STATUS".into(), "UP / SINCE".into(), "STACK".into(), "PORTS".into(), "IMAGE".into()]];
    for c in cs {
        rows.push(vec![
            name(c),
            status(c),
            s(c, "/Status").to_string(),
            label(c, "com.docker.compose.project").unwrap_or("-").to_string(),
            ports(c),
            s(c, "/Image").to_string(),
        ]);
    }
    table(&rows)
}

const EXIT_HINT: &[(i64, &str)] = &[
    (0, "finished normally"),
    (1, "the app reported an error; check its logs"),
    (125, "docker could not run it"),
    (126, "command not executable"),
    (127, "command not found"),
    (137, "killed: out of memory or force-stopped"),
    (139, "crashed (segmentation fault)"),
    (143, "stopped politely (SIGTERM)"),
];

fn inspect_text(d: &Value) -> String {
    let st = &d["State"];
    let mut out = String::new();
    let mut line = |k: &str, v: String| {
        if !v.is_empty() {
            out.push_str(&format!("{k:<14}{v}\n"));
        }
    };
    line("name", s(d, "/Name").trim_start_matches('/').to_string());
    line("state", s(d, "/State/Status").to_string());
    if let Some(h) = st.pointer("/Health/Status").and_then(Value::as_str) {
        let streak = st.pointer("/Health/FailingStreak").and_then(Value::as_u64).unwrap_or(0);
        line("health", if streak > 0 { format!("{h} ({streak} failed in a row)") } else { h.to_string() });
    }
    if !st["Running"].as_bool().unwrap_or(false) && !s(d, "/State/FinishedAt").starts_with("0001") {
        let code = st["ExitCode"].as_i64().unwrap_or(0);
        let hint = EXIT_HINT.iter().find(|(c, _)| *c == code).map_or("", |(_, h)| h);
        line("exit", format!("{code} {hint}").trim().to_string());
    }
    if st["OOMKilled"].as_bool().unwrap_or(false) {
        line("oom", "ran out of memory".into());
    }
    line("started", s(d, "/State/StartedAt").to_string());
    line("restarts", format!("{} (policy {})", d["RestartCount"].as_u64().unwrap_or(0), s(d, "/HostConfig/RestartPolicy/Name")));
    line("image", s(d, "/Config/Image").to_string());
    let cmd: Vec<&str> = ["/Config/Entrypoint", "/Config/Cmd"]
        .iter()
        .flat_map(|p| d.pointer(p).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str))
        .collect();
    line("command", cmd.join(" "));
    if let Some(p) = d.pointer("/NetworkSettings/Ports").and_then(Value::as_object) {
        let mut v: Vec<String> = p
            .iter()
            .map(|(k, b)| match b.as_array().and_then(|a| a.first()) {
                Some(b) => format!("{}->{k}", s(b, "/HostPort")),
                None => k.clone(),
            })
            .collect();
        v.sort();
        v.dedup();
        line("ports", v.join(", "));
    }
    if let Some(n) = d.pointer("/NetworkSettings/Networks").and_then(Value::as_object) {
        line("networks", n.iter().map(|(k, v)| format!("{k} {}", s(v, "/IPAddress"))).collect::<Vec<_>>().join(", "));
    }
    if let Some(m) = d["Mounts"].as_array() {
        for m in m {
            let src = if s(m, "/Name").is_empty() { s(m, "/Source") } else { s(m, "/Name") };
            line("mount", format!("{src} -> {}{}", s(m, "/Destination"), if m["RW"].as_bool() == Some(false) { " (ro)" } else { "" }));
        }
    }
    let labels = &d["Config"]["Labels"];
    if let Some(p) = labels.get("com.docker.compose.project").and_then(Value::as_str) {
        line("stack", format!("{p} / service {}", s(labels, "/com.docker.compose.service")));
        let deps: Vec<&str> = s(labels, "/com.docker.compose.depends_on").split(',').filter_map(|x| x.split(':').next()).filter(|x| !x.is_empty()).collect();
        line("depends on", deps.join(", "));
        line("compose file", s(labels, "/com.docker.compose.project.config_files").to_string());
    }
    let mem = d.pointer("/HostConfig/Memory").and_then(Value::as_u64).unwrap_or(0);
    if mem > 0 {
        line("memory limit", format!("{} MB", mem / 1024 / 1024));
    }
    out
}

fn strip_timestamps(logs: &str) -> String {
    logs.lines()
        .map(|l| match l.split_once(' ') {
            Some((ts, rest)) if ts.len() > 20 && ts.as_bytes().get(4) == Some(&b'-') && ts.ends_with('Z') => rest,
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn mb(b: f64) -> String {
    if b >= 1e9 { format!("{:.1} GB", b / 1e9) } else if b >= 1e6 { format!("{:.0} MB", b / 1e6) } else { format!("{:.0} KB", b / 1e3) }
}

fn rate_text(b: f64) -> String {
    if b >= 1e6 { format!("{:.1} MB/s", b / 1e6) } else if b >= 1e3 { format!("{:.0} KB/s", b / 1e3) } else { format!("{b:.0} B/s") }
}

/// Containers by CPU, from the server's cgroup sampler (10 s resolution).
fn top_text(list: &Value, m: &Value) -> String {
    let mut rows: Vec<(f64, Vec<String>)> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let x = &m["containers"][c["Id"].as_str()?.get(..16)?];
            let f = |k: &str| x[k].as_f64().unwrap_or(0.0);
            x.is_object().then(|| {
                (f("cpu"), vec![name(c), format!("{:.1}%", f("cpu")), mb(f("mem")), rate_text(f("rx")), rate_text(f("tx")), rate_text(f("read") + f("write")), format!("{}", f("pids") as u64)])
            })
        })
        .collect();
    if rows.is_empty() {
        return "No metrics yet (the server samples every 10 s; cgroup v2 required).\n".into();
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut t = vec![vec!["NAME".into(), "CPU".into(), "MEMORY".into(), "NET IN".into(), "NET OUT".into(), "DISK IO".into(), "PIDS".into()]];
    t.extend(rows.into_iter().map(|r| r.1));
    let h = &m["host"]["latest"];
    format!(
        "host: CPU {:.0}% · load {:.2} · memory {} of {}\n\n{}",
        h["cpu"].as_f64().unwrap_or(0.0),
        h["load1"].as_f64().unwrap_or(0.0),
        mb(h["mem"].as_f64().unwrap_or(0.0)),
        mb(h["mem_total"].as_f64().unwrap_or(0.0)),
        table(&t)
    )
}

/// Now / peak / average per field, plus the memory trend: what an agent needs, not 360 points.
fn metrics_summary(name: &str, range: &str, h: &Value) -> String {
    let f = &h["fields"];
    let series = |k: &str| f[k].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>()).unwrap_or_default();
    let cpu = series("cpu");
    if cpu.is_empty() {
        return format!("{name}: no samples in the last {range} (not running?)\n");
    }
    let line = |label: &str, v: &[f64], fmt: &dyn Fn(f64) -> String| {
        let avg = v.iter().sum::<f64>() / v.len() as f64;
        let peak = v.iter().cloned().fold(f64::MIN, f64::max);
        format!("{label:<12}now {:<10} peak {:<10} avg {}\n", fmt(*v.last().unwrap_or(&0.0)), fmt(peak), fmt(avg))
    };
    let mem = series("mem");
    let trend = match (mem.first(), mem.last()) {
        (Some(a), Some(b)) if *a > 0.0 => format!("{:+.0}% over the window", (b - a) / a * 100.0),
        _ => "-".into(),
    };
    let pct = |x: f64| format!("{x:.1}%");
    let (by, rt) = (|x: f64| mb(x), |x: f64| rate_text(x));
    format!(
        "{name}, last {range} ({} samples)\n{}{}memory trend {trend}\n{}{}{}{}",
        cpu.len(),
        line("cpu", &cpu, &pct),
        line("memory", &mem, &by),
        line("net in", &series("rx"), &rt),
        line("net out", &series("tx"), &rt),
        line("disk read", &series("read"), &rt),
        line("disk write", &series("write"), &rt),
    )
}

fn patterns_text(name: &str, p: &Value, limit: usize) -> String {
    let ps = p["patterns"].as_array().cloned().unwrap_or_default();
    let mut out = format!(
        "{name}: {} lines since {} grouped into {} patterns (<*> = varies)\n",
        p["lines"],
        ago(p["since"].as_u64().unwrap_or(0)),
        ps.len()
    );
    let mut rows = vec![vec!["COUNT".into(), "LEVEL".into(), "LAST".into(), "PATTERN".into()]];
    for x in ps.iter().take(limit) {
        let new = if x["new"] == true { "NEW " } else { "" };
        rows.push(vec![x["count"].to_string(), s(x, "/level").into(), ago(x["last"].as_u64().unwrap_or(0)), format!("{new}{}", s(x, "/pattern"))]);
    }
    out.push_str(&table(&rows));
    if ps.len() > limit {
        out.push_str(&format!("… {} more\n", ps.len() - limit));
    }
    out
}

/// Milliseconds, short: 0.42ms, 12ms, 3.1s.
fn ms(v: &Value) -> String {
    match v.as_f64() {
        None => "-".into(),
        Some(x) if x < 0.01 => "<0.01ms".into(),
        Some(x) if x < 1.0 => format!("{x:.2}ms"),
        Some(x) if x < 1000.0 => format!("{x:.0}ms"),
        Some(x) => format!("{:.1}s", x / 1000.0),
    }
}

fn rate(errors: &Value, total: &Value) -> String {
    match (errors.as_f64(), total.as_f64()) {
        (Some(e), Some(t)) if t > 0.0 => format!("{:.1}%", e / t * 100.0),
        _ => "-".into(),
    }
}

fn services_text(v: &Value) -> String {
    let mut sv = v["services"].as_array().cloned().unwrap_or_default();
    if sv.is_empty() {
        return "No app has reported over OpenTelemetry yet. Point OTEL_EXPORTER_OTLP_ENDPOINT at this server \
                (OTLP/HTTP, compression none); see the Telemetry page for snippets.\n"
            .into();
    }
    // Most failing requests first, then busiest.
    let key = |x: &Value| x["errors_1h"].as_f64().unwrap_or(0.0) * 1e6 + x["rpm"].as_f64().unwrap_or(0.0);
    sv.sort_by(|a, b| key(b).total_cmp(&key(a)));
    let mut rows = vec![vec!["SERVICE".into(), "REQ/MIN".into(), "ERRORS".into(), "P50".into(), "P95".into(), "P99".into(), "LOGS".into(), "LAST".into()]];
    for x in &sv {
        rows.push(vec![
            s(x, "/service").into(),
            format!("{:.1}", x["rpm"].as_f64().unwrap_or(0.0)),
            rate(&x["errors_1h"], &x["requests_1h"]),
            ms(&x["p50"]),
            ms(&x["p95"]),
            ms(&x["p99"]),
            x["logs"].to_string(),
            ago(x["last"].as_u64().unwrap_or(0)),
        ]);
    }
    let mut out = format!("{} services · {} batches received · last hour\n", sv.len(), v["received_batches"]);
    out.push_str(&table(&rows));
    out
}

fn service_text(x: &Value) -> String {
    let mut out = format!(
        "{}: {:.1} req/min · {} of {} requests failed ({}) · p50 {} p95 {} p99 {} · last report {}\n",
        s(x, "/service"),
        x["rpm"].as_f64().unwrap_or(0.0),
        x["errors_1h"],
        x["requests_1h"],
        rate(&x["errors_1h"], &x["requests_1h"]),
        ms(&x["p50"]),
        ms(&x["p95"]),
        ms(&x["p99"]),
        ago(x["last"].as_u64().unwrap_or(0))
    );
    let ops = x["operations"].as_array().cloned().unwrap_or_default();
    if !ops.is_empty() {
        out.push_str("\nOperations (last hour)\n");
        let mut rows = vec![vec!["COUNT".into(), "ERRORS".into(), "P50".into(), "P95".into(), "P99".into(), "OPERATION".into()]];
        for o in ops.iter().take(15) {
            rows.push(vec![o["count_1h"].to_string(), rate(&o["errors_1h"], &o["count_1h"]), ms(&o["p50"]), ms(&o["p95"]), ms(&o["p99"]), s(o, "/operation").into()]);
        }
        out.push_str(&table(&rows));
    }
    for (title, key) in [("Failed requests", "errors"), ("Slowest requests", "slowest")] {
        let ex = x[key].as_array().cloned().unwrap_or_default();
        if ex.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{title}\n"));
        let mut rows = vec![vec!["TOOK".into(), "WHEN".into(), "TRACE".into(), "OPERATION · DETAIL".into()]];
        for e in ex.iter().take(8) {
            let detail = [s(e, "/message"), s(e, "/detail")].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" · ");
            rows.push(vec![ms(&e["ms"]), ago(e["at"].as_u64().unwrap_or(0)), s(e, "/trace_id").into(), format!("{} {detail}", s(e, "/operation"))]);
        }
        out.push_str(&table(&rows));
    }
    if x["log_patterns"]["patterns"].as_array().is_some_and(|p| !p.is_empty()) {
        out.push('\n');
        out.push_str(&patterns_text(&format!("{} (OTLP logs)", s(x, "/service")), &x["log_patterns"], 15));
    }
    out
}

fn incidents_text(v: &Value) -> String {
    let is = v["incidents"].as_array().cloned().unwrap_or_default();
    if v["enabled"] == false {
        return "The flight recorder is off (it needs the audit log's state directory).\n".into();
    }
    if is.is_empty() {
        return "No crashes recorded.\n".into();
    }
    let mut rows = vec![vec!["ID".into(), "CONTAINER".into(), "REASON".into(), "TIMES".into(), "WHEN".into(), "LAST LINE".into()]];
    for i in &is {
        let exit = if i["reason"] == "unhealthy" { "unhealthy".to_string() } else if i["oom_killed"] == true { "OOM".to_string() } else { i["exit_code"].to_string() };
        rows.push(vec![s(i, "/id").into(), s(i, "/container/name").into(), exit, i["repeats"].to_string(), ago(i["last"].as_u64().unwrap_or(0)), s(i, "/headline").chars().take(90).collect()]);
    }
    table(&rows)
}

fn incident_text(i: &Value) -> String {
    let code = i["exit_code"].as_i64().unwrap_or(0);
    let hint = EXIT_HINT.iter().find(|(c, _)| *c == code).map_or("", |(_, h)| h);
    let mem: Vec<f64> = i["metrics"]["mem"].as_array().into_iter().flatten().filter_map(Value::as_f64).collect();
    let cpu: Vec<f64> = i["metrics"]["cpu"].as_array().into_iter().flatten().filter_map(Value::as_f64).collect();
    let peak = |v: &[f64]| v.iter().cloned().fold(0.0, f64::max);
    let mut out = format!(
        "{}{} {}: {}\n{}× in a row · restarts {} · first {} · last {}\n",
        s(i, "/container/name"),
        match (s(i, "/container/stack"), s(i, "/container/service")) {
            ("", _) => String::new(),
            (st, sv) => format!(" ({st}/{sv})"),
        },
        if i["reason"] == "unhealthy" {
            "started FAILING ITS HEALTH CHECK".to_string()
        } else if i["oom_killed"] == true {
            "stopped: OUT OF MEMORY".to_string()
        } else {
            format!("stopped with exit code {code}")
        },
        if i["reason"] == "unhealthy" { "still running" } else { hint },
        i["repeats"],
        i["restarts"],
        ago(i["first"].as_u64().unwrap_or(0)),
        ago(i["last"].as_u64().unwrap_or(0)),
    );
    if !mem.is_empty() {
        out.push_str(&format!(
            "before the end (15 min): cpu peak {:.1}% · memory peak {} · last {}\n",
            peak(&cpu),
            mb(peak(&mem)),
            mb(*mem.last().unwrap_or(&0.0))
        ));
    }
    for h in i["health"]["checks"].as_array().into_iter().flatten() {
        out.push_str(&format!("health check (exit {}): {}\n", h["exit_code"], s(h, "/output")));
    }
    for (title, key) in [("error patterns", "error_patterns"), ("new since start", "new_patterns")] {
        let ps = i[key].as_array().cloned().unwrap_or_default();
        if !ps.is_empty() {
            out.push_str(&format!("\n{title}:\n"));
            for p in ps.iter().take(8) {
                out.push_str(&format!("  {:>6}× {}\n", p["count"], s(p, "/pattern")));
            }
        }
    }
    let lines: Vec<&str> = i["last_lines"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    out.push_str(&format!("\nlast {} lines:\n", lines.len().min(40)));
    for l in &lines[lines.len().saturating_sub(40)..] {
        out.push_str(&format!("  {l}\n"));
    }
    out
}

fn stacks_text(list: &Value) -> String {
    let mut by: std::collections::BTreeMap<String, Vec<&Value>> = Default::default();
    for c in list.as_array().into_iter().flatten() {
        if let Some(p) = label(c, "com.docker.compose.project") {
            by.entry(p.to_string()).or_default().push(c);
        }
    }
    if by.is_empty() {
        return "No Compose stacks.\n".into();
    }
    let mut rows = vec![vec!["STACK".into(), "UP".into(), "NEEDS ATTENTION".into(), "SERVICES".into()]];
    for (p, cs) in &by {
        let up = cs.iter().filter(|c| s(c, "/State") == "running").count();
        let bad: Vec<String> = cs.iter().filter(|c| {
            let st = status(c);
            st == "unhealthy" || st.starts_with("failed") || st == "restarting"
        }).map(|c| label(c, "com.docker.compose.service").unwrap_or("?").to_string()).collect();
        let mut svcs: Vec<&str> = cs.iter().filter_map(|c| label(c, "com.docker.compose.service")).collect();
        svcs.sort();
        rows.push(vec![p.clone(), format!("{up}/{}", cs.len()), if bad.is_empty() { "-".into() } else { bad.join(",") }, svcs.join(",")]);
    }
    table(&rows)
}

fn events_text(ndjson: &str) -> String {
    let mut rows = vec![vec!["WHEN".into(), "WHO".into(), "WHAT".into()]];
    for v in ndjson.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        let action = s(&v, "/Action");
        if action.starts_with("exec_") {
            continue;
        }
        let who = v.pointer("/Actor/Attributes/name").and_then(Value::as_str).unwrap_or(s(&v, "/Actor/ID"));
        let exit = v.pointer("/Actor/Attributes/exitCode").and_then(Value::as_str);
        let what = match (action, exit) {
            ("die", Some(c)) if c != "0" => format!("stopped with exit code {c}"),
            ("die", _) => "stopped".into(),
            (a, _) => a.to_string(),
        };
        rows.push(vec![ago(v["time"].as_u64().unwrap_or(0)), who.chars().take(48).collect(), what]);
    }
    if rows.len() == 1 {
        return "No events in this window.\n".into();
    }
    table(&rows)
}

fn activity_text(a: &Value) -> String {
    let es = a["entries"].as_array().cloned().unwrap_or_default();
    if es.is_empty() {
        return "Nothing recorded yet.\n".into();
    }
    let mut rows = vec![vec!["WHEN".into(), "ACTION".into(), "TARGET".into(), "WHO".into(), "RESULT".into()]];
    for e in &es {
        let who = format!("{} ({})", e["user"].as_str().unwrap_or("anonymous"), s(e, "/via"));
        let res = if e["ok"].as_bool() == Some(true) { "ok".to_string() } else { format!("failed: {}", s(e, "/message")) };
        rows.push(vec![ago(e["ts"].as_u64().unwrap_or(0)), s(e, "/action").into(), s(e, "/target").chars().take(40).collect(), who, res]);
    }
    table(&rows)
}

fn info_text(info: &Value, me: &Value) -> String {
    format!(
        "host          {}\ndocker        {}\nos            {} ({})\ncpus          {}\nmemory        {} MB\ncontainers    {} ({} running)\nimages        {}\nserver        {} {} pid {} · {} KB memory\naudit log     {}\n",
        s(info, "/name"),
        s(info, "/version"),
        s(info, "/os"),
        s(info, "/arch"),
        info["cpus"],
        info["memory"].as_u64().unwrap_or(0) / 1024 / 1024,
        info["containers"],
        info["running"],
        info["images"],
        s(me, "/name"),
        s(me, "/version"),
        me["pid"],
        me["rss"].as_u64().unwrap_or(0) / 1024,
        me["audit"].as_str().unwrap_or("off"),
    )
}

fn stats_text(st: &Value) -> String {
    let mb = |k: &str| st[k].as_f64().map_or("-".into(), |b| format!("{:.1} MB", b / 1048576.0));
    format!(
        "cpu      {}\nmemory   {} of {}\nnet in   {}\nnet out  {}\nprocs    {}\n",
        st["cpu"].as_f64().map_or("-".into(), |c| format!("{c:.1}%")),
        mb("mem"),
        mb("mem_limit"),
        mb("rx"),
        mb("tx"),
        st["pids"],
    )
}

/* ---------- CLI ---------- */

struct Args {
    host: Option<String>,
    token: Option<String>,
    skill_from: Option<String>,
    json: bool,
    follow: bool,
    timestamps: bool,
    new: bool,
    csv: bool,
    parquet: bool,
    n: Option<u32>,
    since: Option<u64>,
    pos: Vec<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        host: None,
        token: None,
        skill_from: None,
        json: false,
        follow: false,
        timestamps: false,
        new: false,
        csv: false,
        parquet: false,
        n: None,
        since: None,
        pos: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(x) = it.next() {
        let (flag, inline) = match x.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (x.as_str(), None),
        };
        let mut val = |name: &str| inline.clone().or_else(|| it.next().cloned()).ok_or_else(|| format!("{name} needs a value"));
        match flag {
            "--host" | "-H" => a.host = Some(val("--host")?),
            "--skill-from" => a.skill_from = Some(val("--skill-from")?),
            "--token" => a.token = Some(val("--token")?),
            "--json" => a.json = true,
            "-f" | "--follow" => a.follow = true,
            "-t" | "--timestamps" => a.timestamps = true,
            "--new" => a.new = true,
            "--csv" => a.csv = true,
            "--parquet" => a.parquet = true,
            "-n" | "--tail" => a.n = Some(val("-n")?.parse().map_err(|_| "-n needs a number")?),
            "--since" => a.since = Some(val("--since")?.parse().map_err(|_| "--since needs minutes")?),
            "-h" | "--help" => a.pos.insert(0, "help".into()),
            f if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown option '{f}'")),
            _ => a.pos.push(x.clone()),
        }
    }
    Ok(a)
}

/// Run a client command; `args` starts with the command name. Returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("contraire: {e}\nTry 'contraire ps --help'.");
            return 2;
        }
    };
    if a.pos.first().map(String::as_str) == Some("help") || a.pos.iter().any(|p| p == "help") {
        out(HELP);
        return 0;
    }
    let cmd = a.pos.first().map(String::as_str).unwrap_or("");
    // Precedence: flag > environment > session folder > default.
    let session = find_session();
    let host = a
        .host
        .clone()
        .or_else(|| std::env::var("CONTRAIRE_HOST").ok().filter(|h| !h.is_empty()))
        .or_else(|| session.as_ref().and_then(|s| s.host.clone()))
        .unwrap_or_else(|| "http://127.0.0.1:8083".into());
    let token = a
        .token
        .clone()
        .or_else(|| std::env::var("CONTRAIRE_TOKEN").ok().filter(|t| !t.is_empty()))
        .or_else(|| session.as_ref().and_then(Session::token));
    if cmd == "init" {
        return match init(&a, &host, token.as_deref()) {
            Ok(text) => {
                out(&text);
                0
            }
            Err(e) => {
                eprintln!("contraire: {e}");
                1
            }
        };
    }
    let mut remote = match Remote::connect(&host, token.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("contraire: {e}");
            return 2;
        }
    };
    remote.log = session.map(|s| s.log);
    if cmd == "mcp" {
        return mcp(&remote);
    }
    let result = cli(&remote, cmd, &a);
    let change = matches!(cmd, "start" | "stop" | "restart" | "pause" | "unpause" | "stack");
    remote.record(&format!("contraire {}", a.pos.join(" ")), change, &result);
    match result {
        Ok(text) => {
            out(&text);
            0
        }
        Err(e) => {
            eprintln!("contraire: {e}");
            1
        }
    }
}

/// Write to stdout, ignoring a closed pipe (`contraire ps | head` must not panic).
fn out(text: &str) {
    let _ = io::stdout().lock().write_all(text.as_bytes());
}

/* ---------- sessions: `contraire init` ---------- */

const SKILL_URL: &str = "https://raw.githubusercontent.com/he2s/contraire/main/skills/contraire/SKILL.md";
const REFERENCE_URL: &str = "https://raw.githubusercontent.com/he2s/contraire/main/skills/contraire/reference.md";

/// The per-project facts file `init` writes: headings only, filled in per use case.
const PROJECT_TEMPLATE: &str = "\
# Project

What this host is for, in facts the agent can't see from Docker. Fill it in, keep it current.
General Docker, Compose and Linux knowledge is in `.claude/skills/contraire/reference.md`; this
file is only about *this* setup.

## Purpose
<!-- What runs here and who relies on it. -->

## Stacks and services
<!-- One line each: name, what it does, how critical (can it be down for a minute? an hour?). -->

## Dependencies Compose doesn't show
<!-- External databases, APIs, NAS mounts, DNS, other hosts. -->

## Normal behavior
<!-- Known noisy logs, expected restarts, nightly jobs, usual CPU/memory, error rates that are fine. -->

## Hands off
<!-- Containers, volumes or times that must not be touched without asking, and why. -->

## How to deploy and roll back
<!-- Where compose.yaml lives, how images are updated, how to go back. -->

## Contacts
<!-- Who to ask or notify, and when. -->
";

/// Downloads `url` with curl (the binary has no TLS). `check` rejects error pages.
fn fetch(url: &str, check: impl Fn(&[u8]) -> bool) -> Option<String> {
    std::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "20", url])
        .output()
        .ok()
        .filter(|o| o.status.success() && check(&o.stdout))
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

struct Session {
    dir: PathBuf,
    host: Option<String>,
    log: PathBuf,
}

impl Session {
    fn token(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("token")).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
    }
}

/// The nearest `.contraire/session.json` in this directory or a parent.
fn find_session() -> Option<Session> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let cfg = dir.join(".contraire/session.json");
        if let Ok(text) = std::fs::read_to_string(&cfg) {
            let v: Value = serde_json::from_str(&text).ok()?;
            let base = dir.join(".contraire");
            let log = base.join(v.get("log").and_then(Value::as_str).unwrap_or("actions.log"));
            return Some(Session { host: v.get("host").and_then(Value::as_str).map(str::to_string), log, dir: base });
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn init(a: &Args, host: &str, token: Option<&str>) -> Result<String, String> {
    let dir = PathBuf::from(a.pos.get(1).ok_or("'init' needs a folder, e.g. contraire init ops-pi")?);
    if dir.join(".contraire/session.json").exists() {
        return Err(format!("{} is already a contraire session", dir.display()));
    }
    let remote = Remote::connect(host, token)?;
    let skill = match &a.skill_from {
        Some(p) => std::fs::read_to_string(p).map_err(|e| format!("cannot read {p}: {e}"))?,
        // The skill lives on GitHub, not in the binary.
        None => fetch(SKILL_URL, |b| b.starts_with(b"---"))
            .ok_or_else(|| format!("could not download the skill from {SKILL_URL} (curl missing or offline?); use --skill-from <SKILL.md>"))?,
    };
    // The general reference sits next to the skill; with --skill-from, next to that file.
    let reference = match &a.skill_from {
        Some(p) => std::fs::read_to_string(std::path::Path::new(p).with_file_name("reference.md")).ok(),
        None => fetch(REFERENCE_URL, |b| b.starts_with(b"# ")),
    };
    for d in [".contraire", ".claude/skills/contraire"] {
        std::fs::create_dir_all(dir.join(d)).map_err(|e| format!("cannot create {}: {e}", dir.join(d).display()))?;
    }
    write(&dir.join(".claude/skills/contraire/SKILL.md"), &skill)?;
    if let Some(r) = &reference {
        write(&dir.join(".claude/skills/contraire/reference.md"), r)?;
    }
    write(&dir.join("PROJECT.md"), PROJECT_TEMPLATE)?;
    write(&dir.join(".contraire/session.json"), &format!("{:#}\n", json!({ "host": remote.host, "log": "actions.log" })))?;
    write(&dir.join(".gitignore"), "# The API token never goes into git.\n.contraire/token\n")?;
    if let Some(t) = token {
        let path = dir.join(".contraire/token");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        opts.open(&path).and_then(|mut f| f.write_all(t.as_bytes())).map_err(|e| format!("cannot write token: {e}"))?;
    }
    // Looking around needs no confirmation; changes still ask.
    let reads = ["ps", "top", "inspect", "logs", "patterns", "incidents", "incident", "stacks", "events", "services", "service", "info", "activity"];
    let settings = json!({ "permissions": { "allow": reads.iter().map(|c| format!("Bash(contraire {c}:*)")).collect::<Vec<_>>() } });
    write(&dir.join(".claude/settings.json"), &format!("{settings:#}\n"))?;

    let info = remote.get("/api/info").ok();
    let about = info.as_ref().map_or_else(
        || "server not reachable yet".to_string(),
        |i| format!("{}, Docker {}, {}", s(i, "/name"), s(i, "/version"), s(i, "/os")),
    );
    write(
        &dir.join("CLAUDE.md"),
        &format!(
            "# Docker operations session\n\n\
             This folder is a working session for the Docker host behind {host} ({about}), operated with the `contraire` CLI.\n\n\
             - Use the **contraire** skill (`.claude/skills/contraire/`) for anything about containers, logs, stacks, telemetry or recent changes. \
             Its `reference.md` holds general Docker, Compose and Linux facts and the rules for changing things.\n\
             - Read `PROJECT.md` before the first task: what this host is for, what is normal here, what not to touch. \
             When you learn a lasting fact about this setup (and the user confirms it), add it there. It starts empty; ask the user to fill in the gaps that matter for the task.\n\
             - Every `contraire` command run here is appended automatically to `.contraire/actions.log` (time, ok/FAIL, read/change, command). Don't edit it.\n\
             - `SESSION.md` is the journal for humans. After each task, append a dated entry: what you checked, what you found (with evidence), \
             what you changed, and what you recommend. Changes matter most.\n\
             - The API token is in `.contraire/token`, which git ignores. Never print it or put it on a command line; the CLI reads it on its own.\n",
            host = remote.host
        ),
    )?;
    write(
        &dir.join("SESSION.md"),
        &format!(
            "# Session journal\n\nHost: {} ({about})  \nStarted: {}  \nRaw command log: `.contraire/actions.log`\n",
            remote.host,
            rfc3339(now())
        ),
    )?;
    let git = std::process::Command::new("git").args(["init", "-q"]).current_dir(&dir).status().is_ok_and(|s| s.success());
    if git {
        let _ = std::process::Command::new("git").args(["add", "-A"]).current_dir(&dir).status();
    }
    Ok(format!(
        "Created session {dir}\n  host      {host} ({about})\n  token     {tok}\n  skill     .claude/skills/contraire/SKILL.md\n  reference {refr}\n  project   PROJECT.md (fill in what this host is for)\n  log       .contraire/actions.log\n  journal   SESSION.md\n  git       {git}\n\nNext:\n  cd {dir} && claude\n",
        dir = dir.display(),
        host = remote.host,
        tok = if token.is_some() { "saved in .contraire/token (git-ignored)" } else { "none: set CONTRAIRE_TOKEN, or add .contraire/token" },
        git = if git { "initialized (files staged, not committed)" } else { "not available" },
        refr = if reference.is_some() { ".claude/skills/contraire/reference.md" } else { "not found (download failed); the skill works without it" },
    ))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `2026-10-10T12:34:56Z` without a date library (days-to-civil, proleptic Gregorian).
fn rfc3339(t: u64) -> String {
    let (days, rem) = ((t / 86_400) as i64, t % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn cli(r: &Remote, cmd: &str, a: &Args) -> Result<String, String> {
    let arg = |i: usize, what: &str| a.pos.get(i).cloned().ok_or_else(|| format!("'{cmd}' needs a {what}"));
    let raw = |v: Value| Ok(format!("{}\n", serde_json::to_string_pretty(&v).unwrap_or_default()));
    match cmd {
        "ps" => {
            let v = r.get("/api/containers")?;
            if a.json { raw(v) } else { Ok(ps_text(&v)) }
        }
        "inspect" => {
            let v = r.get(&format!("/api/containers/{}", enc(&arg(1, "container")?)))?;
            if a.json { raw(v) } else { Ok(inspect_text(&v)) }
        }
        "logs" => {
            let c = arg(1, "container")?;
            let n = a.n.unwrap_or(100);
            if a.follow {
                follow_logs(r, &c, n, a.timestamps)?;
                return Ok(String::new());
            }
            let t = r.get_text(&format!("/api/containers/{}/logs?tail={n}", enc(&c)))?;
            Ok(if a.timestamps { t } else { strip_timestamps(&t) + "\n" })
        }
        "start" | "stop" | "restart" | "pause" | "unpause" => {
            let c = arg(1, "container")?;
            r.post(&format!("/api/containers/{}/{cmd}", enc(&c)), None)?;
            Ok(format!("{c}: {cmd} done\n"))
        }
        "patterns" => {
            let c = arg(1, "container")?;
            let v = r.get(&format!("/api/logs/patterns/{}{}", enc(&c), if a.new { "?new=1" } else { "" }))?;
            if a.json { raw(v) } else { Ok(patterns_text(&c, &v, a.n.unwrap_or(40) as usize)) }
        }
        "incidents" => {
            let v = r.get("/api/incidents")?;
            if a.json { raw(v) } else { Ok(incidents_text(&v)) }
        }
        "incident" => {
            let v = r.get(&format!("/api/incidents/{}", enc(&arg(1, "incident id (see 'contraire incidents')")?)))?;
            if a.json { raw(v) } else { Ok(incident_text(&v)) }
        }
        "top" => {
            let (l, m) = (r.get("/api/containers")?, r.get("/api/metrics?spark=0")?);
            if a.json { raw(m) } else { Ok(top_text(&l, &m)) }
        }
        "stacks" => {
            let v = r.get("/api/containers")?;
            Ok(stacks_text(&v))
        }
        "stack" => {
            let (p, verb) = (arg(1, "stack name")?, arg(2, "action (start, stop or restart)")?);
            let v = r.post(&format!("/api/projects/{}/{}", enc(&p), enc(&verb)), None)?;
            let failed = v["failed"].as_array().map_or(0, Vec::len);
            if a.json { raw(v) } else { Ok(format!("{p}: {verb} on {} containers, {failed} failed\n", v["count"])) }
        }
        "events" => {
            let t = r.get_text(&format!("/api/events?since={}", a.since.unwrap_or(60) * 60))?;
            if a.json { Ok(t) } else { Ok(events_text(&t)) }
        }
        "services" => {
            let v = r.get("/api/telemetry")?;
            if a.json { raw(v) } else { Ok(services_text(&v)) }
        }
        "service" => {
            let v = r.get(&format!("/api/telemetry/{}", enc(&arg(1, "service name (see 'contraire services')")?)))?;
            if a.json { raw(v) } else { Ok(service_text(&v)) }
        }
        "export" => {
            let what = arg(1, "source: metrics or telemetry")?;
            if what != "metrics" && what != "telemetry" {
                return Err(format!("export what? 'metrics' or 'telemetry', not '{what}'"));
            }
            if a.parquet {
                // Binary: straight to stdout, never to a terminal.
                use std::io::IsTerminal;
                if io::stdout().is_terminal() {
                    return Err(format!("Parquet is binary: redirect it, e.g. contraire export {what} --parquet > {what}.parquet"));
                }
                let b = r.call("GET", &format!("/api/{what}/export?format=parquet"), None, 30)?;
                let _ = io::stdout().lock().write_all(&b);
                return Ok(String::new());
            }
            r.get_text(&format!("/api/{what}/export{}", if a.csv { "?format=csv" } else { "" }))
        }
        "activity" => {
            let v = r.get(&format!("/api/audit?limit={}", a.n.unwrap_or(30)))?;
            if a.json { raw(v) } else { Ok(activity_text(&v)) }
        }
        "info" => {
            let (i, me) = (r.get("/api/info")?, r.get("/api/self")?);
            if a.json { raw(json!({ "host": i, "server": me })) } else { Ok(info_text(&i, &me)) }
        }
        "" => Err("missing command; see 'contraire ps --help'".into()),
        other => Err(format!("unknown command '{other}'")),
    }
}

/// Follow logs over the server's live stream (SSE) until the container stops.
fn follow_logs(r: &Remote, c: &str, tail: u32, ts: bool) -> Result<(), String> {
    let path = format!("/api/stream?c={}&tail={tail}", enc(c));
    let mut s = match r.http.stream(&path, Duration::from_secs(20)) {
        Ok(Ok(s)) => s,
        Ok(Err(reply)) => return Err(format!("stream refused (HTTP {})", reply.status)),
        Err(e) => return Err(format!("cannot reach {}: {e}", r.host)),
    };
    let (mut buf, mut event) = (Vec::new(), String::new());
    let mut out = io::stdout().lock();
    loop {
        let tick = s.read_into(&mut buf).map_err(|e| e.to_string())?;
        while let Some(i) = buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&buf[..i]).into_owned();
            buf.drain(..=i);
            if let Some(e) = line.strip_prefix("event: ") {
                event = e.to_string();
            } else if let Some(d) = line.strip_prefix("data: ") {
                match event.as_str() {
                    "logs" => {
                        let d = if ts { d.to_string() } else { strip_timestamps(d) };
                        let _ = writeln!(out, "{d}");
                    }
                    "fail" => return Err(d.to_string()),
                    _ => {}
                }
            } else if line.is_empty() && event == "logend" {
                return Ok(());
            }
        }
        let _ = out.flush();
        if matches!(tick, Tick::Eof) {
            return Ok(());
        }
    }
}

/* ---------- MCP server (stdio, JSON-RPC 2.0, newline-delimited) ---------- */

const PROTOCOL: &str = "2025-06-18";

/// The MCP tool catalogue as plain JSON data: parsed on request instead of expanding
/// `json!` into ~24 KB of code. Annotations: read-only tools never change anything;
/// start/stop/restart/stack_action do, and are marked destructive where downtime results.
const TOOLS: &str = r#"[{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"All containers on the Docker host with plain-language status (running, unhealthy, failed (code), paused, exited), uptime, compose stack, published ports and image.","inputSchema":{"properties":{},"type":"object"},"name":"list_containers","title":"List containers"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"State, health check streak, exit code with an explanation, restarts, image, command, ports, networks, mounts, memory limit and compose stack/service/dependencies.","inputSchema":{"properties":{"container":{"description":"Container name or ID","type":"string"}},"required":["container"],"type":"object"},"name":"inspect_container","title":"Inspect a container"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"The last lines of a container's logs (stdout and stderr). Use 'grep' to keep only matching lines.","inputSchema":{"properties":{"container":{"description":"Container name or ID","type":"string"},"grep":{"description":"Case-insensitive substring filter","type":"string"},"tail":{"description":"Number of lines, default 100, max 2000","type":"integer"}},"required":["container"],"type":"object"},"name":"container_logs","title":"Container logs"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"Current CPU %, memory (and limit), network bytes and process count of a running container.","inputSchema":{"properties":{"container":{"description":"Container name or ID","type":"string"}},"required":["container"],"type":"object"},"name":"container_stats","title":"Container resource usage"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"Current CPU (% of one core), memory, network and disk rates per container, busiest first, plus host CPU, load and memory. Sampled from cgroups every 10 s.","inputSchema":{"properties":{},"type":"object"},"name":"top_containers","title":"Resource use per container"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"Now / peak / average CPU, memory, network and disk over the last hour (10 s samples) or 24 hours (1 min averages), plus the memory trend. Use it to spot leaks, spikes before a crash, or a noisy neighbor.","inputSchema":{"properties":{"container":{"description":"Container name or ID","type":"string"},"range":{"description":"Default 1h","enum":["1h","24h"],"type":"string"}},"required":["container"],"type":"object"},"name":"container_metrics","title":"Container resource history"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"The container's log since it started, grouped into patterns with counts (variable parts shown as <*>), errors first, with first/last seen and whether the pattern is NEW since start. Far cheaper than reading raw logs; read raw lines afterwards only for the patterns that matter.","inputSchema":{"properties":{"container":{"description":"Container name or ID","type":"string"},"only_new":{"description":"Only patterns that first appeared after startup (what changed)","type":"boolean"}},"required":["container"],"type":"object"},"name":"log_patterns","title":"Log patterns of a container"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"Incidents the flight recorder caught (non-zero exit, OOM kill, or a health check starting to fail): without an id, the list; with an id, the postmortem with the reason (and the health check's own output), last log lines, error and new patterns and CPU/memory in the 15 minutes before.","inputSchema":{"properties":{"id":{"description":"Incident id from the list","type":"string"}},"type":"object"},"name":"incidents","title":"Incidents (crash postmortems)"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"Telemetry: what the apps themselves report over OpenTelemetry, last hour. Without 'service': every service with requests/min, error rate and p50/p95/p99 latency, most failing first. With 'service': its operations, latest failed and slowest requests (with trace IDs and HTTP/RPC attributes) and its OTLP log patterns. Use it when a container looks healthy but users see errors or slowness.","inputSchema":{"properties":{"service":{"description":"service.name as reported, from the list","type":"string"}},"type":"object"},"name":"service_health","title":"Telemetry: service health"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"Compose stacks on the host: how many services are up and which need attention.","inputSchema":{"properties":{},"type":"object"},"name":"list_stacks","title":"List Compose stacks"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"Container starts, stops (with exit codes), restarts, OOM kills, health changes and image pulls in the last N minutes.","inputSchema":{"properties":{"minutes":{"description":"Window in minutes, default 60, max 1440","type":"integer"}},"type":"object"},"name":"recent_events","title":"Recent Docker events"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"Who changed what through the dashboard, when, and whether it worked. Needs an admin token.","inputSchema":{"properties":{"limit":{"description":"Entries, default 30","type":"integer"}},"type":"object"},"name":"activity","title":"Audit log"},{"annotations":{"openWorldHint":false,"readOnlyHint":true},"description":"Docker host name, version, OS, CPUs, memory, container and image counts, and the dashboard's own footprint.","inputSchema":{"properties":{},"type":"object"},"name":"host_info","title":"Host info"},{"annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false},"description":"Start a stopped container.","inputSchema":{"properties":{"container":{"description":"Container name or ID","type":"string"}},"required":["container"],"type":"object"},"name":"start_container","title":"Start a container"},{"annotations":{"destructiveHint":true,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false},"description":"Restart a container. Brief downtime for that service.","inputSchema":{"properties":{"container":{"description":"Container name or ID","type":"string"}},"required":["container"],"type":"object"},"name":"restart_container","title":"Restart a container"},{"annotations":{"destructiveHint":true,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false},"description":"Stop a running container. Its service is down until started again.","inputSchema":{"properties":{"container":{"description":"Container name or ID","type":"string"}},"required":["container"],"type":"object"},"name":"stop_container","title":"Stop a container"},{"annotations":{"destructiveHint":true,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false},"description":"Act on every container of a Compose stack in dependency order (dependencies start first, stop last).","inputSchema":{"properties":{"action":{"enum":["start","stop","restart"],"type":"string"},"stack":{"description":"Stack (Compose project) name","type":"string"}},"required":["stack","action"],"type":"object"},"name":"stack_action","title":"Start, stop or restart a Compose stack"}]"#;

fn tools() -> Value {
    serde_json::from_str(TOOLS).unwrap_or_default()
}

fn call_tool(r: &Remote, name: &str, args: &Value) -> Result<String, String> {
    let str_arg = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("missing argument '{k}'"));
    let int_arg = |k: &str, d: u64, max: u64| args.get(k).and_then(Value::as_u64).unwrap_or(d).clamp(1, max);
    match name {
        "list_containers" => r.get("/api/containers").map(|v| ps_text(&v)),
        "inspect_container" => r.get(&format!("/api/containers/{}", enc(&str_arg("container")?))).map(|v| inspect_text(&v)),
        "container_logs" => {
            let c = str_arg("container")?;
            let t = r.get_text(&format!("/api/containers/{}/logs?tail={}", enc(&c), int_arg("tail", 100, 2000)))?;
            let t = strip_timestamps(&t);
            Ok(match args.get("grep").and_then(Value::as_str).map(str::to_lowercase) {
                Some(g) => t.lines().filter(|l| l.to_lowercase().contains(&g)).collect::<Vec<_>>().join("\n"),
                None => t,
            })
            .map(|t| if t.trim().is_empty() { "(no matching log lines)".into() } else { t })
        }
        "container_stats" => r.get(&format!("/api/containers/{}/stats", enc(&str_arg("container")?))).map(|v| stats_text(&v)),
        "list_stacks" => r.get("/api/containers").map(|v| stacks_text(&v)),
        "log_patterns" => {
            let c = str_arg("container")?;
            let only_new = args.get("only_new").and_then(Value::as_bool).unwrap_or(false);
            r.get(&format!("/api/logs/patterns/{}{}", enc(&c), if only_new { "?new=1" } else { "" })).map(|v| patterns_text(&c, &v, 40))
        }
        "incidents" => match args.get("id").and_then(Value::as_str) {
            Some(id) => r.get(&format!("/api/incidents/{}", enc(id))).map(|v| incident_text(&v)),
            None => r.get("/api/incidents").map(|v| incidents_text(&v)),
        },
        "top_containers" => Ok(top_text(&r.get("/api/containers")?, &r.get("/api/metrics?spark=0")?)),
        "container_metrics" => {
            let c = str_arg("container")?;
            let range = if args.get("range").and_then(Value::as_str) == Some("24h") { "24h" } else { "1h" };
            r.get(&format!("/api/metrics/{}?range={range}", enc(&c))).map(|h| metrics_summary(&c, range, &h))
        }
        "recent_events" => r.get_text(&format!("/api/events?since={}", int_arg("minutes", 60, 1440) * 60)).map(|t| events_text(&t)),
        "activity" => r.get(&format!("/api/audit?limit={}", int_arg("limit", 30, 500))).map(|v| activity_text(&v)),
        "service_health" => match args.get("service").and_then(Value::as_str) {
            Some(n) => r.get(&format!("/api/telemetry/{}", enc(n))).map(|v| service_text(&v)),
            None => r.get("/api/telemetry").map(|v| services_text(&v)),
        },
        "host_info" => Ok(info_text(&r.get("/api/info")?, &r.get("/api/self")?)),
        "start_container" | "stop_container" | "restart_container" => {
            let verb = name.trim_end_matches("_container");
            let c = str_arg("container")?;
            r.post(&format!("/api/containers/{}/{verb}", enc(&c)), None)?;
            let d = r.get(&format!("/api/containers/{}", enc(&c)))?;
            Ok(format!("{verb} {c}: done. Now:\n{}", inspect_text(&d)))
        }
        "stack_action" => {
            let (p, verb) = (str_arg("stack")?, str_arg("action")?);
            if !matches!(verb.as_str(), "start" | "stop" | "restart") {
                return Err("action must be start, stop or restart".into());
            }
            let v = r.post(&format!("/api/projects/{}/{verb}", enc(&p)), None)?;
            let failed: Vec<String> = v["failed"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| format!("{}: {}", s(f, "/name"), s(f, "/message")))
                .collect();
            Ok(format!("{verb} {p}: {} containers, {} failed{}", v["count"], failed.len(), if failed.is_empty() { String::new() } else { format!("\n{}", failed.join("\n")) }))
        }
        other => Err(format!("unknown tool '{other}'")),
    }
}

fn rpc(r: &Remote, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let ok = |result: Value| Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    let err = |code: i64, m: &str| Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": m } }));
    // Notifications (no id) never get a response.
    id.as_ref()?;
    match method {
        "initialize" => {
            let asked = msg.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL);
            let version = if ["2024-11-05", "2025-03-26", "2025-06-18"].contains(&asked) { asked } else { PROTOCOL };
            ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "contraire", "title": crate::DISPLAY_NAME, "version": env!("CARGO_PKG_VERSION") },
                "instructions": format!(
                    "Tools for the Docker host behind {}. Start with list_containers or list_stacks; for a broken \
                     container use inspect_container (exit code explained), then container_logs with a grep. \
                     Changes are limited by the API token's scope and recorded in the audit log under its name. \
                     Ask the user before stopping or restarting anything they did not ask you to touch.", r.host),
            }))
        }
        "ping" => ok(json!({})),
        "tools/list" => ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
            let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
            // Tool failures are results the model should see, not protocol errors.
            let result = call_tool(r, name, &args);
            let change = matches!(name, "start_container" | "stop_container" | "restart_container" | "stack_action");
            r.record(&format!("mcp {name} {args}"), change, &result);
            let (text, is_error) = match result {
                Ok(t) => (t, false),
                Err(e) => (e, true),
            };
            ok(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
        }
        _ => err(-32601, &format!("method not found: {method}")),
    }
}

fn mcp(r: &Remote) -> i32 {
    eprintln!("contraire: MCP server on stdio for {}", r.host);
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let rs: Vec<Value> = batch.iter().filter_map(|m| rpc(r, m)).collect();
                (!rs.is_empty()).then_some(Value::Array(rs))
            }
            Ok(m) => rpc(r, &m),
            Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("parse error: {e}") } })),
        };
        if let Some(reply) = reply
            && writeln!(out, "{reply}").and_then(|_| out.flush()).is_err()
        {
            break;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing() {
        assert_eq!(Remote::connect("http://pi.local", None).unwrap().host, "http://pi.local:8083");
        assert_eq!(Remote::connect("http://10.0.0.2:9000/", None).unwrap().host, "http://10.0.0.2:9000");
        assert!(Remote::connect("https://pi.local", None).is_err());
        assert!(Remote::connect("http://pi.local/x", None).is_err());
    }

    #[test]
    fn statuses_and_tables() {
        let c = json!([
            { "Names": ["/web"], "State": "running", "Status": "Up 2 hours (unhealthy)", "Image": "nginx", "Labels": { "com.docker.compose.project": "shop", "com.docker.compose.service": "web" },
              "Ports": [{ "PublicPort": 8080, "PrivatePort": 80 }, { "PublicPort": 8080, "PrivatePort": 80 }] },
            { "Names": ["/job"], "State": "exited", "Status": "Exited (137) 5 minutes ago", "Image": "busybox", "Labels": {} },
        ]);
        let t = ps_text(&c);
        assert!(t.contains("unhealthy") && t.contains("failed (137)") && t.contains("8080->80"));
        assert!(!t.contains("8080->80,8080->80"));
        let st = stacks_text(&c);
        assert!(st.contains("shop") && st.contains("1/1"), "{st}");
    }

    #[test]
    fn mcp_protocol_basics() {
        let r = Remote::connect("http://127.0.0.1:1", None).unwrap();
        let init = rpc(&r, &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } })).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(rpc(&r, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
        let list = rpc(&r, &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).unwrap();
        assert!(list["result"]["tools"].as_array().unwrap().len() >= 14);
        // Unreachable server: a tool error result, not a protocol error.
        let call = rpc(&r, &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "list_containers", "arguments": {} } })).unwrap();
        assert_eq!(call["result"]["isError"], true);
        assert_eq!(rpc(&r, &json!({ "jsonrpc": "2.0", "id": 4, "method": "nope" })).unwrap()["error"]["code"], -32601);
    }

    #[test]
    fn rfc3339_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_791_631_609), "2026-10-10T11:26:49Z");
    }

    #[test]
    fn strips_docker_timestamps() {
        assert_eq!(strip_timestamps("2026-10-10T09:50:03.701695991Z hello\nplain"), "hello\nplain");
    }
}
