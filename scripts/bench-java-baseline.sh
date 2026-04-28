#!/usr/bin/env bash
# bench-java-baseline.sh — bring up the upstream Java TAK Server via
# the pvarki/takserver community image, capture loadgen JSON for the
# Java row in docs/perf-comparison.md, tear down.
#
# Issue: https://github.com/copyleftdev/tak-rs/issues/37 (M5)
#
# Why pvarki: their image bakes in the multi-process orchestration
# (config + messaging + API + plugin manager + retention) plus cert
# generation and Flyway schema migrations. Building those from
# `.scratch/takserver-java/` reaches "ports bound" but the messaging
# service hits a DistributedFederationManager NPE we don't have time
# to fix. See bench/java/README.md for details.
#
# Usage:
#   scripts/bench-java-baseline.sh [--connections N] [--rate R] [--duration S]
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
PVARKI_DIR="$REPO_ROOT/.scratch/pvarki-tak/docker-atak-server"
BENCH_JAVA="$REPO_ROOT/bench/java"

CONNECTIONS=2000
RATE=100
DURATION=20
TAG="java-baseline"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --connections) CONNECTIONS="$2"; shift 2 ;;
        --rate)        RATE="$2"; shift 2 ;;
        --duration)    DURATION="$2"; shift 2 ;;
        --tag)         TAG="$2"; shift 2 ;;
        -h|--help)     sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 1 ;;
    esac
done

# ---------------------------------------------------------------------------
# 1. Clone pvarki/docker-atak-server if missing
# ---------------------------------------------------------------------------
if [[ ! -d "$PVARKI_DIR" ]]; then
    echo "[bench-java] cloning pvarki/docker-atak-server..."
    mkdir -p "$(dirname "$PVARKI_DIR")"
    git clone --depth 1 https://github.com/pvarki/docker-atak-server.git "$PVARKI_DIR"
fi
cd "$PVARKI_DIR"

# ---------------------------------------------------------------------------
# 2. Patch the template + compose
# ---------------------------------------------------------------------------
# Add stcp:8088 input if not already present
if ! grep -q "streamtcp.*stcp.*8088" templates/CoreConfig.tpl; then
    echo "[bench-java] patching CoreConfig.tpl: adding stcp:8088 input..."
    sed -i 's|<input _name="stdssl" protocol="tls" port="8089" coreVersion="2"/>|<input _name="stdssl" protocol="tls" port="8089" coreVersion="2"/>\n        <input _name="streamtcp" protocol="stcp" port="8088" auth="anonymous" coreVersion="2"/>|' templates/CoreConfig.tpl
fi

# Mount the template into each takserver_* service so our edits take effect
if ! grep -q 'CoreConfig.tpl:/opt/templates' docker-compose.yml; then
    echo "[bench-java] patching docker-compose.yml: bind-mounting CoreConfig.tpl..."
    sed -i 's|takserver_data:/opt/tak/data$|takserver_data:/opt/tak/data\n      - ./templates/CoreConfig.tpl:/opt/templates/CoreConfig.tpl:ro|g' docker-compose.yml
fi

# Add 8088 to exposed ports on takserver_config (the network namespace owner)
if ! grep -q "8088:8088" docker-compose.yml; then
    echo "[bench-java] patching docker-compose.yml: exposing 8088..."
    sed -i "s|^      - .8089:8089.|      - \"127.0.0.1:8088:8088\"\n      - \"127.0.0.1:8089:8089\"|" docker-compose.yml
fi

# ---------------------------------------------------------------------------
# 3. Provision the env file
# ---------------------------------------------------------------------------
if [[ ! -f takserver.env ]]; then
    echo "[bench-java] writing takserver.env..."
    cat > takserver.env <<'ENVEOF'
TAK_SERVER_ADDRESS=127.0.0.1
TAK_SERVER_NAME=tak-bench
POSTGRES_PASSWORD=bench-only
ADMIN_CERT_PASS=atakatak
TAKSERVER_CERT_PASS=atakatak

COUNTRY=US
CA_NAME=tak-bench-ca
CA_PASS=atakatak
STATE=VA
CITY=Reston
ORGANIZATION=tak-rs-bench
ORGANIZATIONAL_UNIT=bench

ADMIN_CERT_NAME=admin
POSTGRES_DB=cot
POSTGRES_USER=martiuser
POSTGRES_ADDRESS=takdb
POSTGRES_SUPERUSER=martiuser
POSTGRES_SUPER_PASSWORD=bench-only

