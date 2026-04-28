//! `Scenario` trait + outcome type.
//!
//! A scenario encapsulates one wire-protocol contract that the
//! server must satisfy to be ATAK-compatible. Each scenario:
//!
//! 1. Receives a started [`crate::TestServer`].
//! 2. Drives one or more [`crate::AtakMockClient`]s through a
//!    deterministic sequence of sends/receives.
//! 3. Asserts an observable invariant — typically byte-level
//!    identity of the fan-out frame, or persistence row equality.
//! 4. Returns an [`Outcome`].

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::TestServer;

/// Result of running a [`Scenario`].
#[derive(Debug)]
pub enum Outcome {
    /// All assertions passed.
    Pass,
    /// Scenario failed an assertion. The string is operator-readable:
    /// it should name the divergence (e.g. "frame[0..258] mismatch
    /// at byte 42: expected 0xBF got 0xBE").
    Fail(String),
    /// Scenario could not run for an environmental reason — Docker
    /// missing, port in use, etc. Distinguishes "test broken" from
    /// "system broken."
    Skipped(String),
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass => f.write_str("PASS"),
            Self::Fail(why) => write!(f, "FAIL: {why}"),
            Self::Skipped(why) => write!(f, "SKIPPED: {why}"),
        }
    }
}

/// One conformance contract.
pub trait Scenario: Send + Sync {
    /// Short human-readable name. Goes into the test output and
    /// any operator-facing report.
    fn name(&self) -> &'static str;

    /// One-line description of what the scenario asserts.
    fn description(&self) -> &'static str;

    /// Run the scenario against the supplied test server.
    fn run<'a>(
        &'a self,
        server: &'a TestServer,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>>;
}
