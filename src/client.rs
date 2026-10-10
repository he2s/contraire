//! Client mode: the same binary, run on a workstation, talking to a contraire server.
//!
//!     contraire ps | inspect <c> | logs <c> [-f] | start|stop|restart|pause|unpause <c>
//!     contraire stacks | stack <name> start|stop|restart | events | activity | info
//!     contraire mcp      # MCP server over stdio for Claude Code and other agents
//!
//! Connection: --host / CONTRAIRE_HOST (http://host:port) and --token / CONTRAIRE_TOKEN.
//! Plain HTTP only (no TLS in the binary): reach a remote Pi over an SSH tunnel or a VPN.
//! Every call goes through the server's API, so token scopes and the audit log apply.

use std::io::{self, BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

use crate::docker::{Docker, Tick};

pub const COMMANDS: &[&str] =
    &["ps", "inspect", "logs", "start", "stop", "restart", "pause", "unpause", "stacks", "stack", "events", "activity", "info", "mcp"];

const HELP: &str = "\
Client mode: control a contraire server from this machine.

Usage: contraire <COMMAND> [ARGS] [--host URL] [--token TOKEN] [--json]

Commands:
  ps                         List containers
  inspect <container>        State, health, exit reason, ports, mounts, compose info
  logs <container> [-n N] [-f] [-t]   Last N lines (default 100); -f follows live, -t shows timestamps
  start|stop|restart|pause|unpause <container>
  stacks                     List compose stacks
  stack <name> start|stop|restart     Whole stack, in depends_on order
  events [--since MIN]       Docker events of the last MIN minutes (default 60)
  activity [-n N]            The server's audit log (admin token)
  info                       Host and server facts
  mcp                        Run an MCP server on stdio for Claude Code and other agents

Options:
  --host <URL>    Server, e.g. http://pi.local:8083   [env: CONTRAIRE_HOST] [default: http://127.0.0.1:8083]
  --token <TOKEN> API token from the Access page      [env: CONTRAIRE_TOKEN]
  --json          Print the raw JSON instead of a table

Claude Code:
  claude mcp add contraire --env CONTRAIRE_TOKEN=ctr_... -- contraire mcp --host http://pi.local:8083
";

pub struct Remote {
    http: Docker,
    pub host: String,
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
        Ok(Remote { http, host: format!("http://{addr}") })
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

fn stacks_text(list: &Value) -> String {
    let mut by: std::collections::BTreeMap<String, Vec<&Value>> = Default::default();
    for c in list.as_array().into_iter().flatten() {
        if let Some(p) = label(c, "com.docker.compose.project") {
            by.entry(p.to_string()).or_default().push(c);
        }
    }
    if by.is_empty() {
        return "No compose stacks.\n".into();
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
    host: String,
    token: Option<String>,
    json: bool,
    follow: bool,
    timestamps: bool,
    n: Option<u32>,
    since: Option<u64>,
    pos: Vec<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        host: std::env::var("CONTRAIRE_HOST").unwrap_or_else(|_| "http://127.0.0.1:8083".into()),
        token: std::env::var("CONTRAIRE_TOKEN").ok(),
        json: false,
        follow: false,
        timestamps: false,
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
            "--host" | "-H" => a.host = val("--host")?,
            "--token" => a.token = Some(val("--token")?),
            "--json" => a.json = true,
            "-f" | "--follow" => a.follow = true,
            "-t" | "--timestamps" => a.timestamps = true,
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
        print!("{HELP}");
        return 0;
    }
    let remote = match Remote::connect(&a.host, a.token.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("contraire: {e}");
            return 2;
        }
    };
    let cmd = a.pos.first().map(String::as_str).unwrap_or("");
    if cmd == "mcp" {
        return mcp(&remote);
    }
    match cli(&remote, cmd, &a) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(e) => {
            eprintln!("contraire: {e}");
            1
        }
    }
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

fn tools() -> Value {
    let c = json!({ "type": "object", "properties": { "container": { "type": "string", "description": "Container name or ID" } }, "required": ["container"] });
    let ro = json!({ "readOnlyHint": true, "openWorldHint": false });
    let act = |destructive: bool| json!({ "readOnlyHint": false, "destructiveHint": destructive, "idempotentHint": true, "openWorldHint": false });
    json!([
        { "name": "list_containers", "title": "List containers",
          "description": "All containers on the Docker host with plain-language status (running, unhealthy, failed (code), paused, exited), uptime, compose stack, published ports and image.",
          "inputSchema": { "type": "object", "properties": {} }, "annotations": ro },
        { "name": "inspect_container", "title": "Inspect a container",
          "description": "State, health check streak, exit code with an explanation, restarts, image, command, ports, networks, mounts, memory limit and compose stack/service/dependencies.",
          "inputSchema": c, "annotations": ro },
        { "name": "container_logs", "title": "Container logs",
          "description": "The last lines of a container's logs (stdout and stderr). Use 'grep' to keep only matching lines.",
          "inputSchema": { "type": "object", "properties": {
              "container": { "type": "string", "description": "Container name or ID" },
              "tail": { "type": "integer", "description": "Number of lines, default 100, max 2000" },
              "grep": { "type": "string", "description": "Case-insensitive substring filter" } }, "required": ["container"] },
          "annotations": ro },
        { "name": "container_stats", "title": "Container resource usage",
          "description": "Current CPU %, memory (and limit), network bytes and process count of a running container.",
          "inputSchema": c, "annotations": ro },
        { "name": "list_stacks", "title": "List compose stacks",
          "description": "Docker Compose projects on the host: how many services are up and which need attention.",
          "inputSchema": { "type": "object", "properties": {} }, "annotations": ro },
        { "name": "recent_events", "title": "Recent Docker events",
          "description": "Container starts, stops (with exit codes), restarts, OOM kills, health changes and image pulls in the last N minutes.",
          "inputSchema": { "type": "object", "properties": { "minutes": { "type": "integer", "description": "Window in minutes, default 60, max 1440" } } },
          "annotations": ro },
        { "name": "activity", "title": "Audit log",
          "description": "Who changed what through the dashboard, when, and whether it worked. Needs an admin token.",
          "inputSchema": { "type": "object", "properties": { "limit": { "type": "integer", "description": "Entries, default 30" } } },
          "annotations": ro },
        { "name": "host_info", "title": "Host info",
          "description": "Docker host name, version, OS, CPUs, memory, container and image counts, and the dashboard's own footprint.",
          "inputSchema": { "type": "object", "properties": {} }, "annotations": ro },
        { "name": "start_container", "title": "Start a container", "description": "Start a stopped container.", "inputSchema": c, "annotations": act(false) },
        { "name": "restart_container", "title": "Restart a container",
          "description": "Restart a container. Brief downtime for that service.", "inputSchema": c, "annotations": act(true) },
        { "name": "stop_container", "title": "Stop a container",
          "description": "Stop a running container. Its service is down until started again.", "inputSchema": c, "annotations": act(true) },
        { "name": "stack_action", "title": "Start, stop or restart a compose stack",
          "description": "Act on every container of a compose stack in dependency order (dependencies start first, stop last).",
          "inputSchema": { "type": "object", "properties": {
              "stack": { "type": "string", "description": "Compose project name" },
              "action": { "type": "string", "enum": ["start", "stop", "restart"] } }, "required": ["stack", "action"] },
          "annotations": act(true) },
    ])
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
        "recent_events" => r.get_text(&format!("/api/events?since={}", int_arg("minutes", 60, 1440) * 60)).map(|t| events_text(&t)),
        "activity" => r.get(&format!("/api/audit?limit={}", int_arg("limit", 30, 500))).map(|v| activity_text(&v)),
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
            let (text, is_error) = match call_tool(r, name, &args) {
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
        assert!(list["result"]["tools"].as_array().unwrap().len() >= 10);
        // Unreachable server: a tool error result, not a protocol error.
        let call = rpc(&r, &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "list_containers", "arguments": {} } })).unwrap();
        assert_eq!(call["result"]["isError"], true);
        assert_eq!(rpc(&r, &json!({ "jsonrpc": "2.0", "id": 4, "method": "nope" })).unwrap()["error"]["code"], -32601);
    }

    #[test]
    fn strips_docker_timestamps() {
        assert_eq!(strip_timestamps("2026-10-10T09:50:03.701695991Z hello\nplain"), "hello\nplain");
    }
}
