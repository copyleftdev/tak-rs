# tak-rs — Architecture Deep-Dive

A from-scratch Rust reimplementation of the TAK Server core. This document maps the
existing Java reference (`github.com/TAK-Product-Center/Server`, cloned at
`.scratch/takserver-java/`) onto a Rust workspace, calls out where we deliberately
diverge, and identifies the perf wins worth chasing.

All Java path/class references are real, taken from a recon pass on the upstream
tree. Anything speculative is flagged "TBD".

---

## 1. Goals & non-goals

### Goals (v1)

- Be a **drop-in replacement for the messaging core**: a process you can point
  ATAK / iTAK / WinTAK at and have it work — same wire formats, same default
  ports (8087 / 8088 / 8089), same multicast mesh (239.2.3.1:6969).
- **Single-node only.** No clustering. No Ignite.
- **Postgres-compatible schema.** Same tables (`cot_router`, mission tables) so
  an existing TAK Server's database can be opened by ours and vice-versa.
- **mTLS streaming firehose at 10k+ concurrent client connections per node**,
  with per-message group-bitvector authorization, on a single mid-range box.
- **Mission API parity for the read path.** Subscribe to a mission, receive
  changes, sync existing data. Write path can lag.

### Non-goals (v1)

- Federation (v1 byte protocol or v2 gRPC). Designed in but not built.
- Plugin SDK over Ignite topics — a plugin is a separate process talking
  Ignite. Not happening. We'll expose an in-process trait + an out-of-process
  WebSocket fan-out instead. Deferred.
- WebTAK / web UI / admin REST. Deferred.
- Cluster mode. The Java cluster product is essentially "core + Ignite +
  Kubernetes manifests". Out of scope until single-node is solid.
- Full CoreConfig.xml schema parity. We'll parse the subset we use and reject
  the rest with clear errors.

### Non-goal that's actually a goal: schema-level XML preservation

CoT's `<detail>` element is half-typed in the protobuf schema (Contact, Group,
PrecisionLocation, Status, Takv, Track) and half-untyped (`xmlDetail` string,
`detail.proto:53`). A round-trip XML→proto→XML must produce equivalent XML or
ATAK clients render wrong. This is a **correctness** requirement, not a perf
one, and it's where most third-party TAK implementations break.

---

## 2. The performance thesis

The Java server's costs, as visible from the code (not benchmarked yet):

| Cost | Where | Rust win |
|------|-------|----------|
| Per-connection threads via `OrderedExecutor` pools | `Resources.tcpProcessor`, `udpProcessor`, `brokerMatchingProcessor`, `messageProcessor` (`takserver-core/src/main/java/com/bbn/marti/service/Resources.java`) | tokio task per connection; one runtime, M:N scheduling |
| GC pauses on the firehose | Netty `ByteBuf` pooling helps but JVM GC still bites at sustained 50k msg/s | `bytes::Bytes` ref-counted buffers; no GC; fan-out can be zero-copy |
| XML-in-protobuf parsing twice | `MessageConverter` parses CoT XML → protobuf, then for each subscriber that wants XML it serializes back | We parse once into a borrowed view; subscribers that want XML get the original byte slice; subscribers that want protobuf get the encoded protobuf — never re-encode |
| BigInteger group bitvector AND on every subscriber per message | `RemoteUtil.bitVectorStringToInt` + `BigInteger.and` in `DistributedSubscriptionManager` (~line 2106) | Fixed-width `[u64; N]` bitvector with const N (typical TAK deployments have <256 groups; 4×u64 = 256 bits, AND in 4 instructions) |
| Ignite as message bus + cache | `DistributedCotMessenger`, `IgniteCacheHolder` everywhere | Single-node: in-process channels + dashmap. Multi-node: deferred. |
| Per-message regex/XPath subscription filters | `DistributedSubscriptionManager.getSubscriptionsForMessage` (~line 696) walks all subs, applies XPath | Compile filters once into a small DFA (type prefix tree + geo R-tree + UID set); never touch unmatched subs |

