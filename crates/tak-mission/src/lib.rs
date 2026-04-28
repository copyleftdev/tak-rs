//! Mission API — REST + change feed.
//!
//! Replaces the upstream `MissionApi` Spring controller (a 2800-line single
//! class) with axum endpoints split by resource. See `docs/architecture.md` §5.5.
//!
//! Stack: axum + tower + hyper (Sean McArthur lineage). REST principles per
//! Roy Fielding (skill: `fielding`). Change feed delivered as SSE for HTTP/1.1
//! proxy compatibility, with WebSocket as fallback.
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

/// Errors emitted by the mission API layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying storage error.
    #[error(transparent)]
    Store(#[from] tak_store::Error),
}
