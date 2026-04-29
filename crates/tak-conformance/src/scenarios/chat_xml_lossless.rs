//! `Detail.xmlDetail` lossless round-trip on a chat frame.
//!
//! ATAK chat frames carry `<detail>` blobs with arbitrary nested
//! elements (`<__chat>`, `<remarks>`, `<link>`, plugin-specific
//! tags). Codec invariant H2 says these survive as a borrowed
//! `&str` slice with no re-encoding. This scenario pins that down
//! end-to-end through the firehose, byte-identical.
//!
//! The synthetic fixture (`02_chat.xml`) doesn't exercise every
//! XML edge case (namespaces, CDATA, mixed content with the
//! whitespace patterns ATAK actually emits) — the runbook in
//! `docs/conformance.md` covers the real-ATAK capture step. But
//! byte-identity on a non-trivial detail block is the floor:
//! anything below this is broken before we even start.

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

const FIXTURE_CHAT: &str = include_str!("../../../tak-cot/tests/fixtures/02_chat.xml");

/// Pin chat-frame `<detail>` byte-identity through fan-out.
#[derive(Debug, Default)]
pub struct ChatXmlLossless;

impl Scenario for ChatXmlLossless {
    fn name(&self) -> &'static str {
        "chat_xml_lossless"
    }

    fn description(&self) -> &'static str {
        "chat frame detail block round-trips byte-identical through fan-out"
    }

    fn run<'a>(
        &'a self,
        firehose: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
        Box::pin(async move {
            let frame = match bake_chat_frame() {
                Ok(b) => b,
                Err(e) => return Outcome::Fail(e),
            };

            let mut sub = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("subscriber connect: {e:?}")),
            };
            tokio::time::sleep(Duration::from_millis(50)).await;

            let mut publisher = match AtakMockClient::connect(firehose).await {
                Ok(c) => c,
                Err(e) => return Outcome::Fail(format!("publisher connect: {e:?}")),
            };
            tokio::time::sleep(Duration::from_millis(50)).await;

            if let Err(e) = publisher.send_frame(&frame).await {
                return Outcome::Fail(format!("publisher send: {e:?}"));
            }

            let received = match sub.recv_frame_with_timeout(Duration::from_secs(2)).await {
                Ok(f) => f,
                Err(e) => return Outcome::Fail(format!("subscriber recv: {e:?}")),
            };

            if received[..] != frame[..] {
                let div = first_divergence(&frame, &received);
                return Outcome::Fail(format!(
                    "chat frame mismatch: sent {} bytes, received {} bytes; \
                     first divergence at byte {div}",
                    frame.len(),
                    received.len(),
                ));
            }

            // Drain publisher's own fanout copy so its writer
            // doesn't back-pressure the bus on test exit.
            let _ = publisher
                .recv_frame_with_timeout(Duration::from_secs(1))
                .await;

            Outcome::Pass
        })
    }
}

fn bake_chat_frame() -> Result<Bytes, String> {
    let view = decode_xml(FIXTURE_CHAT).map_err(|e| format!("fixture decode: {e:?}"))?;
    let msg = view_to_takmessage(&view).map_err(|e| format!("fixture proto: {e:?}"))?;
    let proto_bytes = msg.encode_to_vec();
    let mut framed = Vec::with_capacity(proto_bytes.len() + 8);
    framing::encode_stream(&proto_bytes, &mut std::io::Cursor::new(&mut framed))
        .map_err(|e| format!("fixture frame: {e:?}"))?;
    Ok(Bytes::from(framed))
}

fn first_divergence(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b.iter())
        .position(|(x, y)| x != y)
        .unwrap_or(usize::min(a.len(), b.len()))
}
