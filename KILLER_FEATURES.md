# Killer features

Ideas where this project can do something genuinely new, not just a smaller Portainer. They all follow one principle:

> **Thin edge, thick brain.** The managed host (a Pi with 1 GB of RAM) stays tiny and deterministic. It observes, compresses and executes. The intelligence (Claude, Claude Code, MCP clients) runs on the developer's workstation and talks to the edge through a narrow, audited contract.

Most AI-ops tools put a model or a heavy agent next to the workload. Here the edge stays under 10 MB of memory (about 7 MB with 28 containers) and *prepares* the world for an AI: compact, token-budgeted, verifiable. Every action is gated by a person.

## At a glance

| # | Feature | New because | Edge cost | Effort | Status |
|---|---|---|---|---|---|
| 1 | Token-budgeted context endpoint | Observability designed for LLM context windows | ~0 | S | idea |
| 2 | Log template mining at the edge | A million lines become 30 patterns, on a Pi, with no allocations | ~1 MB | M | **shipped** |
| 3 | Incident flight recorder | Postmortems survive `docker rm` and reboots | bounded file, e.g. 8 MB | M | **shipped** (as files, not mmap; health-check trigger open) |
| 4 | Infrastructure pull requests | Agents propose, humans approve a diff with its blast radius | ~0 | M | idea |
| 5 | Closed-loop deploys with Claude Code | The agent deploys, watches, and rolls back in one session | 0 on the edge | M | partly (skill: change and verify) |
| 6 | Discovered vs. declared dependencies | Real traffic compared with `depends_on` | small | M | idea |
| 7 | Capability-scoped agent tokens | Least privilege for AI, per container and per verb | ~0 | S | partly (scopes; per-container limits open) |
| 8 | Push-native MCP | Agents subscribe to changes instead of polling | 0 on the edge | S | idea |
| 9 | Plain-language status, no AI | A narrative a skeptic can trust: rules, not guesses | ~0 | S | partly (Overview findings) |
| 10 | Live lessons | The docs explain *your* containers | 0 (docs on GitHub) | S | partly (live blocks) |
| 11 | An OpenTelemetry edge backend in a few MB | OTLP in, columnar files out, on a Pi | bounded | L | **shipped** in memory (Parquet open) |
| 12 | Watts per container | Energy cost an executive can read | ~0 | S | idea |
| 13 | Serverless fleet | Several Pis, one UI, no central server | ~0 | M | idea |

## 1. Token-budgeted context endpoint

