//! Conformance harness — proves `tak-rs` speaks ATAK's wire protocol
//! correctly enough to be a drop-in for the upstream Java server.
//!
//! # What this crate is
//!
//! A scenario-driven harness that boots an in-process `tak-server`
//! against a Postgres testcontainer, drives one or more
//! [`AtakMockClient`]s through canonical TAK Protocol exchanges, and
//! asserts the observable contract: bytes-out match bytes-in,
//! persistence rows match, fan-out reaches every subscribed peer.
//!
//! # What this crate is *not*
//!
//! It is **not** a substitute for testing against a real ATAK device.
//! ATAK's wire surface has corners (mission packages, attachments,
//! GeoChat ack flows, certificate enrollment) that synthetic frames
//! never exercise. The runbook in `docs/conformance.md` describes
//! how to point a real ATAK Android at the same harness; that flow
//! is the actual gold standard.
//!
//! Treat this crate as the **floor**: every scenario it enforces is a
//! regression gate, but a passing run does not prove ATAK interop.
//!
//! # Layout
//!
//! - [`server`] — boot a tak-server inside the test process, return
//!   handles to its firehose listener and metric endpoint.
//! - [`mock_atak`] — `AtakMockClient`: connects to the firehose,
//!   sends framed CoT, drains fan-out from the server.
//! - [`scenario`] — the [`Scenario`] trait + [`Outcome`] type.
//! - [`scenarios`] — concrete scenarios. The implemented set is the
//!   minimum viable wire-protocol contract; gap scenarios are
//!   `#[ignore]`'d with a docstring stating what's missing.
//!
//! # Status
//!
//! - **Implemented:** PLI byte-identical fan-out.
//! - **Stubbed (gap):** chat lossless `xmlDetail`, mission
//!   subscription replay, replay-on-reconnect, mTLS handshake
//!   conformance.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented
    )
)]
#![warn(missing_docs, missing_debug_implementations)]

pub mod mock_atak;
pub mod scenario;
pub mod scenarios;
pub mod server;

pub use mock_atak::{AtakMockClient, MockClientError};
pub use scenario::{Outcome, Scenario};
pub use server::{TestServer, TestServerError};
