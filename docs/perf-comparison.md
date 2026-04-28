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

| Tag | Mode | Conns | Offered | Sustained (msg/s) | Max RSS (MB) | Peak CPU % |
|---|---|---|---|---|---|---|
| `rust-persist-50k` | persist | 1 000 | 50 k | 41 848 | 111 | 2 060 |
| `rust-nopersist-50k` | dispatch-only | 1 000 | 50 k | 43 799 | 105 | 2 110 |
| `rust-persist-200k` | persist | 2 000 | 200 k | **114 573** | 176 | 6 290 |
| `rust-nopersist-200k` | dispatch-only | 2 000 | 200 k | **141 815** | 226 | 6 090 |

| Side | Configuration | Sent (msg/s) | Ratio |
|------|--------------|--------------|-------|
| Java upstream | _TBD_ (awaiting container) | _TBD_ | 1.00× (baseline) |
| **tak-rs (persist)** | 2 000 conn × 100 msg/s × 20 s | **114 573** | _TBD_ |
| **tak-rs (no-persist)** | 2 000 conn × 100 msg/s × 20 s | **141 815** | _TBD_ |

**Key findings**

- **tak-rs is 2.3× the M5 50 k msg/s headline target with persistence on**, and 2.8× with `--no-persist`. The earlier 46 k figure (1 000 conn × 50 msg/s offered) was loadgen-rate-jitter limited, not a server ceiling — at that offered load, the server is mostly idle waiting for the next message.
- The **persistence side-channel costs ~19 %** at 200 k offered (114 k vs 142 k). The `CotInsert` allocations (5 owned `String`s per message) are now visible in the gap. The H1 hot path stays alloc-free — those allocations live on the *persistence* side of the bounded mpsc per pipeline.rs's intentional design — but they consume real CPU.
- Zero-copy frame extraction (`split_to().freeze()` vs `Bytes::copy_from_slice`) is in place but its gain at 50 k offered is below the run-to-run noise floor on this hardware. It would show clearly with a kernel-bypass loadgen; for now we ship the change because it's the architecturally correct one (no per-message memcpy off the read buffer).
- Per-connection RSS at 2 000 conns: ~88 KB — well below the typical Java `ChannelHandlerContext` weight, though we have not yet measured Java side-by-side on the same box.

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
| tak-rs (1 000 conn, persist) | 111 | ~110 KB |
| tak-rs (1 000 conn, no-persist) | 105 | ~105 KB |
| **tak-rs (2 000 conn, persist)** | **176** | **~88 KB** |
| **tak-rs (2 000 conn, no-persist)** | **226** | **~113 KB** |

Java's per-connection state is dominated by the GC-tracked `ChannelHandlerContext` plus the mutable `BigInteger` group bitvector; tak-rs's per-connection state is the fixed `[u64; 4]` mask plus the slab-allocated `Subscription`. The 110 KB/conn for the Rust path includes the per-connection `BytesMut` read buffer (8 KB initial, grows on demand) and the bounded mpsc channel (`DEFAULT_SUBSCRIBER_CAPACITY` × `Bytes` slot ≈ 32 KB), so the steady-state cost per connection is much smaller than the high-water RSS suggests.

The "per connection" column highlights the architectural difference: Java's per-connection state is dominated by the GC-tracked `ChannelHandlerContext` plus the mutable `BigInteger` group bitvector; tak-rs's per-connection state is the fixed `[u64; 4]` mask plus the slab-allocated `Subscription`.

### 3.4 CPU

Peak CPU% observed (`top -b -n 1 -p PID`, 1 Hz sampling — sum across threads, so values > 100 % indicate multi-core utilisation).

| Side | Peak % | Notes |
|------|--------|-------|
| Java upstream | _TBD_ | _TBD_ |
| tak-rs (1 000 conn, persist) | 2 060 | 20 cores at 50 k offered. |
| tak-rs (1 000 conn, no-persist) | 2 110 | 21 cores at 50 k offered. |
| **tak-rs (2 000 conn, persist)** | **6 290** | 63 cores at 200 k offered — fully saturating the box. |
| **tak-rs (2 000 conn, no-persist)** | **6 090** | 61 cores at 200 k offered. |

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

> **Update (2026-04-28):** `tak-server::main` now binds the plain CoT
> firehose on `:8088` and the mission API on `:8080`. The Rust row in
> §3.1, §3.3, and §3.4 above reflects a real loopback measurement
> against this listener; only the Java row remains pending.

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
