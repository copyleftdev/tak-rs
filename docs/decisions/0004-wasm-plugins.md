# 0004 — Wasm plugin contract

- **Date:** 2026-04-28
- **Status:** Proposed (design only — no code yet)
- **Issue:** Tracker, will open once the WIT lands
- **Replaces:** the upstream Java plugin manager (Spring Boot loader + JVM-only plugins) for new deployments

## Context

The upstream TAK Server's plugin story is the loudest operator-pain we've heard from people who run TAK in production:

1. **JVM-only.** Plugins must be Java/Kotlin/etc. Operators with Python data pipelines or Rust CoT enrichers have no path in.
2. **Untrusted-code sandbox is shaky.** Plugins run inside the messaging service's JVM; a misbehaving plugin can OOM the whole firehose. There's no real isolation.
3. **Deploy is "drop a jar in the manager dir."** No versioning, no signature check, no resource limits, no rollback.
4. **The plugin API is the messaging service's internal API.** It's huge, tied to Spring's lifecycle, and changes between TAK Server releases break plugins silently.

The 2026 modernization is **WebAssembly Component Model** plugins:

- **Polyglot.** Any language that compiles to wasm (Rust, Go, C/C++, Python via Pyodide-style, JS, AssemblyScript, MoonBit, …) can ship a tak-rs plugin.
- **Sandboxed by default.** wasmtime gives us memory limits, CPU/fuel limits, deterministic resource usage, and zero ambient capability — the plugin sees only what we explicitly hand it.
- **Versioned ABI.** WIT (Wasm Interface Types) gives us a typed contract that plugin authors generate bindings from. ABI breaks fail at load, not at the first message.
- **Hot reload.** A plugin component is a file; the host can swap one out at runtime without restarting the server.

## Decision

Adopt the **wasmtime 25+** runtime with **WIT-based component-model** plugins. Define **two** plugin interfaces in the v0 contract:

- `tak:plugin/inbound` — sees every CoT event after frame decode, before bus dispatch. Returns an action: `pass`, `drop`, or `replace`.
- `tak:plugin/mission-extension` — registers extra HTTP routes alongside `tak-mission`'s built-in `/missions/*` surface.

Two more interfaces are reserved for v1 (out of scope here): `subscription-eval` (custom delivery policy) and `auth-provider` (custom identity backend).

## The contract (WIT)

`crates/tak-plugin-api/wit/firehose.wit`:

```wit
package tak:plugin@0.1.0;

interface log {
  /// Plugins log via the host so all output lands in the same
  /// `tracing` schema as the rest of tak-rs. Plugins do NOT get
  /// stdout/stderr — those are explicitly closed.
  enum level { trace, debug, info, warn, error }
  record fields {
    keys:   list<string>,
    values: list<string>,
  }
  emit: func(level: level, message: string, fields: fields);
}

interface clock {
  /// Monotonic milliseconds since plugin load. Plugins do NOT get
  /// wall-clock — that is non-deterministic and a side channel.
  now-ms: func() -> u64;
}

interface inbound {
  /// One CoT event seen by the firehose, view-only.
  record cot-event {
    /// Wire bytes (length-prefixed protobuf TakMessage). Borrow only;
    /// the host re-uses the underlying allocation across calls.
    wire-bytes: list<u8>,
    cot-type: string,
    uid: string,
    callsign: option<string>,
    lat: f64,
    lon: f64,
    hae: f64,
    send-time-ms: u64,
    /// First 64 bits of the sender's group bitvector. Higher bits
    /// (we have 256 total) are not exposed to plugins in v0.
    sender-groups-low: u64,
  }

  /// What the host does with the event after the plugin returns.
  variant action {
    /// Forward unchanged. ~all messages should hit this path.
    pass,
    /// Drop silently. Host increments `tak.plugin.<name>.dropped`.
    drop,
    /// Replace wire bytes. Host re-decodes and re-dispatches; the
    /// returned bytes MUST still be a valid TAK Protocol v1 frame.
    /// Cost: one extra decode pass per replaced message.
    replace(list<u8>),
  }

  /// Called once per inbound CoT, on the plugin worker thread.
  /// The plugin's CPU time is bounded — see "Perf contract" below.
  on-inbound: func(event: cot-event) -> action;

  /// Called once at plugin load with the operator-supplied JSON
  /// config (TOML-decoded by the host into a JSON string for the
  /// plugin to parse). Must return within 1 s.
  init: func(config-json: string) -> result<_, string>;

  /// Called once at plugin unload. Plugin should flush any state.
  /// Must return within 1 s.
  shutdown: func();
}

world tak-plugin {
  export inbound;
  import log;
  import clock;
}
```

