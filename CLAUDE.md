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
- an OTLP endpoint
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
```

Default listen address: `0.0.0.0:8083`.

**CLI:** every option is defined in the `OPTS` table in `src/cli.rs`, which drives parsing, `--help` output and typo suggestions.
- Each option also has an environment variable. Precedence is flag > env > default.
- The password is read from `--password-file` or `CONTRAIRE_PASSWORD` only, never from a flag, because argv is visible in `ps`.
- `parse` takes the environment as a closure, so tests never touch process state.
- The parser is hand-written on purpose. clap was measured at +193–218 KB of binary and about 3× the clean build time (19 crates including `syn` with derive), which is too much for the Pi. Keep new options in the table rather than reaching for clap.

**UI check:** the connected Chrome extension may run on a different machine, so `127.0.0.1` there is not this host. Use local headless Chrome instead. A URL hash `#c/<container-id>` opens the detail drawer directly:

```sh
google-chrome --headless=new --no-sandbox --user-data-dir=<scratch> --window-size=1280,900 \
  --virtual-time-budget=6000 [--force-dark-mode] --screenshot=out.png "http://127.0.0.1:18080/#c/<id>"
```

## Architecture

The binary has three Rust modules plus one embedded page.

- `src/http.rs` is a hand-rolled HTTP/1.1 server: keep-alive, Content-Length bodies only, and hard caps on line, header and body size. Every response gets security headers here.
- `src/docker.rs` is a Docker Engine API client over the unix socket. It opens one connection per call with `Connection: close` and reads to EOF, then decodes chunked transfer-encoding. It also demultiplexes non-TTY log streams, which use an 8-byte frame header, and passes TTY logs through raw.
- `src/main.rs` holds `App`: the auth check, the CSRF check, and routing by `match` on `(method, path segments)`.
  - Most routes are thin proxies to the Docker API.
  - `/api/info` and `/api/containers/{id}/stats` slim down Docker's large payloads server-side. CPU% is computed from the cpu/precpu delta, and memory subtracts page cache to match `docker stats`.
  - The connection limit is enforced by a `Slot` drop guard around an atomic counter.
- `static/index.html` is the entire frontend, compiled in via `include_str!`, so the binary is self-contained. It is vanilla JS and CSS with no build step and no external requests, which keeps it working on an offline LAN.
  - Colors are CSS custom properties, with a dark palette under `prefers-color-scheme`.
  - Containers are grouped by the `com.docker.compose.project` label.
  - The detail drawer polls stats every 3 s and logs every 2.5 s, only while it is open.

**Security invariants. Keep these when adding routes:**
- Every path segment that is interpolated into a Docker API path goes through `valid_id`, which blocks traversal. Query values go through `http::url_encode`.
- Every non-GET request needs the `X-Contraire` header. That is the CSRF defence: cross-origin pages can't send a custom header without a CORS preflight, and the server never answers preflights. The frontend's `api()` wrapper adds the header.
- The default bind is all interfaces (`0.0.0.0:8083`). Access to the Docker socket is root-equivalent, so the server warns at startup when it is network-reachable without a password.
- The UI renders untrusted strings (container names, labels, logs). Anything put into `innerHTML` must go through `esc()`.

**Docker API notes:**
- Requests carry no API version prefix, so older daemons on a Pi still work.
- Image pulls always send an explicit tag, because an empty `tag` makes Docker pull every tag.
- Pull errors arrive in-band inside a 200 JSON-lines stream.
- Start/stop and similar actions return 304 when the container is already in that state. The server maps that to 204.
