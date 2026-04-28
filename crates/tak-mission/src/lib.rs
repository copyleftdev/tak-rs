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
//! - **#32:** [`MissionRouter::build`] skeleton with `/health` and the
//!   tower-http middleware stack.
//! - **#33 (this issue):** `GET /missions` and `GET /missions/:name`.
//! - **#34:** `POST /missions/:name/subscription`.
//! - **#35:** SSE change feed at `/missions/:name/changes`.
//!
//! # Example
//! ```no_run
//! # use tak_mission::MissionRouter;
//! # use tak_store::Store;
//! # async fn ex() -> Result<(), Box<dyn std::error::Error>> {
//! let store = Store::connect_and_migrate("postgres://...").await?;
//! let router: axum::Router = MissionRouter::build(store);
//! # let _ = router;
//! # Ok(())
//! # }
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
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use tak_store::Store;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

/// Errors emitted by the mission API layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying storage error.
    #[error(transparent)]
    Store(#[from] tak_store::Error),

    /// Underlying SQL error during a query.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// Shared application state for mission API handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Storage handle. Cloning is cheap (the inner `PgPool` is `Arc`d).
    pub store: Store,
}

/// JSON shape for one mission row, returned by `GET /missions` and
/// `GET /missions/:name`.
///
/// v0 includes `name`, `description`, `tool`. Additional fields from the
/// upstream Java `Mission` entity (chat_room, base_layer, bbox, path,
/// classification, create_time, creator_uid, guid, etc.) will land in
/// follow-up issues as clients need them.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct Mission {
    /// Mission name (unique). Required.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Optional tool name (e.g., `"public"`, `"VBM"`).
    pub tool: Option<String>,
}

/// Builder for the mission API `axum::Router`.
#[derive(Debug, Default)]
pub struct MissionRouter;

impl MissionRouter {
    /// Build the router with the M4 routes + middleware stack.
    ///
    /// Routes today:
    /// - `GET /health` — 200 OK with body `"ok"`. No state, no auth.
    /// - `GET /missions` — JSON array of [`Mission`]s, ordered by name.
    /// - `GET /missions/:name` — JSON [`Mission`] or 404.
    pub fn build(store: Store) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/missions", get(list_missions))
            .route("/missions/{name}", get(get_mission))
            .with_state(AppState { store })
            .layer(TraceLayer::new_for_http())
            .layer(CompressionLayer::new())
    }
}

/// `GET /health` handler.
async fn health() -> (StatusCode, &'static str) {
    (StatusCode::OK, "ok")
}

/// `GET /missions` — list all missions, ordered by name.
async fn list_missions(State(state): State<AppState>) -> Result<Json<Vec<Mission>>, ApiError> {
    let rows =
        sqlx::query_as::<_, Mission>("SELECT name, description, tool FROM mission ORDER BY name")
            .fetch_all(state.store.pool())
            .await?;
    Ok(Json(rows))
}

/// `GET /missions/:name` — one mission or 404.
async fn get_mission(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Mission>, ApiError> {
    let row =
        sqlx::query_as::<_, Mission>("SELECT name, description, tool FROM mission WHERE name = $1")
            .bind(&name)
            .fetch_optional(state.store.pool())
            .await?;
    row.map(Json).ok_or(ApiError::NotFound)
}

/// Internal axum-side error type. Maps domain errors to HTTP status codes.
#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("not found")]
    NotFound,
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, body) = match &self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not found"),
            Self::Sqlx(e) => {
                tracing::warn!(error = ?e, "mission api: sqlx error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        };
        (status, body).into_response()
    }
}
