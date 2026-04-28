# geofence-redact — sample tak-rs plugin

Drops every CoT event whose latitude is below a configured
threshold. Smoke test for the wasm-component plugin pipeline
(decision 0004).

## Build

```bash
rustup target add wasm32-wasip2

cd examples/plugins/geofence-redact
cargo build --release --target wasm32-wasip2

# Output: target/wasm32-wasip2/release/geofence_redact.wasm
```

## Run alongside tak-server

```bash
mkdir /tmp/tak-plugins
cp target/wasm32-wasip2/release/geofence_redact.wasm /tmp/tak-plugins/
cp geofence_redact.toml /tmp/tak-plugins/

tak-server \
    --database-url postgres://tak:takatak@127.0.0.1/tak \
    --listen-cot 127.0.0.1:8088 \
    --plugin-dir /tmp/tak-plugins
```

The plugin's logs land on tak-server's `tracing` output under
`plugin = geofence_redact`. With the bench loadgen pumping the
fixture corpus, expect to see the periodic
`heartbeat — seen N events, dropped M (lat<…)` lines.

## Config

`<plugin-stem>.toml` next to the wasm controls the plugin. Schema
mirrors `docs/decisions/0004-wasm-plugins.md`:

```toml
[plugin]
name = "geofence_redact"
enabled = true               # set false to skip loading
priority = 100

[limits]
max-memory-mb = 32           # wasmtime per-instance cap (enforced)
max-cpu-ms-per-msg = 1       # epoch budget (parsed; ticker is future work)
max-rss-leak-mb = 0          # parsed; inert in v0

[capabilities]
filesystem = []              # parsed; inert (deny-everything WasiCtx)
network = []                 # parsed; inert
plugin-config = '{ "drop_below_lat": 36.0 }'
```

The `plugin-config` JSON is what gets passed to the plugin's
`init()`. For `geofence-redact` the schema is just one field:

```json
{ "drop_below_lat": 30.0 }
```

Anything else is ignored. Set the threshold above 36 to drop the
canonical PLI fixture (`lat = 35.x`); the bundled
`geofence_redact.toml` does this so the smoke test exercises the
drop path.
