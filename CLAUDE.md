# tak-rs

A high-performance Rust kernel for the TAK (Team Awareness Kit) ecosystem.
Drop-in replacement for the messaging core of the reference Java server,
single-node, mTLS streaming firehose at 10k+ concurrent connections per box.

## Read these first

- `docs/architecture.md` — full design, Java→Rust component map, hot-path walkthrough.
- `docs/invariants.md` — non-negotiable rules (correctness, hot path, discipline). Machine-enforced where possible.
- `docs/personas.md` — the elite engineers we model on, with their public bodies of work.
- `.scratch/takserver-java/` — shallow clone of upstream Java server. Read it; don't replicate it blindly.

## Locked architectural decisions

These were debated and locked. Don't relitigate without a written reason:

1. **Single-node only in v1.** No Ignite, no clustering. Federation deferred.
2. **mTLS streaming (8089) is the production path.** Plain CoT (8087/8088) is opt-in.
3. **`Detail.xmlDetail` is preserved as a borrowed `&str`** into the original message. Lossless XML round-trip is a correctness requirement (ATAK breaks otherwise).
4. **Group bitvector is fixed-width `[u64; 4]`.** ~4 instructions to AND vs Java's BigInteger. Widen if a deployment legitimately needs >256 groups.
5. **Filters are compiled per-subscription** into struct (type prefix trie + geo R-tree + UID set + group mask). No XPath at runtime.
6. **`tak-bus` knows nothing about sockets or storage.** Pure `(in) → (sub_id, out)*`.
7. **Persistence MUST NOT block fan-out.** Bounded channel; if full, drop persistence not delivery.

## Persona routing — invoke the right Skill

For non-trivial work in these areas, invoke the named Skill. These are public engineers whose code/talks/books we're modeling on. Full table: `docs/personas.md`.

| Working on | Invoke Skill |
|---|---|
| `tak-net` listeners, tokio runtime, mpsc/Service patterns | `lerche` |
| `tak-cot` codec, `tak-proto` types, error types, any serde | `tolnay` |
| `tak-bus` lock-free maps, atomics, the bitvector intersection | `bos` |
| Connection state machines, plugin API, public crate ergonomics | `gjengset` |
| Borrow-checker puzzles on the zero-copy hot path | `matsakis` |
| Designing a public crate API that will outlive us | `turon` |
| `tak-mission` REST surface, Mission API endpoints | `fielding` |
| `tracing` schema, metrics, runtime introspection | `cantrill` |
| Hot-path perf review, cache lines, branch prediction | `muratori` |
| Postgres + PostGIS schema, query plans, the spatial index | `pavlo` |
| Federation + cluster (later) | `kleppmann`, `lamport`, `helland` |
| Mission change feed as event log | `young` |
| Property tests for any codec or invariant | `hegel`, `property-based` |
| `tak-bus` concurrency model checking | `deterministic-simulation` |
| Threat modeling the wire surface | `mitre-attack` |
| Dependency vulnerability check | `vulngraph` |
| Failure-injection / chaos tests | `chaoslab` |

## Hot-path invariants (cheat sheet — full list in `docs/invariants.md`)

- **No allocation in steady state** in `tak_bus::dispatch`. Enforced by a `dhat` test.
- **Fan-out is `Bytes::clone`** (Arc bump). Never `Vec<u8>::clone`. Enforced by review.
- **Decoders borrow from input.** `Codec::decode<'a>(&'a [u8]) -> Borrowed<'a, _>`.
- **No `.unwrap()` / `.expect()` / `panic!` / `todo!` in lib code.** Clippy `-D` set.
- **All public types implement `Debug`.** Clippy.
- **All errors are `thiserror`-derived enums** in lib crates. `anyhow` only in `taktool` and `tak-server` binaries.

## Crate philosophy — use the ecosystem

We're standing on the shoulders of the persona list. Don't reinvent.

**Mandated** (use these, not alternatives):
- `tokio` for async runtime, `tokio-rustls` + `rustls` for TLS, `quinn` for QUIC
- `prost` (+ `tonic` later) for protobuf
- `quick-xml` (borrowed mode) for XML
- `bytes::Bytes` end-to-end for payloads
- `sqlx` (compile-time-checked SQL) for Postgres
- `axum` + `tower` + `hyper` for HTTP
- `tracing` + `tracing-subscriber` for logs/spans
- `metrics` + `metrics-exporter-prometheus` for metrics
- `thiserror` (lib) / `anyhow` (binary) for errors
- `jiff` for time (CoT timestamps, mission timing)
- `mimalloc` as global allocator

**Banned** (enforced via `deny.toml`):
- `openssl-sys`, `native-tls` — use rustls
- `chrono`, `time` — use jiff
- `log` (the bare crate) — use tracing
- `lazy_static` — use std `OnceLock` / `LazyLock`

## Test discipline

- **`cargo nextest run`** is the test runner. Faster, better output.
- **`proptest`** for every codec and every invariant that's "for all inputs, X holds".
- **`loom`** for `tak-bus` concurrency. Required before merging any change to dispatch.
- **`criterion`** for hot-path benches. `/bench-hot` runs them and diffs against baseline.
- **`insta`** for protobuf/XML snapshot tests on canonical CoT samples.
- **`testcontainers`** for any test that needs Postgres. No mocks — see invariants.
- **`cargo-fuzz`** on `tak-cot` decoders. `/fuzz-codec` runs a 5-min round.

## Common workflows (slash commands)

- `/proto-sync` — pull latest `.proto` from upstream Java tree, regenerate `tak-proto`
- `/bench-hot` — run firehose criterion benches, diff against baseline, post summary
- `/fuzz-codec` — 5-min `cargo-fuzz` round on `tak-cot`, report any crashes
- `/check-invariants` — runs the gauntlet: clippy, deny, machete, dhat-on-bus, loom
- `/replay-pcap <file>` — replay a captured pcap through `tak-server`, verify outputs

## Working style

- **Read the Java reference before porting.** Real file paths, not your prior. Recon report style: cite class + line.
- **Bench before optimizing.** "Faster than Java" is a claim, not a fact. `criterion` + `dhat` + `perf stat`.
- **Spawn subagents for parallel exploration** (use `Explore` for codebase recon, persona Skills for design review).
- **Commit small.** Each commit either adds a test, makes one green, or refactors with tests staying green.
- **No comments explaining what code does.** Comments only for why-the-code-is-weird (workaround, hidden invariant, bug-driven shape).
