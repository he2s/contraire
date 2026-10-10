//! Append-only audit log: who changed what, when, from where, and whether it worked.
//!
//! One JSON object per line. Each entry is a single `O_APPEND` write, which Linux keeps
//! atomic for regular files, so concurrent handler threads need no lock. The file is
//! bounded: past `MAX_BYTES` it is renamed to `<file>.1` (replacing the previous one),
//! so the log never holds more than ~2 × MAX_BYTES on disk. Two threads rotating at the
//! same instant can at worst drop a rotation, never corrupt a line.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

pub const MAX_BYTES: u64 = 1024 * 1024;

pub struct Audit {
    path: Option<PathBuf>,
    max_bytes: u64,
}

impl Audit {
    /// `spec` is a file path, `off`, or `auto` (systemd's $STATE_DIRECTORY, else
    /// $XDG_STATE_HOME/contraire, else ~/.local/state/contraire; off when none exists).
    pub fn open(spec: &str, env: impl Fn(&str) -> Option<String>) -> Self {
        let path = match spec {
            "off" | "" => None,
            "auto" => env("STATE_DIRECTORY")
                .map(PathBuf::from)
                .or_else(|| env("XDG_STATE_HOME").map(|d| Path::new(&d).join("contraire")))
                .or_else(|| env("HOME").map(|h| Path::new(&h).join(".local/state/contraire")))
                .map(|d| d.join("audit.jsonl")),
            p => Some(PathBuf::from(p)),
        };
        let path = path.filter(|p| p.parent().is_none_or(|d| d.as_os_str().is_empty() || fs::create_dir_all(d).is_ok()));
        Audit { path, max_bytes: MAX_BYTES }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn record(&self, entry: &Value) {
        let Some(path) = &self.path else { return };
        if fs::metadata(path).is_ok_and(|m| m.len() >= self.max_bytes) {
            let _ = fs::rename(path, rotated(path));
        }
        let mut line = entry.to_string();
        line.push('\n');
        let written = OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = written {
            eprintln!("contraire: audit: cannot write {}: {e}", path.display());
        }
    }

    /// Newest first, optionally only entries whose `target` equals `target`.
    pub fn recent(&self, limit: usize, target: Option<&str>) -> Vec<Value> {
        let Some(path) = &self.path else { return Vec::new() };
        let mut out = Vec::new();
        for file in [path.clone(), rotated(path)] {
            let Ok(text) = fs::read_to_string(&file) else { continue };
            for line in text.lines().rev() {
                let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
                if target.is_some_and(|t| v.get("target").and_then(Value::as_str) != Some(t)) {
                    continue;
                }
                out.push(v);
                if out.len() >= limit {
                    return out;
                }
            }
        }
        out
    }
}

fn rotated(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

/// Short, human label for a client: "browser", "curl", or the user-agent's first word.
pub fn client_kind(user_agent: Option<&str>) -> String {
    let ua = user_agent.unwrap_or("");
    if ua.contains("Mozilla/") {
        "browser".into()
    } else if ua.is_empty() {
        "unknown".into()
    } else {
        ua.split(['/', ' ']).next().unwrap_or(ua).chars().take(40).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("contraire-audit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn records_newest_first_and_filters() {
        let dir = temp_dir("filter");
        let a = Audit::open(dir.join("a.jsonl").to_str().unwrap(), |_| None);
        for i in 0..5 {
            a.record(&json!({ "n": i, "target": if i % 2 == 0 { "x" } else { "y" } }));
        }
        let all = a.recent(10, None);
        assert_eq!(all.len(), 5);
        assert_eq!(all[0]["n"], 4);
        let xs = a.recent(10, Some("x"));
        assert_eq!(xs.iter().map(|v| v["n"].as_i64().unwrap()).collect::<Vec<_>>(), [4, 2, 0]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rotates_and_still_reads_both_files() {
        let dir = temp_dir("rotate");
        let mut a = Audit::open(dir.join("a.jsonl").to_str().unwrap(), |_| None);
        a.max_bytes = 200;
        for i in 0..40 {
            a.record(&json!({ "n": i, "pad": "xxxxxxxxxxxxxxxxxxxx" }));
        }
        assert!(fs::metadata(dir.join("a.jsonl.1")).is_ok());
        assert!(fs::metadata(dir.join("a.jsonl")).unwrap().len() < 400);
        let recent = a.recent(3, None);
        assert_eq!(recent[0]["n"], 39);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn auto_and_off() {
        assert!(Audit::open("off", |_| None).path().is_none());
        assert!(Audit::open("auto", |_| None).path().is_none());
        let dir = temp_dir("auto");
        let d = dir.to_str().unwrap().to_string();
        let a = Audit::open("auto", |k| (k == "STATE_DIRECTORY").then(|| d.clone()));
        assert_eq!(a.path(), Some(dir.join("audit.jsonl").as_path()));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn classifies_clients() {
        assert_eq!(client_kind(Some("Mozilla/5.0 (X11)")), "browser");
        assert_eq!(client_kind(Some("curl/8.18.0")), "curl");
        assert_eq!(client_kind(None), "unknown");
    }
}
