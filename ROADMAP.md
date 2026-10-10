# Roadmap

Where contraire is headed. The priorities in `CLAUDE.md` still come first: memory, then CPU, then disk. These goals are **beautiful AND low footprint**, never one at the expense of the other.

## 1. Beautiful for four very different viewers

| Viewer | What wins them over | What it costs |
|---|---|---|
| Non-technical person | Plain language: "Stopped unexpectedly" instead of "Exited (137)". Friendly empty states, and a hover explanation on every technical term. | Text only |
| Web designer | A strict type scale, spacing grid and motion system, with no stray one-off styles. Archivo and IBM Plex Mono, loaded from Google Fonts without blocking the first render (self-hosting a subset is still open). | 0 in the binary |
| Artist | A visual signature: each Compose stack gets its own generated hue, and an optional "constellation" view of containers and their networks drawn as inline SVG. | A few KB of JS |
| Executive skeptical of AI | Calm and trustworthy, with no AI hype in the UI. An audit trail showing what changed, when, who did it, and whether a person or an agent did it. Undo where possible. | A small append-only log file |

The audit trail is the most important item. It is what makes agent-driven management acceptable to a skeptic. **Done:** Activity (audit log); plain-language status, the Overview's sourced findings and "?" help on every part of the UI.

## 2. Remote control: MCP and CLI, with one contract

- **The HTTP JSON API stays the single contract.** The managed host (the Pi) only ever serves that API.
- **Done: workstation client in the same binary.**
  - `contraire ps|logs|restart|… --host http://pi.local:8083` for people
  - `contraire mcp --host http://pi.local:8083` exposes the same operations as 17 MCP tools over stdio for coding agents; MCP runs on the developer's machine, so the Pi stays lean
  - `contraire init` creates a session folder with the Claude Code skill, `reference.md`, `PROJECT.md`, an action log and a journal
  - A separate client binary (with clap, completions, color) was dropped: one hand-written parser serves both sides.
- **Transport:**
  - No TLS on the Pi: rustls would add ~1 MB.
  - Still to do: `--host ssh://pi` tunnelling. Until then use `ssh -L`, Tailscale or Caddy.
- **Done: API tokens** (`read` < `operate` < `admin`), one per person or agent, so the audit log attributes every action. Basic auth remains for the browser.

## 3. Accessibility, in both senses

**Usable by everyone:**
- full keyboard navigation and visible focus rings
- ARIA live regions for toasts
- WCAG AA contrast
- respect for `prefers-reduced-motion`

**Approachable for learners:**
- **Learn page in the web UI:**
  - a cheat sheet and worked examples
  - background on how containers work on Linux (namespaces, cgroups, image layers, volumes, ports, networking), explained with inline SVG diagrams
- **Copy buttons:** on every code block and instruction, in the UI and in the docs.
- **Man page:** generated from the `OPTS` table in `src/cli.rs`, so it never drifts from `--help`. Plus a printable cheat sheet.
- **Footprint rule: large text never goes into the binary.** Lessons, docs and images live in the repository (`docs/`). The browser fetches them from GitHub at runtime (`--docs-url`, mirrorable). The binary only carries the small shell that renders them.
  - **Done:** Learn page with eight lessons and a Docker cheat sheet in `docs/learn/`, with quizzes and live blocks; the Cheat sheet page for the CLI, skill and MCP; copy buttons everywhere. Still to do: the man page.

## 4. Extensibility: actions (scripts), carefully

Access to the Docker socket is root-equivalent, so running bash written by an agent is the riskiest feature here.

- **Actions are script files in a directory** (e.g. `/etc/contraire/actions/*.sh`). A header comment in each declares its name, description and parameters.
- **Running them:**
  - they are listed in the UI, the CLI and MCP
  - output streams live, with a timeout
  - every run goes to the audit log
- **The API can run actions but never create them.** Uploading an agent-written script needs an opt-in server flag (`--allow-action-upload`) plus a person approving it in the UI.
- **No embedded interpreter** (Lua, Rhai) to start with. bash is already on the host.

## 5. Observability

These follow the principles in `CLAUDE.md`: streaming, bounded memory, append-only files and columnar layouts.

**Done:**
- node and container metrics from `/proc` and cgroup files every 10 s (far cheaper than Docker's stats API), 1 h at 10 s and 24 h at 1 min, the minute history saved to `metrics.bin` so it survives restarts
- Prometheus `/metrics` endpoint
- log collection with live pattern mining, and the crash flight recorder (Incidents)
- OTLP/HTTP endpoint: traces become requests, errors and latency per service and operation, with exemplars; OTLP logs become patterns; `--otlp-forward`
- JSON lines and CSV export of metrics and telemetry
- the Overview and drill-down pages

**Still to do:**
- **Server-side cache of `/containers/json`, shared by all tabs.** Measured: Docker needs ~100 ms to list 30 containers, while the server alone answers ~12k req/s. Every open tab currently triggers its own Docker call on each event. Swap snapshots atomically, without locks.
- telemetry on the Prometheus endpoint, and a postmortem when a health check starts failing
- columnar files (Parquet) for history beyond 24 h

## 6. Budgets (enforced on every automated build)

**Done:** `scripts/footprint.sh` checks these against a live daemon and exits non-zero on a breach; `bench_*` tests report ns/op and allocs/op. The release workflow enforces the binary budget on every build; the others need a Docker daemon and run by hand.

| Budget | Limit |
|---|---|
| Server binary | < 1.25 MB (raised from 1 MB on 2026-10-10 for metrics, log patterns and built-in help: disk ranks below memory and CPU, and explanations are not cut for size) |
| Memory at idle | < 4 MB + 256 KB per followed container |
| Peak memory | < 16 MB |
| CPU at idle, one open tab | < 0.5% |
| Threads after tabs close | back to the idle count |
| Web page, compressed | < 120 KB |
| JS frameworks | none |
| Runtime dependencies | each one must justify itself on the Pi |

Beauty comes from typography, spacing and motion, which are cheap in bytes. Size comes from frameworks and fonts, and both are capped here.

## Suggested order

1. **Audit log and API tokens.** Done: `src/audit.rs`, `src/tokens.rs`, the Activity and Access pages, scopes `read`/`operate`/`admin`. Still to do: per-container and per-stack token limits (KILLER_FEATURES #7).
2. **Client and MCP server.** Done: `contraire <command>`, `contraire mcp` and `contraire init`, same binary. Still to do: `--host ssh://` tunnelling, push-native MCP resources (KILLER_FEATURES #8), shell completions.
3. **Learn, cheat sheet and copy buttons.** Done. Still to do: the man page.
4. **Actions.** Not started.
5. **Observability.** Mostly done (section 5).
