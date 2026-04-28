//! Issue #32 + #33 + #34 + #35 acceptance tests.
//!
//! - The router-level tests (no state) drive the Router via
//!   `tower::ServiceExt::oneshot` so we don't need to bind a TCP socket.
//! - The mission tests use a real Postgres+PostGIS testcontainer and
//!   exercise the actual SQL queries.
//! - The SSE tests poll the streaming response body in a tokio task.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use http_body_util::BodyExt;
use tak_mission::{ChangeBroker, Mission, MissionChange, MissionRouter, SubscriptionResponse};
use tak_store::Store;
use testcontainers::core::{ContainerPort, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use tower::ServiceExt;

const PG_USER: &str = "tak";
const PG_PASS: &str = "takatak";
const PG_DB: &str = "tak";

async fn start_postgis() -> (testcontainers::ContainerAsync<GenericImage>, String) {
    let container = GenericImage::new("postgis/postgis", "16-3.4")
        .with_exposed_port(ContainerPort::Tcp(5432))
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_wait_for(WaitFor::Duration {
            length: Duration::from_secs(1),
        })
        .with_env_var("POSTGRES_USER", PG_USER)
        .with_env_var("POSTGRES_PASSWORD", PG_PASS)
        .with_env_var("POSTGRES_DB", PG_DB)
        .start()
        .await
        .expect("postgis container start");
    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432.tcp())
        .await
        .expect("host port");
    let url = format!("postgres://{PG_USER}:{PG_PASS}@{host}:{port}/{PG_DB}");
    (container, url)
}

async fn router_with_seed(store: Store) -> axum::Router {
    sqlx::query(
        "INSERT INTO mission (name, description, tool, create_time) VALUES \
            ('alpha', 'first mission', 'public', now()), \
            ('bravo', NULL, 'VBM', now()), \
            ('charlie', 'third mission', 'public', now())",
    )
    .execute(store.pool())
    .await
    .expect("seed missions");
    MissionRouter::build(store)
}

