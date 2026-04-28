# Invariants

Rules that must hold across the codebase. Each is either machine-enforced or
agent-reviewed. If you're tempted to break one, the answer is almost always
"don't" — but the rationale is here so you can argue if you must.

Format per rule:
- **What** — the invariant
- **Why** — the cost of breaking it
- **Enforced by** — the gate (clippy lint, deny rule, custom test, agent, manual)

---

## Correctness invariants

### C1 — CoT XML round-trip is lossless on `xmlDetail`

- **What:** For any well-formed CoT XML message `M`, `xml_to_proto(M)` then `proto_to_xml(...)` produces XML that is semantically equivalent to `M` (same elements, same attributes, same text). The `Detail.xmlDetail` field round-trips byte-for-byte for any sub-element not covered by the typed messages.
- **Why:** ATAK clients silently misrender or drop markers when a server lossy-parses their detail blocks. This is the #1 way third-party TAK servers fail in production.
- **Enforced by:** `tak-cot/tests/roundtrip.rs` — proptest generator for arbitrary CoT XML, asserts equivalence after round-trip. Owner: `cot-codec-reviewer` agent.

### C2 — Protobuf schema is byte-stable for canonical samples

- **What:** A fixed corpus of canonical CoT messages (PLI, chat, geofence, route, freehand draw) encodes to byte-identical protobuf across releases.
- **Why:** Federation peers and persisted DB rows depend on the wire format being stable. A re-ordering of optional fields breaks both.
- **Enforced by:** `insta` snapshot tests in `tak-proto/tests/`. Snapshot diffs require explicit acceptance.

### C3 — Group bitvector intersection matches reference

- **What:** `GroupBitvector::intersects(a, b)` returns the same boolean as `BigInteger.and(a_hex, b_hex) != 0` for any inputs derivable from the Java server's group hex strings.
- **Why:** A divergence means a TAK client either sees messages it shouldn't or misses messages it should — a security and correctness bug.
- **Enforced by:** `tak-auth/tests/bitvector.rs` proptest comparing against a reference impl using `num-bigint`.

### C4 — Mission change ordering is monotonic per mission

- **What:** Within a single mission, `MissionChange.timestamp` and `MissionChange.serverTime` are monotonically non-decreasing across all observers.
- **Why:** Clients reconcile state by replaying changes since a timestamp; reordering causes desync.
- **Enforced by:** `tak-mission/tests/change_order.rs` — concurrent producers, single observer, asserts no inversion. `loom` model variant for the locking.

### C5 — TLS handshake never accepts an unverified peer cert

- **What:** No code path in `tak-net::tls` calls into `tak-auth` with an unverified cert chain. The `rustls::ClientCertVerifier` is the only entry point.
- **Why:** Group authorization downstream trusts the cert chain identity. A bypass is total compromise.
- **Enforced by:** `unsafe-auditor` agent reviews every change to `tak-net/src/tls.rs`. Code search ban on `dangerous_configuration` features in the rustls feature set.

---

## Hot-path invariants

### H1 — `tak_bus::dispatch` is allocation-free in steady state

- **What:** A single dispatch of one inbound message to N matched subscribers performs zero heap allocations on the steady-state path. (Allocation only allowed on subscription add/remove.)
- **Why:** This is the firehose. Every alloc is a cache-line bounce and an mimalloc lock contention point. At 50k msg/s × 100 subscribers, even tiny allocs swamp the runtime.
- **Enforced by:** `tak-bus/tests/no_alloc.rs` — uses `dhat` heap profiler in test mode, asserts `total_blocks == 0` for the dispatch loop. Owner: `hot-path-perf` agent.

### H2 — Decoders borrow from input

- **What:** `Codec::decode` returns a borrowed view: signature is `fn decode<'a>(&self, buf: &'a [u8]) -> Result<View<'a>>`. The view holds `&'a [u8]` and `&'a str` slices, never owned `Vec<u8>` or `String`.
- **Why:** Owning the input means a copy. The `Bytes` we received from rustls is already on the heap — we want to ref-count it, not duplicate it.
- **Enforced by:** clippy lint set + `hot-path-perf` agent review. Anti-pattern grep on PR: `String::from`, `to_string()`, `.to_owned()` inside `tak-cot`.

