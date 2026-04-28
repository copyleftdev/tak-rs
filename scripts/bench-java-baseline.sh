#!/usr/bin/env bash
# bench-java-baseline.sh — bring up the upstream Java TAK Server,
# capture loadgen JSON, tear down. Used to populate the Java row in
# docs/perf-comparison.md.
#
# Issue: https://github.com/copyleftdev/tak-rs/issues/37
#
# Steps:
#   1. gradle build (one-time, in eclipse-temurin:17-jdk-jammy)
#   2. assemble bench/java/tak/ from gradle outputs
#   3. docker compose up
#   4. wait for the streaming-TCP CoT input on :8088 to come up
#   5. run scripts/bench-baseline.sh against it with --tag java-baseline
#   6. docker compose down -v
#
# Usage:
#   scripts/bench-java-baseline.sh [--connections N] [--rate R] [--duration S]
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
SCRATCH="$REPO_ROOT/.scratch/takserver-java"
JAVA_BENCH="$REPO_ROOT/bench/java"

CONNECTIONS=2000
RATE=100
DURATION=20

while [[ $# -gt 0 ]]; do
    case "$1" in
        --connections) CONNECTIONS="$2"; shift 2 ;;
        --rate)        RATE="$2"; shift 2 ;;
        --duration)    DURATION="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 1 ;;
    esac
done

# ---------------------------------------------------------------------------
# 1. gradle build
# ---------------------------------------------------------------------------
if ! ls "$SCRATCH/src/takserver-core/build/libs/"takserver-core-*.jar > /dev/null 2>&1; then
    echo "[bench-java] gradle build (this is slow — ~10-30 min on first run)..."
    # Build only :takserver-core:bootJar — we run the Spring Boot fat
    # jar directly with the `messaging` profile and skip the
    # multi-process orchestrator entirely. Avoids the takserver-tool-ui
    # npm dependency that explodes upstream gradle.
    docker run --rm \
        -v "$SCRATCH:/build" \
        -v gradle-cache:/root/.gradle \
        -w /build/src \
        --network host \
        eclipse-temurin:17-jdk-jammy \
        bash -c '
            set -e
            apt-get update -qq
            apt-get install -y -qq patch git rpm > /dev/null
            ./gradlew --no-daemon -x test :takserver-core:bootJar
        '
else
    echo "[bench-java] takserver-core-*.jar found — skipping gradle build"
fi

# ---------------------------------------------------------------------------
# 2. assemble tak/
# ---------------------------------------------------------------------------
echo "[bench-java] assembling bench/java/tak/ from gradle outputs..."
rm -rf "$JAVA_BENCH/tak"
mkdir -p "$JAVA_BENCH/tak"
cp "$SCRATCH/src/takserver-core/build/libs/"takserver-core-*.jar "$JAVA_BENCH/tak/"
# Cert helper scripts the messaging service references on startup
# even when no TLS input is configured.
mkdir -p "$JAVA_BENCH/tak/certs"
if [[ -d "$SCRATCH/src/takserver-core/scripts/certs" ]]; then
    cp -r "$SCRATCH/src/takserver-core/scripts/certs/." "$JAVA_BENCH/tak/certs/"
fi
echo "[bench-java] tak/ contents:"
ls -1 "$JAVA_BENCH/tak/"

# ---------------------------------------------------------------------------
# 3. docker compose up
# ---------------------------------------------------------------------------
cd "$JAVA_BENCH"
echo "[bench-java] docker compose up..."
docker compose up -d --build

# ---------------------------------------------------------------------------
# 4. wait for the listener on :8088
# ---------------------------------------------------------------------------
echo "[bench-java] waiting for tak-server :8088..."
deadline=$(( $(date +%s) + 300 ))   # 5 minutes
until ss -ltn 2> /dev/null | grep -q ':8088' || (( $(date +%s) > deadline )); do
    sleep 2
done
if ! ss -ltn 2> /dev/null | grep -q ':8088'; then
    echo "[bench-java] tak-server :8088 never came up after 5 min — dumping logs:"
    docker compose logs --tail 100 tak-server
    docker compose down -v
    exit 1
fi
echo "[bench-java] :8088 is up; allowing 10 s for JIT warm-up..."
sleep 10

# ---------------------------------------------------------------------------
# 5. run the loadgen via bench-baseline.sh
# ---------------------------------------------------------------------------
TAK_PID="$(docker inspect -f '{{.State.Pid}}' "$(docker compose ps -q tak-server)")"
echo "[bench-java] target pid=$TAK_PID"
bash "$SCRIPT_DIR/bench-baseline.sh" \
    --target 127.0.0.1:8088 \
    --connections "$CONNECTIONS" \
    --rate "$RATE" \
    --duration "$DURATION" \
    --tag java-baseline \
    --pid "$TAK_PID"

# ---------------------------------------------------------------------------
# 6. teardown
# ---------------------------------------------------------------------------
echo "[bench-java] tearing down..."
docker compose down -v

echo "[bench-java] done. Look in bench/history/java-baseline-*.json"
