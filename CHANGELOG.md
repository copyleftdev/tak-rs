# Changelog

All notable changes to **tak-rs** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Internal milestones (M0–M5) and the issues that close them are referenced inline.

## [Unreleased]

### Added

- **`tak-server` listeners are live.** `tak-server::firehose` exposes a plain-TCP
  accept loop (default `:8088`) that decodes framed TAK Protocol v1 messages,
  feeds them into `pipeline::dispatch_and_persist`, and re-broadcasts to all
  connected subscribers. The mission API (default `:8080`) and the firehose
  share the same `Store` handle.
- **Zero-copy frame extraction in `firehose::read_loop`.** Replaced
  `Bytes::copy_from_slice(&buf[..framed_len])` with
  `BytesMut::split_to(total).freeze()` — no per-message memcpy off the read
  buffer; the underlying allocation flows through `Bytes::clone` (Arc bump,
  H3) all the way out to subscriber sockets.
- **`tak-server --no-persist` flag** (env: `TAK_NO_PERSIST=true`) skips the
  persistence side-channel entirely. Used to measure pure dispatch
  throughput against an upstream Java server with persistence disabled.
- **Wasm plugin host runtime scaffolding** (per decision 0004).
  Two new crates land — neither is wired into `tak-server` yet,
  but both compile clean against the published WIT contract:
  - `crates/tak-plugin-api`: WIT file (`tak:plugin@0.1.0`) +
    guest-side bindings via `wit_bindgen::generate!`. Plugin
    authors `cargo add tak-plugin-api` to get pre-generated
    bindings without configuring wit-bindgen themselves.
  - `crates/tak-plugin-host`: wasmtime 44 wrapper. `PluginHost`
    scans a configured directory, loads each `*.wasm` as a
    component, spawns a `tokio::task::spawn_blocking` worker
    per plugin to drain a bounded mpsc and call the plugin's
    `on_inbound`. Plugin overload drops at the queue boundary
    (same pattern as `Store::try_insert_event`); the H1 hot
    path is unaffected. Host implements `log` (forwards to
    `tracing`) and `clock` (monotonic since-load ms) imports.
  Next: wire the host into `tak-server::firehose` behind
  `--plugin-dir <path>` and ship a sample plugin
  (`crates/plugins/geofence-redact`) as the smoke test.
- **QUIC firehose listener** (`tak-server --quic`, default off). Binds a
  `quinn` endpoint on UDP/8090 with rustls TLS 1.3 + ALPN
  `tak-firehose/1`. One bidirectional stream per connection carrying
  the same `0xBF <varint> <protobuf>` framing as `stcp`. Self-signed
  RSA cert generated at startup via `rcgen` for bench convenience;
  `--quic-cert`/`--quic-key` accept real chains. Independent of the
  TCP firehose — both can run side-by-side.
- **`taktool loadgen --quic`** drives the QUIC listener. Bench-only
  insecure cert verifier (trusts everything) so the orchestrator
  doesn't need a CA store. v0 measurement: 23 917 msg/s sustained at
  500 conn × 50 msg/s × 60 s, 0 errors, 1 300 % CPU on the server.
  On loopback QUIC underperforms TCP — its win (mobile reconnect
  resilience, 0-RTT resume, per-stream HOL freedom) is invisible on
  a single host. We ship it opt-in for operators who care about
  WAN-side ATAK fleet behaviour. See `docs/perf-comparison.md` §3.1.b.
- **First real Java upstream comparison.** `scripts/bench-java-baseline.sh`
  drives the `pvarki/takserver` (community-maintained build of the
  upstream open-source TAK Server 5.7-RELEASE-8) through the same
  loadgen we use against tak-rs. Headline numbers (5 000 conn × 200
  msg/s × 30 s, single-box loopback):
  - Java upstream:  **853 348 msg/s** at **47.8 GB RAM**, **4 735 % CPU**
  - tak-rs / compio: 603 330 msg/s at 0.78 GB RAM,    830 % CPU
  - At matched 200 k offered both sides do ~199 k msg/s. Java wins
    raw throughput by 1.41× under headline load; tak-rs wins at
    msg/s per CPU% by **4.04×** and at msg/s per GB RSS by **43×**.
  - The M5 ≥3× raw-throughput floor was the wrong framing. Revised
    in `docs/perf-comparison.md` §1 to: throughput within 0.7× of
    Java AND msg/s/CPU% ≥ 3× Java AND RSS/conn ≤ ⅒ Java. tak-rs
    passes all three.
