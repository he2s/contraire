# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

contraire is a single-binary Docker manager with a web UI. It is built to run on, and **compile on**, a Raspberry Pi 3 (1 GB RAM, 4× Cortex-A53, SD card). It runs fine on the Pi and on anything bigger.

## Priorities (in this order)

1. **Low memory footprint.** RSS stays in the low single-digit MB at idle, and the 1 GB Pi has to be able to compile the crate.
2. **Low CPU.** Idle work stays close to zero, and polling is driven by the browser only while a tab is visible.
3. **Small disk.** Keep both the binary (~450 KB today) and the dependency tree small.
4. **Beautiful UI.** It must stay polished, not "good enough for an admin tool". Both light and dark themes, smooth transitions, no jank.

When there is a choice, take the one that does this:
- **No allocations on hot paths.** Reuse buffers, borrow `&str`/`&[u8]` slices out of the input, and parse in place rather than building owned trees when only a few fields are needed.
- **No locks.** Prefer atomics, per-thread state, or single-writer designs. The only shared state today is an `AtomicUsize` connection counter.
- **Cache-friendly data.** Use struct-of-arrays and fixed-capacity ring buffers for time series, and contiguous `Vec`s over maps and linked structures.
- **Vectorize where volume makes it matter**, e.g. scanning log bytes with `memchr`, or columnar aggregation.
- A new dependency must justify its compile time and memory on the Pi. A full async stack (tokio/hyper/axum) was rejected on purpose: thread-per-connection is enough for a LAN dashboard. Currently the only direct dependency is `serde_json`, with no proc-macro `serde_derive`.

These principles also apply to the planned extensions:
- Prometheus `/metrics`
- an OTLP endpoint (done: `otlp.rs`)
- log collection
- node metrics from `/proc` and `/sys/fs/cgroup`
- storage to files or a columnar store (Parquet, or polars if its weight is acceptable)
- a highly interactive drill-down UI

For high-volume data (log lines, events, samples), design for streaming, bounded memory, append-only files and columnar layouts from the start.

See `ROADMAP.md` for planned features (audit log, API tokens, workstation client and MCP server, Learn section and man page, script actions) and the size and memory budgets. Build toward it.

## Commands

```sh
cargo build --release                         # size-optimized: opt-level=s, LTO, strip, panic=abort
cargo test                                    # unit tests (parsers, base64, log demux)
cargo test demuxes_log_frames                 # a single test, by name substring
cargo clippy --release
cargo check --release --target aarch64-unknown-linux-gnu        # Pi 3, 64-bit OS
cargo check --release --target armv7-unknown-linux-gnueabihf    # Pi 3, 32-bit Raspberry Pi OS
./target/release/contraire -l 127.0.0.1:18080      # run locally (needs docker socket access); --help lists all options
cargo test --release -- --ignored bench_ --test-threads=1 --nocapture   # micro-benchmarks: ns/op, allocs/op, MB/s
scripts/footprint.sh [--quick] [--perf]       # size/RSS/CPU/threads vs. budgets against the real daemon; non-zero on breach
scripts/dist.sh                               # cross-compile linux amd64/arm64/armv7 + windows into ./dist (Docker + zig)
docker build -t contraire .                   # FROM scratch runtime image (~600 KB); --platform linux/arm64 for a Pi
```

**Performance work:**
- Run the micro-benchmarks and `scripts/footprint.sh` before and after every change, and quote the numbers.
- Benchmarks are `#[ignore]` tests named `bench_*`, next to the code they measure.
- `src/bench.rs` installs a counting allocator in test builds.
- Hot paths must stay at 0 allocs/op: relaying log bytes, dechunking, and the SSE write loop. Per-event JSON parsing (events, stats) is allowed to allocate because it runs at most once per second per stream.
- `/api/containers` speed is bounded by Docker itself, at about 100 ms for 30 containers.

Default listen address: `0.0.0.0:8083`.

