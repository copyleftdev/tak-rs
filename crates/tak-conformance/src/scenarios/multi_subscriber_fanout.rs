//! Three subscribers, one publisher, all three receive the same
//! frame. Pins the bus's many-to-one fan-out path: cloning a
//! single `Bytes` payload N times via Arc bump (invariant H3),
//! never re-encoding.
//!
//! The PLI byte-identity scenario already nails the
//! one-publisher-one-subscriber path; this one widens the test to
//! three concurrent subscribers and asserts each receives a frame
//! byte-equal to the one the publisher sent. If any subscriber
//! diverges, the codec is doing per-subscriber work somewhere it
//! shouldn't (a re-encode, an attribute reorder, a whitespace
//! normalize) — exactly the silent class of bug the broader
//! conformance suite is here to catch.

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

/// Pin one-publisher → three-subscriber fan-out byte-identity.
#[derive(Debug, Default)]
pub struct MultiSubscriberFanout;

impl Scenario for MultiSubscriberFanout {
    fn name(&self) -> &'static str {
        "multi_subscriber_fanout"
    }

    fn description(&self) -> &'static str {
        "three subscribers each receive byte-identical fan-out from one publisher"
    }

    fn run<'a>(
        &'a self,
        firehose: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
        Box::pin(async move {
            let frame = match bake_frame() {
                Ok(b) => b,
                Err(e) => return Outcome::Fail(e),
            };

            // Three subscribers join first.
            let mut s1 = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("sub 1 connect: {e:?}")),
            };
            let mut s2 = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("sub 2 connect: {e:?}")),
            };
            let mut s3 = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("sub 3 connect: {e:?}")),
            };
            tokio::time::sleep(Duration::from_millis(50)).await;

            let mut pub_a = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("publisher connect: {e:?}")),
            };
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Err(e) = pub_a.send_frame(&frame).await {
                return Outcome::Fail(format!("publisher send: {e:?}"));
            }

            // Each sub should see byte-identical bytes within a
            // generous deadline (loopback latency + persistence
            // ordering nudge).
            for (label, sub) in [("s1", &mut s1), ("s2", &mut s2), ("s3", &mut s3)] {
                let received = match sub.recv_frame_with_timeout(Duration::from_secs(2)).await {
                    Ok(f) => f,
                    Err(e) => return Outcome::Fail(format!("{label} recv: {e:?}")),
                };
                if received[..] != frame[..] {
                    return Outcome::Fail(format!(
                        "{label} frame mismatch: sent {} bytes, got {} bytes",
                        frame.len(),
                        received.len()
                    ));
                }
            }

            // Drain the publisher's own copy so its writer doesn't
            // back-pressure the bus on test exit.
            let _ = pub_a.recv_frame_with_timeout(Duration::from_secs(1)).await;

            Outcome::Pass
        })
    }
}

fn bake_frame() -> Result<Bytes, String> {
    let view = decode_xml(FIXTURE_PLI).map_err(|e| format!("fixture decode: {e:?}"))?;
    let msg = view_to_takmessage(&view).map_err(|e| format!("fixture proto: {e:?}"))?;
    let proto_bytes = msg.encode_to_vec();
    let mut framed = Vec::with_capacity(proto_bytes.len() + 8);
    framing::encode_stream(&proto_bytes, &mut std::io::Cursor::new(&mut framed))
        .map_err(|e| format!("fixture frame: {e:?}"))?;
    Ok(Bytes::from(framed))
}
