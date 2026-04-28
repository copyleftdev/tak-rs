# bench/ — performance harness output

This directory holds raw run data from `scripts/bench-baseline.sh` and
the generated comparison report at `docs/perf-comparison.md`.

## Layout

```
bench/
├── README.md                            # this file
└── history/
    ├── .gitkeep
    ├── rust-2026-04-28T09-15-00Z.json   # one run per file
    └── java-baseline-2026-04-28T09-20-00Z.json
```

Each `<tag>-<UTC ISO timestamp>.json` is the merged output of one
`bench-baseline.sh` invocation:

```json
{
  "loadgen": { "tag": "rust", "target": "...", "connections": 1000,
               "rate": 10, "msg_per_s": 9876.5, ... },
  "system":  { "peak_cpu_pct": 42.0, "max_rss_mb": 320 },
  "captured_at": "2026-04-28T09-15-00Z"
}
```

## Running

```bash
# Rust side
scripts/bench-baseline.sh \
    --target 127.0.0.1:8088 \
    --connections 1000 --rate 10 --duration 60 \
    --tag rust

# Java upstream side (after standing up the upstream takserver)
scripts/bench-baseline.sh \
    --target 127.0.0.1:8088 \
    --connections 1000 --rate 10 --duration 60 \
    --tag java-baseline
```

## Bringing up the Java upstream server

Issue #37 mandates a "Java baseline harness" but does not vendor a
container — TAK Server licensing requires the operator to fetch the
upstream tarball directly. The recipe:

1. Clone the upstream repo into `.scratch/takserver-java/` (we already
   do this for protocol recon).
2. Build a release tarball: `cd .scratch/takserver-java && ./gradlew
   takserver-package:installDist`. The output lands in
   `takserver-package/build/distributions/`.
3. Provision a Docker image **outside this repo** that
   - installs JDK 17, Postgres + PostGIS, and the upstream tarball,
   - launches `takserver` with a known config that pins port 8088
     (plain CoT) for the bench (mTLS adds variance we don't want for
     this test).
4. Run that image with `-p 8088:8088 -p 5432:5432`.
5. Point `bench-baseline.sh --target 127.0.0.1:8088 --tag
   java-baseline` at it.

We deliberately do not commit the Dockerfile — TAK Server is not
freely redistributable. Each operator builds their own.

## Reading the comparison

`docs/perf-comparison.md` is a TEMPLATE that gets rendered by hand
once both `rust-*.json` and `java-baseline-*.json` exist for the same
load configuration. M5 acceptance: Rust ≥3× Java throughput at the
target load (issue #38).
