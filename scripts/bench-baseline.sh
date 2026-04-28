#!/usr/bin/env bash
# bench-baseline.sh — drive `taktool loadgen` against any TAK server,
# capture throughput JSON + system metrics for the M5 comparison report.
#
# Issue: https://github.com/copyleftdev/tak-rs/issues/37
#
# Usage:
#   scripts/bench-baseline.sh \
#     --target 127.0.0.1:8088 \
#     --connections 1000 \
#     --rate 10 \
#     --duration 60 \
#     --tag java-baseline
#
# Output:
#   bench/history/<tag>-<UTC ISO timestamp>.json
#
# JSON contains the loadgen line plus a `system` block with peak %CPU /
# max RSS-MB observed via `top -b -p PID` while the run is in progress.
# The `target_pid` is auto-detected by netstat — you can override with
# --pid PID for cases where the listener is in a Docker container.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
HISTORY_DIR="$REPO_ROOT/bench/history"
mkdir -p "$HISTORY_DIR"

# --------------------------------------------------------------------------
# Defaults
# --------------------------------------------------------------------------
TARGET="127.0.0.1:8088"
CONNECTIONS=10
RATE=5
DURATION=10
MIX="realistic"
TAG="rust"
TARGET_PID=""

usage() {
    cat <<EOF
bench-baseline.sh — capture a single loadgen run as JSON.

Options:
  --target HOST:PORT   listener to dial (default: 127.0.0.1:8088)
  --connections N      concurrent TCP connections (default: 10)
  --rate R             messages/sec per connection (default: 5)
  --duration S         seconds to run (default: 10)
  --mix PROFILE        realistic | pli-only | uniform (default: realistic)
  --tag NAME           tag embedded in JSON (default: "rust")
  --pid PID            target process pid for CPU/RSS sampling (auto if empty)
  -h, --help           show this help

The run output is written to $HISTORY_DIR/<tag>-<timestamp>.json.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)       TARGET="$2"; shift 2 ;;
        --connections)  CONNECTIONS="$2"; shift 2 ;;
        --rate)         RATE="$2"; shift 2 ;;
        --duration)     DURATION="$2"; shift 2 ;;
        --mix)          MIX="$2"; shift 2 ;;
        --tag)          TAG="$2"; shift 2 ;;
        --pid)          TARGET_PID="$2"; shift 2 ;;
        -h|--help)      usage; exit 0 ;;
        *)              echo "unknown arg: $1" >&2; usage; exit 1 ;;
    esac
done

# --------------------------------------------------------------------------
# Auto-detect listener pid if not supplied
# --------------------------------------------------------------------------
if [[ -z "$TARGET_PID" ]]; then
    TARGET_PORT="${TARGET##*:}"
    if command -v ss > /dev/null; then
        TARGET_PID="$(ss -ltnp 2> /dev/null | awk -v port=":$TARGET_PORT$" '
            $4 ~ port { match($0, /pid=[0-9]+/); if (RSTART) { print substr($0, RSTART+4, RLENGTH-4); exit } }')"
    fi
fi

if [[ -z "$TARGET_PID" ]]; then
    echo "[bench-baseline] WARN: could not detect target pid for $TARGET — system metrics will be empty" >&2
fi

# --------------------------------------------------------------------------
# Sample CPU/RSS in the background while loadgen runs
# --------------------------------------------------------------------------
SYS_LOG="$(mktemp)"
trap 'rm -f "$SYS_LOG"' EXIT

if [[ -n "$TARGET_PID" ]]; then
    (
        # 1Hz, total run duration + 2s buffer
        end=$(( $(date +%s) + DURATION + 2 ))
        while [[ $(date +%s) -lt $end ]]; do
            if [[ -d "/proc/$TARGET_PID" ]]; then
                # %CPU is sum across all threads via /proc/<pid>/stat
                CPU="$(top -b -n 1 -p "$TARGET_PID" 2> /dev/null | tail -1 | awk '{print $9}')"
                RSS_KB="$(awk '/VmRSS/ { print $2 }' /proc/$TARGET_PID/status 2> /dev/null || echo 0)"
                echo "$CPU $RSS_KB" >> "$SYS_LOG"
            fi
            sleep 1
        done
    ) &
    SYS_PID=$!
fi

# --------------------------------------------------------------------------
# Run loadgen
# --------------------------------------------------------------------------
echo "[bench-baseline] target=$TARGET conns=$CONNECTIONS rate=$RATE duration=${DURATION}s mix=$MIX tag=$TAG target_pid=${TARGET_PID:-?}" >&2

LOADGEN_JSON="$(
    cd "$REPO_ROOT" && \
    cargo run --release -q -p taktool -- loadgen \
        --target "$TARGET" \
        --connections "$CONNECTIONS" \
        --rate "$RATE" \
        --duration "$DURATION" \
        --mix "$MIX" \
        --tag "$TAG" \
        --json
)"

if [[ -n "${SYS_PID:-}" ]]; then
    wait "$SYS_PID" 2> /dev/null || true
fi

# --------------------------------------------------------------------------
# Compute system aggregates: peak CPU%, max RSS in MB
# --------------------------------------------------------------------------
PEAK_CPU=0
MAX_RSS_MB=0
SAMPLES=0
if [[ -s "$SYS_LOG" ]]; then
    PEAK_CPU="$(awk '{ if ($1+0 > max) max = $1+0 } END { print max+0 }' "$SYS_LOG")"
    MAX_RSS_MB="$(awk '{ if ($2+0 > max) max = $2+0 } END { print int((max+0)/1024) }' "$SYS_LOG")"
    SAMPLES="$(wc -l < "$SYS_LOG" | tr -d ' ')"
fi

TS="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
OUT="$HISTORY_DIR/${TAG}-${TS}.json"

# Merge loadgen JSON with system block. Hand-rolled JSON merge (no jq
# dep) — both halves are well-known shape.
cat > "$OUT" <<EOF
{
  "loadgen": $LOADGEN_JSON,
  "system": {
    "target_pid": "${TARGET_PID:-}",
    "samples": $SAMPLES,
    "peak_cpu_pct": $PEAK_CPU,
    "max_rss_mb": $MAX_RSS_MB
  },
  "captured_at": "$TS"
}
EOF

echo "[bench-baseline] wrote $OUT" >&2
echo "$OUT"
