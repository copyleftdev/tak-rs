# Personas — engineers we model on

This project is shaped by the public work of a specific set of engineers.
Each owns a domain, has a Skill that captures their style, and has a
recommended reading list of their own code.

When working on a domain, **invoke the Skill** before designing or
reviewing — it brings their specific reasoning patterns into context.

---

## Runtime, networking, async — Carl Lerche

- **GitHub:** [carllerche](https://github.com/carllerche)
- **Skill:** `lerche`
- **Owns in tak-rs:** `tak-net`, the runtime model in `tak-server`, the Service/Layer wiring in `tak-mission`, anywhere we touch `bytes::Bytes`
- **Read these:**
  - [tokio-rs/tokio](https://github.com/tokio-rs/tokio) — runtime arch; `runtime/`, `sync/mpsc/`, `task/`
  - [tokio-rs/bytes](https://github.com/tokio-rs/bytes) — `Bytes` ref-counting, the zero-copy primitive we live and die by
  - [tower-rs/tower](https://github.com/tower-rs/tower) — Service trait; the right abstraction for middleware
  - [tokio-rs/mio](https://github.com/tokio-rs/mio) — what's under tokio
  - [tokio-rs/prost](https://github.com/tokio-rs/prost) — what's under our `tak-proto`
- **Signature ideas:** typed channels with explicit backpressure; Service/Layer for composability; small structs, big builders.

---

## Codecs, proc macros, errors, ergonomics — David Tolnay

- **GitHub:** [dtolnay](https://github.com/dtolnay)
- **Skill:** `tolnay`
- **Owns in tak-rs:** `tak-cot`, `tak-proto`, the error types in every crate, derive macros if we ever write one
- **Read these:**
  - [serde-rs/serde](https://github.com/serde-rs/serde) — the gold standard for serialization API design
  - [dtolnay/anyhow](https://github.com/dtolnay/anyhow) and [dtolnay/thiserror](https://github.com/dtolnay/thiserror) — the rule we follow: anyhow in binaries, thiserror in libs
  - [dtolnay/syn](https://github.com/dtolnay/syn), [dtolnay/quote](https://github.com/dtolnay/quote) — proc macro hygiene
  - [dtolnay/cxx](https://github.com/dtolnay/cxx) — careful FFI design (relevant if we ever wrap libtak)
- **Signature ideas:** zero-cost derive macros; error types that compose without losing source; APIs where the compiler does the work.

---

## Atomics, lock-free, concurrency primitives — Mara Bos

- **GitHub:** [m-ou-se](https://github.com/m-ou-se)
- **Skill:** `bos`
- **Owns in tak-rs:** the subscription registry in `tak-bus`, the group bitvector AND, anywhere we reach for `Atomic*`
- **Read these:**
  - [Rust Atomics and Locks (free online)](https://marabos.nl/atomics/) — read it cover to cover before touching atomics
  - [m-ou-se/inline-default](https://github.com/m-ou-se/inline-default), [m-ou-se/format-args](https://github.com/m-ou-se/format-args)
  - Her std-lib team work on `OnceCell`/`OnceLock`, `LazyLock`, `Cell`/`RefCell` doc passes
- **Signature ideas:** prefer the weakest memory ordering that's correct; `Relaxed` for counters, `Acquire`/`Release` for handoffs; document the ordering rationale inline.

---

## Production patterns, type-state, library APIs — Jon Gjengset

- **GitHub:** [jonhoo](https://github.com/jonhoo)
- **Skill:** `gjengset`
- **Owns in tak-rs:** the connection lifecycle state machine (`Handshaking → Authed → Streaming`), the plugin SDK trait surface, public types in any crate that ships
- **Read these:**
  - "Rust for Rustaceans" — the book, especially chapters on traits and unsafe
  - [jonhoo/left-right](https://github.com/jonhoo/left-right) — wait-free reads via double-buffering; potential model for the subscription routing table
  - [jonhoo/evmap](https://github.com/jonhoo/evmap) — concurrent map design
  - [jonhoo/noria](https://github.com/jonhoo/noria) — dataflow at scale
- **Signature ideas:** type-state for compile-time-checked protocols; sealed traits when you don't want third-party impls; explicit "your code is wrong" error conditions.

---

## Lifetimes, borrow checker, zero-copy — Niko Matsakis

- **GitHub:** [nikomatsakis](https://github.com/nikomatsakis)
- **Skill:** `matsakis`
- **Owns in tak-rs:** any borrow-checker puzzle on the hot path (esp. `tak-cot` borrowed decoders into `Bytes`), the lifetime story for `Detail::xmlDetail`
- **Read these:**
  - His [Polonius](https://github.com/rust-lang/polonius) work — what the borrow checker really wants to do
  - [salsa-rs/salsa](https://github.com/salsa-rs/salsa) — incremental computation; informs cache invalidation thinking for routing
  - His blog [smallcultfollowing.com/babysteps](https://smallcultfollowing.com/babysteps/) — read on `async fn in trait`, GATs, lending iterators
- **Signature ideas:** if it compiles in his head it'll compile for us; if not, the API is wrong.

---

## Public crate API design — Aaron Turon

- **GitHub:** [aturon](https://github.com/aturon)
- **Skill:** `turon`
- **Owns in tak-rs:** the public surface of `tak-cot` and `tak-bus`, anything we'd want to publish to crates.io
- **Read these:**
  - The [futures 0.1 → 0.3 evolution](https://github.com/rust-lang/futures-rs) and the async/await RFCs he drove
  - The [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) (he co-wrote)
  - "Aturon's blog" archives on language design tradeoffs
- **Signature ideas:** small set of orthogonal types; no leaky abstractions; if you must leak, leak via clearly-marked extension points.

---

## Performance parsing, CLIs, careful benching — Andrew Gallant (BurntSushi)

- **GitHub:** [BurntSushi](https://github.com/BurntSushi)
- **Skill:** none yet — read his code directly, capture as we go
- **Owns in tak-rs:** the type-prefix trie in `tak-bus` (CoT type strings like `a-f-G-U-C-I` are exactly his domain), `taktool` CLI, any hot bytestring scanning
- **Read these:**
  - [BurntSushi/ripgrep](https://github.com/BurntSushi/ripgrep) — how to make the fastest tool in a category and document why
  - [rust-lang/regex](https://github.com/rust-lang/regex) — DFA construction; the prefix trie pattern for our type filters
  - [BurntSushi/bstr](https://github.com/BurntSushi/bstr) — bytestring ops without `String` overhead
  - [BurntSushi/memchr](https://github.com/BurntSushi/memchr) — SIMD bytestring search; useful in `tak-cot` framing
  - His [benchmarking blog posts](https://blog.burntsushi.net/) — the discipline for "claim X is faster"
- **Signature ideas:** measure first, commit second; document the methodology in the README; allocate once and reuse.

---

## HTTP servers, hyper-stack — Sean McArthur

- **GitHub:** [seanmonstar](https://github.com/seanmonstar)
- **Skill:** none yet — read directly
- **Owns in tak-rs:** `tak-mission` axum/hyper layering, the SSE change feed, backpressure on the REST surface
- **Read these:**
  - [hyperium/hyper](https://github.com/hyperium/hyper) — the foundation under axum
  - [seanmonstar/warp](https://github.com/seanmonstar/warp) — Filter combinators; not what we use, but worth reading for routing thinking
  - [seanmonstar/reqwest](https://github.com/seanmonstar/reqwest) — client API ergonomics
- **Signature ideas:** keep the unsafe surface inside one crate; explicit `Service`/`MakeService` separation; HTTP/2 done right.

---

## TLS, cryptography — Brian Smith + Dirkjan Ochtman

- **GitHub:** [briansmith](https://github.com/briansmith), [djc](https://github.com/djc)
- **Skill:** none yet — read directly
- **Owns in tak-rs:** the TLS path in `tak-net` (rustls config, peer cert chain, cipher negotiation against legacy ATAK clients), QUIC in v1.1
- **Read these:**
  - [briansmith/ring](https://github.com/briansmith/ring) — the crypto under rustls
  - [rustls/rustls](https://github.com/rustls/rustls) — the TLS impl we use; read `ServerConfig`, the cert verification path
  - [quinn-rs/quinn](https://github.com/quinn-rs/quinn) — QUIC; what we'll use for 8090
  - [djc/instant-acme](https://github.com/djc/instant-acme) — ACME if we ever auto-cert
- **Signature ideas:** small attack surface; ban known-bad config at compile time; never roll your own crypto.

---

## Observability, tracing, sharded data structures — Eliza Weisman

- **GitHub:** [hawkw](https://github.com/hawkw)
- **Skill:** none yet — combine with `cantrill` for the philosophy layer
- **Owns in tak-rs:** the `tracing` schema across all crates, the connection table data structure, anything async + lock-free
- **Read these:**
  - [tokio-rs/tracing](https://github.com/tokio-rs/tracing) — span/event/subscriber model; the right way to instrument async code
  - [hawkw/sharded-slab](https://github.com/hawkw/sharded-slab) — lock-free, append-only slab; perfect for "10k+ live connections, churn at the edges"
  - Her tokio internals work
- **Signature ideas:** spans encode causality, not just function calls; sharded data structures avoid contention without sacrificing correctness.

---

## Observability as design — Bryan Cantrill

- **GitHub:** [bcantrill](https://github.com/bcantrill), [oxidecomputer](https://github.com/oxidecomputer)
- **Skill:** `cantrill`
- **Owns in tak-rs:** the principle that the running server must be debuggable from outside without recompiling. Drives the `tracing` schema, the metrics surface, the dynamic log-level endpoint.
- **Read these:**
  - DTrace papers (his original work) — the philosophy of "production debugging is a first-class feature"
  - Oxide Computer's [omicron](https://github.com/oxidecomputer/omicron) — Rust at scale with observability throughout
  - His talks on post-mortems and debuggability
- **Signature ideas:** if you can't see what's happening, it doesn't matter how fast it is.

---

## Performance, hardware sympathy — Casey Muratori

- **Skill:** `muratori`
- **Owns in tak-rs:** the periodic perf review of `tak-bus` and `tak-cot`. Cache lines, branch prediction, allocator pressure.
- **Read these:**
  - "Performance-Aware Programming" course — memory hierarchy, throughput vs latency
  - Handmade Hero archives — building from first principles
- **Signature ideas:** know what the hardware does; "clean code" claims are testable claims.

---

## Postgres + spatial — Andy Pavlo

- **Skill:** `pavlo`
- **Owns in tak-rs:** the `cot_router` table design, PostGIS GiST index choices, query plans for mission queries
- **Read these:**
  - CMU 15-445 / 15-721 lectures (free) — DB internals at the level we need
- **Signature ideas:** know your access patterns before designing the schema; benchmark with realistic data shapes.

---

## Distributed systems (when we get there) — Kleppmann, Lamport, Helland

- **Skills:** `kleppmann`, `lamport`, `helland`
- **Owns in tak-rs:** federation v1.1, cluster mode v2, anything that crosses a process boundary
- **Read these:**
  - "Designing Data-Intensive Applications" (Kleppmann) — the textbook
  - Lamport's papers on Paxos, logical clocks, time + ordering
  - Helland's "Life Beyond Distributed Transactions"
- **Signature ideas:** failure is the common case; idempotency is the API; events are the source of truth.

---

## Mission API as event log — Greg Young

- **Skill:** `young`
- **Owns in tak-rs:** the `tak-mission` change feed design, `MissionChange` table, replay/rebuild semantics
- **Signature ideas:** the event log is the source of truth; projections are caches; never lose an event.

---

## Testing — multiple

- **Property tests:** `hegel` (existing tool), `property-based` (Hughes / QuickCheck philosophy). Owns: every codec invariant, every algebra-style claim.
- **TDD discipline:** `beck-tdd`. Owns: how new modules grow.
- **Deterministic simulation:** `deterministic-simulation` (TigerBeetle / Joran Greef). Owns: `tak-bus` correctness under adversarial schedules.
- **Chaos:** `chaoslab`. Owns: failure-injection for the persistence path, network partitions in federation tests.

---

## Security

- **Threat modeling:** `mitre-attack`. Owns: adversary model for the wire surface; what does an evil federate look like.
- **Vuln intel:** `vulngraph`. Owns: dep advisory check; runs automatically via `cargo deny check advisories`.
- **Detection rules:** `roth`. Owns (later): if we expose audit logs, what Sigma rules detect lateral movement through TAK.

---

## How to use this list

1. Identify the domain you're working in.
2. Find the persona above.
3. Invoke their Skill before designing or reviewing.
4. If multiple personas overlap (e.g., `tak-mission` REST surface = Fielding for principles + McArthur for hyper specifics), invoke the one closest to the problem.
5. If a persona has no Skill yet (BurntSushi, McArthur, Smith/Ochtman, Weisman), open the linked repos and read first.

This list is not closed. When we discover a new domain, we add a persona.
When a persona's lessons stop being useful, we remove them. The point is
that every design decision in tak-rs has a name attached to it — someone
whose reputation depends on getting this kind of thing right.
