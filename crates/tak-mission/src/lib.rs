//! Mission API — REST + change feed.
//!
//! Replaces the upstream `MissionApi` Spring controller (a 2800-line single
//! class) with axum endpoints split by resource. See `docs/architecture.md`
//! §5.5.
//!
//! Stack: axum + tower + hyper (Sean McArthur lineage). REST principles per
//! Roy Fielding (skill: `fielding`). Change feed delivered as SSE for HTTP/1.1
//! proxy compatibility, with WebSocket as fallback.
//!
//! # Layout (M4 issue progression)
//!
//! - **#32 (this issue):** `MissionRouter::build` skeleton with `/health`
//!   and the tower-http middleware stack (Trace + Compression).
//! - **#33:** `GET /missions` and `GET /missions/{name}`.
//! - **#34:** `POST /missions/{name}/subscription`.
//! - **#35:** SSE change feed at `/missions/{name}/changes`.
//!
//! # Example
//! ```
//! use tak_mission::MissionRouter;
//! let _router: axum::Router = MissionRouter::build();
//! ```
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

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

/// Errors emitted by the mission API layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying storage error.
    #[error(transparent)]
    Store(#[from] tak_store::Error),
}

/// Builder for the mission API `axum::Router`.
///
/// Subsequent M4 issues compose more routes onto the router built here;
/// each new endpoint adds a `.route(...)` call. Middleware is shared:
/// every route sees the trace + compression layers.
#[derive(Debug, Default)]
pub struct MissionRouter;

impl MissionRouter {
    /// Build the router with the M4-baseline routes + middleware stack.
    ///
    /// Routes:
    /// - `GET /health` — returns 200 with body `"ok"`. No state, no auth.
    ///
    /// Middleware (outermost → innermost):
    /// - [`TraceLayer`] — tracing spans on every request with method,
    ///   path, status, latency. Default `tower_http` schema.
    /// - [`CompressionLayer`] — opportunistic gzip when the client
    ///   advertises `Accept-Encoding: gzip`. The change feed (#35) and
    ///   list endpoints (#33) benefit; `/health` doesn't.
    pub fn build() -> Router {
        Router::new()
            .route("/health", get(health))
            .layer(TraceLayer::new_for_http())
            .layer(CompressionLayer::new())
    }
}

/// `GET /health` handler. Returns 200 OK with body `"ok"`.
async fn health() -> (StatusCode, &'static str) {
    (StatusCode::OK, "ok")
}
