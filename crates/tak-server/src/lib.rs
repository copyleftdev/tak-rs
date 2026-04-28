//! tak-server library entry-point.
//!
//! This crate is both a library and a binary. The binary in `src/main.rs`
//! handles process bring-up; everything composable lives here so it can
//! be exercised by integration tests against real components.
//!
//! Today it exposes the [`pipeline`] module — the glue that ties bus
//! dispatch (M2) to persistent storage (M3) into a single per-message
//! call. Over the M4/M5 milestones this will grow to also include:
//! - the listener accept loop (uses `tak_net`)
//! - the per-connection writer task (drains subscription mpsc → socket)
//! - the mission-API HTTP server (uses `tak_mission`)
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

pub mod pipeline;