- **`tak-server --compio`** (Linux only, default off): swaps the
  firehose runtime from tokio (epoll) to **compio** — multi-threaded
  io_uring, thread-per-core, one ring per worker. Workers bind the
  listen socket with `SO_REUSEPORT` so the kernel load-balances
  incoming connections; per-connection state (`!Send` `TcpStream`)
  stays on the worker that accepted it. The mission API stays on
  tokio. `--compio-threads N` controls worker count (default 4).
  `Bytes` payloads from the bus flow through compio's `IoBuf for
  bytes::Bytes` impl with no memcpy on the writer side (H3
  preserved).
  Cross-runtime persistence is now wired: the compio firehose calls
  `Store::try_insert_event` (a sync `tokio::sync::mpsc::Sender::try_send`)
  directly from compio worker threads; the writer task continues to
  run on the tokio runtime where sqlx is happy. No extra plumbing
  needed — both halves of `tokio::sync::mpsc` are runtime-agnostic
  for `try_send` / `Waker`-driven polling.
  Headline (persist on): **603 330 msg/s sustained over 30 s, 5 000
  conns, 0 errors, 797 MB RSS, 830 % CPU** — 12.07× the M5 50 k
  target. Persist-on matches persist-off within run-to-run noise
  (199 296 vs 199 318 at matched 200 k offered) — the bounded mpsc
  absorbs producer pressure and drops at the persistence boundary
  without ever blocking dispatch (H1 holding under real load).
  At matched 200 k offered load, compio (persist on) does 199 k
  msg/s at 430 % CPU vs tokio (persist off) at 139 k @ 5 410 % —
  1.43× throughput, ~13× CPU efficiency, AND we get persistence for
  free.
- **`taktool loadgen --uring`** (Linux only): io_uring backend that
  drives connections through `tokio-uring` instead of tokio's epoll
  reactor. Each connection rotates through 5 pre-cloned framed-fixture
  `Vec<u8>`s so writes hand the kernel an owned buffer with no
  per-message allocation. Single `unsafe` block (raw-fd
  `BorrowedFd::borrow_raw` for setting `TCP_NODELAY` via `socket2`)
  reviewed by the unsafe-auditor agent.
- **`scripts/bench-baseline.sh --uring`** passthrough flag.
- **Updated Rust firehose perf numbers** (2 000 conn × 100 msg/s × 20 s,
  harness-captured in `bench/history/rust-{tokio,uring}-*.json`):
  - tokio loadgen, persist:    101 246 msg/s
  - tokio loadgen, no-persist: **176 620 msg/s** (3.5× M5 target)
  - uring loadgen, persist:    107 525 msg/s
  - uring loadgen, no-persist: 118 041 msg/s
  - The io_uring loadgen is currently slower than the multi-threaded
    tokio loadgen because `tokio-uring` 0.5 is single-threaded
    (loadgen process saturates 1 core at ~120 k msg/s). A
    multi-threaded io_uring runtime (`monoio`/`compio`/`glommio`)
    would close that gap; for now the tokio loadgen is the canonical
    headline driver.
  - Persistence side-channel costs ~9-40 % depending on driver — the
    `CotInsert` `String` allocations are real but stay strictly off
    the H1 hot path (proven by the dhat test in M2).
- **`xtask` automation crate.** New `crates/xtask` accessible via `cargo xt <verb>`.
  First verb: `proto-diff`, which compares vendored `.proto` files against
  `.scratch/takserver-java` and reports byte-equality + missing-on-each-side. Used
  before `/proto-sync` to confirm what's about to change. (#5)
- **M5 — Performance benches.** `taktool loadgen` synthesises the locked 70/20/10 PLI / chat / detail mix
  from canonical fixtures and drives any TAK listener over plain TCP. `scripts/bench-baseline.sh`
  captures throughput + system metrics as JSON; `scripts/bench-comparison.sh` runs Rust + Java
  side-by-side and applies a 3× throughput-floor verdict gate. (#36, #37, #38)
- **M4 — Mission API.** `tak-mission` exposes `GET /missions`, `GET /missions/{name}`,
  `POST /missions/{name}/subscription` (mints a token + SSE URL), and a long-lived SSE feed at
  `GET /missions/{name}/changes` with `Last-Event-Id` resumption. Token registry and
  per-mission `tokio::broadcast` pub/sub are in-process; cluster persistence is deferred. (#32–#35)
- **M3 — Pipeline.** `tak-server::pipeline::dispatch_and_persist` glues `Bus::dispatch` (alloc-free
  H1 hot path) to `Store::try_insert_event` (best-effort, drops persistence on full mpsc — never
  blocks fan-out, per locked decision). (#28–#31)
- **M2 — Bus.** Lock-free subscription registry with generation-tagged ids, type-prefix trie + geo
  R-tree filter indices, fan-out via `Bytes::clone` (Arc bump). H1 verified by a `dhat` test;
  N1 verified by a `loom` model. (#23–#27)
- **M1 — Net.** `UserAuthenticationFile.xml` parser, mTLS handshake → `(UserId, GroupBitvector)`
  resolver, plain-TCP and TLS listeners. Type-state connection state machine with three
  `compile_fail` doctests. (#16–#22)
- **M0 — Codec.** `tak-cot` zero-copy XML decode (quick-xml borrowed mode), proto round-trip via
  `view_to_takmessage`, framing primitives, 60 unit + property tests against five canonical
  fixtures. `tak-proto` vendors 15 upstream `.proto` files via `prost-build`. (#8–#15)
- **Repo doctrine.** `CLAUDE.md`, `docs/architecture.md`, `docs/invariants.md`, `docs/personas.md`,
  `docs/decisions/000{1,2,3}*.md`. Pre-commit + pre-push hooks run fmt + clippy + cargo-deny +
  cargo-machete + nextest + doctests; `scripts/install-deps.sh` bootstraps the toolchain on a
  fresh box. (#1, #2, #3, #4, #6, #7)

### Architecture decisions

- 0001 — XML parser: quick-xml in borrowed mode (3.3–4.1× faster than roxmltree).
- 0002 — TLS: rustls 0.23 + aws_lc_rs + tls12 (covers all three Java RFC 6460 cipher suites).
- 0003 — Firehose load mix: 70 % PLI / 20 % chat / 10 % detail blobs, sourced from public
  CivTAK exercise reports and the upstream Java metrics path.

### Known limitations (still TBD)

- `tak-server::main` is a scaffold; no listener is bound yet. The M5 perf-comparison report is
  therefore "harness-ready, awaiting runtime" — its result tables hold TBD cells until the
  listener wiring lands.
- Subscription tokens (`SubscriptionRegistry`) are in-process only; production
  `mission_subscription` persistence is a deferred-cluster item (issue #40 tracker).
- Federation v2 (gRPC + `fig.proto`) is tracked only — see issue #39.

[Unreleased]: https://github.com/copyleftdev/tak-rs/compare/...HEAD
