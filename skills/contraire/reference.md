# Reference: Docker, Compose and Linux for operators

General facts and rules that hold on every host. Read the section you need while diagnosing; you don't need all of it for every task. Facts about *this* host and its applications belong in the session's `PROJECT.md`, not here.

## Contents

1. Exit codes and signals
2. Container states and restart policies
3. Health checks
4. Memory, CPU and the OOM killer
5. Logs
6. Compose
7. Networking
8. Storage
9. Images and architectures
10. Small hosts (Raspberry Pi, SD cards)
11. Telemetry (OpenTelemetry)
12. Rules for changing things

## 1. Exit codes and signals

| Code | Meaning | Usual cause |
|---|---|---|
| 0 | Finished normally | One-shot job done, or stopped cleanly |
| 1 | Generic application error | Read the last log lines: config error, failed connection, exception |
| 2 | Misuse of a shell builtin / bad arguments | Wrong command or flags in `command:` |
| 125 | Docker itself failed to run the container | Bad option, port already in use, missing mount source |
| 126 | Command found but not executable | Missing `+x`, wrong file type |
| 127 | Command not found | Typo in `command:`/`entrypoint:`, binary missing from the image |
| 128+n | Killed by signal n | See below |
| 137 | 128+9, SIGKILL | Out of memory (check `OOMKilled`), or `docker kill`/stop timeout |
| 139 | 128+11, SIGSEGV | Crash in native code; wrong CPU architecture or a bad native library |
| 143 | 128+15, SIGTERM | Asked to stop (`docker stop`, compose down); normal |

