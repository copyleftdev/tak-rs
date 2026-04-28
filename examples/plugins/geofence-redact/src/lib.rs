//! `geofence-redact` — drop every CoT event whose latitude falls
//! below a configured threshold. Smoke-test plugin for the
//! tak-rs wasm component pipeline (per decision 0004).
//!
//! Config:
//!
//! ```json
//! { "drop_below_lat": 30.0 }
//! ```
//!
//! No config = drop nothing (threshold defaults to `f64::MIN`).

use std::cell::RefCell;

use tak_plugin_api::bindings::exports::tak::plugin::inbound::{Action, CotEvent, Guest};
use tak_plugin_api::bindings::tak::plugin::log;

struct State {
    drop_below_lat: f64,
    seen: u64,
    dropped: u64,
}

// Plugin instances are single-threaded by design — wasmtime gives
// each plugin its own Store, and the worker pool serializes calls
// per Store. RefCell here is fine; we'd never accidentally cross
// threads inside the wasm sandbox.
thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State {
        drop_below_lat: f64::MIN,
        seen: 0,
        dropped: 0,
    }) };
}

struct GeofenceRedact;

impl Guest for GeofenceRedact {
    fn init(config_json: String) -> Result<(), String> {
        // Tiny "JSON" parser — we only care about one float field
        // and don't want to drag a JSON crate into a wasm plugin
        // for what amounts to `{"drop_below_lat": 30.0}`. Real
        // plugins would use serde_json.
        let threshold = parse_drop_below_lat(&config_json).unwrap_or(f64::MIN);
        STATE.with(|s| s.borrow_mut().drop_below_lat = threshold);
        log::emit(
            log::Level::Info,
            &format!("geofence-redact: drop_below_lat = {threshold}"),
        );
        Ok(())
    }

    fn on_inbound(event: CotEvent) -> Action {
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.seen += 1;
            // Heartbeat: log every 1 000 events seen so operators
            // can confirm the plugin pipeline is live without
            // cranking log verbosity on the host.
            if st.seen.is_multiple_of(1000) {
                log::emit(
                    log::Level::Info,
                    &format!(
                        "geofence-redact: heartbeat — seen {} events, dropped {} (lat<{})",
                        st.seen, st.dropped, st.drop_below_lat
                    ),
                );
            }
            if event.lat < st.drop_below_lat {
                st.dropped += 1;
                Action::Drop
            } else {
                Action::Pass
            }
        })
    }

    fn shutdown() {
        STATE.with(|s| {
            let st = s.borrow();
            log::emit(
                log::Level::Info,
                &format!(
                    "geofence-redact: shutdown — saw {} events, dropped {} (lat<{})",
                    st.seen, st.dropped, st.drop_below_lat
                ),
            );
        });
    }
}

/// Hand-rolled scanner for `{"drop_below_lat": <number>}`. Returns
/// `None` for anything the test fixture doesn't produce; the
/// caller falls back to the disabled default. Keeps the wasm
/// blob tiny.
fn parse_drop_below_lat(s: &str) -> Option<f64> {
    let needle = "\"drop_below_lat\"";
    let idx = s.find(needle)?;
    let rest = s[idx + needle.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| {
            !c.is_ascii_digit() && c != '.' && c != '-' && c != '+' && c != 'e' && c != 'E'
        })
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

tak_plugin_api::bindings::export!(GeofenceRedact with_types_in tak_plugin_api::bindings);
