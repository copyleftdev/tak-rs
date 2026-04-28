//! The host-side mirror of the plugin's `cot-event` record.
//!
//! Lives outside the `bindings` module so the firehose can build
//! these without dragging the wasmtime macro types into its own
//! type signatures.

use bytes::Bytes;

/// One CoT event handed to a plugin worker.
///
/// `payload` is the framed wire bytes (the same `Bytes` the bus
/// dispatch holds). The host clones via `Bytes::clone` (Arc bump,
/// H3) when stuffing the queue, so plugin overload doesn't waste
/// allocator on dropped messages.
#[derive(Debug, Clone)]
pub struct PluginEvent {
    /// Framed wire bytes (`0xBF <varint length> <protobuf>`).
    pub payload: Bytes,
    /// CoT type (e.g. `a-f-G-U-C`).
    pub cot_type: String,
    /// Sender's UID.
    pub uid: String,
    /// Optional callsign (from `<contact callsign="...">`).
    pub callsign: Option<String>,
    /// Latitude.
    pub lat: f64,
    /// Longitude.
    pub lon: f64,
    /// Height above ellipsoid.
    pub hae: f64,
    /// Send time, ms since epoch.
    pub send_time_ms: u64,
    /// Sender's group bitvector, low 64 bits only (see WIT
    /// `sender-groups-low`).
    pub sender_groups_low: u64,
}

/// What the plugin returned for an event. Mirrors the WIT
/// `action` variant.
#[derive(Debug, Clone)]
pub enum PluginAction {
    /// Forward unchanged. ~all messages should hit this path.
    Pass,
    /// Drop silently. The plugin's `dropped` counter is
    /// incremented by the host.
    Drop,
    /// Replace wire bytes. Caller (the firehose) is responsible
    /// for re-decoding and re-dispatching.
    Replace(Bytes),
}