The realistic perf goal: **5-10× throughput per box, ~½ p99 latency, ~⅕
memory** vs the Java server, on the firehose hot path. Mission API and other
cold paths: tie or modest gains; correctness is the main thing there.

---

## 3. Wire protocol reference (concise)

### 3.1 Framing

Every framed message starts with magic byte `0xBF`.

| Transport | Framing | Notes |
|-----------|---------|-------|
| Mesh (UDP unicast / multicast) | `0xBF 0x01 0xBF <protobuf payload>` | Fixed 3-byte header, single UDP datagram |
| Stream (TCP, TLS, QUIC) | `0xBF <varint length> <protobuf payload>` | Length-prefixed; varint is google protobuf wire format |
| Legacy v0 | Raw CoT XML, no header | Plain TCP port 8087 ("open-squirt-close"); UDP mesh also accepts raw XML |

### 3.2 Protobuf schema (canonical, from `takserver-protobuf/src/main/proto/`)

```
TakMessage (takmessage.proto)
├── TakControl  takControl       — protocol negotiation
├── CotEvent    cotEvent         — the actual event
├── uint64      submissionTime   — server-stamped
└── uint64      creationTime     — client-stamped

CotEvent (cotevent.proto)
├── string  type, access, qos, opex, caveat, releaseableTo, uid, how
├── uint64  sendTime, startTime, staleTime  — ms since epoch
├── double  lat, lon, hae, ce, le           — 999999 = unknown
└── Detail  detail                          — see below

Detail (detail.proto)
├── string             xmlDetail   — un-typed leftover XML (CRITICAL: see §1)
├── Contact            contact     — callsign, endpoint
├── Group              group       — __group element (team color/role)
├── PrecisionLocation  precisionLocation
├── Status             status      — battery
├── Takv               takv        — device/version info
└── Track              track       — speed, course
```

The server-side envelope used between the API process and the messaging
process is `Message` (`message.proto`):

```
Message
├── TakMessage           payload
├── string               source, clientId, connectionId, feedUuid
├── repeated string      groups, destClientUids, destCallsigns, provenance
├── bool                 archive
└── repeated BinaryPayload  bloads
```

This is the routing envelope. Our equivalent will be a Rust struct, not
protobuf — it never leaves the process.

### 3.3 Federation (deferred)

`fig.proto` defines the gRPC service `FederatedChannel` with bidi-streaming
RPCs `ServerEventStream`, `ClientEventStream`, `ServerROLStream`,
`ClientROLStream`, plus group-exchange and X.509 token RPCs. Wire format is
`FederatedEvent`/`GeoEvent`, *not* `TakMessage` — a totally separate but
isomorphic schema. We'll write a converter layer when we tackle federation.

### 3.4 Default ports

| Port | Proto | Purpose |
|------|-------|---------|
| 8087/tcp | CoT XML | Legacy "open-squirt-close" |
| 8088/tcp | CoT XML or TAK v1 | Streaming TCP, no TLS |
| 8089/tcp | TAK v1 over TLS 1.2+ | Production streaming, mTLS |
| 8090/udp | TAK v1 over QUIC | Newer alternative |
| 6969/udp on 239.2.3.1 | TAK v1 mesh | SA multicast mesh |
| 8443/tcp, 8446/tcp | HTTPS | Admin REST + Mission API + WebTAK |
| 9001/tcp | gRPC over TLS | Federation v2 |
| 9000/tcp | bespoke binary over TLS | Federation v1 (legacy) |

---

## 4. Crate layout

```
tak-rs/
├── Cargo.toml                     # workspace
├── crates/
│   ├── tak-cot/                   # CoT XML codec + framing
│   ├── tak-proto/                 # generated protobuf types (vendored .proto)
│   ├── tak-net/                   # tokio listeners, mTLS, codecs
│   ├── tak-bus/                   # subscription registry + fan-out core
│   ├── tak-store/                 # Postgres + spatial index access
│   ├── tak-mission/               # mission API (REST + change feed)
│   ├── tak-config/                # CoreConfig.xml subset parser
│   ├── tak-server/                # binary that wires it all together
│   └── taktool/                   # CLI: pub, sub, replay, fuzz
├── docs/
│   └── architecture.md            # this file
└── .scratch/
    └── takserver-java/            # shallow clone for reference
```