**CLI:** every option is defined in the `OPTS` table in `src/cli.rs`, which drives parsing, `--help` output and typo suggestions.
- Each option also has an environment variable. Precedence is flag > env > default.
- The password is read from `--password-file` or `CONTRAIRE_PASSWORD` only, never from a flag, because argv is visible in `ps`.
- `parse` takes the environment as a closure, so tests never touch process state.
- The parser is hand-written on purpose. clap was measured at +193–218 KB of binary and about 3× the clean build time (19 crates including `syn` with derive), which is too much for the Pi. Keep new options in the table rather than reaching for clap.

**UI check:** the connected Chrome extension may run on a different machine, so `127.0.0.1` there is not this host. Use `scripts/shot.mjs` instead. It drives local headless Chrome over the DevTools protocol, waits a fixed time, then saves a screenshot. It exits non-zero on console errors, and `--eval` prints a JS expression's value.

Plain `chrome --screenshot` doesn't work here: the page holds an SSE stream open forever, so Chrome never sees the page as finished loading.

```sh
node scripts/shot.mjs "http://127.0.0.1:18080/#c/<container-id>" out.png [--dark] [--height=1400] [--eval='document.title']
```

URL hashes:
- `#c/<container-id>` opens a container's detail drawer.
- `#learn/<lesson>` opens a lesson.

To test the Learn tab before its content is pushed, serve `docs/learn/` locally with CORS and pass `--docs-url http://127.0.0.1:<port>/`.

## Architecture

The binary has six Rust modules plus one embedded page. Long-form content lives on GitHub.

- **Naming:** `DISPLAY_NAME` (`src/main.rs`) is the name people see, currently "mopsfraugibtdentonan". The crate, binary and repository stay `contraire`. The UI gets the display name from `/api/config`, so never hard-code it in the page.
- **`src/http.rs`:** a hand-rolled HTTP/1.1 server.
  - Keep-alive, Content-Length bodies only, and hard caps on line, header and body size.
  - Every response gets security headers here.
  - `Response::sse` hands the raw socket to a closure for long-lived Server-Sent Events.
- **`src/docker.rs`:** the Docker Engine API client.
  - Transports: unix socket, `tcp://`, and Windows named pipe (`npipe://`), selected with `cfg`.
  - Buffered calls use `Connection: close` and read to EOF.
  - `Docker::stream` returns a `Stream` that decodes chunked encoding incrementally. Reads time out regularly, which gives callers an `Idle` tick.
  - `Demux` splits non-TTY log frames incrementally.
- **`src/live.rs`:** one SSE stream per browser tab (`/api/stream?c=<id>`).
  - Each Docker stream gets one small relay thread: events, plus stats and logs for the open container.
  - Relays feed a bounded `sync_channel`, and the connection thread batches what arrives into single writes.
  - The connection thread peeks the socket every 2 s to notice a closed tab (heartbeat every 15 s). Shutdown is an `AtomicBool` that relays check after each read (5 s timeout), so threads outlive a closed tab by about 7 s.
  - One stream per tab, not one per data source, because browsers allow only about 6 HTTP/1.1 connections per host.
- **`src/compose.rs`:** project-level start/stop/restart, using compose labels only (no compose CLI).
  - The order comes from `com.docker.compose.depends_on`: dependencies start first and stop last.
  - Containers in the same dependency wave run in parallel via `thread::scope`.
- **`src/audit.rs`:** the append-only audit log, one JSON line per change.
  - `App::handle` records every non-GET request after it runs; `audit_subject` maps the route to `kind.verb` plus a target.
  - Each entry is a single `O_APPEND` write, so threads need no lock.
  - Past 1 MB the file is renamed to `.1`, which bounds it to about 2 MB on disk.
  - `/api/audit?limit=&target=` returns entries newest first.
  - Any new mutating route must be added to `audit_subject`, otherwise it goes unrecorded.
- **`src/tokens.rs`:** API tokens for scripts and agents, sent as `Authorization: Bearer ctr_<id>_<secret>`.
  - Only a SHA-256 of each token is stored, in `tokens.json` next to the audit log (mode 0600, replaced atomically on write).
  - SHA-256 and the random source (`/dev/urandom`) are hand-written, so no new dependencies.
  - Scopes are `read` < `operate` < `admin`, mapped from method and path by `required_scope` in `main.rs`.
  - The token table sits behind an `RwLock<Arc<Vec<Token>>>`. This is the one deliberate lock in the server: it is read on every request but written only on create or revoke. `last_used` is a per-token atomic.
  - Tokens require the audit log's state directory; with `--audit-file off`, tokens are off too.
