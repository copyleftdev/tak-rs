//! `echo-replace` — return `Action::Replace(event.wire_bytes)` for
//! every CoT event, so the firehose re-dispatches each frame
//! through the bus a second time. Smoke-test plugin for the
//! `Action::Replace` path of the tak-rs plugin pipeline.
//!
//! Real plugins doing PII redaction would decode the protobuf,
//! mutate fields, and re-encode. Pulling prost into a wasm
//! component is heavy, so this demo uses pass-through bytes —
//! the goal is to prove the host-side wiring (replay drainer +
//! dispatch re-entry), not the plugin-side codec story.
//!
//! No config consumed.
//!
//! ```json
//! {}
//! ```

use std::cell::RefCell;

use tak_plugin_api::bindings::exports::tak::plugin::inbound::{Action, CotEvent, Guest};
use tak_plugin_api::bindings::tak::plugin::log;

struct State {
    replaced: u64,
}

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State { replaced: 0 }) };
}

struct EchoReplace;

impl Guest for EchoReplace {
    fn init(_config_json: String) -> Result<(), String> {
        log::emit(log::Level::Info, "echo-replace: ready");
        Ok(())
    }

    fn on_inbound(event: CotEvent) -> Action {
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.replaced += 1;
            if st.replaced.is_multiple_of(1000) {
                log::emit(
                    log::Level::Info,
                    &format!("echo-replace: heartbeat — replaced {} events", st.replaced),
                );
            }
        });
        Action::Replace(event.wire_bytes)
    }

    fn shutdown() {
        STATE.with(|s| {
            let st = s.borrow();
            log::emit(
                log::Level::Info,
                &format!(
                    "echo-replace: shutdown — replaced {} events total",
                    st.replaced
                ),
            );
        });
    }
}

tak_plugin_api::bindings::export!(EchoReplace with_types_in tak_plugin_api::bindings);