### Why this split

- **`tak-cot` is `no_std`-friendly and depends only on `bytes` + `quick-xml`.**
  Means embedded gateways can use it. Means we can fuzz the parser in
  isolation.
- **`tak-proto` is generated only.** Pure prost output, no business logic.
  Lets us regenerate without churn.
- **`tak-net` knows nothing about subscriptions.** It produces an
  `(InboundCot, ConnectionHandle)` stream and consumes an
  `OutboundCot` stream. Clean seam for testing.
- **`tak-bus` knows nothing about sockets or storage.** Pure
  `(message_in) → (subscriber_id, message_out)*`. Benchmarkable in isolation.
- **`tak-store` is a port; `tak-mission` is a service that uses it.** Mission
  logic doesn't talk to Postgres directly.
- **`tak-config` is its own crate** because CoreConfig.xml's schema is
  enormous and we want strict feature gating ("we support sections X, Y, Z;
  fail loudly on the rest").

---

## 5. Java → Rust component map

Each section: what's in the Java tree, what we build in Rust, deviations.

### 5.1 Network listeners → `tak-net`

**Java reference**: `com.bbn.marti.nio.netty.NioNettyBuilder` (`takserver-core/src/main/java/com/bbn/marti/nio/netty/NioNettyBuilder.java`, lines 60–349). All transports built here: `buildTcpServer`, `buildStcpServer`, `buildTlsServer`, `buildUdpServer`, `buildMulticastServer`, `buildGrpcServer`, `buildQuicServer`. Library is **Netty**; epoll opt-in via CoreConfig flag. Handlers extend `NioNettyHandlerBase`; TLS is `NioNettyTlsServerHandler`.

**Surprise from recon**: a legacy hand-rolled `NioServer` (raw `java.nio.channels.Selector`) coexists with Netty for UDP/multicast paths. This is dead weight we don't replicate.

**Rust mapping**:

```
tak-net/
├── tcp.rs        # plain TCP on 8087/8088 — accepts XML or TAK v1
├── tls.rs        # TLS 1.3 on 8089 — rustls + tokio-rustls
├── quic.rs       # QUIC on 8090 — quinn (deferred to v1.1)
├── udp.rs        # UDP unicast and multicast on 6969/239.2.3.1
├── codec.rs      # tokio_util::codec::Decoder/Encoder for the framing
└── conn.rs       # ConnectionHandle: id, peer cert chain, send queue
```

Stack: tokio + rustls + tokio-rustls. One `tokio::spawn` per accepted
connection — no thread pool sizing knobs needed.

**Connection lifecycle**:
1. Accept TCP, do TLS handshake, extract peer cert chain.
2. Pass cert chain to `tak-auth` to resolve identity → group bitvector.
3. Allocate `ConnectionId(u64)` (monotonic), insert into bus.
4. Read loop: framed decode → push `(ConnectionId, TakMessage)` into bus inbound.
5. Write loop: per-connection bounded mpsc; on full, drop or disconnect per
   policy (`<dissemination smartRetry>` in CoreConfig).

**Codec detail**: the magic-byte / varint framing means we can't use
`LengthDelimitedCodec` as-is; write a custom `Decoder` that peeks the magic
byte to decide v0 (XML) vs v1 (proto).

### 5.2 Auth + groups → `tak-auth` (lives inside `tak-net` initially)

**Java reference**: `com.bbn.marti.groups.DistributedPersistentGroupManager` (line 72), `X509Authenticator.authenticate` (line 117). X.509 DN/OU → group lookup, with file-based / LDAP / OAuth fallbacks. Groups stored in Ignite replicated cache.

**Group bitvector**: stored as hex string of a Java `BigInteger`. Per-message AND in `DistributedSubscriptionManager` ~line 2106. Per-connection cached in `IgniteCacheHolder.getIgniteUserOutboundGroupCache`.

**Rust mapping**:

```rust
// tak-auth/src/lib.rs
pub struct GroupBitvector([u64; 4]);   // 256 groups; widen to [u64; 8] if needed

impl GroupBitvector {
    #[inline]
    pub fn intersects(&self, other: &Self) -> bool {
        self.0[0] & other.0[0]
        | self.0[1] & other.0[1]
        | self.0[2] & other.0[2]
        | self.0[3] & other.0[3] != 0
    }
}
```

This is the single biggest CPU win on the hot path: ~4 instructions vs
`BigInteger.and()` allocation.

Group resolution: file-based (UserAuthenticationFile.xml) only in v1; LDAP and
OAuth deferred. Cert DN parsing via `x509-parser` crate.

### 5.3 Message routing / fan-out → `tak-bus`

**Java reference**: 
- `com.bbn.marti.service.SubmissionService` (line 153) — inbound entry point.
- `com.bbn.marti.service.SubscriptionStore` (line 64) — `ConcurrentHashMap`s for `uidSubscriptionMap`, `callsignMap`, `clientUidToSubMap`.
- `com.bbn.marti.service.BrokerService` (line 28) — iterates matched subs.
- `DistributedSubscriptionManager.getSubscriptionsForMessage` (line 696) — XPath predicates.
- Ignite topics for cluster fan-out (`DistributedCotMessenger`, line 55).

**Rust mapping**:

```rust
// tak-bus/src/lib.rs
pub struct Bus {
    subs:        DashMap<SubId, Subscription>,
    by_uid:      DashMap<String, SubId>,        // direct addressing
    by_callsign: DashMap<String, SubId>,
    geo_index:   RwLock<RTree<GeoSub>>,         // bbox-filtered subs
    type_index:  RwLock<TypePrefixTrie>,        // type='a-f-G-*'-style filters
    inbound:     mpsc::Receiver<Inbound>,
}
```

**Per-message dispatch**:
1. Decode envelope; resolve sender's group bitvector from connection state.
2. If `destClientUids` or `destCallsigns` is set → direct lookup, done.
3. Else: candidate set = type_index ∩ geo_index lookup (both small).
4. For each candidate: bitvector AND on group, push to subscriber's mpsc.

**Backpressure**: each subscription has a bounded mpsc (default 1024). On
full, default policy is **drop oldest** for SA-type messages
(latest-position-wins) and **disconnect** for chat. Configurable per-type via
CoreConfig `<dissemination>`.

**Ordering**: Java uses `OrderedExecutor` to guarantee per-subscriber
in-order delivery. mpsc is single-consumer so we get this for free per
subscription.

### 5.4 Storage → `tak-store`

**Java reference**: Flyway migrations in `takserver-schemamanager/src/main/resources/db/migration/`. Base schema in `V7__create_base_schema.sql`. `cot_router` table stores all CoT events with PostGIS `event_pt` (GiST-indexed). Mission tables from `V12__mission_api_tables.sql`. PostGIS is mandatory.

**Surprise**: mix of JPA (`Mission`, `Resource`, `MissionChange` etc. as `@Entity`) and raw `JdbcTemplate` (`RepositoryService`, line 717). Some Ignite SQL too.

**Rust mapping**: 
- `sqlx` with compile-time-checked queries against Postgres.
- PostGIS stays — we use `geo-types` + `sqlx-postgres` PostGIS support.
- Reuse the Java schema verbatim. Vendor the Flyway scripts into
  `tak-store/migrations/` and run them at startup if the schema is missing.
  This buys us bidirectional compatibility with existing Java deployments.
- No ORM. Plain SQL, mapped to Rust structs.

**Hot-path persistence**: `cot_router` writes are async + batched. The Java
server uses `messagePersistenceProcessor` thread pool (`Resources.java` line
88). We use a dedicated tokio task that drains a bounded channel and bulk-
inserts every 100ms or 1000 rows, whichever first. **Persistence MUST NOT
block fan-out** — if the persistence channel fills, we drop persistence
before we drop delivery.

### 5.5 Mission API → `tak-mission`

**Java reference**: `com.bbn.marti.sync.api.MissionApi` (`takserver-core/takserver-war/src/main/java/com/bbn/marti/sync/api/MissionApi.java`, line 129). Spring `@RestController`. **2800+ lines, single class.** Endpoint surface includes mission CRUD, `/missions/{name}/subscription`, `/missions/{name}/contents`, `/missions/{name}/archive`, `/missions/{name}/changes`, keyword management, `/sync/search`.

**Rust mapping**:
- `axum` for HTTP, `tower-http` for middleware.
- Endpoints split by resource: `mission.rs`, `subscription.rs`, `content.rs`, `change.rs`, `sync.rs`.
- Mission change feed delivered via SSE (HTTP/1.1) and WebSocket — Java uses
  long-polling + WebSocket; SSE is simpler and works through proxies.
- v1 covers: create/list/get/delete mission, subscribe, list contents, fetch
  changes since timestamp. Defer: archive, COT-history queries, KML export.

**The `MissionApi` class is a smell, not a model.** It's a single class
because Spring made it convenient. We split it.

### 5.6 Federation → `tak-fed` (v1.1+)

**Java reference**: `tak.server.federation.FederationServer` (line 161), `TakFigClient` (line 192). gRPC/mTLS using Netty. v1 legacy is bespoke binary (`NioNettyFederationServerHandler`).

**Rust mapping (when we get there)**:
- `tonic` over `tokio-rustls`.
- Vendor `fig.proto` into `tak-proto`.
- Bidirectional `FederatedEvent` ↔ `TakMessage` converter — they're
  isomorphic but not identical (`GeoEvent` flattens what `CotEvent + Detail`
  splits, and adds `federateProvenance`/`federateHops`).
- Skip v1 entirely. Anyone running v1 federation in 2026 has bigger problems.

### 5.7 Plugins → out-of-process WebSocket fan-out (deferred)

**Java reference**: `tak.server.plugins.TakServerPlugin` annotation; plugins run in `takserver-plugin-manager` JVM and talk to core over Ignite topics (`PLUGIN_SUBSCRIBE_TOPIC`, `PLUGIN_PUBLISH_TOPIC`).

**Rust mapping**: don't replicate Ignite. Expose:
- An **in-process** `trait MessageInterceptor` for compiled-in extensions.
- An **out-of-process** WebSocket endpoint at `/firehose` that streams the
  same envelope as `Message` (probably as JSON for now, protobuf later) and
  accepts injected messages back. This is functionally equivalent to the
  plugin SDK without forcing a JVM-style lifecycle.

### 5.8 Config → `tak-config`

**Java reference**: `CoreConfig.xsd` in `takserver-common/src/main/xsd/`, example at `takserver-core/example/CoreConfig.example.xml`. Sections: `<network>`, `<auth>`, `<submission>`, `<subscription>`, `<repository>`, `<repeater>`, `<dissemination>`, `<filter>`, `<buffer>`, `<security>`, `<federation>`.

**Rust mapping**: parse the subset we use with `serde-xml-rs` or `quick-xml`. Hard error on unknown sections (don't silently ignore).

v1 supports: `<network>` (input + connector), `<auth>` (file only), `<repository>`, `<security>` (TLS), `<dissemination>`. Defers everything else.

---

## 6. Hot-path walkthrough: a CoT event from socket to N subscribers

1. **TLS bytes arrive** on a tokio socket bound to 8089.
2. `tak-net::tls` rustls completes the handshake; we get a `&[Certificate]` for the peer.
3. `tak-auth` extracts the cert DN, looks up file-based user, computes
   `GroupBitvector` → cached in `ConnectionState` on the connection task.
4. Read loop in `tak-net::codec`:
   - Read 1 byte: `0xBF`? → it's TAK v1.
   - Read varint length.
   - Read N bytes into a `Bytes` slice (zero-copy from rustls).
   - `prost::decode` on `TakMessage`.
5. Wrap in `Inbound { msg, conn_id, sender_groups, raw_bytes }` and push into `tak-bus` inbound mpsc.
6. **Bus dispatch task** (one per CPU core, sharded by `conn_id` hash):
   - Compute candidate subscriber set from indices (type prefix + geo bbox).
   - For each candidate, `sender_groups.intersects(sub.groups)`?
   - If destClientUids/Callsigns set: bypass index, direct lookup.
   - For each match: `sub.outbound.try_send(raw_bytes.clone())` — `Bytes` is
     ref-counted, clone is `Arc`-bump.
7. **Per-subscriber writer task**: drain mpsc, write to socket. If subscriber
   wants XML and message arrived as proto: `tak-cot::proto_to_xml`
   (allocates). If subscriber matches sender's protocol: zero re-encode.
8. **Persistence side-channel**: same `Inbound` is pushed (try_send,
   non-blocking) to a persistence task that batches into `cot_router`.
9. **Mission side-channel**: if any subscribed mission's filter matches, push
   to that mission's change feed (which feeds SSE/WS subscribers via
   `tak-mission`).

Allocations on the hot path: the protobuf decode (one), the candidate Vec
(reusable per-task buffer), the per-subscriber `Bytes::clone` (no-alloc Arc
bump). **Target: zero alloc for the steady-state SA case where all clients
speak the same protocol.**

---

## 7. Threading & runtime

- **One tokio runtime, multi-threaded scheduler**, default to `num_cpus`.
- **Connection tasks** are cheap; spawn one per connection. No bespoke
  executor.
- **Bus dispatch** is sharded by `conn_id` hash across N tasks (N = num_cpus).
  This preserves per-connection ordering without locking.
- **No global mutexes on the hot path.** `dashmap` for the subscriber
  registries; `parking_lot::RwLock` for the geo + type indices (writes are
  rare — only on subscribe/unsubscribe).
- **Persistence and mission feeds** run on the same runtime but on
  lower-priority tasks. We'll add a separate runtime if benchmarks show
  starvation, but starting simple.

The Java `OrderedExecutor` model — guaranteed per-key in-order, bounded queue
— maps naturally to "one mpsc per subscription". We don't need to replicate
the executor abstraction.

---

## 8. What's deliberately weird

These are the choices that will surprise someone coming from the Java code.
Each has a reason; flagging them here so they don't get "fixed" later.

1. **No Ignite, no message bus.** Single-node only in v1. The Java server's
   distributed everything is a constraint of being designed for cluster mode
   from day one. Single-node is the 95% case.

2. **No JPA-style ORM.** Raw SQL via sqlx. The Java entities buy you nothing
   we need.

3. **`Detail.xmlDetail` is preserved as a borrowed `&str` slice into the
   original message.** No round-trip to a DOM. This makes the lossless XML
   round-trip cheap.

4. **Group bitvector is fixed-width `[u64; 4]`.** If a deployment has >256
   groups, we widen. Almost no one does.

5. **mTLS is the default; plain TCP on 8087/8088 is opt-in.** Java treats
   them as equal. We make the secure default the easy path.

6. **Filters are compiled, not interpreted.** The Java path runs XPath per
   message per subscription. We compile each subscription's filter (type
   prefix + geo bbox + UID set + group mask) into a struct and lookup is
   index-driven.

