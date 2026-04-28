//! Issue #32 acceptance tests.
//!
//! Drives the router via `tower::ServiceExt::oneshot` so we don't need
//! to bind a TCP socket — the router is itself a `Service`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tak_mission::MissionRouter;
use tower::ServiceExt;

#[tokio::test]
async fn health_returns_200_with_ok_body() {
    let app = MissionRouter::build();
    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let app = MissionRouter::build();
    let req = Request::builder()
        .uri("/does-not-exist")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn health_responds_to_get_only() {
    // axum's default behavior: routes registered with `get(...)` reject
    // other methods with 405 Method Not Allowed.
    let app = MissionRouter::build();
    let req = Request::builder()
        .method("POST")
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