- `docker stop` sends SIGTERM (or the image's `STOPSIGNAL`), waits 10 s, then SIGKILL. A 137 right after a stop usually means the app ignores SIGTERM, not a crash.
- PID 1 in a container doesn't get default signal handlers. Apps run through a shell script (`sh -c app`) often never see SIGTERM. `init: true` in Compose adds a tiny init that forwards signals.

## 2. Container states and restart policies

- States: `created` → `running` ⇄ `paused` → `exited` (or `dead`). `restarting` means a restart policy is retrying after an exit.
- Policies: `no` (default), `on-failure[:N]` (only non-zero exits), `always` (also after a daemon restart, even if stopped by hand before), `unless-stopped` (like always, but a manual stop sticks).
- Docker backs off between restarts (doubling, up to about a minute). A container that is "Up 3 seconds" every time you look is in a crash loop; the cause is in the first lines after each start.
- The restart count (`RestartCount`) resets when the container is recreated, e.g. by `docker compose up` after a change.

## 3. Health checks

- A `HEALTHCHECK` runs a command inside the container every `interval`; after `retries` consecutive failures the container becomes `unhealthy`. During `start_period`, failures don't count.
- Docker does **not** restart unhealthy containers on its own. Orchestrators do, plain Docker doesn't. Something has to act on it.
- Unhealthy but running often means: the app is overloaded or deadlocked, a dependency is down, or the check itself is wrong (missing `curl` in the image, wrong port, checking `localhost` while the app binds another interface).
- `depends_on: condition: service_healthy` makes Compose wait for the health check before starting dependents, but only at `up` time.

## 4. Memory, CPU and the OOM killer

- Memory limits (`mem_limit`, `deploy.resources.limits.memory`, `--memory`) are enforced by cgroups. Above the limit the kernel kills a process in the container: exit 137 with `OOMKilled: true`.
- Without a limit, a leaking container can push the whole host into the OOM killer, which may kill *another* container or the Docker daemon. On small hosts every service should have a limit.
- "Memory" in dashboards is usually cgroup `memory.current` minus reclaimable page cache (`inactive_file`). The kernel can drop cache any time; a container "using" a lot of cache isn't leaking.
- A steady climb over hours ending in an OOM is a leak; a sudden jump is a burst (big request, batch job). Compare with the 24 h history before raising a limit.
- CPU: 100% means one full core. `cpus: 0.5` caps at half a core; throttled apps get slow, not killed.
- Load average counts runnable *and* uninterruptible (I/O-waiting) tasks. Load above the core count with low CPU means waiting on disk or network.

## 5. Logs

- `docker logs` shows what PID 1 writes to stdout/stderr. Apps that log to files inside the container show nothing there.
- The default `json-file` driver keeps logs forever unless `max-size`/`max-file` are set. The `local` driver rotates by default. Logs are deleted with the container (`docker rm`).
- Logs are line-oriented; stack traces span many lines, and the first line after a start often explains a crash loop better than the last line before it.
- Timestamps (`-t`) are added by Docker on receipt; compare them with events to line up crashes and restarts.

## 6. Compose

- A **project** (stack) is the set of containers with the same `com.docker.compose.project` label, by default named after the folder of `compose.yaml`. Services are the entries under `services:`; a service can run several containers (`scale`/`replicas`).
- `docker compose up -d` creates or recreates only what changed (image, config, environment). `restart` restarts containers *without* applying changes to `compose.yaml`; to apply a change, run `up -d` again.
- `down` removes containers and the project network but keeps named volumes; `down -v` deletes volumes too (data loss).
- `depends_on` orders start and stop only; it doesn't restart dependents when a dependency restarts later. Containers must handle a dependency disappearing (retry, reconnect).
- Environment: `environment:` values beat `env_file:`, which beats the image's `ENV`. A `.env` file next to `compose.yaml` fills `${VARS}` in the file itself, not in the containers.
- Several files are merged in order (`-f a.yaml -f b.yaml`); later files override earlier ones. Always use the same file list and project name (`-p`) the stack was created with, or Compose sees a different project.

## 7. Networking

- Each Compose project gets a bridge network; services reach each other by **service name** through Docker's DNS (127.0.0.11 inside containers). Container names and `localhost` don't work across containers.
- `localhost` inside a container is the container itself. To reach a service on the host, use the host's IP or `host.docker.internal` (Docker Desktop, or `extra_hosts: ["host.docker.internal:host-gateway"]` on Linux).
- `ports: "8080:80"` publishes container port 80 on host port 8080 on **all interfaces**, and Docker's iptables rules bypass host firewalls such as ufw. Use `127.0.0.1:8080:80` for local-only.
- "Connection refused" means nothing listens there (service down, wrong port, app bound to 127.0.0.1 inside the container). "Name resolution failed / no such host" means the service name is wrong, the container isn't running, or it's on another network. A timeout means something drops packets (firewall, wrong network).

## 8. Storage

- A container's own filesystem is a thin writable layer, deleted with the container. Data that matters lives in a **named volume** (managed by Docker) or a **bind mount** (a host folder).
- Bind mounts keep host ownership and permissions: a container running as UID 1000 can't write a root-owned folder. On SELinux hosts (Fedora, RHEL) bind mounts need `:z`/`:Z` or a `container_file_t` label, or access is denied despite correct permissions.
- `docker system df` shows images, containers, volumes and build cache. `image prune` removes dangling images only; `system prune -a --volumes` removes much more. Prune is destructive: confirm with a person.
- A full disk shows up as failed writes, crashed databases and containers that won't start. Check it early.

## 9. Images and architectures

- Images are built per CPU architecture. A Raspberry Pi needs `arm64` (64-bit OS) or `arm/v7` (32-bit). An amd64-only image exits at once with "exec format error" (or 139).
- `docker manifest inspect <image>` lists the architectures an image provides.
- Tags move: `:latest` and `:1` can point to a new image tomorrow. Pin versions (`:1.4.2`) or digests for anything that matters, and note what was running before an update so you can go back.

## 10. Small hosts (Raspberry Pi, SD cards)

- 1 GB of RAM fills fast. Prefer `alpine`/`slim` images, set memory limits, and watch swap: a swapping Pi becomes unresponsive long before anything is killed.
- SD cards are slow and wear out with writes. Rotate logs (`max-size`), keep databases on USB SSDs, avoid chatty debug logging.
- Under-voltage throttles the CPU and corrupts SD cards. `vcgencmd get_throttled` (non-zero = trouble) on Raspberry Pi OS.
- After a power cut only containers with a restart policy come back.

## 11. Telemetry (OpenTelemetry)

- Traces show requests as a tree of spans across services. Server and consumer spans are the requests a service *receives*; client spans are calls it makes. Error status is set by the instrumentation (HTTP 5xx, exceptions, gRPC errors).
- An error rate is per service. Failures caused by a dependency show up in the callers too: when many services fail with the same message (name resolution, connection refused, deadline exceeded), look for the one dependency they share and check its container first.
- Percentiles: p50 is the typical request, p95 the slowest 1 in 20, p99 the slowest 1 in 100. A high p99 with a normal p50 points at occasional stalls (GC pauses, locks, cold caches, retries), not a generally slow service.
- A trace ID finds the full request in a tracing tool (Jaeger, Tempo). No telemetry from a service only means it isn't instrumented or isn't sending here, not that it's healthy.

## 12. Rules for changing things

1. **Look before you touch.** Know the current state and the cause before acting. A restart that "fixes" an unknown cause hides the evidence (logs of the crashed process, the state of the moment).
2. **Smallest step first.** Restart one container before the stack; the stack before the host. Never remove, prune, or `down -v` without a person's explicit go-ahead.
3. **Mind dependencies.** Stopping a database stops everything that needs it. Start dependencies first, stop them last.
4. **Verify after every change.** Check status, health and the first log lines after a restart. "Started" isn't "working".
5. **Write it down.** What you changed, when, and why, in the session journal, so the next person (or agent) doesn't repeat it.
6. **Prefer the declared config.** Hand changes (`docker update`, edits inside a container) are lost on the next `compose up`. Lasting fixes go into `compose.yaml`; say so when you make a temporary one.
