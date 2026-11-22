//! Remote ID / UAS telemetry to Cursor on Target.
//!
//! The codec in [`crate::xml`] and [`crate::proto`] encodes and decodes the
//! CoT this kernel already carries (PLI, chat, geofence, route, drawing). It
//! has no path from a drone Remote ID broadcast to a correctly typed airframe
//! event, so an uncrewed aircraft or its operator cannot be put on the TAK
//! map. This module adds that path with the
//! [`uas-cot`](https://docs.rs/uas-cot) crate: a merged Remote ID record
//! becomes the aircraft event (for example `a-u-A-M-F-Q`) and the operator
//! spot, each a complete CoT 2.0 document that [`crate::xml::decode_xml`]
//! parses and a TAK Server accepts.

/// The [`remoteid`](https://docs.rs/remoteid) crate `uas-cot` is built
/// against, re-exported so callers name its message types through one path.
pub use uas_cot::remoteid;

use uas_cot::remoteid::UasData;
use uas_cot::rid::{self, Options};

/// Turn a merged Remote ID record into ready-to-send CoT XML documents: the
/// aircraft event first, then the operator/takeoff spot when the System
/// message carries a position. `fallback_id` names the aircraft (for example
/// the radio address) before it has broadcast a Basic ID; `now_us` is the
/// send time in microseconds since the Unix epoch.
#[must_use]
pub fn remote_id_to_cot(data: &UasData, fallback_id: Option<&str>, now_us: u64) -> Vec<String> {
    rid::events(data, fallback_id, now_us, &Options::default())
        .iter()
        .map(uas_cot::Event::to_xml)
        .collect()
}
