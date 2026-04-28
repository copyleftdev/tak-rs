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

tak-server \
    --database-url postgres://tak:takatak@127.0.0.1/tak \
    --listen-cot 127.0.0.1:8088 \
    --plugin-dir /tmp/tak-plugins
```

The plugin's logs land on tak-server's `tracing` output under
`plugin = geofence_redact`. With the bench loadgen pumping the
fixture corpus, expect to see the periodic
`dropped N of M (lat<…)` lines once the threshold is set above
the fixture latitudes (defaults to `f64::MIN` = drop nothing).

## Config

The plugin reads `--plugin-config` JSON at init (currently
hard-coded to empty string in `tak-plugin-host`; per-plugin TOML
that supplies this is a future step). The shape:

```json
{ "drop_below_lat": 30.0 }
```

Anything else is ignored. Set above 36 to drop the canonical PLI
fixture (`lat = 35.x`).
