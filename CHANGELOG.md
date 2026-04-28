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
  share the same `Store` handle. First real Rust baseline captured: **46 385
  msg/s, 1000 conns, 16.5 MB/s, 111 MB RSS** at 50 k msg/s offered load —
  93 % of the M5 50 k headline target. Numbers in `docs/perf-comparison.md`.
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
