#!/usr/bin/env bash
# Push `tak-agent` to a connected Android device, run a scenario,
# harvest the output, exit with the agent's status.
#
# Usage:
#   ./scripts/run-agent-scenario.sh <agent-binary> <scenario|all> [target]
#
# Examples:
#   ./scripts/run-agent-scenario.sh \
#       target/aarch64-linux-android/release/tak-agent \
#       all \
#       192.168.1.42:8088
#
#   TARGET=10.0.2.2:8088 ./scripts/run-agent-scenario.sh \
#       target/aarch64-linux-android/release/tak-agent pli_dispatch_byte_identity
#
# Requires: adb on PATH, exactly one device authorized
# (`adb devices` shows it). Output is captured as JSON-lines for
# downstream `jq` consumption.
#
# The device path defaults to /data/local/tmp because that's
# writable + executable on stock Android without root. ATAK CIV's
# license terms cover only the ATAK app itself; running an
# arbitrary native binary alongside it via `/data/local/tmp` is
# normal Android development practice (`adb shell` workflow).

set -euo pipefail

AGENT_BIN="${1:-}"
SCENARIO="${2:-}"
TARGET="${3:-${TAK_AGENT_TARGET:-127.0.0.1:8088}}"

DEVICE_DIR="${DEVICE_DIR:-/data/local/tmp}"
DEVICE_BIN="$DEVICE_DIR/tak-agent"
OUT_DIR="${OUT_DIR:-./tak-agent-runs}"

if [[ -z "$AGENT_BIN" || -z "$SCENARIO" ]]; then
    echo "Usage: $0 <agent-binary> <scenario|all> [target]" >&2
    exit 64
fi

if [[ ! -x "$AGENT_BIN" ]]; then
    echo "$AGENT_BIN: not found or not executable" >&2
    exit 66
fi

if ! command -v adb >/dev/null 2>&1; then
    echo "adb not on PATH; install android-tools" >&2
    exit 127
fi

DEVICE_COUNT=$(adb devices | awk 'NR>1 && $2=="device" {n++} END {print n+0}')
if [[ "$DEVICE_COUNT" -lt 1 ]]; then
    echo "no authorized adb device — check \`adb devices\`" >&2
    exit 1
fi
if [[ "$DEVICE_COUNT" -gt 1 ]]; then
    echo "multiple devices; set ANDROID_SERIAL=<serial> to disambiguate" >&2
    exit 1
fi

echo "==> push $AGENT_BIN -> device:$DEVICE_BIN"
adb push "$AGENT_BIN" "$DEVICE_BIN" >/dev/null
adb shell "chmod 755 $DEVICE_BIN"

mkdir -p "$OUT_DIR"
TS=$(date -u +%Y%m%dT%H%M%SZ)
OUT_FILE="$OUT_DIR/$TS-$SCENARIO.jsonl"

# Choose the right verb. `all` runs every scenario; anything else
# gets passed as a single scenario name to `tak-agent run`.
echo "==> running on device against target=$TARGET"
if [[ "$SCENARIO" == "all" ]]; then
    REMOTE_CMD="$DEVICE_BIN all --target '$TARGET' --json"
else
    REMOTE_CMD="$DEVICE_BIN run '$SCENARIO' --target '$TARGET' --json"
fi

# stderr (logs) goes to a sidecar so stdout stays clean JSON-lines.
ERR_FILE="$OUT_DIR/$TS-$SCENARIO.stderr"
set +e
adb shell "$REMOTE_CMD" 1> "$OUT_FILE" 2> "$ERR_FILE"
RC=$?
set -e

echo "==> exit $RC"
echo "==> stdout (jsonl): $OUT_FILE"
echo "==> stderr:         $ERR_FILE"
echo
cat "$OUT_FILE"

exit "$RC"
