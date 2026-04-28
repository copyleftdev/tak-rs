//! Integration test: spin up a real PostGIS container, apply the full
//! migration chain, verify the schema is what we expect.
//!
//! Verifies invariant policy: tests that need Postgres hit a REAL Postgres
//! (no mocks) per `docs/invariants.md`. testcontainers spins up the
//! `postgis/postgis:16-3.4` image and the test connects via sqlx.
//!
//! Marked `#[ignore]` so default `cargo test` doesn't pull the docker
//! image. Run via:
//!
//! ```sh
//! cargo test -p tak-store --test migrations -- --ignored
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use sqlx::Row;
use tak_store::Store;
use testcontainers::core::{ContainerPort, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

const POSTGRES_USER: &str = "tak";
const POSTGRES_PASSWORD: &str = "takatak";
const POSTGRES_DB: &str = "tak";

/// Spin up a PostGIS container; return (container, connection url).
async fn start_postgis() -> (testcontainers::ContainerAsync<GenericImage>, String) {
    let container = GenericImage::new("postgis/postgis", "16-3.4")
        .with_exposed_port(ContainerPort::Tcp(5432))
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_wait_for(WaitFor::Duration {
            length: Duration::from_secs(1),
        })
        .with_env_var("POSTGRES_USER", POSTGRES_USER)
        .with_env_var("POSTGRES_PASSWORD", POSTGRES_PASSWORD)
        .with_env_var("POSTGRES_DB", POSTGRES_DB)
        .start()
        .await
        .expect("postgis container start");

    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432.tcp())
        .await
        .expect("host port");
    let url = format!("postgres://{POSTGRES_USER}:{POSTGRES_PASSWORD}@{host}:{port}/{POSTGRES_DB}");
    (container, url)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run via `cargo test -p tak-store --test migrations -- --ignored`"]
async fn migrations_apply_cleanly_to_fresh_postgis() {
    let (_container, url) = start_postgis().await;

    let store = Store::connect_and_migrate(&url)
        .await
        .expect("connect + migrate");

    // The pool is live; basic round-trip works.
    let row = sqlx::query("SELECT 1::int4 AS one")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let one: i32 = row.try_get("one").unwrap();
    assert_eq!(one, 1);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn cot_router_table_and_postgis_extension_present() {
    let (_container, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let pool = store.pool();

    // PostGIS extension is installed.
    let postgis_row = sqlx::query("SELECT extname FROM pg_extension WHERE extname = 'postgis'")
        .fetch_optional(pool)
        .await
        .unwrap();
    assert!(postgis_row.is_some(), "postgis extension missing");

    // The cot_router table exists.
    let cot_router_row =
        sqlx::query("SELECT to_regclass('public.cot_router') IS NOT NULL AS present")
            .fetch_one(pool)
            .await
            .unwrap();
    let present: bool = cot_router_row.try_get("present").unwrap();
    assert!(present, "cot_router table not created by migrations");

    // The mission table exists (from V12+).
    let mission_row = sqlx::query("SELECT to_regclass('public.mission') IS NOT NULL AS present")
        .fetch_one(pool)
        .await
        .unwrap();
    let mission_present: bool = mission_row.try_get("present").unwrap();
    assert!(mission_present, "mission table not created by migrations");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn schema_reports_max_migration_version() {
    let (_container, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();

    // sqlx maintains _sqlx_migrations as the bookkeeping table.
    let max_row = sqlx::query("SELECT MAX(version) AS max_v FROM _sqlx_migrations")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let max_v: i64 = max_row.try_get("max_v").unwrap();
    assert!(
        max_v >= 99,
        "expected to apply migrations through V99, max_v={max_v}"
    );
}

// ===========================================================================
// Issue #29 — batched insert
// ===========================================================================

use tak_store::CotInsert;

fn synth(uid: &str) -> CotInsert {
    CotInsert {
        uid: uid.to_owned(),
        cot_type: "a-f-G-U-C".to_owned(),
        time_ms: 1_777_266_000_000,
        start_ms: 1_777_266_000_000,
        stale_ms: 1_777_266_090_000,
        how: "m-g".to_owned(),
        lat: 34.0,
        lon: -118.0,
        hae: 245.0,
        ce: 9.0,
        le: 9_999_999.0,
        detail: r#"<takv platform="ATAK-CIV"/>"#.to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn batched_insert_persists_all_events() {
    let (_container, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();

    const N: usize = 250;
    for i in 0..N {
        store
            .try_insert_event(synth(&format!("ANDROID-{i}")))
            .expect("channel not full");
    }

    let drained = store
        .wait_for_drain(std::time::Duration::from_secs(10))
        .await;
    assert_eq!(
        drained, N as u64,
        "writer should have inserted all N events"
    );

    let count_row = sqlx::query("SELECT COUNT(*)::bigint AS n FROM cot_router")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let n: i64 = count_row.try_get("n").unwrap();
    assert_eq!(n, i64::try_from(N).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn try_insert_drops_when_channel_full() {
    let (_container, url) = start_postgis().await;
    // Tiny channel so we can fill it. flush_interval is huge so the writer
    // doesn't drain before we check.
    let store = tak_store::Store::connect_and_migrate_with(
        &url,
        4,                                 // channel capacity
        2,                                 // batch_max
        std::time::Duration::from_secs(5), // flush_interval
    )
    .await
    .unwrap();

    // Burst until something drops. With cap=4, we expect ~4-6 successes
    // (depending on writer-task drain timing) and the rest dropped.
    let mut dropped: u64 = 0;
    let total: u64 = 50;
    for i in 0..total {
        if store.try_insert_event(synth(&format!("UID-{i}"))).is_err() {
            dropped += 1;
        }
    }
    assert!(dropped > 0, "with cap=4 and 50 sends, some MUST drop");
    assert_eq!(
        store.dropped_count(),
        dropped,
        "dropped_count counter mismatch"
    );

    // Eventually persists what made it through.
    let drained = store
        .wait_for_drain(std::time::Duration::from_secs(15))
        .await;
    assert_eq!(
        drained,
        total - dropped,
        "successful sends should all persist"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn inserted_rows_have_postgis_geometry() {
    let (_container, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();

    store.try_insert_event(synth("GEO-1")).unwrap();
    let drained = store
        .wait_for_drain(std::time::Duration::from_secs(5))
        .await;
    assert_eq!(drained, 1);

    // The PostGIS geometry column should be populated with a valid Point.
    let row = sqlx::query(
        "SELECT ST_X(event_pt) AS lon, ST_Y(event_pt) AS lat, ST_SRID(event_pt) AS srid \
         FROM cot_router WHERE uid = 'GEO-1'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    let lon: f64 = row.try_get("lon").unwrap();
    let lat: f64 = row.try_get("lat").unwrap();
    let srid: i32 = row.try_get("srid").unwrap();
    assert!((lon - -118.0).abs() < 1e-9);
    assert!((lat - 34.0).abs() < 1e-9);
    assert_eq!(srid, 4326, "PostGIS SRID must be WGS-84");
}
