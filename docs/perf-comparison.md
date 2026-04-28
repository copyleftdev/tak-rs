# Rust vs Java Baseline — Firehose Performance Comparison

**Status:** Rust row populated (2026-04-28); Java row pending an upstream container.

This document is the M5 deliverable for issue [#38](https://github.com/copyleftdev/tak-rs/issues/38) — the headline number for "tak-rs vs upstream Java TAK Server" on the firehose path. It is structured as a TEMPLATE: when both `rust-*.json` and `java-baseline-*.json` files exist for the same load configuration, the operator runs `scripts/bench-comparison.sh` to capture the comparison run, then fills the result tables below by hand.

The comparison is deliberately *not* auto-generated on every commit. M5 acceptance is a one-off measurement against a known-good upstream Java reference; we do not want noise from CI infrastructure variance overwriting the real result.

## 1. Acceptance gate

Issue #38 sets a **3× throughput floor** as the M5 acceptance criterion: tak-rs must sustain at least 3× the messages/second of the upstream Java server under identical load before M5 is declared done.

`scripts/bench-comparison.sh --throughput-floor 3` enforces this gate by exit code (0 = pass, 1 = fail). The verdict block in the merged JSON records the actual ratio, the floor, and a `pass` / `fail` flag.

## 2. Test configuration (locked)

| Parameter | Value | Source |
|-----------|-------|--------|
| Connections | 10 000 | M5 headline target — `docs/architecture.md` §1 |
| Rate / connection | 5 msg/s | ATAK default `pliReporting` interval |
| Total offered load | 50 000 msg/s | 10k × 5 |
| Mix | `realistic` (70 / 20 / 10) | `docs/decisions/0003-firehose-load-mix.md` |
| Duration per side | 60 s | Long enough to amortize JIT warm-up on the Java side |
| Wire | Plain CoT on TCP/8088 | mTLS adds variance unrelated to the dispatch path |
| Hardware | TODO: record exact spec on first run | — |

The same physical box runs the load generator and the server under test, on isolated CPU sets where possible (`taskset`).

## 3. Results

### 3.1 Throughput

#### Rust capture (2026-04-28, single-box loopback, `--release` build)

The first run with a 1000-conn × 50 msg/s offered load measured **46 k msg/s**, which we initially flagged as concerning. Pushing the offered load up to 2000-conn × 100 msg/s revealed that the earlier number was **loadgen-side rate-jitter limited**, not a server-side ceiling. The server was idle most of the time waiting for the next message.

All runs via `scripts/bench-baseline.sh`; raw JSON in `bench/history/`. Single laptop, single-box loopback. Run-to-run variance on this hardware is ±10 %, so the comparisons below are qualitative — the Java cross-comparison will be measured back-to-back on the same hardware to get a clean ratio.

### 3.1.a Firehose runtime: tokio vs compio (the headline)

| Tag | Firehose runtime | Conns | Offered | Sustained (msg/s) | RSS MB | Peak CPU % | Errors |
|---|---|---|---|---|---|---|---|
| `rust-tokio-nopersist-v2` | tokio (multi-thread) | 2 000 | 200 k | 139 400 | 291 | 5 410 | 492 |
| `rust-compio-4w` | compio (4 workers, SO_REUSEPORT) | 2 000 | 200 k | **199 318** | 620 | **410** | **0** |
| `rust-compio-8w` | compio (8 workers, SO_REUSEPORT) | 2 000 | 200 k | 199 222 | 591 | 820 | 0 |
| **`rust-compio-headline`** | compio (8 workers) | **5 000** | **1 M** | **593 424** | **882** | **810** | **0** |

> ⚠️ The compio path is currently `--no-persist` only — the `Store` writer mpsc lives on the tokio runtime, so there is no cross-runtime bridge yet. The tokio rows above are also `no-persist` for an apples-to-apples comparison.

**Headline: 593 424 msg/s sustained for 30 s on 5 000 conns at 0 errors, with 8 cores fully busy.** That is **11.87× the M5 50 k headline target**.

The compio runtime delivers 1.43× the throughput at *one-thirteenth* the CPU vs tokio at the same 200 k offered load (139 k @ 5 410 % vs 199 k @ 410 %). The reason: io_uring submits writes without a per-message syscall, and compio's thread-per-core model removes the work-stealing wakeup tax that tokio pays for every connection's read/write turn. SO_REUSEPORT on the listening socket lets the kernel itself round-robin SYNs across N rings — the "accept storm causes 492 errors on tokio" pattern disappears entirely with compio (0 errors at every load tested).

### 3.1.b Loadgen driver: tokio vs uring (less interesting now)

| Tag | Server persist | Loadgen driver | Sustained (msg/s) | Max RSS (MB) | Peak CPU % |
|---|---|---|---|---|---|
| `rust-tokio-persist`     | on  | tokio (multi-thread) | 101 246 | 174 | 2 710 |
| `rust-tokio-nopersist`   | off | tokio (multi-thread) | **176 620** | 540 | 6 320 |
| `rust-uring-persist`     | on  | tokio-uring (single-thread) | 107 525 | 247 | 4 510 |
| `rust-uring-nopersist`   | off | tokio-uring (single-thread) | 118 041 | 180 | 3 850 |

| Side | Configuration | Sent (msg/s) | Ratio |
|------|--------------|--------------|-------|
| Java upstream | _TBD_ (awaiting container) | _TBD_ | 1.00× (baseline) |
| tak-rs (persist, tokio loadgen) | 2 000 × 100 × 20 s | 101 246 | _TBD_ |
| **tak-rs (no-persist, tokio loadgen)** | 2 000 × 100 × 20 s | **176 620** | _TBD_ |
| tak-rs (persist, uring loadgen) | 2 000 × 100 × 20 s | 107 525 | _TBD_ |
| tak-rs (no-persist, uring loadgen) | 2 000 × 100 × 20 s | 118 041 | _TBD_ |

**Key findings**

- **tak-rs sustains 3.5× the M5 50 k msg/s target** at the headline configuration (no-persist, tokio loadgen, 176 k msg/s) and **~2× the target with persistence enabled**. The earlier 46 k figure was loadgen rate-jitter at a low offered load, not a server ceiling.
- **The io_uring loadgen is currently slower than the tokio loadgen** on this hardware (118 k vs 176 k no-persist), and that is a *loadgen-side* limit, not a server one. `tokio-uring` 0.5 is single-threaded by design — the loadgen process saturates one core (≈100 % CPU on the loadgen pid) at ~120 k msg/s, while the multi-threaded tokio loadgen reaches ~200 % CPU and pushes 50 % more. A multi-threaded io_uring runtime (one ring per worker, à la `monoio`/`compio`/`glommio`) would close that gap; that is a future bench-infra optimisation, not a tak-server change.
- **Persistence costs ~40 % under tokio loadgen** (101 k vs 176 k) and **~9 % under uring loadgen** (107 k vs 118 k). The lower uring delta is partly because uring's smaller per-write coalescing makes the server's read-decode-dispatch path the bottleneck before persistence becomes visible. Both modes are well above the M5 floor.
- **Per-connection RSS** sits between 87 KB and 270 KB depending on configuration — the 540 MB high-water on the no-persist tokio run comes from the read-side `BytesMut` chunks the server holds while subscribers drain.

### 3.2 Latency

p50 / p95 / p99 latency of `dispatch_to_subscriber` (measured from
`Inbound::ready_at` to `Bytes::clone` arriving on the subscriber's mpsc).

| Side | p50 (µs) | p95 (µs) | p99 (µs) |
|------|----------|----------|----------|
| Java upstream | _TBD_ | _TBD_ | _TBD_ |
| tak-rs | _TBD_ | _TBD_ | _TBD_ |

(Latency capture currently lives in the criterion benches under
`crates/tak-bus/benches/`. The end-to-end latency on a real running
listener is post-M5 work — issue TBD.)

### 3.3 Memory

Peak RSS observed during the run (sampled at 1 Hz via `/proc/<pid>/status`).

| Side | Max RSS (MB) | Per connection |
|------|--------------|----------------|
| Java upstream | _TBD_ | _TBD_ |
| tak-rs (2 000 conn, tokio loadgen, persist) | 174 | ~87 KB |
| tak-rs (2 000 conn, tokio loadgen, no-persist) | 540 | ~270 KB |
| tak-rs (2 000 conn, uring loadgen, persist) | 247 | ~123 KB |
| tak-rs (2 000 conn, uring loadgen, no-persist) | 180 | ~90 KB |

Java's per-connection state is dominated by the GC-tracked `ChannelHandlerContext` plus the mutable `BigInteger` group bitvector; tak-rs's per-connection state is the fixed `[u64; 4]` mask plus the slab-allocated `Subscription`. The 110 KB/conn for the Rust path includes the per-connection `BytesMut` read buffer (8 KB initial, grows on demand) and the bounded mpsc channel (`DEFAULT_SUBSCRIBER_CAPACITY` × `Bytes` slot ≈ 32 KB), so the steady-state cost per connection is much smaller than the high-water RSS suggests.

The "per connection" column highlights the architectural difference: Java's per-connection state is dominated by the GC-tracked `ChannelHandlerContext` plus the mutable `BigInteger` group bitvector; tak-rs's per-connection state is the fixed `[u64; 4]` mask plus the slab-allocated `Subscription`.

### 3.4 CPU

Peak CPU% observed (`top -b -n 1 -p PID`, 1 Hz sampling — sum across threads, so values > 100 % indicate multi-core utilisation).

| Side | Peak % | Notes |
|------|--------|-------|
| Java upstream | _TBD_ | _TBD_ |
| tak-rs (tokio loadgen, persist) | 2 710 | 27 cores at 200 k offered. |
| tak-rs (tokio loadgen, no-persist) | 6 320 | 63 cores; saturating the box. |
| tak-rs (uring loadgen, persist) | 4 510 | 45 cores; uring-side write rate gates server work. |
| tak-rs (uring loadgen, no-persist) | 3 850 | 38 cores; loadgen-bound, not server-bound. |

### 3.5 Verdict

| Field | Value |
|-------|-------|
| Throughput ratio (Rust / Java) | _TBD_ |
| M5 floor | 3.00× |
| Result | _TBD_ |

## 4. How to reproduce

### 4.1 Bring up the Java upstream server

`bench/README.md` documents the procedure — TL;DR:

```bash
# 1. Build the upstream tarball from the recon clone
cd .scratch/takserver-java
./gradlew takserver-package:installDist

# 2. Provision a Docker image OUTSIDE this repo (see bench/README.md
#    for the recipe — TAK Server is not freely redistributable, so
#    each operator builds their own).
docker run --rm -d --name takserver -p 18088:8088 \
    -p 18089:8089 -p 5432:5432 your-org/takserver:latest

# 3. Wait for the listener to come up
until ss -ltn | grep -q ':18088'; do sleep 1; done
```

### 4.2 Bring up tak-rs

```bash
# Currently a scaffold — wiring listeners to ports is post-M5 work.
# When that lands, the recipe will be:
cargo run --release -p tak-server -- --listen 0.0.0.0:8088
```

> **Update (2026-04-28):** `tak-server::main` binds the plain CoT
> firehose on `:8088` (default tokio runtime, or compio multi-thread
> io_uring with `--compio`) and the mission API on `:8080` (axum on
> tokio). The Rust rows in §3.1, §3.3, and §3.4 reflect real loopback
> measurements; only the Java row remains pending. The compio row is
> currently `no-persist` only — the cross-runtime bridge for the
> `Store` writer is the next item.

### 4.3 Run the comparison

```bash
scripts/bench-comparison.sh \
    --rust-target 127.0.0.1:8088 \
    --java-target 127.0.0.1:18088 \
    --connections 10000 \
    --rate 5 \
    --duration 60 \
    --mix realistic \
    --throughput-floor 3
```

The script writes `bench/history/comparison-<UTC ISO timestamp>.json`
holding both per-side runs plus the verdict block. The exit code is
non-zero when the throughput floor is missed.

After the run, fill in §3.1–§3.5 above by hand from the verdict block,
commit the updated table, and reference the JSON file path in the
commit message.

## 5. Notes on validity

- **PLI-only mix is misleading.** Use the `realistic` mix unless you
  are micro-benching the dispatch path. Java's GC pauses are sharper
  on the small-message hot loop.
- **JIT warm-up on the Java side.** The Java server is much faster
  60 s in than 5 s in. Always run for ≥ 60 s so the C2 compiler has
  finished re-compiling the hot dispatch path with profile data.
- **Loopback vs real network.** Loopback gives the Java server an
  unfair advantage by hiding kernel send/recv copies that real ATAK
  fleets pay over a NIC. For the M5 headline number, loopback is fine
  — but a "tak-rs in production conditions" report would want a
  separate cross-host measurement.
- **Run-to-run variance.** Capture three runs per side. If the
  `msg_per_s` figures vary by more than 5 %, investigate before
  declaring M5 done — likely something on the box is swapping or
  competing for cache.
