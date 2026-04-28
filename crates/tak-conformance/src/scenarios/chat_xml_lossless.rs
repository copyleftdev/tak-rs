//! `Detail.xmlDetail` lossless round-trip on a chat frame.
//!
//! **Status: stub.** ATAK chat frames carry `<detail>` blobs with
//! arbitrary nested elements (`<__chat>`, `<remarks>`, `<link>`,
//! plugin-specific tags). The codec invariant says these survive as
//! a borrowed `&str` slice with no re-encoding; this scenario will
//! pin that down end-to-end through the firehose.
//!
//! # Why it isn't implemented yet
//!
//! - The decode + bake path is the same as `pli_dispatch_byte_identity`,
//!   so byte-identity gives us *some* coverage already.
//! - The interesting failure mode is when the codec round-trips a
//!   frame whose detail block contains namespaces or CDATA that
//!   `quick-xml` borrowed mode handles differently from the Java
//!   server. Catching that needs a real ATAK chat capture (the
//!   synthetic fixture in `02_chat.xml` is a good start but
//!   doesn't exercise namespace edge cases).
//!
//! When this lands: replace the body with the same shape as
//! `PliDispatchByteIdentity`, but with the chat fixture, and add
//! attribute-set equality across the original and received `<detail>`
//! XML in addition to byte equality. The XML compare guards against
//! the case where an upstream proxy mangles bytes but preserves
//! semantics.

use std::future::Future;
use std::pin::Pin;

use crate::TestServer;
use crate::scenario::{Outcome, Scenario};

/// Stub scenario for the chat-detail lossless contract. See module
/// docs for the implementation plan.
#[derive(Debug, Default)]
pub struct ChatXmlLossless;

impl Scenario for ChatXmlLossless {
    fn name(&self) -> &'static str {
        "chat_xml_lossless"
    }

    fn description(&self) -> &'static str {
        "STUB: chat frame detail block round-trips byte-identical and XML-equivalent"
    }

    fn run<'a>(
        &'a self,
        _server: &'a TestServer,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
        Box::pin(async move {
            Outcome::Skipped(
                "not implemented; needs real ATAK chat capture for namespace edge cases".to_owned(),
            )
        })
    }
}
