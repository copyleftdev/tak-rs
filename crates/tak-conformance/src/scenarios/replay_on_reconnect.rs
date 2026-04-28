//! Replay-on-reconnect.
//!
//! When an ATAK client drops and reconnects, the Java server replays
//! the last N seconds of relevant events from `cot_router` so the
//! client's situational picture is restored without waiting for live
//! PLIs from every peer. Without this, reconnecting clients see
//! ghosts (stale local-cache icons, no fresh data).
//!
//! This scenario:
//!
//! 1. Connects publisher A.
//! 2. A sends K canonical PLI frames.
//! 3. Waits long enough for persistence to flush.
//! 4. A disconnects.
//! 5. Subscriber B connects fresh.
//! 6. B should receive K frames from replay, byte-identical to
//!    what A sent.
//!
//! The scenario assumes the server was booted with a non-zero
//! `replay_window`. The conformance harness's
//! `TestServer::start_with(Some(window))` does this. When the
//! agent runs against a remote server, the operator must ensure
//! `--replay-window-secs` is set on the target.

use std::collections::HashSet;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use prost::Message;
use tak_cot::framing;
use tak_cot::proto::view_to_takmessage;
use tak_cot::xml::decode_xml;

use crate::AtakMockClient;
use crate::scenario::{Outcome, Scenario};

const FIXTURE_PLI: &str = include_str!("../../../tak-cot/tests/fixtures/01_pli.xml");
const REPLAY_EVENT_COUNT: usize = 5;

/// Stub scenario for the replay-on-reconnect contract. See module
/// docs for the implementation plan.
#[derive(Debug, Default)]
pub struct ReplayOnReconnect;

impl Scenario for ReplayOnReconnect {
    fn name(&self) -> &'static str {
        "replay_on_reconnect"
    }

    fn description(&self) -> &'static str {
        "fresh subscriber receives recent events from cot_router replay"
    }

    fn run<'a>(
        &'a self,
        firehose: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
        Box::pin(async move {
            // Bake one canonical frame.
            let frame = match bake_pli_frame() {
                Ok(b) => b,
                Err(e) => return Outcome::Fail(e),
            };

            // Publisher A sends K identical frames, then drops.
            // (Identical bytes is fine: cot_router's PRIMARY KEY
            // is `id`, not `uid`, so all K rows persist.)
            {
                let mut a = match AtakMockClient::connect(firehose).await {
                    Ok(c) => c,
                    Err(e) => return Outcome::Fail(format!("client A connect: {e:?}")),
                };
                for _ in 0..REPLAY_EVENT_COUNT {
                    if let Err(e) = a.send_frame(&frame).await {
                        return Outcome::Fail(format!("client A send: {e:?}"));
                    }
                }
                // Wait long enough for the persistence batcher to
                // flush. Default flush is ~150 ms; 1 s is plenty.
                tokio::time::sleep(Duration::from_secs(1)).await;
                drop(a);
            }

            // Subscriber B connects fresh.
            let mut b = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("client B connect: {e:?}")),
            };

            // Drain up to K frames within a generous deadline.
            // Server-side replay is unicast on subscribe; if the
            // server doesn't implement replay, B will time out
            // waiting for the first frame.
            let mut received: Vec<Bytes> = Vec::with_capacity(REPLAY_EVENT_COUNT);
            for i in 0..REPLAY_EVENT_COUNT {
                match b.recv_frame_with_timeout(Duration::from_secs(2)).await {
                    Ok(f) => received.push(f),
                    Err(e) => {
                        return Outcome::Fail(format!(
                            "client B timed out on replay event {}/{REPLAY_EVENT_COUNT}: {e:?}; \
                             server's replay-on-reconnect path is not delivering",
                            i + 1
                        ));
                    }
                }
            }

            // Every received frame must be byte-identical to what A
            // sent. Replay rebuilds the wire bytes from the stored
            // BYTEA column, so this is the load-bearing assertion
            // that nothing got re-encoded between insert + replay.
            //
            // Use a Counter rather than zip() because frame ordering
            // by servertime should be preserved, but we tolerate
            // duplicates and want a clear "bytes drifted" signal
            // instead of "ordering drifted."
            let mut distinct = HashSet::new();
            for (i, recv) in received.iter().enumerate() {
                if recv[..] != frame[..] {
                    return Outcome::Fail(format!(
                        "replay frame {i} bytes mismatch: \
                         sent {} bytes, received {} bytes",
                        frame.len(),
                        recv.len(),
                    ));
                }
                distinct.insert(recv.as_ref().to_vec());
            }
            if distinct.is_empty() {
                return Outcome::Fail("no frames received from replay".to_owned());
            }

            Outcome::Pass
        })
    }
}

fn bake_pli_frame() -> Result<Bytes, String> {
    let view = decode_xml(FIXTURE_PLI).map_err(|e| format!("fixture decode: {e:?}"))?;
    let msg = view_to_takmessage(&view).map_err(|e| format!("fixture proto: {e:?}"))?;
    let proto_bytes = msg.encode_to_vec();
    let mut framed = Vec::with_capacity(proto_bytes.len() + 8);
    framing::encode_stream(&proto_bytes, &mut std::io::Cursor::new(&mut framed))
        .map_err(|e| format!("fixture frame: {e:?}"))?;
    Ok(Bytes::from(framed))
}