- **`src/metrics.rs`:** one sampler thread reads cgroup v2 files for every container, plus `/proc` for the host, every 10 s.
  - It never calls the Docker API, which costs about 1 s of daemon time per container.
  - History lives in rings of `AtomicU32` (f32 bits): 1 h at 10 s and 24 h at 1-minute averages, about 50 KB per container, allocated on first sight.
  - One writer, lock-free readers. Slots outlive their container and are reused least-recently-seen.
  - Routes: `/api/metrics` (latest values and sparklines), `/api/metrics/<id>?range=1h|24h`, and `/metrics` (Prometheus text format, labelled with name, stack and service).
  - Inside a container it needs `--cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:ro --pid=host`; without them only host metrics work.
- **`src/logwatch.rs`:** log patterns and the crash flight recorder.
  - One follower thread per running container. A supervisor watches Docker `start` and `kill` events.
  - Each line is masked into a reused buffer: numbers, times, IPs, IDs and unit values become `<*>`; words are split at `/[](),;`; runs of `<*>` collapse.
  - Lines matching a known pattern allocate nothing. Unknown ones merge into a similar pattern (same length and first word, ≥70% equal) or start a new one, up to 128 per container.
  - The last 32 KB of output sits in a fixed ring buffer. On a non-zero exit or OOM kill (but not 0, 143, or 137 shortly after a `kill` event) a postmortem goes to `<state>/incidents/`. Crash loops update one file; at most 100 files are kept.
  - Snapshots are published through `Mutex<Arc<_>>` cells, swapped every 5 s: the second deliberate lock.
  - Memory grows with followed containers; `footprint.sh` budgets 4 MB + 256 KB per container.
- **`src/otlp.rs`:** OTLP/HTTP receiver on `POST /v1/traces|logs|metrics`, protobuf (hand-written reader, no codegen) or JSON. gzip gets 415, gRPC isn't served.
  - The HTTP thread decodes spans into small fixed records and hands a batch to one aggregator thread through a bounded `sync_channel`; a full queue answers 503 so the sender retries. Only the aggregator touches the per-service state, so there are no locks there.
  - Only server and consumer spans (or root spans) count as requests. Per service: 60 one-minute buckets with a 28-bucket log2-µs histogram (percentiles interpolate inside a bucket); per operation, 12 five-minute buckets. Exemplars keep the latest errors and the slowest requests with trace IDs.
  - Up to 64 services and 32 operations each; names are looked up before insert, so a known service costs no allocation (4 allocs per 1,000-span batch, ~240 MB/s).
  - OTLP logs go to `logwatch` as per-service pattern miners. Metrics points are only counted.
  - `--otlp-forward` re-posts each raw batch from a separate thread; drops are counted, never block the receiver.
  - Routes: `/api/telemetry`, `/api/telemetry/<service>`, `/api/telemetry/export` (and `/api/metrics/export`): JSON lines, or CSV with `?format=csv`.
- **`src/cli.rs`:** argument parsing (see above).
- **`src/client.rs`:** client mode. When the first argument is a command (`ps`, `logs`, `mcp`, …), `main` hands off here and the server code never runs.
  - It talks to a server's HTTP API, reusing `docker.rs`'s HTTP client over `tcp://` with extra headers (`Bearer` token, `X-Contraire`).
  - `contraire mcp` is an MCP server: JSON-RPC 2.0, one message per line on stdio. Logs go to stderr only, because stdout carries the protocol.
  - Tool failures are returned as `isError` results, not protocol errors, so the model can read them.
  - The tool set deliberately has no remove, prune or pull. Server-side token scopes still apply.
  - Text formatting is shared between the CLI and the MCP tools: compact and plain language, including exit-code hints.
