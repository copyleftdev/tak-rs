//! Per-message pipeline: bus dispatch + persistence side-channel.
//!
//! Issue #31 wiring. For each inbound CoT event we:
//!
//! 1. Run [`tak_bus::Bus::dispatch`] — fan out to interested subscribers.
//!    This is the H1 alloc-free hot path.
//! 2. Submit the same event to [`tak_store::Store::try_insert_event`] —
//!    a non-blocking enqueue onto a bounded mpsc consumed by the
//!    background writer task. Best-effort: full channel → drop the
//!    row, increment the `tak.persistence.dropped` counter, never block
//!    the dispatcher.
//!
//! The persistence side-channel **does** allocate (the `CotInsert`
//! struct moves owned `String`s into the store's mpsc). That's by
//! design — invariant H1 governs `tak_bus::dispatch` only, not the
//! storage edge. Persistence is best-effort and not on the firehose
//! latency budget.
//!
//! ```text
//!     Inbound + TakMessage
//!            │
//!            ├──▶ Bus::dispatch  (alloc-free per H1)
//!            │       └──▶ DispatchStats
//!            │
//!            └──▶ Store::try_insert_event  (best-effort; drops on full)
//!                    └──▶ persisted: bool
//! ```

use bytes::Bytes;
use tak_bus::{Bus, DispatchScratch, DispatchStats, GroupBitvector, Inbound};
use tak_proto::v1::TakMessage;
use tak_store::{CotInsert, Store};

/// Result of one [`dispatch_and_persist`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineStats {
    /// Outcome of the bus dispatch (delivered, filtered, dropped).
    pub dispatch: DispatchStats,
    /// `true` if the event made it into the store's mpsc; `false` if
    /// the channel was full (and the persistence-dropped counter was
    /// incremented).
    pub persisted: bool,
}

/// Errors raised by [`dispatch_and_persist`].
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// `TakMessage` was missing its `cot_event` payload — can't be
    /// dispatched or persisted.
    #[error("pipeline: TakMessage missing cot_event")]
    MissingCotEvent,
}

/// Dispatch a CoT event to subscribers and enqueue it for persistence.
///
/// `payload` is the on-wire bytes (as received from the connection
/// reader). It is cloned via `Bytes::clone` to each matched subscriber's
/// channel — invariant H3 (Arc bump, no memcpy).
///
/// `sender_groups` is the resolved [`GroupBitvector`] of the event's
/// origin connection (from `tak_net::auth::Authenticator`).
///
/// `scratch` is reused across calls to keep the dispatch path
/// alloc-free in steady state (invariant H1). The pipeline allocates on
/// the persistence side only — the dispatch path stays clean.
///
/// # Errors
///
/// - [`PipelineError::MissingCotEvent`] if `msg.cot_event` is `None`.
#[allow(clippy::needless_pass_by_value)] // by-value to make caller intent explicit
pub fn dispatch_and_persist(
    bus: &Bus,
    store: &Store,
    msg: &TakMessage,
    sender_groups: GroupBitvector,
    payload: Bytes,
    scratch: &mut DispatchScratch,
) -> Result<PipelineStats, PipelineError> {
    let cot = msg
        .cot_event
        .as_ref()
        .ok_or(PipelineError::MissingCotEvent)?;

    let inbound = Inbound {
        payload: payload.clone(),
        sender_groups,
        cot_type: &cot.r#type,
        lat: cot.lat,
        lon: cot.lon,
        uid: Some(&cot.uid),
        callsign: None, // would require digging into cot.detail.contact; M3+ as needed
    };
    let dispatch_stats = bus.dispatch(&inbound, scratch);
    record_dispatch_metrics(&dispatch_stats);

    // Persistence: build owned CotInsert and try_send. The .ok() collapses
    // the dropped error to a bool — Store internally has already incremented
    // its counter and the metric.
    let cot_insert = CotInsert {
        uid: cot.uid.clone(),
        cot_type: cot.r#type.clone(),
        time_ms: i64::try_from(cot.send_time).unwrap_or(i64::MAX),
        start_ms: i64::try_from(cot.start_time).unwrap_or(i64::MAX),
        stale_ms: i64::try_from(cot.stale_time).unwrap_or(i64::MAX),
        how: cot.how.clone(),
        lat: cot.lat,
        lon: cot.lon,
        hae: cot.hae,
        ce: cot.ce,
        le: cot.le,
        detail: cot
            .detail
            .as_ref()
            .map(|d| d.xml_detail.clone())
            .unwrap_or_default(),
    };
    let persisted = store.try_insert_event(cot_insert).is_ok();

    Ok(PipelineStats {
        dispatch: dispatch_stats,
        persisted,
    })
}

/// Dispatch only — bypasses the persistence side-channel entirely.
/// Used by `tak-server --no-persist` to measure the pure firehose
/// dispatch throughput against the upstream Java baseline.
///
/// This is the apples-to-apples number for comparing against
/// `takserver` configurations that have persistence disabled or
/// off-box.
///
/// # Errors
///
/// - [`PipelineError::MissingCotEvent`] if `msg.cot_event` is `None`.
#[allow(clippy::needless_pass_by_value)]
pub fn dispatch_only(
    bus: &Bus,
    msg: &TakMessage,
    sender_groups: GroupBitvector,
    payload: Bytes,
    scratch: &mut DispatchScratch,
) -> Result<DispatchStats, PipelineError> {
    let cot = msg
        .cot_event
        .as_ref()
        .ok_or(PipelineError::MissingCotEvent)?;

    let inbound = Inbound {
        payload,
        sender_groups,
        cot_type: &cot.r#type,
        lat: cot.lat,
        lon: cot.lon,
        uid: Some(&cot.uid),
        callsign: None,
    };
    let stats = bus.dispatch(&inbound, scratch);
    record_dispatch_metrics(&stats);
    Ok(stats)
}

/// Fold the per-call [`DispatchStats`] into the process-wide metric
/// counters. Five `metrics::counter!()` increments per dispatch — each
/// one is a static lookup + atomic add (a few nanoseconds), comfortably
/// outside the H1 hot allocation budget. When no exporter is installed
/// these are a no-op; with the Prometheus exporter wired (see
/// `tak-server --listen-metrics`) they become scrapeable as
/// `tak_bus_*_total`.
fn record_dispatch_metrics(stats: &DispatchStats) {
    if stats.delivered != 0 {
        metrics::counter!("tak_bus.delivered").increment(u64::from(stats.delivered));
    }
    if stats.dropped_full != 0 {
        metrics::counter!("tak_bus.dropped_full").increment(u64::from(stats.dropped_full));
    }
    if stats.dropped_closed != 0 {
        metrics::counter!("tak_bus.dropped_closed").increment(u64::from(stats.dropped_closed));
    }
    if stats.filtered_groups != 0 {
        metrics::counter!("tak_bus.filtered_groups").increment(u64::from(stats.filtered_groups));
    }
    if stats.filtered_geo != 0 {
        metrics::counter!("tak_bus.filtered_geo").increment(u64::from(stats.filtered_geo));
    }
}
