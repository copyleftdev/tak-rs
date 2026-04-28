# bench/java/ — Upstream Java TAK Server bench harness

Drives the upstream open-source TAK Server through the same loadgen we use against tak-rs and captures throughput numbers for `docs/perf-comparison.md` §3.1.

## What runs

Pulled from Docker Hub: **`pvarki/takserver:5.7-RELEASE-8-...`** — a community-maintained image that wraps the upstream `5.7-RELEASE-8` source tree (matching what we recon under `.scratch/takserver-java/`). Pvarki's image bundles the multi-process orchestration, Flyway schema migrations, cert generation, and Spring Boot configurations that the from-scratch gradle build path requires us to reassemble by hand. Their `docker-compose.yml` runs **6 containers**:

- `takdb` (postgis-15-3.3)
- `takserver_initialization` (one-shot: cert + schema setup)
- `takserver_config` — Ignite config service
- `takserver_messaging` — the firehose / `<input>` listener (the bench target)
- `takserver_api` — Spring REST / web UI
- `takserver_pluginmanager`
- `takserver_retention`

We do **two** small modifications to the pvarki setup to make it match what `taktool loadgen` sends:

1. `templates/CoreConfig.tpl` gets a second `<input>` element on plain `stcp:8088` (alongside their default `tls:8089`). See `CoreConfig.tpl.patch` for the exact diff.
2. `docker-compose.yml` mounts the patched template into each takserver_* service via a volume bind.

## Drive it

The orchestrator is `scripts/bench-java-baseline.sh`:

```bash
# Default: 2000 conn × 100 msg/s × 20 s
scripts/bench-java-baseline.sh

# Match the Rust headline (1 M offered)
scripts/bench-java-baseline.sh --connections 5000 --rate 200 --duration 30
```

Output lands in `bench/history/java-baseline-*.json`. The script:

1. Clones pvarki/docker-atak-server to `.scratch/pvarki-tak/docker-atak-server` if not present
2. Patches `templates/CoreConfig.tpl` with our `stcp:8088` input
3. Patches `docker-compose.yml` to bind-mount the template
4. `docker pull pvarki/takserver:...` (~1.6 GB)
5. `docker compose up -d`, waits for `:8088` to bind + messaging service to log "Started TAK Server messaging Microservice"
6. Runs `scripts/bench-baseline.sh` against `127.0.0.1:8088` with `--tag java-baseline`
7. `docker compose down -v`

## Captured baseline (2026-04-28)

`bench/history/java-baseline-headline-2026-04-28T16-22-00Z.json`:

| Configuration | Sustained | RSS (peak) | CPU (peak) | Errors |
|---|---|---|---|---|
| 5 000 conn × 200 msg/s × 30 s | **853 348 msg/s** | 47.8 GB across all 5 containers (44 GB on messaging alone) | **4 735 % across all** (4 677 % on messaging alone) | 0 |

That's ~17 % more throughput than tak-rs's compio path (603 k msg/s) — but at **~6 × the CPU** and **~50 × the RAM**. tak-rs's win is efficiency, not raw throughput, when the upstream is given enough hardware to throw at the problem. See `docs/perf-comparison.md` §3.1.a for the full comparison.

## Why pvarki's image instead of building from source

We tried building from `.scratch/takserver-java/` directly via gradle. It works in pieces but the upstream multi-process startup has hard dependencies (Ignite cluster topology, RSA keystore for the JWT bean, federation manager initialization order) that the official `docker_entrypoint.sh` handles via a precise launch sequence we don't fully reproduce. The pvarki image bakes those in. From-source build is ~20-30 min on a cold gradle cache and adds little for the bench compared to a 2-minute image pull, so the orchestrator now uses pvarki by default.

If you need to bench against a from-source build (e.g. for a tak-rs vs internal-fork comparison), the gradle invocation that succeeded was:

```bash
docker run --rm \
    -v "$PWD/.scratch/takserver-java:/build" \
    -v gradle-cache-host:/.gradle \
    -w /build/src \
    --network host \
    eclipse-temurin:17-jdk-jammy \
    bash -c '
        apt-get update -qq && apt-get install -y -qq patch git rpm
        useradd -u 1000 -m -s /bin/bash builder
        chown -R builder:builder /build /tmp/home
        runuser -u builder -- ./gradlew --no-daemon -x test \
            :takserver-tool-ui:bundle \
            :takserver-core:bootJar \
            :takserver-core:bootWar \
            :takserver-schemamanager:shadowJar
    '
```

— produces the bootJar/bootWar/SchemaManager fat jars under `.scratch/takserver-java/src/*/build/libs/`. The remaining work to make those into a standalone runtime is the same multi-process orchestration that pvarki already solved.
