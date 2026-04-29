//! One publisher bursts K frames as fast as the socket accepts;
//! M subscribers each receive K frames byte-identical to the
//! input (allowing for re-ordering across the dispatch path,
//! though TCP-FIFO + per-sub mpsc keeps this straight in
//! practice).
//!
//! Why a burst, not a steady rate: the bus's H1 alloc-free path
//! is exercised hardest under bursts where back-pressure from
//! the per-sub mpsc could cause `dropped_full` to bump. With
//! `K << per-sub capacity (1024)` and steady-state subscribers
//! that read promptly, the contract is "no drops, all frames
//! delivered byte-equal."
//!
//! Failure here means the dispatch loop is allocating, the
//! mpsc backpressure is interfering with delivery, or the
//! framing is being modified somewhere on the fan-out side.

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

const SUBSCRIBER_COUNT: usize = 5;
const BURST_FRAMES: usize = 100;

/// Pin K-frame burst delivery to M subscribers with no drops.
#[derive(Debug, Default)]
pub struct DispatchUnderBurst;

impl Scenario for DispatchUnderBurst {
    fn name(&self) -> &'static str {
        "dispatch_under_burst"
    }

    fn description(&self) -> &'static str {
        "5 subscribers each receive 100 burst frames byte-identical, no drops"
    }

    fn run<'a>(
        &'a self,
        firehose: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
        Box::pin(async move {
            let frame = match bake() {
                Ok(b) => b,
                Err(e) => return Outcome::Fail(e),
            };
            let frame_bytes = frame.to_vec();

            let mut subs: Vec<AtakMockClient> = Vec::with_capacity(SUBSCRIBER_COUNT);
            for i in 0..SUBSCRIBER_COUNT {
                match AtakMockClient::connect(firehose).await {
                    Ok(c) => subs.push(c),
                    Err(e) => return Outcome::Fail(format!("sub {i} connect: {e:?}")),
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;

            let mut publisher = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("pub connect: {e:?}")),
            };
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Burst send. The PLI fixture is identical bytes each
            // frame; subscribers should see exactly K identical
            // frames each.
            for i in 0..BURST_FRAMES {
                if let Err(e) = publisher.send_frame(&frame).await {
                    return Outcome::Fail(format!("burst send #{i}: {e:?}"));
                }
            }

            // Each subscriber drains exactly K frames within a
            // generous deadline — generous because the burst can
            // arrive fragmented and the per-sub mpsc fills up to
            // 100 / 1024 cap easily.
            for (idx, sub) in subs.iter_mut().enumerate() {
                let mut received_count = 0usize;
                let mut bad_byte_count = 0usize;
                let deadline = Duration::from_secs(10);
                let start = std::time::Instant::now();
                while received_count < BURST_FRAMES {
                    let remaining = deadline.checked_sub(start.elapsed()).unwrap_or_default();
                    if remaining.is_zero() {
                        break;
                    }
                    match sub.recv_frame_with_timeout(remaining).await {
                        Ok(f) => {
                            if f[..] != frame_bytes[..] {
                                bad_byte_count += 1;
                            }
                            received_count += 1;
                        }
                        Err(e) => {
                            return Outcome::Fail(format!(
                                "sub {idx} recv #{received_count}: {e:?}"
                            ));
                        }
                    }
                }
                if received_count != BURST_FRAMES {
                    return Outcome::Fail(format!(
                        "sub {idx} got {received_count}/{BURST_FRAMES} frames"
                    ));
                }
                if bad_byte_count != 0 {
                    return Outcome::Fail(format!(
                        "sub {idx}: {bad_byte_count}/{BURST_FRAMES} frames diverged from publisher bytes"
                    ));
                }
            }

            // Drain publisher's own fan-out copies so its writer
            // doesn't back-pressure the bus on test exit.
            // Cap drain count: same K plus a small buffer.
            let mut drained = 0usize;
            // Avoid clippy::let_underscore_future on the timeout
            // path by binding to `_seen`.
            while drained < BURST_FRAMES {
                match publisher
                    .recv_frame_with_timeout(Duration::from_millis(200))
                    .await
                {
                    Ok(_) => drained += 1,
                    Err(_) => break,
                }
            }
            Outcome::Pass
        })
    }
}

fn bake() -> Result<Bytes, String> {
    let view = decode_xml(FIXTURE_PLI).map_err(|e| format!("fixture decode: {e:?}"))?;
    let msg = view_to_takmessage(&view).map_err(|e| format!("fixture proto: {e:?}"))?;
    let proto_bytes = msg.encode_to_vec();
    let mut framed = Vec::with_capacity(proto_bytes.len() + 8);
    framing::encode_stream(&proto_bytes, &mut std::io::Cursor::new(&mut framed))
        .map_err(|e| format!("fixture frame: {e:?}"))?;
    Ok(Bytes::from(framed))
}