7. **Persistence can be turned off.** `cot_router` insertion is optional
   per-deployment; many edge gateways don't need event history.

---

## 9. Open questions

These need a decision before code lands; flagging now.

1. ~~**rustls or openssl for TLS?**~~ **Resolved 2026-04-28.** Decision:
   rustls 0.23 + aws_lc_rs + `tls12` feature. The Java server pins
   exactly three RFC 6460 ("Suite B") cipher suites
   (`SSLConfig.java:404-408`); rustls supports a strict superset. See
   `docs/decisions/0002-tls-ciphers.md`. Confirmation pcap pending real
   ATAK device — capture method in `scripts/capture-atak-handshake.sh`.

2. **QUIC: quinn or s2n-quic?** quinn is more mature; s2n is what AWS uses.
   Either works; defer until we actually build the QUIC listener.

3. **Mission change feed transport: SSE, WebSocket, or both?** Java does
   long-poll + WS. SSE is simpler; WS is what WinTAK uses today. Probably
   ship both behind a single endpoint that content-negotiates.

4. **CoT XML parser: quick-xml borrowed mode, or roxmltree?** Borrowed
   quick-xml is faster but the API is verbose. roxmltree allocates but is
   ergonomic. Benchmark both on a 10kB CoT message before deciding.

5. **Bench harness: what counts as "the firehose"?** Need a synthetic load
   pattern that matches real ATAK traffic. Probably: 70% PLI updates (small,
   periodic), 20% chat/markers, 10% large detail blobs. *Action: get a
   pcap from a real exercise if possible.*

