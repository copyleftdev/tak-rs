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
//! - **#33:** `GET /missions` and `GET /missions/:name`.
//! - **#34 (this issue):** `POST /missions/:name/subscription` — issues a
//!   token + SSE URL the client uses to attach to the change feed.
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

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use parking_lot::Mutex;
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

/// In-memory subscription token registry.
///
/// v0 keeps live tokens in process memory. Production should persist via
/// the upstream `mission_subscription` table — that lets clients reconnect
/// to a different `tak-server` instance without losing their feed
/// position. Tracker for that work is in the deferred-cluster issue.
#[derive(Debug, Default)]
pub struct SubscriptionRegistry {
    next_id: AtomicU64,
    by_token: Mutex<HashMap<String, SubscriptionInfo>>,
}

/// Per-token subscription state. Owned by the registry.
#[derive(Debug, Clone)]
pub struct SubscriptionInfo {
    /// Mission this subscription is attached to.
    pub mission_name: String,
}

impl SubscriptionRegistry {
    /// New empty registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Mint a new token for `mission_name` and store it.
    #[must_use]
    pub fn mint(&self, mission_name: String) -> String {
        // v0 token format: monotonic hex counter. Not crypto-strong; fine
        // for routing the SSE stream. Pre-prod will swap to UUID/randomness
        // when auth lands on the read path.
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let token = format!("sub-{id:016x}");
        self.by_token
            .lock()
            .insert(token.clone(), SubscriptionInfo { mission_name });
        token
    }

    /// Look up a subscription by its token.
    #[must_use]
    pub fn lookup(&self, token: &str) -> Option<SubscriptionInfo> {
        self.by_token.lock().get(token).cloned()
    }

    /// Remove a subscription. Used by the SSE handler when the client
    /// disconnects.
    pub fn release(&self, token: &str) {
        self.by_token.lock().remove(token);
    }

    /// Number of live subscriptions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_token.lock().len()
    }

    /// True iff no live subscriptions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Shared application state for mission API handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Storage handle. Cloning is cheap (the inner `PgPool` is `Arc`d).
    pub store: Store,
    /// Live subscription tokens.
    pub subs: Arc<SubscriptionRegistry>,
}

/// JSON shape for one mission row.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct Mission {
    /// Mission name (unique). Required.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Optional tool name (e.g., `"public"`, `"VBM"`).
    pub tool: Option<String>,
}

/// JSON shape returned by `POST /missions/:name/subscription`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubscriptionResponse {
    /// Opaque token. Pass on subsequent SSE connections.
    pub token: String,
    /// SSE URL path with token query param appended. Caller can use this
    /// directly without re-deriving the path.
    pub sse_url: String,
}

/// Builder for the mission API `axum::Router`.
#[derive(Debug, Default)]
pub struct MissionRouter;

impl MissionRouter {
    /// Build the router with all M4 routes + middleware.
    pub fn build(store: Store) -> Router {
        let state = AppState {
            store,
            subs: SubscriptionRegistry::new(),
        };
        Router::new()
            .route("/health", get(health))
            .route("/missions", get(list_missions))
            .route("/missions/{name}", get(get_mission))
            .route("/missions/{name}/subscription", post(create_subscription))
            .with_state(state)
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

/// `POST /missions/:name/subscription` — issue a subscription token for the
/// SSE change feed.
async fn create_subscription(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<SubscriptionResponse>), ApiError> {
    // Verify the mission exists.
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM mission WHERE name = $1)")
            .bind(&name)
            .fetch_one(state.store.pool())
            .await?;
    if !exists {
        return Err(ApiError::NotFound);
    }

    let token = state.subs.mint(name.clone());
    let sse_url = format!("/missions/{name}/changes?token={token}");
    Ok((
        StatusCode::CREATED,
        Json(SubscriptionResponse { token, sse_url }),
    ))
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
