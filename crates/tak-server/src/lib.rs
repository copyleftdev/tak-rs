//! tak-server library entry-point.
//!
//! This crate is both a library and a binary. The binary in `src/main.rs`
//! handles process bring-up; everything composable lives here so it can
//! be exercised by integration tests against real components.
//!
//! Modules:
//! - [`pipeline`] — glue that ties bus dispatch (M2) to persistent
//!   storage (M3) into a single per-message call.
//! - [`firehose`] — TCP accept loop + per-connection reader/writer
//!   tasks that drive the bus from the wire.
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

pub mod firehose;
pub mod pipeline;