Future interfaces (sketches; not exported in v0):

```wit
interface mission-extension {
  /// Register HTTP routes under /plugins/<plugin-name>/...
  record route {
    method: string,    // "GET" | "POST" | …
    path:   string,    // "/widgets/:id"
  }
  routes: func() -> list<route>;

  /// Handle a request. Host strips the /plugins/<name> prefix.
  handle: func(req: request) -> response;
  // (request/response types elided for brevity)
}
```

## Lifecycle

```text
┌─────────────────┐    operator drops a .wasm file
│  watch dir      │     into <plugin-dir>
│  inotify event  │
└────────┬────────┘
         │
         ▼
┌─────────────────┐    wasmtime::Engine::precompile_module
│  precompile     │     (cached on disk under <plugin-cache>/)
└────────┬────────┘
         │
         ▼
┌─────────────────┐    Linker installs `log`/`clock` host fns
│  instantiate    │    Plugin's `init(config-json)` called
└────────┬────────┘    Returns Err → plugin marked as failed
         │
         ▼
┌─────────────────┐    Host pumps events into `on-inbound`
│  serve          │    Watches for file change → re-precompile +
│                 │    instantiate (zero-downtime swap with
│                 │    "drain old, switch, drain new" pattern)
└────────┬────────┘
         │ operator removes file, sends SIGHUP, etc.
         ▼
┌─────────────────┐    `shutdown()` called with 1 s budget
│  unload         │    Resources released
└─────────────────┘
```

A plugin-config file (`<plugin-dir>/<plugin>.toml`) controls per-plugin policy:

```toml
[plugin]
name = "geofence-redact"
enabled = true
priority = 100              # lower runs first

[limits]
max-memory-mb = 32          # wasmtime memory cap
max-cpu-ms-per-msg = 1      # epoch interrupt budget per call
max-rss-leak-mb = 0         # plugin instance recycled at this delta

[capabilities]
filesystem = []             # no fs access
network    = []             # no net access
plugin-config = """         # JSON passed to init()
{ "redact_below_lat": 30.0 }
"""
```

## Perf contract

**Plugins are NOT on the H1 hot path.**

The bus dispatch loop stays alloc-free per invariant H1. Plugin invocation happens on a *separate* tokio task that drains a bounded mpsc fed from the dispatch path:

```text
┌─────────────────┐
│  firehose       │
│  read_loop      │ ─── decode_stream ─── TakMessage::decode
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│  Bus::dispatch  │ ─── alloc-free fan-out (H1 contract)
│  (no plugins)   │     to subscribers
└────────┬────────┘
         │
         ▼  best-effort try_send (drops if full, like persistence)
┌─────────────────┐
│  Plugin runtime │
│  worker pool    │ ─── on-inbound() with epoch interrupt
│  (N threads)    │     after action: re-dispatch only if Replace
└─────────────────┘
```

Implications:

1. **Plugins observe AFTER bus dispatch.** Their `pass`/`drop`/`replace` returns affect a *secondary* fan-out, not the primary hot-path delivery. Subscribers see the original message at H3 latency; plugin-mutated copies, if any, are a separate event.
2. **`replace` is rare and slow.** It re-enters dispatch with new bytes. We document this and discourage it for any plugin that runs on >1 % of messages.
3. **Plugin overload drops messages.** If the plugin worker can't keep up, the bounded mpsc fills and new messages bypass the plugin entirely. Same back-pressure model as `Store::try_insert_event`. Counter: `tak.plugin.<name>.dropped`.
4. **Per-call CPU budget.** wasmtime epoch interruption fires after `max-cpu-ms-per-msg`. A plugin that exceeds its budget gets `action::drop` injected on its behalf; repeated overruns mark the plugin as failed and unload it.
5. **Per-instance memory cap.** wasmtime `Linker::limit_memory` enforces `max-memory-mb`. OOM in the plugin → plugin unloaded, host unaffected.

