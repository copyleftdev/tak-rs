//! Replay-on-reconnect.
//!
//! **Status: known-broken.** When an ATAK client drops and
//! reconnects, the Java server replays the last N hours of relevant
//! events from `cot_router` so the client's situational picture is
//! restored without waiting for live PLIs from every peer.
//! `tak-rs` does not currently implement this — it has the write
//! path (every event hits `cot_router`) but no read-on-subscribe
//! path. Reconnecting ATAK clients see ghosts (stale icons in their
//! local cache, no fresh data until peers re-emit).
//!
//! This is **Tier-1 punch-list item #2** in the drop-in readiness
//! assessment. When it lands, this scenario:
//!
//! 1. Spins up TestServer.
//! 2. Client A publishes K PLIs over T seconds, then disconnects.
//! 3. Client B connects (fresh subscription).
//! 4. Asserts B receives all K PLIs (in some order) within a
//!    reasonable replay window.
//!
//! Until then, this remains [`Outcome::Skipped`] so it doesn't gate
//! the suite but is visible in the report.

use std::future::Future;
use std::pin::Pin;

use crate::TestServer;
use crate::scenario::{Outcome, Scenario};

/// Stub scenario for the replay-on-reconnect contract. See module
/// docs for the implementation plan.
#[derive(Debug, Default)]
pub struct ReplayOnReconnect;

impl Scenario for ReplayOnReconnect {
    fn name(&self) -> &'static str {
        "replay_on_reconnect"
    }

    fn description(&self) -> &'static str {
        "STUB: reconnecting client receives backlog of recent events from cot_router"
    }

    fn run<'a>(
        &'a self,
        _server: &'a TestServer,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
        Box::pin(async move {
            Outcome::Skipped(
                "not implemented; replay-on-reconnect path missing in firehose subscribe handler"
                    .to_owned(),
            )
        })
    }
}
