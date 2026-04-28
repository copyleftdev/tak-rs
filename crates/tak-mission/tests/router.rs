//! Issue #32 + #33 acceptance tests.
//!
//! - The router-level tests (no state) drive the Router via
//!   `tower::ServiceExt::oneshot` so we don't need to bind a TCP socket.
//! - The mission tests use a real Postgres+PostGIS testcontainer and
//!   exercise the actual SQL queries.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tak_mission::{Mission, MissionRouter};
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