/// Build a router that doesn't actually need a database (only the no-state
/// /health route is exercised). Uses a real connection though, since
/// MissionRouter::build now requires a Store; an unreachable URL would
/// fail-loud at construction time which is fine for these unit tests.
async fn router_with_real_store() -> (testcontainers::ContainerAsync<GenericImage>, axum::Router) {
    let (container, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    (container, MissionRouter::build(store))
}

// ===========================================================================
// #32 — health + middleware
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn health_returns_200_with_ok_body() {
    let (_c, app) = router_with_real_store().await;
    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn unknown_route_returns_404() {
    let (_c, app) = router_with_real_store().await;
    let req = Request::builder()
        .uri("/does-not-exist")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn health_responds_to_get_only() {
    let (_c, app) = router_with_real_store().await;
    let req = Request::builder()
        .method("POST")
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ===========================================================================
// #33 — GET /missions and /missions/:name
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn list_missions_returns_all_ordered_by_name() {
    let (_c, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let app = router_with_seed(store).await;

    let req = Request::builder()
        .uri("/missions")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let missions: Vec<Mission> = serde_json::from_slice(&body).unwrap();

    // Migration V33 adds two baseline rows (`citrap`, `exchecktemplates`).
    // We seeded three more (alpha, bravo, charlie). All five returned in
    // alpha order by name.
    let names: Vec<&str> = missions.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["alpha", "bravo", "charlie", "citrap", "exchecktemplates"],
        "expected alpha-ordered list of all 5 missions"
    );

    let alpha = &missions[0];
    assert_eq!(alpha.description.as_deref(), Some("first mission"));
    assert_eq!(alpha.tool.as_deref(), Some("public"));

    let bravo = &missions[1];
    assert_eq!(bravo.description, None);
    assert_eq!(bravo.tool.as_deref(), Some("VBM"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn get_mission_by_name_returns_one() {
    let (_c, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let app = router_with_seed(store).await;

    let req = Request::builder()
        .uri("/missions/bravo")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let mission: Mission = serde_json::from_slice(&body).unwrap();

    assert_eq!(mission.name, "bravo");
    assert_eq!(mission.description, None);
    assert_eq!(mission.tool.as_deref(), Some("VBM"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn get_mission_unknown_returns_404() {
    let (_c, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let app = router_with_seed(store).await;

    let req = Request::builder()
        .uri("/missions/does-not-exist")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// #34 — POST /missions/:name/subscription
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn subscribe_to_existing_mission_returns_201_with_token() {
    let (_c, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let app = router_with_seed(store).await;

    let req = Request::builder()
        .method("POST")
        .uri("/missions/alpha/subscription")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let sub: SubscriptionResponse = serde_json::from_slice(&body).unwrap();
    assert!(sub.token.starts_with("sub-"), "got token: {}", sub.token);
    assert_eq!(
        sub.sse_url,
        format!("/missions/alpha/changes?token={}", sub.token)
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn subscribe_to_unknown_mission_returns_404() {
    let (_c, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let app = MissionRouter::build(store);

    let req = Request::builder()
        .method("POST")
        .uri("/missions/does-not-exist/subscription")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn subscription_tokens_are_unique() {
    let (_c, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let app = router_with_seed(store).await;

    let req1 = Request::builder()
        .method("POST")
        .uri("/missions/alpha/subscription")
        .body(Body::empty())
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    let body1 = resp1.into_body().collect().await.unwrap().to_bytes();
    let s1: SubscriptionResponse = serde_json::from_slice(&body1).unwrap();

    let req2 = Request::builder()
        .method("POST")
        .uri("/missions/alpha/subscription")
        .body(Body::empty())
        .unwrap();
    let resp2 = app.oneshot(req2).await.unwrap();
    let body2 = resp2.into_body().collect().await.unwrap().to_bytes();
    let s2: SubscriptionResponse = serde_json::from_slice(&body2).unwrap();

    assert_ne!(s1.token, s2.token, "two POSTs must yield distinct tokens");
}

// ===========================================================================
// #35 — GET /missions/:name/changes (SSE)
// ===========================================================================

/// Build a router that shares a `ChangeBroker` with the test, so the
/// test can publish events directly. Mirrors how a future mutation
/// endpoint will publish from inside the handler.
async fn router_with_broker(store: Store) -> (axum::Router, Arc<ChangeBroker>) {
    sqlx::query(
        "INSERT INTO mission (name, description, tool, create_time) VALUES \
            ('alpha', 'first mission', 'public', now())",
    )
    .execute(store.pool())
    .await
    .expect("seed alpha");
    let broker = ChangeBroker::new();
    let router = MissionRouter::build_with_broker(store, broker.clone());
    (router, broker)
}

/// Read SSE frames from the body until `needle` appears in the
/// accumulated buffer, the stream ends, or `timeout` expires. Returns
/// whatever was collected.
async fn read_until(body: axum::body::Body, needle: &str, timeout: Duration) -> String {
    let mut stream = body.into_data_stream();
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_default();
        let next = tokio::time::timeout(remaining, stream.next()).await;
        match next {
            Ok(Some(Ok(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                if buf.contains(needle) {
                    return buf;
                }
            }
            // End-of-stream, error, or timeout — return what we have.
            _ => return buf,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn sse_stream_delivers_published_change() {
    let (_c, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let (app, broker) = router_with_broker(store).await;

    // POST /subscription → token
    let sub_req = Request::builder()
        .method("POST")
        .uri("/missions/alpha/subscription")
        .body(Body::empty())
        .unwrap();
    let sub_resp = app.clone().oneshot(sub_req).await.unwrap();
    let body = sub_resp.into_body().collect().await.unwrap().to_bytes();
    let sub: SubscriptionResponse = serde_json::from_slice(&body).unwrap();

    // GET /changes — start the SSE stream
    let req = Request::builder()
        .uri(format!("/missions/alpha/changes?token={}", sub.token))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap_or_default()),
        Some("text/event-stream"),
        "SSE responses must declare text/event-stream"
    );

    // Spawn a reader before publishing so we don't miss the event.
    let reader = tokio::spawn(async move {
        read_until(resp.into_body(), "ANDROID-test", Duration::from_secs(5)).await
    });

    // Give the stream a moment to attach to the broker.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let n = broker.publish(MissionChange {
        id: 42,
        mission_name: "alpha".to_owned(),
        change_type: 0,
        ts_ms: 1_700_000_000_000,
        uid: Some("ANDROID-test".to_owned()),
        hash: None,
    });
    assert_eq!(n, 1, "publish should reach the one connected subscriber");

    let buf = reader.await.unwrap();
    assert!(
        buf.contains("event: mission-change"),
        "stream missed the published event; got: {buf:?}"
    );
    assert!(buf.contains("id: 42"), "missing id frame; got: {buf:?}");
    assert!(
        buf.contains("ANDROID-test"),
        "payload UID not in stream; got: {buf:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn sse_backfills_via_last_event_id() {
    let (_c, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let (app, _broker) = router_with_broker(store.clone()).await;

    // Seed two changes that happened "while the client was offline".
    sqlx::query(
        "INSERT INTO mission_change (id, mission_name, ts, change_type) VALUES \
            (100, 'alpha', now(), 1), \
            (101, 'alpha', now(), 2)",
    )
    .execute(store.pool())
    .await
    .unwrap();

    // POST /subscription → token
    let sub_req = Request::builder()
        .method("POST")
        .uri("/missions/alpha/subscription")
        .body(Body::empty())
        .unwrap();
    let sub_resp = app.clone().oneshot(sub_req).await.unwrap();
    let body = sub_resp.into_body().collect().await.unwrap().to_bytes();
    let sub: SubscriptionResponse = serde_json::from_slice(&body).unwrap();

    // Reconnect with Last-Event-Id: 99 (i.e. saw nothing yet) → expect
    // both 100 and 101 to backfill.
    let req = Request::builder()
        .uri(format!("/missions/alpha/changes?token={}", sub.token))
        .header("last-event-id", "99")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let buf = read_until(resp.into_body(), "id: 101", Duration::from_secs(5)).await;
    assert!(buf.contains("id: 100"), "missing backfill 100: {buf:?}");
    assert!(buf.contains("id: 101"), "missing backfill 101: {buf:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn sse_rejects_bogus_token() {
    let (_c, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let (app, _broker) = router_with_broker(store).await;

    let req = Request::builder()
        .uri("/missions/alpha/changes?token=sub-deadbeefdeadbeef")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn sse_rejects_token_from_different_mission() {
    let (_c, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    sqlx::query(
        "INSERT INTO mission (name, description, tool, create_time) VALUES \
            ('alpha', 'first', 'public', now()), \
            ('bravo', 'second', 'public', now())",
    )
    .execute(store.pool())
    .await
    .unwrap();
    let broker = ChangeBroker::new();
    let app = MissionRouter::build_with_broker(store, broker);

    // Token issued for alpha…
    let sub_req = Request::builder()
        .method("POST")
        .uri("/missions/alpha/subscription")
        .body(Body::empty())
        .unwrap();
    let sub_resp = app.clone().oneshot(sub_req).await.unwrap();
    let body = sub_resp.into_body().collect().await.unwrap().to_bytes();
    let sub: SubscriptionResponse = serde_json::from_slice(&body).unwrap();

    // …used against bravo → 403.
    let req = Request::builder()
        .uri(format!("/missions/bravo/changes?token={}", sub.token))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn fresh_database_returns_baseline_missions_only() {
    // V33 inserts two baseline rows (`citrap`, `exchecktemplates`) used
    // by upstream tooling. A "fresh" tak-rs database after running
    // migrations starts with these two; this is wire-compat with Java
    // upstream. Truly empty would be a regression.
    let (_c, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let app = MissionRouter::build(store);

    let req = Request::builder()
        .uri("/missions")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let missions: Vec<Mission> = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = missions.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, vec!["citrap", "exchecktemplates"]);
}