Performance budget at the design level:

| Workload | Plugin overhead per msg | At 50 k msg/s | At 600 k msg/s |
|---|---|---|---|
| trivial pass-through (~10 wasm instructions) | ~200 ns | 0.01 % CPU | 0.12 % |
| typical filter (lookup table + comparison) | ~5 µs | 0.25 % | 3 % |
| heavy enrichment (regex + state mutation) | ~50 µs | 2.5 % | 30 % (likely overloaded → drops) |

Plugins that run on every message *and* take >5 µs are encouraged to sample (`if (counter % 100 == 0)`) instead.

## Security model

| Surface | Default | Override |
|---------|---------|----------|
| Filesystem | none | `[capabilities] filesystem = ["/path"]` (read-only) |
| Network | none | `[capabilities] network = ["dns://...", "http://..."]` (allow-list) |
| Wall-clock time | none | always denied (deterministic perf, side-channel risk) |
| Stdout/stderr | redirected to `tracing::warn!` | always — no override |
| Random | per-plugin seeded ChaCha20 | always — host-provided entropy via `import random` |
| Process spawn / signal | none | always denied |
| Plugin-to-plugin call | none | future v1 (`tak:plugin/registry`) |

Threat model:

- **Malicious plugin author**: contained by wasmtime sandbox + capability deny-by-default. Worst case: plugin DoS's its own input queue (host kills it, others unaffected).
- **Compromised plugin author**: same as above plus signature verification (operator can pin a plugin by sha256 in the per-plugin TOML).
- **Plugin author + host operator collude**: out of scope. The operator already trusts the binary.

## Why now

| Argument | For | Against |
|---|---|---|
| 2026 wasm tooling is production-ready | wasmtime 25+ is stable, used by Fastly + Shopify in prod | yes |
| Polyglot ATAK ecosystem demand | yes — Python data scientists want CoT enrichers without learning Rust | — |
| Migration path from Java plugins | non-existent today; we'd ship a "JVM plugin shim" for back-compat | — |
| Cost of the contract being wrong | rev the package version (`tak:plugin@0.2.0`); old plugins refuse to load | — |
| Cost of NOT shipping plugins | operators stay on the Java upstream for plugin support → tak-rs niche-only | — |

## Out of scope (v0)

- **JVM plugin shim** — operators with existing TAK plugin jars can't migrate overnight. We will ship a separate compatibility layer in a later milestone.
- **Plugin marketplace / signing infra** — operators bring their own .wasm files for now.
- **Cross-plugin invocation** — plugins are isolated; chaining is via `priority` ordering, not direct calls.
- **Async plugin functions** — v0 is sync `on-inbound`. Async (yields to wait on host I/O) is wasm component-model `future`/`stream` and adds a lot of complexity.

## Action

1. ✅ This document.
2. Open the issue: "wasm plugin host runtime + hello-world plugin" (M6).
3. Implement `crates/tak-plugin-api` (the WIT + generated bindings used by both host and plugin authors).
4. Implement `crates/tak-plugin-host` (wasmtime wrapper, plugin loader, mpsc-fed worker pool).
5. Wire plugin worker into `tak-server` as a tokio task that drains a clone of the bus dispatch output. Opt-in via `--plugin-dir <path>`.
6. Ship a sample `crates/plugins/geofence-redact` that drops events with `lat < <threshold>`. Used as the integration smoke test.
7. Document plugin-author workflow in `docs/plugins.md` with `cargo component` example.

Estimate: ~1 week for the host runtime + sample plugin, plus a separate week for the JVM compat shim if/when an operator asks for it.
