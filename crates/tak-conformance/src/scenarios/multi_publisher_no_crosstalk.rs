//! Two publishers, two subscribers. Each subscriber must receive
//! BOTH publishers' frames byte-identical, and never see one frame
//! twice. Pins the dispatch loop's "one inbound → many subscribers"
//! contract under concurrent producers — a regression here would
//! manifest as either dropped frames (subscriber misses a frame
//! that should fan out) or duplicated frames (subscriber sees the
//! same frame twice from the same publisher).
//!
//! Failure modes this catches that the single-publisher scenario
//! doesn't:
//! - dispatch racing with itself across two inbound paths
//! - a per-subscription buffer reusing slots before the previous
//!   frame is acked
//! - a fan-out path that batches by publisher and accidentally
//!   coalesces

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
const FIXTURE_CHAT: &str = include_str!("../../../tak-cot/tests/fixtures/02_chat.xml");

/// Pin two-publisher → two-subscriber fan-out without crosstalk
/// or duplication.
#[derive(Debug, Default)]
pub struct MultiPublisherNoCrosstalk;

impl Scenario for MultiPublisherNoCrosstalk {
    fn name(&self) -> &'static str {
        "multi_publisher_no_crosstalk"
    }

    fn description(&self) -> &'static str {
        "two publishers, two subscribers; each sub sees both frames byte-identical, no duplication"
    }

    fn run<'a>(
        &'a self,
        firehose: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
        Box::pin(async move {
            let frame_pli = match bake(FIXTURE_PLI) {
                Ok(b) => b,
                Err(e) => return Outcome::Fail(e),
            };
            let frame_chat = match bake(FIXTURE_CHAT) {
                Ok(b) => b,
                Err(e) => return Outcome::Fail(e),
            };

            let mut s1 = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("s1 connect: {e:?}")),
            };
            let mut s2 = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("s2 connect: {e:?}")),
            };
            tokio::time::sleep(Duration::from_millis(50)).await;

            let mut p1 = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("p1 connect: {e:?}")),
            };
            let mut p2 = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("p2 connect: {e:?}")),
            };
            tokio::time::sleep(Duration::from_millis(50)).await;

            if let Err(e) = p1.send_frame(&frame_pli).await {
                return Outcome::Fail(format!("p1 send: {e:?}"));
            }
            if let Err(e) = p2.send_frame(&frame_chat).await {
                return Outcome::Fail(format!("p2 send: {e:?}"));
            }

            // Each sub must collect exactly {frame_pli, frame_chat}.
            // (Order is not pinned — the bus dispatches per inbound
            // sequentially, but two concurrent inbounds may interleave
            // arbitrarily at the per-sub mpsc.)
            for (label, sub) in [("s1", &mut s1), ("s2", &mut s2)] {
                let mut seen: HashSet<Vec<u8>> = HashSet::new();
                for _ in 0..2 {
                    match sub.recv_frame_with_timeout(Duration::from_secs(2)).await {
                        Ok(f) => {
                            seen.insert(f.to_vec());
                        }
                        Err(e) => return Outcome::Fail(format!("{label} recv: {e:?}")),
                    }
                }
                let expected: HashSet<Vec<u8>> = [frame_pli.to_vec(), frame_chat.to_vec()]
                    .into_iter()
                    .collect();
                if seen != expected {
                    return Outcome::Fail(format!(
                        "{label} did not receive both publishers' frames: got {} unique frames",
                        seen.len()
                    ));
                }
            }

            // Drain self-fanout copies so writers don't back-pressure on exit.
            for sender in [&mut p1, &mut p2] {
                for _ in 0..2 {
                    let _ = sender.recv_frame_with_timeout(Duration::from_secs(1)).await;
                }
            }

            Outcome::Pass
        })
    }
}

fn bake(xml: &str) -> Result<Bytes, String> {
    let view = decode_xml(xml).map_err(|e| format!("fixture decode: {e:?}"))?;
    let msg = view_to_takmessage(&view).map_err(|e| format!("fixture proto: {e:?}"))?;
    let proto_bytes = msg.encode_to_vec();
    let mut framed = Vec::with_capacity(proto_bytes.len() + 8);
    framing::encode_stream(&proto_bytes, &mut std::io::Cursor::new(&mut framed))
        .map_err(|e| format!("fixture frame: {e:?}"))?;
    Ok(Bytes::from(framed))
}
