//! Concrete conformance scenarios.
//!
//! Each scenario is a pin-down of one observable behavior the server
//! must implement to talk to ATAK. The implemented set is the
//! minimum viable contract; the stubbed set documents known gaps so
//! they're tracked and run-on-fix rather than forgotten.

pub mod chat_xml_lossless;
pub mod pli_dispatch_byte_identity;
pub mod replay_on_reconnect;
