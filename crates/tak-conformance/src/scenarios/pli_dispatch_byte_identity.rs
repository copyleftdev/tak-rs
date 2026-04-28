//! PLI fan-out byte-identity contract.
//!
//! ATAK clients see the same byte stream every other client sees;
//! any divergence (re-encoding, attribute reordering, whitespace
//! drift) breaks situational-awareness icons silently. This
//! scenario pins it down:
//!
//! 1. Two mock clients connect.
//! 2. Client A sends the canonical PLI fixture, framed.
//! 3. Client B receives a fan-out frame.
//! 4. Frame B is asserted byte-identical to frame A.
//!
//! If this fails, ATAK-side icon state will lose attributes.

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

/// Two mock clients; A publishes a PLI; B receives byte-identical.
#[derive(Debug, Default)]
pub struct PliDispatchByteIdentity;

impl Scenario for PliDispatchByteIdentity {
    fn name(&self) -> &'static str {
        "pli_dispatch_byte_identity"
    }

    fn description(&self) -> &'static str {
        "subscriber receives byte-identical frame fan-out for a published PLI"
    }

    fn run<'a>(
        &'a self,
        firehose: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
        Box::pin(async move {
            // Bake the fixture into a Protocol-v1 frame the way
            // taktool loadgen does.
            let view = match decode_xml(FIXTURE_PLI) {
                Ok(v) => v,
                Err(e) => return Outcome::Fail(format!("fixture decode: {e:?}")),
            };
            let msg = match view_to_takmessage(&view) {
                Ok(m) => m,
                Err(e) => return Outcome::Fail(format!("fixture proto: {e:?}")),
            };
            let proto_bytes = msg.encode_to_vec();
            let mut framed = Vec::with_capacity(proto_bytes.len() + 8);
            if let Err(e) =
                framing::encode_stream(&proto_bytes, &mut std::io::Cursor::new(&mut framed))
            {
                return Outcome::Fail(format!("fixture frame: {e:?}"));
            }
            let frame_a: Bytes = Bytes::from(framed);

            // Subscriber B connects FIRST so it's registered
            // before A's frame arrives.
            let mut client_b = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("client B connect: {e:?}")),
            };
            // Give the firehose accept loop a beat to register the
            // subscription. Subscribe + dispatch are concurrent;
            // without this nudge B might miss A's first frame.
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Publisher A connects + sends.
            let mut client_a = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("client A connect: {e:?}")),
            };
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Err(e) = client_a.send_frame(&frame_a).await {
                return Outcome::Fail(format!("client A send: {e:?}"));
            }

            // B should see the same bytes back.
            let received = match client_b
                .recv_frame_with_timeout(Duration::from_secs(2))
                .await
            {
                Ok(b) => b,
                Err(e) => return Outcome::Fail(format!("client B recv: {e:?}")),
            };

            if received[..] != frame_a[..] {
                return Outcome::Fail(format!(
                    "frame mismatch: sent {} bytes, received {} bytes; \
                     first divergence at byte {}",
                    frame_a.len(),
                    received.len(),
                    first_divergence(&frame_a, &received)
                ));
            }

            // A also receives its own publication (every conn is a
            // wildcard subscriber in v0). Drain so A's writer doesn't
            // back-pressure the bus on test exit.
            let _ = client_a
                .recv_frame_with_timeout(Duration::from_secs(2))
                .await;

            Outcome::Pass
        })
    }
}

fn first_divergence(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b.iter())
        .position(|(x, y)| x != y)
        .unwrap_or(usize::min(a.len(), b.len()))
}
