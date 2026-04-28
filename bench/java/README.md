# bench/java/ — Upstream Java TAK Server bench harness

This directory holds everything needed to spin up the upstream open-source TAK Server (`.scratch/takserver-java/`) under loadgen and capture a Java baseline number for `docs/perf-comparison.md`.

The harness is driven end-to-end by **`scripts/bench-java-baseline.sh`**.

## What's here

| File | Purpose |
|------|---------|
| `Dockerfile` | Wraps `tak/` (assembled at bench time from gradle outputs) + `eclipse-temurin:17-jdk-jammy` |
| `docker-compose.yml` | takserver + postgis-15 sidecar, ports 8087/8088/8443 published on loopback |
| `CoreConfig.xml` | Plain TCP `stcp` on 8088 + plain `tcp` on 8087, anonymous auth, no TLS, no LDAP |
| `TAKIgniteConfig.xml` | Stock Ignite settings for single-node messaging (copied from upstream example) |
| `UserAuthenticationFile.xml` | Empty (anonymous-only — server consults this file but never matches a user) |
| `docker_entrypoint.sh` | Stock upstream entry script (cert generation + 4-process supervisor) |

`tak/` is **not committed**. It gets assembled by `scripts/bench-java-baseline.sh` step (2) from the gradle build outputs under `.scratch/takserver-java/src/`.

## Procedure

```bash
# 1. Make sure .scratch/takserver-java/ is populated.
# Already there per CLAUDE.md.

# 2. Run the harness. First invocation triggers the gradle build
#    inside an eclipse-temurin:17-jdk-jammy container (~10-30 min on a
#    cold gradle cache; subsequent runs hit the cached
#    `gradle-cache` named volume).
scripts/bench-java-baseline.sh \
    --connections 2000 --rate 100 --duration 20

# 3. Output lands at bench/history/java-baseline-<UTC>.json
#    (same JSON shape as the rust runs).
```

## Why this is in the repo

We don't redistribute upstream binaries — every invocation pulls + builds from `.scratch/takserver-java/` (which is itself a clone the operator does once). The Dockerfile / config / scripts here are purely orchestration glue and small enough to commit.

## What this does NOT measure

- **No mTLS handshake on 8089.** TAK production uses mTLS; the bench uses plain `stcp` on 8088 to match what the Rust firehose currently exposes. Both sides therefore measure raw dispatch + persistence; both skip the same TLS overhead.
- **No federation, no plugin manager, no API/web UI in the loop.** The bench only exercises the messaging service (the firehose path). The API service is launched alongside (Spring Boot expects it to bind 8443) but receives zero traffic.
- **JIT warm-up** — `bench-java-baseline.sh` sleeps 10 s after the listener comes up before launching loadgen. For a strict "fair fight" against tak-rs's already-warm `--release` binary, run with `--duration 60` and discard the first 10 s of metrics manually.

## When the upstream build breaks

The upstream gradle build is fragile (~50 subprojects, deprecated dependencies, occasional Java version friction). Failure modes we've hit:

- Missing `patch` / `git` / `rpm` in the builder. Already worked-around in `bench-java-baseline.sh` step 1.
- Java version mismatch — upstream README says "Requires Java 17". The orchestrator pins `eclipse-temurin:17-jdk-jammy` so host JDK doesn't matter.
- npm tooling missing — some subprojects have a JS web UI built via `npm run`. The script currently builds `:takserver-core` only, which doesn't pull the UI in. If you ever need the full distribution, install Node 18 in the builder.

## After the run

`bench/history/java-baseline-*.json` matches the schema of the Rust runs. Drop the numbers into the Java row of `docs/perf-comparison.md` §3.1.a and re-run the comparison verdict via `scripts/bench-comparison.sh`.

## Current status (2026-04-28)

This harness **builds and runs the upstream messaging service end-to-end** through the following sequence:

1. ✅ **Gradle build** — `:takserver-core:bootJar` produces a 218 MB Spring Boot fat jar containing the messaging + config + API profiles.
2. ✅ **Cert generation** — `start.sh` creates a self-signed RSA keystore + JKS truststore at container start so the JWT encoder bean can instantiate (without it, messaging NPEs on startup).
3. ✅ **Multi-profile launch** — `start.sh` spawns config + messaging in parallel; they self-coordinate via Ignite cluster discovery (sequential startup deadlocks because config doesn't fully come up until a peer joins).
4. ✅ **CoreConfig minimal** — federation disabled, persistence disabled, plain `stcp` on 8088 with `coreVersion="2"`.
5. ✅ **Listener binds** — ports 8087, 8088, 8443 are visible via `ss -ltn` ~60 s after container start.
6. ⚠️ **Frame handling blocks at the first message.** The Java `stcp` input accepts a TCP connection, reads the first framed `TakMessage`, then closes the connection (`BrokenPipe`). Investigation of `takserver-messaging.log` shows `DistributedFederationManager.init` raises an `IgniteServiceProcessor` NPE during Ignite service deployment — even with `<federation enableFederation="false"/>`, the federation manager's deployment task still runs and fails on a null `SSLConfig.getInstance()` reference. This appears to be a known interaction in the upstream codebase and likely needs either a working Ignite cluster topology with the `api` profile also running, or a code-level workaround we'd have to patch in.

The harness as committed gets *very* close — what's blocking is upstream-Java initialization quirks, not anything on the tak-rs side. A TAK Server admin who knows the official deployment recipe (CoreConfig.xsd + production cert provisioning + Ignite tuning) could likely close the gap by:

- Running the full upstream `docker_entrypoint.sh` (which also launches the API + plugin manager profiles, plus runs `SchemaManager` migrations) instead of our minimal `start.sh`,
- Or pulling a pre-built `takserver-full` image from the operator's internal registry instead of building from source.

Until then the Java row in `docs/perf-comparison.md` §3.1.a stays TBD. The Rust numbers in that table are real and reproducible; the comparison verdict is just waiting on a working Java target.
