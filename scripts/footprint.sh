#!/usr/bin/env bash
# Footprint and performance check against the budgets in ROADMAP.md.
# Starts the release binary against the local Docker daemon, measures, and exits
# non-zero if any budget is exceeded. Results also go to target/footprint.json.
#
#   scripts/footprint.sh            full run (~45 s)
#   scripts/footprint.sh --quick    skip the slow thread-cleanup check
#   scripts/footprint.sh --perf     add `perf stat` counters for the load phase
set -euo pipefail
cd "$(dirname "$0")/.."

PORT=${PORT:-18183}
REQS=${REQS:-2000}
CONC=${CONC:-8}
STREAMS=${STREAMS:-10}
QUICK=0 PERF=0
for a in "$@"; do
  case $a in
    --quick) QUICK=1 ;;
    --perf) PERF=1 ;;
    -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done

# Budgets (KB, %, count). Keep in sync with ROADMAP.md.
B_BINARY=1024 B_PAGE=120 B_IDLE_RSS=5120 B_IDLE_CPU=0.5 B_PEAK_RSS=16384 B_THREADS_AFTER=1

BIN=target/release/contraire
cargo build --release --quiet
URL=http://127.0.0.1:$PORT
$BIN -l 127.0.0.1:$PORT >/dev/null 2>&1 &
PID=$!
STREAM_PIDS=()
cleanup() { kill "${STREAM_PIDS[@]}" 2>/dev/null || true; kill $PID 2>/dev/null || true; }
trap cleanup EXIT
for _ in $(seq 50); do curl -sf -o /dev/null $URL/api/config && break; sleep 0.1; done

status() { awk -v k="$1:" '$1 == k { print $2 }' /proc/$PID/status; }
ticks() { awk '{ print $14 + $15 }' /proc/$PID/stat; }
CLK=$(getconf CLK_TCK)
fail=0
results=()
row() { # label value budget unit
  local ok=ok
  awk "BEGIN { exit !($2 <= $3) }" || { ok=OVER; fail=1; }
  printf "  %-36s %10s %-4s  budget %7s  %s\n" "$1" "$2" "$4" "$3" "$ok"
  results+=("\"$1\": $2")
}
info() { printf "  %-36s %10s %s\n" "$1" "$2" "$3"; results+=("\"$1\": $2"); }

# One curl process, keep-alive, CONC in parallel: measures the server, not process spawning.
load() { # path count -> requests per second
  local cfg; cfg=$(mktemp)
  for _ in $(seq "$2"); do printf 'url = "%s%s"\noutput = "/dev/null"\n' "$URL" "$1"; done > "$cfg"
  local t0 t1; t0=$(date +%s.%N)
  curl -s --parallel --parallel-max "$CONC" -K "$cfg"
  t1=$(date +%s.%N); rm -f "$cfg"
  awk -v n="$2" -v a="$t0" -v b="$t1" 'BEGIN { printf "%.0f", n / (b - a) }'
}

echo "Static"
row "binary size" $(( $(stat -c %s $BIN) / 1024 )) $B_BINARY KB
row "web page, gzip -9" $(( $(gzip -9 -c static/index.html | wc -c) / 1024 )) $B_PAGE KB

echo "Idle"
sleep 1
row "RSS at idle" "$(status VmRSS)" $B_IDLE_RSS KB
curl -sN $URL/api/stream >/dev/null & STREAM_PIDS+=($!)
sleep 1
t0=$(ticks); sleep 10; t1=$(ticks)
row "CPU, idle with 1 live stream (10 s)" "$(awk -v d=$((t1 - t0)) -v c="$CLK" 'BEGIN { printf "%.2f", d / c / 10 * 100 }')" $B_IDLE_CPU %

echo "Load ($REQS requests, $CONC parallel, keep-alive)"
info "req/s /api/config (server only)" "$(load /api/config "$REQS")" ""
if [ $PERF = 1 ] && command -v perf >/dev/null; then
  mkdir -p target
  perf stat -e task-clock,context-switches,page-faults -p $PID 2> target/perf.txt &
  PERF_PID=$!
fi
# Bounded by Docker itself: listing ~30 containers takes the daemon ~100 ms.
info "req/s /api/containers (via Docker)" "$(load /api/containers 100)" ""
if [ -n "${PERF_PID:-}" ]; then
  kill -INT $PERF_PID 2>/dev/null || true
  wait $PERF_PID 2>/dev/null || true
  grep -E 'task-clock|context-switches|page-faults|elapsed' target/perf.txt | sed 's/^ */    perf: /' || true
fi
info "RSS after load" "$(status VmRSS)" KB

echo "Streams ($STREAMS live streams)"
for _ in $(seq "$STREAMS"); do curl -sN $URL/api/stream >/dev/null & STREAM_PIDS+=($!); done
sleep 1
info "threads with $((STREAMS + 1)) streams" "$(status Threads)" ""
info "RSS with $((STREAMS + 1)) streams" "$(status VmRSS)" KB
kill "${STREAM_PIDS[@]}" 2>/dev/null || true
STREAM_PIDS=()
if [ $QUICK = 0 ]; then
  sleep 17 # relay threads notice closed clients within one 15 s idle tick
  row "threads after streams closed" "$(status Threads)" $B_THREADS_AFTER ""
fi
row "peak RSS (VmHWM)" "$(status VmHWM)" $B_PEAK_RSS KB

mkdir -p target
printf '{ "time": "%s", "commit": "%s", %s }\n' "$(date -Iseconds)" "$(git rev-parse --short HEAD 2>/dev/null)" \
  "$(IFS=,; echo "${results[*]}")" > target/footprint.json
[ $fail = 0 ] && echo "All budgets met." || echo "Budget exceeded." >&2
exit $fail
