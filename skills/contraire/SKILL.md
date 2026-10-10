---
name: contraire
description: Inspect, diagnose and operate Docker containers on a (remote) host through the `contraire` CLI. Use this skill whenever the user asks what is running, broken, unhealthy, crashing, restarting or slow on their Docker host, Raspberry Pi, homelab or server; wants container logs explained; asks "why did X die / keep restarting"; wants a container or compose stack started, stopped or restarted and verified; or asks what changed recently. Use it even if they say "docker", "my Pi", "the stack" or a service name rather than "contraire", as long as a contraire server is configured (CONTRAIRE_HOST) or the repo/binary is present.
---

# Operating a Docker host with contraire

`contraire` is a tiny Docker dashboard. The same binary is also a client: run on the user's machine, it talks to the dashboard's API. Everything you do through it uses the user's API token, respects its scope (`read` < `operate` < `admin`), and lands in the dashboard's audit log under the token's name. That accountability is the reason to prefer it over raw `docker` commands, even when `docker` is available.

## Before the first command

1. **Find the binary.** Try `command -v contraire`. If it's not on PATH and you're in the contraire repo, use `./target/release/contraire` (build it with `cargo build --release` if it's missing) or a binary in `./dist/`.
2. **Check the connection** with `contraire info`. It shows the host, Docker version and the server's own footprint. `CONTRAIRE_HOST` sets the server (default `http://127.0.0.1:8083`).
3. **Tokens stay in the environment.** The CLI reads `CONTRAIRE_TOKEN` on its own. Don't pass `--token`, and don't echo, print, `cat` or `env | grep` the token. Command lines end up in shell history, process lists and this transcript. If a call fails with `not authorized` and no token is set, ask the user to `export CONTRAIRE_TOKEN=…` themselves.
4. **Scope errors are answers, not obstacles.** `token scope 'read' does not allow this (needs 'operate')` means the user deliberately limited this token. Report it and say which scope is needed. Don't look for another route around it, such as running `docker` directly.

## In a session folder

If the working directory (or a parent) contains `.contraire/session.json`, the user made it with `contraire init` to keep a record:

- The host and token come from `.contraire/`, so you don't need to export anything. The token file is git-ignored; leave it alone.
- Every `contraire` command you run is appended automatically to `.contraire/actions.log`. You don't write to it.
- **`SESSION.md` is yours to keep.** After each task, append a dated entry so a person can see later what happened without reading the transcript:

  ```markdown
  ## 2026-10-10 14:20 · <short title of the task>
  - Checked: <what you looked at>
  - Found: <cause, with the decisive evidence>
  - Changed: <exact actions, or "nothing">
  - Next: <recommendation, if any>
  ```

  Write it even if you changed nothing; "looked, all healthy" is useful history. Get the time with `date -u +"%Y-%m-%d %H:%M"`.

## Look before you touch

Start read-only. These are cheap and safe:

```sh
contraire ps                         # everything, with plain-language status
contraire stacks                     # compose stacks and which services need attention
contraire inspect <name>             # state, health streak, exit code explained, restarts, ports, mounts, depends_on
contraire logs <name> -n 200         # recent output (stdout + stderr)
contraire events --since 60          # starts, stops with exit codes, OOM kills, health changes
contraire activity -n 30             # who changed what through the dashboard (admin token)
```

Add `--json` when you need to filter precisely (pipe to `jq`). The plain output is usually enough and costs fewer tokens.

## Workflow: something is broken

The status column already sorts problems for you: `unhealthy`, `failed (<code>)` and `restarting` need attention. `exited` usually means it was stopped on purpose (exit 0).

1. `contraire ps`. List the containers that need attention. If there are many, start with the ones others depend on, such as databases, brokers and collectors. Compose `depends_on` in `inspect` tells you this. A broken dependency often explains several failing dependents at once.
2. `contraire inspect <name>` for each of them. The exit code comes with a plain-language hint: 137 means out of memory or force-stopped, 1 means an app error, 127 means command not found. Also note `restarts`, the health streak, `oom` and the memory limit.
3. Read logs **narrowly**. Logs are the most expensive thing you can read:
   ```sh
   contraire logs <name> -n 300 | grep -iE 'error|fatal|panic|exception|refused|denied|timeout|no such|killed|oom' | tail -40
   contraire logs <name> -n 60          # then the last lines before the crash, unfiltered
   ```
   Use `-t` when timing matters, e.g. to line errors up with a restart from `events`. For a container in a restart loop, the cause is usually in the first lines after each start, not the last ones.
4. `contraire events --since 60` shows whether it's a loop (repeated start/stop with the same exit code), an OOM kill, or a single crash.
5. Report (format below). Suggest the fix; don't apply it unless asked.

## Workflow: change something

Stop, start, restart, pause and stack actions change a running system that other people may rely on.

- **Act only on what the user named.** "Restart frontend-proxy" authorises exactly that. If your diagnosis says *another* service should be restarted, or a whole stack, say so and ask first. The user may know about things you don't: a migration running, someone mid-demo.
- **Prefer the smallest action.** Restart one service rather than the stack, and restart rather than stop.
- **Record "before":** `contraire inspect <name>`. Note state, health and restart count.
- **Act:** `contraire restart <name>` (or `start`, `stop`, `pause`, `unpause`), or `contraire stack <stack> restart`. Stack actions run in `depends_on` order: dependencies start first and stop last.
- **Verify, don't assume.** A command returning "done" only means Docker accepted it.
  ```sh
  for i in $(seq 12); do contraire ps | grep -E '^<name> '; sleep 5; done   # up to a minute
  contraire inspect <name>                 # running? health "healthy"? restart count stable?
  contraire logs <name> -n 100 | grep -iE 'error|fatal|panic|exception' | tail -20
  ```
  Stop polling as soon as it's healthy. If the container has no health check, "running for 30+ s, restart count stable, no new errors" is the bar.
- Report what changed, with the before and after state. If it didn't recover, say so plainly and show the evidence.

Don't use `logs -f` without a bound. It never returns on its own, so wrap it: `timeout 15 contraire logs <name> -f`.

## Workflow: what changed?

`contraire events --since <minutes>` shows what Docker saw. `contraire activity` shows who did it through the dashboard: a person, or a token such as `claude-laptop`. Combine them to tell "it crashed on its own" apart from "someone restarted it". Changes made with plain `docker` on the host appear in events but not in activity.

## Reporting

Keep it short and evidence-first. The user wants to know what's wrong, why, and what to do:

```
**<one-line verdict>** (e.g. "2 of 29 containers need attention; both trace back to kafka")

- **<name>**: <status>. <cause in plain words>.
  Evidence: <exit code + hint, or the decisive log line, quoted, or the event>
  Next: <the one command or change you'd make> (not done yet)
```

Quote at most a few log lines; the decisive one beats a wall of output. Say when you're inferring rather than reading it directly from evidence.

## Command reference

| Command | Does | Scope |
|---|---|---|
| `contraire ps` / `stacks` / `info` | Overview | read |
| `contraire inspect <c>` | Details, exit-code hint, depends_on | read |
| `contraire logs <c> [-n N] [-t] [-f]` | Logs (stdout+stderr) | read |
| `contraire events [--since MIN]` | Docker events | read |
| `contraire start\|stop\|restart\|pause\|unpause <c>` | One container | operate |
| `contraire stack <name> start\|stop\|restart` | Whole stack, dependency order | operate |
| `contraire activity [-n N]` | Audit log | admin |

There is deliberately no remove, prune or pull command. Those belong in the dashboard, with a person clicking.