`GET /api/context?budget=4000&focus=cart` returns the most useful description of the system that fits in N tokens:
- states and recent transitions
- containers that died, and why
- top log templates (see #2)
- resource anomalies
- compose topology

It is ranked by relevance and truncated deterministically.

- **Why it's new:** monitoring tools optimize for screens and humans. This optimizes for a model's context window, which is the scarce resource once an agent is in the loop. "Summarize this machine for me" costs 4k tokens instead of 400k lines of logs.
- **Fits here:** it's pure data reduction done where the data lives, and that is the edge's job.

## 2. Log template mining at the edge

**Status: shipped** (`logwatch.rs`, the Log patterns tab, `contraire patterns`): about 21 MB/s per core with no allocations for known patterns.

The server learns templates from streamed logs as it reads them, in the style of the Drain algorithm. For example, `GET /api/items <*> <*>ms user=<*>` is one template seen 48,212 times, while `panic: <*>` is a **new** template, first seen 12:04:31.
- Tokenizing reuses buffers; templates live in a fixed-size, contiguous table.
- It shows up in the UI as "3 new kinds of messages since the last deploy."

- **Why it's new:** template mining normally lives in central log platforms. Doing it at the edge with no allocations turns the log firehose into a few hundred bytes per minute. That makes #1, #3 and #5 cheap, and it's a measurable showcase for the project's principles. The log-relay benchmark already runs at 789 MB/s with zero allocations, so the budget is there.

## 3. Incident flight recorder

**Status: shipped** (`logwatch.rs`, the Incidents page, `contraire incidents`). Postmortems are JSON files in the state folder rather than one memory-mapped ring, and the minute metrics history survives restarts. Still open: a postmortem when a health check starts failing, Parquet output.

Each container gets a fixed-size ring buffer in one memory-mapped file: events, per-second stats, the log tail and template counts. When a container dies, OOMs or fails a health check, the window around the event is frozen into a compact postmortem. Optionally it's written as Parquet.

- **Why it's new:** `docker logs` is gone after `docker rm`, and `docker stats` keeps no history. Postmortems on small hosts usually don't exist at all. This one survives removal and reboots, is bounded by construction (never fills the SD card), and is exactly what an agent needs to answer "why did it die at 3 a.m.?"

## 4. Infrastructure pull requests

Agents never act directly. Every change an agent wants (restart a project, pull a new image, run a script) becomes a **plan**:
- what will happen
- in which order (from compose `depends_on`)
- what else is affected (the blast radius)
- how to undo it

The plan appears in the UI as a reviewable card. A person approves or rejects it, and the outcome is recorded in the audit log, attributed to agent and person.

- **Why it's new:** "AI with root on your server" is the nightmare of the skeptical executive. "AI opens a pull request against your server" is a workflow they already trust. This makes agent operations reviewable, reversible and attributable.

## 5. Closed-loop deploys with Claude Code

On the workstation, `contraire mcp --host http://pi.local:8083` gives Claude Code tools, and a hook links the session to the edge's live stream. When the agent runs `docker compose up -d`:
1. It watches the rollout in real time (health, restarts, new log templates).
2. It compares against the pre-deploy baseline from #3.
3. It reports success, or proposes a rollback as an infrastructure PR (#4).

- **Why it's new:** today, coding agents "deploy and hope", because they can't see production. This closes the loop from editing code to a verified deploy inside one agent session, with a human approving the risky step.

## 6. Discovered vs. declared dependencies

The edge reads `/proc/net/nf_conntrack` (or per-container `/proc/<pid>/net/tcp`) to see who actually talks to whom. It draws the result as a live "constellation" and compares it with compose `depends_on`:
- *"api talks to redis, but doesn't declare it: a restart-order bug waiting to happen."*
- *"web declares db but never talked to it in 7 days."*

- **Why it's new:** service maps normally need a mesh, eBPF or agents in every container. Reading conntrack is free on stock Linux, even on a Pi 3, and comparing reality with the compose file is a finding no other lightweight tool produces.

## 7. Capability-scoped agent tokens

Each token names its holder ("claude@henning-laptop") and its powers. For example: `logs:read on project shop`, `restart on service cart`, `actions: backup-db only`, expiring in 8 hours. The UI shows live which agent holds which power, and lets you revoke it with one click.

- **Why it's new:** MCP servers today are typically all-or-nothing. Least privilege for AI agents, enforced at the edge and visible to people, is the missing piece for letting agents near real systems.

## 8. Push-native MCP

The MCP server exposes containers, stacks, incidents and log templates as **subscribable resources**, backed by the edge's existing SSE stream. Agents get `resources/updated` notifications instead of polling.

- **Why it's new:** most MCP integrations are request/response snapshots. This system is push-first all the way down: Docker events, then the edge, then SSE, then MCP notifications, then the agent. An agent can genuinely *keep watch* at near-zero cost.

## 9. Plain-language status, no AI

**Status: partly shipped:** the Overview lists findings from rules, each naming its source and linking to its evidence.

A deterministic narrator turns state into sentences:
- *"Everything is running. cart restarted 3 times in the last hour because it ran out of memory (limit 256 MB). Its memory has grown 4% per hour since yesterday's deploy."*

It is built from rules and thresholds, and every sentence links to its evidence.

- **Why it's new:** this is honest automation for the skeptical executive and the non-technical user. It's readable like an AI summary, but reproducible and never wrong in the way a model can be. Claude can optionally elaborate on top. The baseline needs no model at all.

## 10. Live lessons

Lessons come from GitHub but are *bound to this host*. The ports lesson highlights your real published ports. The data lesson lists your real volumes and flags ones nothing uses. The compose lesson draws your actual stacks. A "try it" button runs the example as an infrastructure PR (#4).

- **Why it's new:** docs and tools are always separate worlds. Here the curriculum costs zero bytes in the binary and teaches with the learner's own system as the example.

## 11. An OpenTelemetry edge backend in a few MB

**Status: shipped in memory** (`otlp.rs`, the Telemetry page, `contraire services`): OTLP/HTTP traces and logs, one hour per service in fixed rings, `--otlp-forward`, CSV/JSON lines export. Still open: rolling up into Parquet, and telemetry on `/metrics`.

The edge accepts OTLP (metrics, logs, traces) from local apps:
- **bounded:** it keeps fixed-size ring buffers
- **columnar:** it rolls data up into Parquet files
- **ships out:** it forwards them later (S3, a NAS, or the workstation)

The same data also feeds the Prometheus `/metrics` endpoint.

- **Why it's new:** OTel collectors and backends are tens to hundreds of MB. An edge-first, bounded, columnar OTel sink that fits a Pi 3 makes real observability possible where it's usually skipped. The `opentelemetry-demo` in this workspace is the perfect test workload.

## 12. Watts per container

Each container's CPU share, multiplied by a per-board power model (Pi 3, Pi 4, Pi 5, or a measured USB power meter), gives watts, kWh per month, and cost per month for each container and project.

- **Why it's new:** an executive won't read CPU percentages, but they will read "this side project costs €0.80 a month and 3% of the board's energy." It's cheap to compute and almost nobody shows it.

## 13. Serverless fleet

Several hosts find each other on the LAN (mDNS) or through a static list. Any one UI shows all of them, and each host still serves only its own data. There's no central database and no coordinator. If the "primary" dies, any other host is the primary.

- **Why it's new:** fleet management usually means a control plane bigger than the fleet. This scales by adding Pis, not servers.

## Guardrails that make all of this credible

- **Edge stays deterministic.** No model runs on the managed host. AI output never executes without a plan (#4) and a scoped token (#7).
- **Every byte is bounded.** Ring buffers, fixed tables, bounded channels. `scripts/footprint.sh` budgets apply to every feature above.
- **Everything is attributable.** Who (human or agent), what, when, why, and how to undo it.

## Suggested first three

1. **#2 log templates** (done). It's the engine behind #1, #3, #5 and #9, and a showcase for the no-allocation design.
2. **#7 scoped tokens + #4 plans.** These are the trust foundation that makes the agent features acceptable.
3. **#5 closed-loop deploys.** It's the demo that makes people say "I want that."
