<p align="center">
  <img src="assets/brand/banner.svg" alt="tak-rs" width="100%"/>
</p>

# tak-rs

A high-performance Rust kernel for the [TAK (Team Awareness Kit)](https://tak.gov)
ecosystem. Drop-in replacement for the messaging core of the upstream Java
[TAK Server](https://github.com/TAK-Product-Center/Server), optimized for
single-node deployments at 10k+ concurrent mTLS streaming clients.

## Status

Pre-1.0. The wire kernel is live: 8088 plain TCP and 8089 mTLS streaming
firehose, framed TAK Protocol v1 in / out, subscription registry with
type-prefix + geo + UID + group filters, Postgres-backed persistence
that never blocks fan-out, and graceful SIGTERM/SIGINT drain. Mission
API surface is partial. Federation and clustering are deliberately
out of scope for v1.

## Layout

```
crates/
  tak-cot         — Cursor-on-Target codec (XML + protobuf TAK Protocol v1)
  tak-proto       — generated protobuf types (vendored from upstream)
  tak-net         — tokio listeners (TCP / TLS / UDP / multicast / QUIC)
  tak-bus         — subscription registry + fan-out (the firehose core)
  tak-store       — Postgres + PostGIS persistence
  tak-mission     — Mission API (axum + tower)
  tak-config      — CoreConfig.xml subset parser
  tak-server      — binary
  taktool         — CLI (replay, sub, pub, latency, loadgen)
  tak-bus-vopr    — deterministic simulator + replay/minimize for tak-bus
  tak-soak        — wall-clock soak harness (RSS-drift + p99 latency gates)
  tak-conformance — wire-level scenarios pinned against the reference behaviour
```

## Read these first

- `CLAUDE.md` — entry-point doctrine; loaded automatically.
- `docs/architecture.md` — full design with Java→Rust component map.
- `docs/personas.md` — engineers we model on.
- `docs/invariants.md` — non-negotiable rules (correctness, hot path, discipline).

## Verification harnesses

- **Unit + integration** — `cargo nextest run --workspace`.
- **Property + round-trip** — `proptest` on every codec, lossless XML
  round-trip enforced as a correctness invariant.
- **Fuzz** — `cargo-fuzz` targets on the XML decoder and the streaming
  framer, seeded from canonical CoT fixtures.
- **Hot-path allocation** — `dhat` test asserts zero steady-state
  allocations in `tak_bus::dispatch`.
- **Concurrency** — `loom` model on the dispatch path.
- **Deterministic simulation** — `tak-bus-vopr` runs the bus under
  controlled schedules with `--replay` and `--minimize` for shrinking.
- **Soak** — `tak-soak` runs a wall-clock workload with a pinned
  `taktool latency` probe; gates on RSS drift and p99.
- **Conformance** — `tak-conformance` pins wire scenarios against
  observed reference behaviour.

## Common workflows

```sh
cargo nextest run --workspace        # tests
cargo bench --bench firehose         # hot-path bench
cargo deny check                     # dep policy gate
cargo clippy --workspace -- -D warnings
```

Slash commands (in Claude Code):

- `/proto-sync` — pull `.proto` from upstream, regenerate
- `/bench-hot` — run firehose bench, diff vs baseline
- `/check-invariants` — full gauntlet
- `/fuzz-codec` — cargo-fuzz round
- `/replay-pcap <file>` — replay a capture through the server

## Contributing

See `CONTRIBUTING.md`. Quality gates live in `.githooks/` — run
`./scripts/install-hooks.sh` once after cloning. There is intentionally
no cloud CI; the hooks enforce the same gates locally.

## Security

Report vulnerabilities privately via the channel described in
`SECURITY.md`. The machine-readable contact lives at
`.well-known/security.txt`.

## License

MIT OR Apache-2.0.