- **`src/main.rs`:** holds `App`: the auth check, the CSRF check, and routing by `match` on `(method, path segments)`.
  - Most routes are thin proxies to the Docker API.
  - `/api/info` and the stats routes slim Docker's large payloads down server-side (`live::summarize_stats`).
  - The Content-Security-Policy is built per request so it allows exactly the configured docs origin.
- **Self-telemetry and history routes:**
  - `/api/self` reports this process's PID, executable, RSS, CPU seconds, threads, open files, connections and requests, all read from `/proc/self`.
  - `/api/events?since=N` returns recent Docker event history, using the same server-side event filter as the live stream.
- **Visual design:** the UI follows the "Modernist" style of the Incident Playbook design (claude.ai/design/p/368c794f-…).
  - Swiss grid: hard 2 px rules, square corners, panels separated by rules instead of gaps.
  - One signal red (#E8341C) for the active item, failures and key numbers. Small labels are uppercase and letter-spaced.
  - Typefaces: Archivo for headings and body, IBM Plex Mono for code.
  - Code blocks are black, with an attached Copy cell. Charts are red and black bars, not lines.
  - Content always uses the full browser width.
  - Themes: light, dark and auto. The choice is set via `data-theme` on `<html>` and remembered in localStorage. Dark tokens are defined twice, once for the explicit choice and once under `prefers-color-scheme`.
  - Scrollbars use the theme tokens (`scrollbar-color` plus `::-webkit-scrollbar`).
  - Fonts are injected by JS from Google Fonts so they never block the first render; offline, system fonts stand in.
- **`build.rs`:** gzips `static/index.html` into the binary when a `gzip` tool exists (128 KB becomes 34 KB), and falls back to the raw page otherwise. The server sends it with `Content-Encoding: gzip`; clients without gzip support (plain `curl`) get a 406 with a hint.
- **`static/index.html`:** the whole frontend, compiled in via `include_str!`. Vanilla JS and CSS with no build step.
  - Colors are CSS custom properties, with a dark palette under `prefers-color-scheme`.
  - Data arrives by push. Docker events trigger a debounced list re-fetch. Stats and logs stream while the drawer is open.
  - A 15 s fallback poll only refreshes relative texts such as "Up 5 minutes".
- **`docs/learn/`:** the Learn tab's content (Markdown, SVG and `index.json`).
  - **Never embedded in the binary.** The browser fetches it from `--docs-url`, by default GitHub raw on `main`, so lesson changes go live on push.
  - Images are fetched and shown through `blob:` URLs, so SVGs render whatever content type the host sends.
  - Diagrams are standalone SVGs with their own `prefers-color-scheme` styles.
  - The page includes a small Markdown renderer: headings, lists, fenced code with copy buttons, `>` callouts, tables, images, and `x.md` links between lessons.

**Security invariants. Keep these when adding routes:**
- Every path segment that is interpolated into a Docker API path goes through `valid_id`, which blocks traversal. Query values go through `http::url_encode`.
- Every route needs a scope in `required_scope`. The fallback is `admin`, so a new route is safe by default; lower it deliberately. Denied attempts are audited.
- Identity comes from exactly one source: a bearer token, the Basic-auth password, or open access when no password is set. Password and open access act as `admin`.
- Every non-GET browser request needs the `X-Contraire` header. Bearer requests skip this check, because browsers never send bearer tokens on their own. That is the CSRF defence: cross-origin pages can't send a custom header without a CORS preflight, and the server never answers preflights. The frontend's `api()` wrapper adds the header.
- The default bind is all interfaces (`0.0.0.0:8083`). Access to the Docker socket is root-equivalent, so the server warns at startup when it is network-reachable without a password.
- The UI renders untrusted strings (container names, labels, logs). Anything put into `innerHTML` must go through `esc()`.

**Docker API notes:**
- Requests carry no API version prefix, so older daemons on a Pi still work.
- Image pulls always send an explicit tag, because an empty `tag` makes Docker pull every tag.
- Pull errors arrive in-band inside a 200 JSON-lines stream.
- Start/stop and similar actions return 304 when the container is already in that state. The server maps that to 204.