### H3 — Fan-out is `Bytes::clone`, never `Vec<u8>::clone`

- **What:** When dispatching one inbound payload to N subscribers, the underlying byte storage is shared via `bytes::Bytes::clone()` (Arc bump). Never `Vec::clone()` or `to_vec()`.
- **Why:** N subscribers should mean N pointer bumps, not N memcpys.
- **Enforced by:** `hot-path-perf` agent review. Anti-pattern grep: `to_vec()` in `tak-bus/src/dispatch.rs`.

### H4 — Group AND is `[u64; 4]`, not arbitrary bigint

- **What:** `GroupBitvector` is exactly `[u64; 4]`. The `intersects` impl is the bitwise OR of four ANDs.
- **Why:** ~4 instructions vs Java's `BigInteger.and()` allocation. Single biggest CPU win on the hot path. We accept the 256-group cap; it's never been a real constraint.
- **Enforced by:** type definition in `tak-auth`. Widening requires architectural review.

### H5 — Per-subscription channel is bounded

- **What:** Every `mpsc::channel` for subscriber outbound is bounded (default capacity 1024). Unbounded channels are forbidden in lib code.
- **Why:** A slow subscriber must not exhaust memory. Bounded channels force a backpressure decision (drop, disconnect, slow producer).
- **Enforced by:** clippy custom lint or grep on `mpsc::unbounded_channel` in lib crates.

### H6 — No XPath at runtime

- **What:** Subscription filters are compiled at subscribe-time into a struct with: type prefix, geo bbox, UID set, group mask. Runtime dispatch is index lookup + bitwise AND, never XPath evaluation.
- **Why:** Java's per-message XPath is the largest single CPU cost in their hot path. We index instead.
- **Enforced by:** dependency ban on XPath crates in `deny.toml`. Architectural review.

---

## Discipline invariants

### D1 — No `unwrap` / `expect` / `panic!` / `todo!` / `unimplemented!` in lib code

- **What:** Lib crates (everything in `crates/` except `taktool` and `tak-server` binaries) deny these via `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::todo, clippy::unimplemented)]`.
- **Why:** A panic in a library is a denial-of-service on the user's process. They should choose how to handle errors, not us.
- **Enforced by:** `clippy.toml` + `#![deny(...)]` in each lib's `lib.rs`. Tests/benches opt out via `#[cfg(test)]`.

### D2 — All errors are `thiserror` enums in lib code

- **What:** Every lib crate has its own `Error` enum derived with `thiserror`. `anyhow::Error` is forbidden in lib code; allowed only in `taktool` and `tak-server`.
- **Why:** Library users need to match on error variants. `anyhow` collapses that into a string.
- **Enforced by:** `deny.toml` — `anyhow` is `[bans] deny` for lib crates, allowed in binaries.

### D3 — Time is `jiff`, never `chrono` or `time`

- **What:** All wall-clock time uses `jiff`. CoT timestamps (ms since epoch) deserialize into `jiff::Timestamp`. Durations use `jiff::Span`.
- **Why:** `jiff` has the right answer for timezones, leap seconds, and parsing — and it's actively maintained. `chrono` has historical safety bugs and an awkward timezone API; `time` is decent but lacks `jiff`'s ergonomics.
- **Enforced by:** `deny.toml` bans `chrono` and `time`.

### D4 — TLS is `rustls`, never `openssl-sys` or `native-tls`

- **What:** All TLS uses `rustls` via `tokio-rustls`. The `openssl`, `openssl-sys`, and `native-tls` crates are banned.
- **Why:** rustls has a smaller attack surface, no C dependency, and the TAK threat model doesn't need anything OpenSSL-only offers.
- **Enforced by:** `deny.toml` bans the three crates outright.

### D5 — Logging is `tracing`, never `log` or `println!`

