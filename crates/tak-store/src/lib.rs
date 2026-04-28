//! Postgres + PostGIS persistence for tak-rs.
//!
//! Same schema as the upstream Java server (`cot_router` with PostGIS GiST
//! index, mission tables) so deployments can switch between implementations
//! without touching the database. See `docs/architecture.md` §5.4.
//!
//! Hot-path note: persistence MUST NOT block fan-out (invariant H1 boundary).
//! Producers push into a bounded channel; if full, persistence is dropped
//! before delivery is.
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

/// Errors emitted by the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying SQL error.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}
