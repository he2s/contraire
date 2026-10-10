# Roadmap

Where contraire is headed. The priorities in `CLAUDE.md` still come first: memory, then CPU, then disk. These goals are **beautiful AND low footprint**, never one at the expense of the other.

## 1. Beautiful for four very different viewers

| Viewer | What wins them over | What it costs |
|---|---|---|
| Non-technical person | Plain language: "Stopped unexpectedly" instead of "Exited (137)". Friendly empty states, and a hover explanation on every technical term. | Text only |
| Web designer | A strict type scale, spacing grid and motion system, with no stray one-off styles. One self-hosted variable font, cut down to the characters actually used. | ~25–35 KB |
| Artist | A visual signature: each compose project gets its own generated hue, and an optional "constellation" view of containers and their networks drawn as inline SVG. | A few KB of JS |
| Executive sceptical of AI | Calm and trustworthy, with no AI hype in the UI. An audit trail showing what changed, when, who did it, and whether a person or an agent did it. Undo where possible. | A small append-only log file |

The audit trail is the most important item. It is what makes agent-driven management acceptable to a sceptic.

## 2. Remote control: MCP and CLI, with one contract

- **The HTTP JSON API stays the single contract.** The managed host (the Pi) only ever serves that API.
- **Workstation client:**
  - `contraire ps|logs|restart|… --host pi.local` for people
  - `contraire mcp --host pi.local`, which exposes the same operations as MCP tools over stdio for coding agents
  - MCP runs on the developer's machine, so the Pi stays lean.
- **Split into two binaries in a Cargo workspace:**
  - **Server:** runs on the Pi. It keeps the hand-written CLI and minimal dependencies.
  - **Client:** runs on the workstation. clap, shell completions and colour output are fine there.
- **Transport:**
  - No TLS on the Pi: rustls would add ~1 MB.
  - The client can tunnel over SSH (`--host ssh://pi`), so nothing needs to be exposed on the network.
  - Otherwise, put it behind Tailscale or Caddy.
- **Auth:** replace Basic auth with API tokens, one per person or agent, so the audit log can attribute every action.

## 3. Accessibility, in both senses

**Usable by everyone:**
- full keyboard navigation and visible focus rings
- ARIA live regions for toasts
- WCAG AA contrast
- respect for `prefers-reduced-motion`

**Approachable for learners:**
- **"Learn" section in the web UI:**
  - a cheat sheet and worked examples
  - background on how containers work on Linux (namespaces, cgroups, image layers, volumes, ports, networking), explained with inline SVG diagrams
- **Copy buttons:** on every code block and instruction, in the UI and in the docs.
- **Man page:** generated from the `OPTS` table in `src/cli.rs`, so it never drifts from `--help`. Plus a printable cheat sheet.
- **Footprint rule: large text never goes into the binary.** Lessons, docs and images live in the repository (`docs/`). The browser fetches them from GitHub at runtime (`--docs-url`, mirrorable). The binary only carries the small shell that renders them.
  - **Done:** Learn tab with eight lessons in `docs/learn/`.

## 4. Extensibility: actions (scripts), carefully

Access to the Docker socket is root-equivalent, so running bash written by an agent is the riskiest feature here.

- **Actions are script files in a directory** (e.g. `/etc/contraire/actions/*.sh`). A header comment in each declares its name, description and parameters.
- **Running them:**
  - they are listed in the UI, the CLI and MCP
  - output streams live, with a timeout
  - every run goes to the audit log
- **The API can run actions but never create them.** Uploading an agent-written script needs an opt-in server flag (`--allow-action-upload`) plus a person approving it in the UI.
- **No embedded interpreter** (Lua, Rhai) to start with. bash is already on the host.

## 5. Observability extensions

These are listed in `CLAUDE.md` and follow its principles: streaming, bounded memory, append-only files and columnar layouts.

- **Server-side cache of `/containers/json`, shared by all tabs.** Measured: Docker needs ~100 ms to list 30 containers, while the server alone answers ~12k req/s. Every open tab currently triggers its own Docker call on each event. Swap snapshots atomically, without locks.
- Prometheus `/metrics` endpoint
- OTLP endpoint
- log collection
- node metrics from `/proc` and `/sys/fs/cgroup`
  - reading cgroup files directly is far cheaper than Docker's stats API
  - this enables per-row CPU and memory in the container list
- storage in files or a columnar store: Parquet, or polars if its weight is acceptable
- highly interactive drill-down UI

## 6. Budgets (enforced on every automated build)

**Done:** `scripts/footprint.sh` checks these against a live daemon and exits non-zero on a breach. `bench_*` tests report ns/op and allocs/op. Not yet wired into an automated build.

| Budget | Limit |
|---|---|
| Server binary | < 1 MB |
| Memory at idle | < 5 MB |
| Web page, compressed | < 120 KB |
| JS frameworks | none |
| Runtime dependencies | each one must justify itself on the Pi |

Beauty comes from typography, spacing and motion, which are cheap in bytes. Size comes from frameworks and fonts, and both are capped here.

## Suggested order

1. **Audit log** (done: `src/audit.rs`, Activity page, per-container history) **and API tokens** (next), the foundation for trust and for agent access.
2. **Client binary and MCP server.**
3. **Learn section, man page, cheat sheet and copy buttons.**
4. **Actions.**
5. **Observability extensions.**