- **What:** All logging goes through `tracing`. The `log` crate (bare) and `println!`/`eprintln!` in lib code are forbidden.
- **Why:** Spans encode causality across async boundaries; flat log lines don't. `println!` bypasses the subscriber and breaks structured output.
- **Enforced by:** `deny.toml` bans `log`. clippy `print_stdout` / `print_stderr` denied in lib crates.

### D6 — Hashing for internal maps is `ahash`

- **What:** `HashMap`/`HashSet` instances internal to lib crates use `ahash::AHashMap` (or equivalent). Crate-public APIs that take a HashMap accept either.
- **Why:** ~2× faster than SipHash on small keys. We auth at the TLS boundary, so we don't need DoS-resistant hashing internally.
- **Enforced by:** review. Not a hard ban — std `HashMap` is fine at boundaries.

### D7 — No global allocator other than `mimalloc`

- **What:** `tak-server` and `taktool` set `mimalloc` as the global allocator via `#[global_allocator]`.
- **Why:** Measurable on alloc-heavy paths. mimalloc is also smaller and has clearer license than jemalloc.
- **Enforced by:** present in `tak-server/src/main.rs`. Audited at the binary level.

---

## API invariants

### A1 — All public types implement `Debug`

- **Enforced by:** `clippy::missing_debug_implementations` denied at crate level.

### A2 — No `pub use` of third-party types in API surface

- **What:** `tak-cot` does not re-export `bytes::Bytes` as part of its API. Either we own it or we don't expose it.
- **Why:** A `pub use` of `bytes::Bytes` couples our API stability to the `bytes` crate's. They version separately; we don't get to dictate.
- **Enforced by:** review. Exception: when the third-party type is *the* type for a domain (e.g., we `pub use prost::Message` because that's what users need). Document the exception inline.

### A3 — Every public crate has a top-level `lib.rs` doctest

- **What:** `crates/<name>/src/lib.rs` has a `//! # Example` block with a fenced `rust` snippet that compiles and demonstrates the primary use case.
- **Why:** The doctest is the spec. If it can't be written in 10 lines, the API is wrong.
- **Enforced by:** `cargo test --doc` in CI.

---

## Concurrency invariants

### N1 — `tak-bus` dispatch passes `loom` model checking

- **What:** `cargo test --test loom_dispatch` (under the `loom` cfg) explores all schedules of concurrent subscribe / unsubscribe / dispatch and asserts no data race, no deadlock, no lost message.
- **Why:** The bus is the heart. A concurrency bug here is silent data loss for users.
- **Enforced by:** `tak-bus/tests/loom_dispatch.rs`. Required-passing CI gate.

### N2 — No `std::sync::Mutex` on the hot path

- **What:** Hot paths (`tak-bus::dispatch`, `tak-net::read_loop`, `tak-net::write_loop`) use `parking_lot::RwLock` for rare-write or `dashmap` for hot maps. `std::sync::Mutex` is allowed only on cold paths (config reload, admin endpoints).
- **Why:** `std::sync::Mutex` parks via syscall on contention; `parking_lot` spins briefly first, which is right for short critical sections.
- **Enforced by:** review. Anti-pattern grep on `std::sync::Mutex` in hot crates.

### N3 — All async tasks have a name and a timeout

- **What:** `tokio::spawn` is forbidden directly in lib code. Use `tak-server::tasks::spawn(name, timeout, future)` which adds tracing span and a Drop guard for orphan detection.
- **Why:** Unnamed tasks are unobservable. Untimed tasks leak.
- **Enforced by:** `deny.toml` ... actually not enforceable that way. Custom xtask grep. Or a clippy custom lint via `dylint`.

---

## How rules get added or removed

- **Adding:** open a PR that includes the rule, the rationale, and the enforcement mechanism. If it can't be enforced (only reviewed), say so explicitly.
- **Removing:** open a PR with the post-mortem of what changed in the world such that the rule is no longer load-bearing. Don't remove a rule because it's inconvenient — fix the inconvenience.
- **Suspending temporarily:** in-line `#[allow(...)]` with a `// SAFETY-WAIVER:` comment citing the issue tracking the proper fix.
