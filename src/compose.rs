//! Compose awareness without the compose CLI: everything comes from the labels
//! `docker compose` puts on its containers. Project actions run in dependency waves
//! (from `com.docker.compose.depends_on`, compose ≥ 2.20): dependencies start first
//! and stop last; containers within a wave run in parallel.

use std::time::Duration;

use serde_json::{Value, json};

use crate::docker::Docker;
use crate::http::{Response, url_encode};

const PROJECT: &str = "com.docker.compose.project";
const SERVICE: &str = "com.docker.compose.service";
const DEPENDS_ON: &str = "com.docker.compose.depends_on";
const ONEOFF: &str = "com.docker.compose.oneoff";

struct Member {
    id: String,
    name: String,
    service: String,
    deps: Vec<String>,
}

pub fn project_action(docker: &Docker, project: &str, action: &str) -> std::io::Result<Response> {
    let filter = json!({ "label": [format!("{PROJECT}={project}")] }).to_string();
    let r = docker.get(&format!("/containers/json?all=1&filters={}", url_encode(&filter)))?;
    if r.status != 200 {
        return Ok(Response::json(r.status, r.body));
    }
    let list: Vec<Value> = serde_json::from_slice(&r.body).unwrap_or_default();
    let label = |c: &Value, k: &str| c.pointer(&format!("/Labels/{}", k.replace('/', "~1"))).and_then(Value::as_str).map(str::to_string);
    let members: Vec<Member> = list
        .iter()
        // `docker compose run` one-offs aren't part of the project's steady state.
        .filter(|c| label(c, ONEOFF).as_deref() != Some("True"))
        .map(|c| Member {
            id: c["Id"].as_str().unwrap_or_default().to_string(),
            name: c["Names"][0].as_str().unwrap_or_default().trim_start_matches('/').to_string(),
            service: label(c, SERVICE).unwrap_or_default(),
            deps: parse_depends_on(&label(c, DEPENDS_ON).unwrap_or_default()),
        })
        .collect();
    if members.is_empty() {
        return Ok(Response::error(404, &format!("no containers in project '{project}'")));
    }

    let waves = waves(&members);
    let mut results = Vec::new();
    match action {
        "start" => run_waves(docker, &members, waves.iter(), "start", &mut results),
        "stop" => run_waves(docker, &members, waves.iter().rev(), "stop", &mut results),
        "restart" => {
            run_waves(docker, &members, waves.iter().rev(), "stop", &mut results);
            results.clear();
            run_waves(docker, &members, waves.iter(), "start", &mut results);
        }
        _ => return Ok(Response::error(400, "unknown action")),
    }
    let failed: Vec<Value> = results
        .iter()
        .filter_map(|(name, err)| err.as_ref().map(|e| json!({ "name": name, "message": e })))
        .collect();
    let body = json!({ "project": project, "action": action, "count": members.len(), "failed": failed });
    Ok(Response::json(200, body.to_string()))
}

/// `"db:service_healthy:false,cache:service_started:true"` → `["db", "cache"]`
fn parse_depends_on(s: &str) -> Vec<String> {
    s.split(',').filter_map(|d| d.split(':').next()).filter(|d| !d.is_empty()).map(str::to_string).collect()
}

/// Group member indices into waves: wave n only depends on services in earlier waves.
/// Cycles and unknown dependencies don't block; leftovers go into a final wave.
fn waves(members: &[Member]) -> Vec<Vec<usize>> {
    let mut placed = vec![false; members.len()];
    let mut out: Vec<Vec<usize>> = Vec::new();
    let known = |svc: &str| members.iter().any(|m| m.service == svc);
    while placed.iter().any(|p| !p) {
        let wave: Vec<usize> = (0..members.len())
            .filter(|&i| !placed[i])
            .filter(|&i| {
                members[i].deps.iter().all(|d| {
                    !known(d) || members.iter().enumerate().all(|(j, m)| m.service != *d || placed[j])
                })
            })
            .collect();
        let wave = if wave.is_empty() { (0..members.len()).filter(|&i| !placed[i]).collect() } else { wave };
        for &i in &wave {
            placed[i] = true;
        }
        out.push(wave);
    }
    out
}

fn run_waves<'a>(
    docker: &Docker,
    members: &[Member],
    waves: impl Iterator<Item = &'a Vec<usize>>,
    action: &str,
    results: &mut Vec<(String, Option<String>)>,
) {
    for wave in waves {
        std::thread::scope(|s| {
            let handles: Vec<_> = wave
                .iter()
                .map(|&i| {
                    let m = &members[i];
                    s.spawn(move || {
                        let r = docker.call("POST", &format!("/containers/{}/{action}", m.id), None, Duration::from_secs(90));
                        let err = match r {
                            Ok(r) if r.status < 300 || r.status == 304 => None,
                            Ok(r) => Some(
                                serde_json::from_slice::<Value>(&r.body)
                                    .ok()
                                    .and_then(|v| v.get("message")?.as_str().map(str::to_string))
                                    .unwrap_or_else(|| format!("status {}", r.status)),
                            ),
                            Err(e) => Some(e.to_string()),
                        };
                        (m.name.clone(), err)
                    })
                })
                .collect();
            results.extend(handles.into_iter().filter_map(|h| h.join().ok()));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(service: &str, deps: &str) -> Member {
        Member { id: service.into(), name: service.into(), service: service.into(), deps: parse_depends_on(deps) }
    }

    #[test]
    fn dependency_waves() {
        let ms = [m("web", "api:service_started:false"), m("api", "db:service_healthy:false,cache:service_started:true"), m("db", ""), m("cache", "")];
        let w = waves(&ms);
        assert_eq!(w, vec![vec![2, 3], vec![1], vec![0]]);
    }

    #[test]
    fn cycles_and_unknown_deps_dont_hang() {
        let ms = [m("a", "b:x:y"), m("b", "a:x:y"), m("c", "ghost:x:y")];
        let w = waves(&ms);
        assert_eq!(w.concat().len(), 3);
    }
}
