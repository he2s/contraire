# mopsfraugibtdentonan

**A tiny, beautiful Docker dashboard for small machines.** One binary of about 1.2 MB, about 4 MB of memory plus 0.2 MB per running container, no dependencies on the host. It is built for a Raspberry Pi 3 and happy anywhere else. (The project and binary are called `contraire`; *mopsfraugibtdentonan* is its display name.)

- **Overview:** one page with everything that needs a look, each finding naming its source (Docker, incidents, telemetry, log patterns, host load), plus host CPU and memory, the busiest containers, errors in the logs, service health, recent incidents and events.
- **Live:** container states, health, CPU and memory stream in as they happen, pushed rather than polled.
- **Stacks:** Compose projects are grouped and colored, services are shown by name, and whole stacks start or stop in dependency order.
- **Logs:** followed live, with errors highlighted and timestamps on demand.
- **Images:** see what's in use; pull, remove and prune.
- **Learn:** eight illustrated lessons on installing, containers, images, Linux, networking, data, Compose and the Raspberry Pi, plus a Docker cheat sheet. They load from this repository, so they add nothing to the binary.
- **Copy buttons** for every command, ID and code sample.
- **Metrics:** CPU, memory, network and disk for every container and the host, read straight from cgroups and `/proc` every 10 s. The last hour is kept at 10 s, the last 24 h at 1 minute; the minute history is saved to disk and survives restarts. `/metrics` serves Prometheus format for an existing Grafana.
- **Log patterns:** every container's log, grouped live into patterns with counts (`GET /api/items/<*>` ×48,212), with errors and newly appearing patterns flagged.
- **Incidents:** a flight recorder writes a postmortem when a container crashes, runs out of memory or starts failing its health check: the reason, last log lines, error patterns, and CPU and memory just before. Crash loops count once.
- **Telemetry:** your own apps can report over OpenTelemetry (OTLP/HTTP, protobuf or JSON). Traces become requests per minute, error rate and p50/p95/p99 latency per service and operation, with the latest failed and slowest requests and their trace IDs. OTLP logs become patterns. One hour in fixed-size rings in memory, nothing on disk; `--otlp-forward` passes every batch on to another collector.
- **Activity:** an append-only audit log of every change made through the dashboard: who, when, from where, and whether it worked. It is bounded to about 2 MB on disk.

Everything kept on disk lives in one state folder (see `--audit-file`): the audit log (≤ 2 MB), API tokens (hashed), incident postmortems (≤ 100 files) and the minute metrics history (≤ 4 MB).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/he2s/contraire/main/install.sh | sh     # Linux and Raspberry Pi
```
```powershell
irm https://raw.githubusercontent.com/he2s/contraire/main/install.ps1 | iex         # Windows
```

Both scripts download the latest [release](https://github.com/he2s/contraire/releases) and verify its SHA-256 before installing. Step by step for each machine, including starting at boot: [Install and run](docs/learn/install-and-run.md).

## Run it

**Binary** (Linux amd64/arm64/armv7, Windows amd64):

```sh
./contraire                      # http://<host>:8083
./contraire --help               # every option, with its environment variable
```

**Docker:**

```sh
docker run -d --name contraire -p 8083:8083 \
  -v /var/run/docker.sock:/var/run/docker.sock contraire
