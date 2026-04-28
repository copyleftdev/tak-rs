#!/usr/bin/env bash
# bench-comparison.sh — run identical loadgen against the Rust and Java
# servers in turn and write a side-by-side comparison summary.
#
# Issue: https://github.com/copyleftdev/tak-rs/issues/38
#
# Usage:
#   scripts/bench-comparison.sh \
#     --rust-target 127.0.0.1:8088 \
#     --java-target 127.0.0.1:18088 \
#     --connections 1000 \
#     --rate 10 \
#     --duration 60
#
# Output: bench/history/comparison-<UTC ISO>.json — merged result with
# both runs and a `verdict` block holding the throughput ratio.
#
# M5 acceptance gate (issue #38): Rust ≥ 3× Java throughput.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
HISTORY_DIR="$REPO_ROOT/bench/history"
mkdir -p "$HISTORY_DIR"

RUST_TARGET="127.0.0.1:8088"
JAVA_TARGET="127.0.0.1:18088"
CONNECTIONS=100
RATE=10
DURATION=30
MIX="realistic"
THROUGHPUT_FLOOR=3

usage() {
    cat <<EOF
bench-comparison.sh — run loadgen against Rust + Java in turn, compare.

Options:
  --rust-target HOST:PORT  Rust listener (default: 127.0.0.1:8088)
  --java-target HOST:PORT  Java listener (default: 127.0.0.1:18088)
  --connections N          (default: 100)
  --rate R                 msgs/sec/conn (default: 10)
  --duration S             seconds per run (default: 30)
  --mix PROFILE            realistic | pli-only | uniform
  --throughput-floor X     minimum Rust:Java throughput ratio
                           required to pass (default: 3)
  -h, --help               show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --rust-target)      RUST_TARGET="$2"; shift 2 ;;
        --java-target)      JAVA_TARGET="$2"; shift 2 ;;
        --connections)      CONNECTIONS="$2"; shift 2 ;;
        --rate)             RATE="$2"; shift 2 ;;
        --duration)         DURATION="$2"; shift 2 ;;
        --mix)              MIX="$2"; shift 2 ;;
        --throughput-floor) THROUGHPUT_FLOOR="$2"; shift 2 ;;
        -h|--help)          usage; exit 0 ;;
        *)                  echo "unknown arg: $1" >&2; usage; exit 1 ;;
    esac
done

# ----------------------------------------------------------------------
# Run each side via bench-baseline.sh, capturing the result file path.
# ----------------------------------------------------------------------
echo "[bench-comparison] === Rust ===" >&2
RUST_OUT="$(
    bash "$SCRIPT_DIR/bench-baseline.sh" \
        --target "$RUST_TARGET" \
        --connections "$CONNECTIONS" \
        --rate "$RATE" \
        --duration "$DURATION" \
        --mix "$MIX" \
        --tag rust
)"

echo "[bench-comparison] === Java baseline ===" >&2
JAVA_OUT="$(
    bash "$SCRIPT_DIR/bench-baseline.sh" \
        --target "$JAVA_TARGET" \
        --connections "$CONNECTIONS" \
        --rate "$RATE" \
        --duration "$DURATION" \
        --mix "$MIX" \
        --tag java-baseline
)"

# ----------------------------------------------------------------------
# Compare. Awk over the loadgen JSON in each run; any sufficiently
# JSON-aware tool would do, but awk keeps the dep surface tight.
# ----------------------------------------------------------------------
RUST_MPS="$(awk -F'"msg_per_s":' 'NF>1 { sub(/[,}].*/, "", $2); print $2+0; exit }' "$RUST_OUT")"
JAVA_MPS="$(awk -F'"msg_per_s":' 'NF>1 { sub(/[,}].*/, "", $2); print $2+0; exit }' "$JAVA_OUT")"
RUST_RSS="$(awk -F'"max_rss_mb":' 'NF>1 { sub(/[,}].*/, "", $2); print $2+0; exit }' "$RUST_OUT")"
JAVA_RSS="$(awk -F'"max_rss_mb":' 'NF>1 { sub(/[,}].*/, "", $2); print $2+0; exit }' "$JAVA_OUT")"
RUST_CPU="$(awk -F'"peak_cpu_pct":' 'NF>1 { sub(/[,}].*/, "", $2); print $2+0; exit }' "$RUST_OUT")"
JAVA_CPU="$(awk -F'"peak_cpu_pct":' 'NF>1 { sub(/[,}].*/, "", $2); print $2+0; exit }' "$JAVA_OUT")"

if (( $(awk "BEGIN { print ($JAVA_MPS > 0) }") )); then
    RATIO="$(awk "BEGIN { printf \"%.2f\", $RUST_MPS / $JAVA_MPS }")"
else
    RATIO="inf"
fi

if (( $(awk "BEGIN { print ($RATIO == \"inf\" || $RATIO >= $THROUGHPUT_FLOOR) }") )); then
    VERDICT="pass"
else
    VERDICT="fail"
fi

# ----------------------------------------------------------------------
# Merge into one file
# ----------------------------------------------------------------------
TS="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
OUT="$HISTORY_DIR/comparison-${TS}.json"

cat > "$OUT" <<EOF
{
  "rust_run":  $(cat "$RUST_OUT"),
  "java_run":  $(cat "$JAVA_OUT"),
  "verdict": {
    "rust_msg_per_s": $RUST_MPS,
    "java_msg_per_s": $JAVA_MPS,
    "throughput_ratio": "$RATIO",
    "throughput_floor": $THROUGHPUT_FLOOR,
    "rust_max_rss_mb": $RUST_RSS,
    "java_max_rss_mb": $JAVA_RSS,
    "rust_peak_cpu_pct": $RUST_CPU,
    "java_peak_cpu_pct": $JAVA_CPU,
    "result": "$VERDICT"
  },
  "captured_at": "$TS"
}
EOF

echo "[bench-comparison] wrote $OUT" >&2

# ----------------------------------------------------------------------
# Console verdict
# ----------------------------------------------------------------------
echo ""
echo "+------------------ Rust vs Java baseline -------------------+"
printf "| Rust msg/s         : %-37s |\n" "$RUST_MPS"
printf "| Java msg/s         : %-37s |\n" "$JAVA_MPS"
printf "| Throughput ratio   : %-37s |\n" "${RATIO}x"
printf "| M5 floor           : %-37s |\n" "${THROUGHPUT_FLOOR}x (rust >= this multiple of java)"
printf "| Rust max RSS (MB)  : %-37s |\n" "$RUST_RSS"
printf "| Java max RSS (MB)  : %-37s |\n" "$JAVA_RSS"
printf "| Rust peak CPU%%     : %-37s |\n" "$RUST_CPU"
printf "| Java peak CPU%%     : %-37s |\n" "$JAVA_CPU"
printf "| Verdict            : %-37s |\n" "$VERDICT"
echo "+------------------------------------------------------------+"

[[ "$VERDICT" == "pass" ]] || exit 1
