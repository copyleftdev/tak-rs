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
//! - **#34:** `POST /missions/:name/subscription` — issues a token + SSE URL.
//! - **#35 (this issue):** SSE change feed at `/missions/:name/changes`.
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
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use futures::stream::{self, Stream, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use tak_store::Store;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};
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

/// One mission_change row, shaped for the SSE feed.
///
/// Mirrors the upstream `mission_change` table but exposes `ts_ms` (epoch
/// millis) instead of a timestamptz, so we can keep the codebase free of
/// chrono/time (invariant D3 — jiff-only at the API layer).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct MissionChange {
    /// Monotonic id from the `mission_change_id_seq` sequence. Used as the
    /// SSE `id:` field — clients reconnect with `Last-Event-Id: <id>`.
    pub id: i64,
    /// Mission this change belongs to.
    pub mission_name: String,
    /// Upstream change-type code. Java enum `MissionChangeType` ordinal.
    pub change_type: i32,
    /// Event time (epoch millis).
    pub ts_ms: i64,
    /// Optional CoT/resource UID this change touched.
    pub uid: Option<String>,
    /// Optional resource hash.
    pub hash: Option<String>,
}

/// In-process pub/sub for mission changes.
///
/// One [`broadcast::Sender`] per mission name, created on first
/// subscribe. The capacity (256) is sized so that a slow client lags
/// before it back-pressures the producer — `broadcast::Receiver`
/// returns `Lagged(n)` instead of stalling, which we surface as a
/// stream-level error and let the client reconnect with
/// `Last-Event-Id` to backfill.
///
/// Distributed delivery (clients reconnecting to a different node)
/// will require pushing changes through Postgres `LISTEN/NOTIFY` or
/// via a federated bus; tracked under the deferred cluster work.
#[derive(Debug, Default)]
pub struct ChangeBroker {
    channels: Mutex<HashMap<String, broadcast::Sender<MissionChange>>>,
}

impl ChangeBroker {
    /// New empty broker.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Send `change` to all live subscribers of `change.mission_name`.
    /// Returns the number of receivers that got the event (0 if no
    /// subscribers — change is dropped silently, not an error).
    pub fn publish(&self, change: MissionChange) -> usize {
        let chans = self.channels.lock();
        chans
            .get(&change.mission_name)
            .map(|tx| tx.send(change).unwrap_or(0))
            .unwrap_or(0)
    }

    /// Subscribe to changes for `mission_name`. Creates a channel on
    /// first call; later subscribers attach to the same channel.
    #[must_use]
    pub fn subscribe(&self, mission_name: &str) -> broadcast::Receiver<MissionChange> {
        let mut chans = self.channels.lock();
        let tx = chans
            .entry(mission_name.to_owned())
            .or_insert_with(|| broadcast::channel(BROADCAST_CAPACITY).0);
        tx.subscribe()
    }
}

/// Broadcast channel depth per mission. 256 events of headroom is enough
/// for ~50ms of mid-mission churn; clients slower than that will lag and
/// reconnect with Last-Event-Id.
const BROADCAST_CAPACITY: usize = 256;

/// Shared application state for mission API handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Storage handle. Cloning is cheap (the inner `PgPool` is `Arc`d).
    pub store: Store,
    /// Live subscription tokens.
    pub subs: Arc<SubscriptionRegistry>,
    /// In-process change feed pub/sub.
    pub broker: Arc<ChangeBroker>,
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

/// `?token=...` query param for the SSE handler.
#[derive(Debug, Deserialize)]
struct TokenQuery {
    token: String,
}

/// Builder for the mission API `axum::Router`.
#[derive(Debug, Default)]
pub struct MissionRouter;

impl MissionRouter {
    /// Build the router with all M4 routes + middleware. Creates a fresh
    /// in-process [`ChangeBroker`]; for prod wiring (where mutation
    /// endpoints publish into the same broker) use
    /// [`Self::build_with_broker`].
    pub fn build(store: Store) -> Router {
        Self::build_with_broker(store, ChangeBroker::new())
    }