PVARKI_DOCKER_REPO=
TAK_RELEASE=5.7-RELEASE-8-d2.8.2-local-254-merge-2026-04-28
DOCKER_TAG_EXTRA=
ENVEOF
fi

# ---------------------------------------------------------------------------
# 4. Up
# ---------------------------------------------------------------------------
echo "[bench-java] docker compose up..."
docker compose down -v 2> /dev/null || true
docker compose up -d

# ---------------------------------------------------------------------------
# 5. Wait for messaging to be ready
# ---------------------------------------------------------------------------
echo "[bench-java] waiting for messaging service (up to 5 min)..."
deadline=$(( $(date +%s) + 300 ))
until docker logs docker-atak-server-takserver_messaging-1 2>&1 | grep -q "Started TAK Server messaging Microservice" \
    || (( $(date +%s) > deadline )); do
    sleep 5
done
if ! docker logs docker-atak-server-takserver_messaging-1 2>&1 | grep -q "Started TAK Server messaging Microservice"; then
    echo "[bench-java] messaging never came up; recent logs:"
    docker compose logs --tail 80 takserver_messaging
    docker compose down -v
    exit 1
fi
echo "[bench-java] messaging ready; sleeping 10 s for JIT warm-up..."
sleep 10

# ---------------------------------------------------------------------------
# 6. Capture per-container CPU/RSS BEFORE the run, and a peak sample MID-run
# ---------------------------------------------------------------------------
echo "[bench-java] running loadgen..."
TAK_PID="$(docker inspect -f '{{.State.Pid}}' docker-atak-server-takserver_messaging-1)"
bash "$SCRIPT_DIR/bench-baseline.sh" \
    --target 127.0.0.1:8088 \
    --connections "$CONNECTIONS" \
    --rate "$RATE" \
    --duration "$DURATION" \
    --tag "$TAG" \
    --pid "$TAK_PID" &
LOADGEN_PID=$!

# Sample docker stats at the midpoint
sleep $((DURATION / 2))
DOCKER_STATS="$(docker stats --no-stream --format '{{.Name}} {{.CPUPerc}} {{.MemUsage}}' \
    docker-atak-server-takserver_messaging-1 \
    docker-atak-server-takserver_config-1 \
    docker-atak-server-takserver_api-1 \
    docker-atak-server-takserver_pluginmanager-1 \
    docker-atak-server-takserver_retention-1 \
    2> /dev/null)"

wait $LOADGEN_PID
JSON_PATH="$(ls -t "$REPO_ROOT/bench/history/${TAG}"-*.json | head -1)"

# ---------------------------------------------------------------------------
# 7. Inject the docker-stats sample into the JSON
# ---------------------------------------------------------------------------
echo "[bench-java] docker stats mid-run:"
echo "$DOCKER_STATS"
python3 - <<PYEOF
import json, re
stats = """$DOCKER_STATS"""
total_cpu = 0.0
total_mem_mb = 0.0
msg_cpu = 0.0
msg_mem_mb = 0.0
for line in stats.strip().splitlines():
    parts = line.split()
    name = parts[0]
    cpu = float(parts[1].rstrip('%'))
    mem = parts[2]  # like "44.18GiB" or "1.024GiB" or "535.7MiB"
    m = re.match(r"([\d.]+)(GiB|MiB)", mem)
    if m:
        v = float(m.group(1))
        if m.group(2) == "GiB": v *= 1024
        total_mem_mb += v
        total_cpu += cpu
        if "messaging" in name:
            msg_cpu = cpu
            msg_mem_mb = v
with open("$JSON_PATH") as f:
    d = json.load(f)
d["system"] = {
    "source": "docker stats (mid-run)",
    "messaging_cpu_pct": msg_cpu,
    "messaging_rss_mb": msg_mem_mb,
    "all_containers_cpu_pct": total_cpu,
    "all_containers_rss_mb": total_mem_mb
}
with open("$JSON_PATH","w") as f:
    json.dump(d, f, indent=2)
print(f"[bench-java] patched {('$JSON_PATH').split('/')[-1]}")
PYEOF

# ---------------------------------------------------------------------------
# 8. Teardown
# ---------------------------------------------------------------------------
docker compose down -v
echo "[bench-java] done. Look in bench/history/${TAG}-*.json"
echo "[bench-java] result: $JSON_PATH"