```

**Compose:** `docker compose up -d` with the `compose.yaml` in this repository.

On Fedora/RHEL with SELinux enforcing, add `--security-opt label=disable` (or uncomment `security_opt` in `compose.yaml`); otherwise the container gets *Permission denied* on the socket.

> **Security:** whoever controls the Docker socket controls the machine. When the port is reachable from your network, set a password with `--password-file` or `CONTRAIRE_PASSWORD`. The server warns at startup if you don't.

## API access for scripts and agents

Create a token on the **Access** page, choosing a scope (`read`, `operate` or `admin`) and an optional expiry. Then:

```sh
curl -H "Authorization: Bearer $CONTRAIRE_TOKEN" http://pi.local:8083/api/containers
curl -X POST -H "Authorization: Bearer $CONTRAIRE_TOKEN" http://pi.local:8083/api/containers/web/restart
```

Every action is recorded in **Activity** under the token's name. Revoking a token takes effect immediately.

## From your workstation: CLI and MCP for Claude Code

All of it on one page: [**the cheat sheet**](docs/learn/cli-cheatsheet.md), also in the dashboard under **Cheat sheet**, with copy buttons and your own server address filled in.

The same binary is also the client. Point it at the server with a token from the **Access** page:

```sh
export CONTRAIRE_HOST=http://pi.local:8083 CONTRAIRE_TOKEN=ctr_...
contraire ps                      # all containers, plain-language status
contraire inspect web             # health streak, exit code explained, ports, mounts, compose info
contraire logs web -f             # follow live
contraire restart web
contraire top                     # CPU, memory, network and disk per container, busiest first
contraire patterns web            # its whole log as counted patterns, errors first
contraire incidents               # crashes and failing health checks the flight recorder caught
contraire stacks                  # Compose stacks and what needs attention
contraire events --since 30       # starts, stops with exit codes, OOM kills, health changes
contraire services                # Telemetry: apps reporting over OpenTelemetry, with requests, errors, latency
contraire export telemetry --csv  # the last hour per service and minute, for a spreadsheet or DuckDB
contraire init ops-pi             # a session folder for Claude Code: skill, reference, PROJECT.md, action log
contraire ps --help               # everything else (add --json for raw output)
```

**Claude Code:** register the built-in MCP server. Use an `operate` token, so the agent can restart things but never delete them:

```sh
claude mcp add contraire --env CONTRAIRE_TOKEN=ctr_... -- contraire mcp --host http://pi.local:8083
```

Claude then gets 17 tools: `list_containers`, `inspect_container`, `container_logs` (with grep), `container_stats`, `top_containers`, `container_metrics`, `log_patterns`, `incidents`, `service_health`, `list_stacks`, `recent_events`, `activity`, `host_info`, `start_container`, `restart_container`, `stop_container` and `stack_action`. Each change it makes appears in **Activity** under the token's name. The binary has no TLS, so for a Pi outside your LAN use an SSH tunnel (`ssh -L 8083:localhost:8083 pi`) or a VPN.

## Options

| Flag | Environment | Default |
|---|---|---|
| `-l, --listen` | `CONTRAIRE_LISTEN` | `0.0.0.0:8083` (a bare port also works) |
| `-H, --docker-host` | `DOCKER_HOST` | `unix:///var/run/docker.sock` (also `tcp://`, `npipe://`) |
| `-u, --user` | `CONTRAIRE_USER` | `admin` |
| `-p, --password-file` | `CONTRAIRE_PASSWORD_FILE` | none (or `CONTRAIRE_PASSWORD`) |
| `--docs-url` | `CONTRAIRE_DOCS_URL` | this repository's `docs/learn/` |
| `--audit-file` | `CONTRAIRE_AUDIT_FILE` | `auto`: `audit.jsonl` in `$STATE_DIRECTORY`, else `$XDG_STATE_HOME/contraire`, else `~/.local/state/contraire`; `off` disables it (and tokens, incidents and saved metrics with it) |
| `--otlp-forward` | `CONTRAIRE_OTLP_FORWARD` | none; e.g. `http://collector:4318` |
| `--max-connections` | `CONTRAIRE_MAX_CONNECTIONS` | `64` |

## Build

```sh
cargo build --release            # compiles on a Raspberry Pi 3 too
scripts/dist.sh                  # all platforms into ./dist, cross-compiled in Docker
docker build -t contraire .      # FROM scratch image; add --platform linux/arm64 for a Pi
```

## Measure

```sh
cargo test --release -- --ignored bench_ --test-threads=1 --nocapture
scripts/footprint.sh --perf
```

`footprint.sh` checks binary size, idle memory, idle CPU, memory under load, and thread cleanup against the budgets in [ROADMAP.md](ROADMAP.md): binary 1.25 MB, idle memory 4 MB + 256 KB per followed container, idle CPU 0.5%, peak memory 16 MB, no leaked threads. It exits non-zero if any budget is exceeded.

## Where it's going

See [ROADMAP.md](ROADMAP.md) for what's next (scriptable actions, infrastructure pull requests, a fleet of hosts) and [KILLER_FEATURES.md](KILLER_FEATURES.md) for the ideas behind it.