    /// Build the router with a caller-provided [`ChangeBroker`]. Used by
    /// tests and by future endpoints that need to publish changes
    /// (mutation handlers, replication consumers).
    pub fn build_with_broker(store: Store, broker: Arc<ChangeBroker>) -> Router {
        let state = AppState {
            store,
            subs: SubscriptionRegistry::new(),
            broker,
        };
        // Compression must NOT buffer text/event-stream — that breaks SSE
        // semantics (clients see nothing until the connection closes).
        let compression =
            CompressionLayer::new().compress_when(SizeAbove::default().and(NotForContentType::SSE));
        Router::new()
            .route("/health", get(health))
            .route("/missions", get(list_missions))
            .route("/missions/{name}", get(get_mission))
            .route("/missions/{name}/subscription", post(create_subscription))
            .route("/missions/{name}/changes", get(stream_changes))
            .with_state(state)
            .layer(TraceLayer::new_for_http())
            .layer(compression)
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

/// `GET /missions/:name/changes?token=...` — long-lived SSE stream.
///
/// 1. Validates the token against the [`SubscriptionRegistry`] and
///    confirms the token was issued for *this* mission name.
/// 2. If the request carries `Last-Event-Id: <id>`, queries
///    `mission_change` for `id > last_id` and streams that backfill
///    first.
/// 3. Subscribes to the broker and streams live events, suppressing
///    any whose id was already covered by the backfill.
async fn stream_changes(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let info = state.subs.lookup(&q.token).ok_or(ApiError::Unauthorized)?;
    if info.mission_name != name {
        return Err(ApiError::Forbidden);
    }

    let last_id: i64 = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let backfill = sqlx::query_as::<_, MissionChange>(
        "SELECT id, \
                mission_name, \
                change_type, \
                (EXTRACT(EPOCH FROM ts) * 1000)::bigint AS ts_ms, \
                uid, \
                hash \
         FROM mission_change \
         WHERE mission_name = $1 AND id > $2 \
         ORDER BY id ASC",
    )
    .bind(&name)
    .bind(last_id)
    .fetch_all(state.store.pool())
    .await?;

    let max_backfill_id = backfill.iter().map(|c| c.id).max().unwrap_or(last_id);

    // Subscribe AFTER snapshotting the backfill — the broker fan-out is
    // strictly newer than what's already in the table, so the de-dupe
    // filter on `id > max_backfill_id` is sufficient.
    let rx = state.broker.subscribe(&name);

    let backfill_stream = stream::iter(backfill).map(|c| change_to_event(&c));
    let live_stream = BroadcastStream::new(rx).filter_map(move |res| {
        let item = match res {
            Ok(c) if c.id > max_backfill_id => Some(change_to_event(&c)),
            // Stale (already backfilled) — suppress.
            Ok(_) => None,
            // Lagged: tell the client; they'll reconnect with
            // Last-Event-Id and backfill via the DB path.
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                Some(Ok(Event::default().event("lagged").data(n.to_string())))
            }
        };
        async move { item }
    });

    let combined = backfill_stream.chain(live_stream);
    Ok(Sse::new(combined).keep_alive(KeepAlive::default()))
}

/// Render a [`MissionChange`] as an SSE event with `id:` set to its
/// monotonic id (so reconnects can resume via `Last-Event-Id`).
fn change_to_event(c: &MissionChange) -> Result<Event, Infallible> {
    let event = Event::default()
        .id(c.id.to_string())
        .event("mission-change");
    // serde_json::to_string is infallible for our derived Serialize, but
    // we surface a fallback rather than a panic to honor the lib-side
    // no-unwrap rule.
    let body = serde_json::to_string(c).unwrap_or_else(|_| "{}".to_owned());
    Ok(event.data(body))
}

/// Internal axum-side error type. Maps domain errors to HTTP status codes.
#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("not found")]
    NotFound,
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, body) = match &self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not found"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "invalid token"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "token mismatch"),
            Self::Sqlx(e) => {
                tracing::warn!(error = ?e, "mission api: sqlx error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        };
        (status, body).into_response()
    }
}