6. **Where do we draw the line on CoreConfig.xml compatibility?** A
   deployment switching from Java to Rust will hand us their existing
   CoreConfig.xml. Do we accept it and ignore unsupported sections (with a
   warning)? Or hard-fail and require a translated config? *Lean toward
   hard-fail with a clear "unsupported section: X" error.*

---

## 10. Out-of-scope reference: the Java tree

Subprojects we are explicitly **not** porting in v1:

- `takserver-cluster` — cluster orchestration; we are single-node.
- `takserver-retention` — scheduled mission archive; cron + sql script later.
- `takserver-plugin-manager` + `takserver-plugins` — Ignite-based plugin
  loader; replaced by in-process trait + WS firehose.
- `takserver-usermanager` — admin web UI for user mgmt; CLI only in v1.
- `takserver-schemamanager` — Flyway runner; we vendor the SQL and run it
  ourselves with sqlx-migrate.
- `takserver-takcl-core` — test client library; we'll build our own in
  `taktool`.
- `takserver-fig-core` — federation v1; deferred.
- `federation-hub-*` — federation hub product; separate deliverable.
- `takserver-tool-ui` — admin UI; separate deliverable.

What we **do** port from Java (in dependency order):

1. `takserver-protobuf/` → vendored into `tak-proto`
2. `takserver-common/src/main/xsd/CoreConfig.xsd` → `tak-config`
3. `takserver-schemamanager/src/main/resources/db/migration/` → `tak-store/migrations/`
4. `takserver-core/src/main/java/com/bbn/marti/nio/netty/` → `tak-net`
5. `takserver-core/src/main/java/com/bbn/marti/groups/` → `tak-auth`
6. `takserver-core/src/main/java/com/bbn/marti/service/` (Submission, Subscription, Broker) → `tak-bus`
7. `takserver-core/takserver-war/src/main/java/com/bbn/marti/sync/api/` → `tak-mission`

---

## 11. Milestones

| M | Deliverable | "Done" looks like |
|---|-------------|-------------------|
| M0 | Workspace scaffolded, `tak-proto` builds | `cargo build` green; `tak-cot` round-trips a CoT XML sample to TakMessage and back, byte-identical on `xmlDetail` |
| M1 | TLS listener accepts a real ATAK client | ATAK on phone connects to `tak-server` on 8089 with mTLS, sees a "you're connected" PLI from a synthetic peer |
| M2 | Two ATAKs see each other | Two phones with different certs in the same group see each other's PLI on the map; chat works |
| M3 | Postgres persistence | Restart the server; replay shows historical CoT |
| M4 | Mission API (read path) | A mission created via REST is visible to a subscribed client; changes stream over SSE |
| M5 | Bench vs Java | 10k connections, 50k msg/s sustained on a 16-core box |

M0–M3 is the meaningful slice. M4 makes it useful for a real deployment. M5
is the "did the perf thesis hold" check.
