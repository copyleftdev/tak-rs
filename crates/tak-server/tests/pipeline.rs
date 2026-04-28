//! Issue #31 integration test.
//!
//! Spins up a real bus + a real Postgres+PostGIS testcontainer + the
//! `dispatch_and_persist` glue, fires CoT events through it, verifies:
//! - Subscribers receive the event (bus dispatch works).
//! - The cot_router table has the row (persistence works).
//! - When the persistence channel is full, dispatch still succeeds
//!   while persistence drops + the counter increments (the H1 boundary
//!   respects).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use bytes::Bytes;
use sqlx::Row;
use tak_bus::{Bus, DispatchScratch, Filter, GroupBitvector};
use tak_proto::v1::{CotEvent, Detail, TakMessage};
use tak_server::pipeline::dispatch_and_persist;
use tak_store::Store;
use testcontainers::core::{ContainerPort, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

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

fn synthetic_takmessage(uid: &str) -> TakMessage {
    TakMessage {
        tak_control: None,
        cot_event: Some(CotEvent {
            r#type: "a-f-G-U-C".to_owned(),
            uid: uid.to_owned(),
            send_time: 1_777_266_000_000,
            start_time: 1_777_266_000_000,
            stale_time: 1_777_266_090_000,
            how: "m-g".to_owned(),
            lat: 34.0,
            lon: -118.0,
            hae: 245.0,
            ce: 9.0,
            le: 9_999_999.0,
            access: String::new(),
            qos: String::new(),
            opex: String::new(),
            caveat: String::new(),
            releaseable_to: String::new(),
            detail: Some(Detail {
                xml_detail: r#"<takv platform="ATAK-CIV"/>"#.to_owned(),
                ..Detail::default()
            }),
        }),
        submission_time: 0,
        creation_time: 0,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run via --ignored"]
async fn dispatch_and_persist_does_both() {
    let (_container, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();

    let bus = Bus::new();
    let (_h, mut rx) = bus.subscribe(Filter {
        group_mask: GroupBitvector::EMPTY.with_bit(0),
        ..Filter::default()
    });

    let msg = synthetic_takmessage("VIPER01");
    let payload = Bytes::from_static(b"wire-bytes-go-here");
    let mut scratch = DispatchScratch::with_capacity(64);

    let stats = dispatch_and_persist(
        &bus,
        &store,
        &msg,
        GroupBitvector::EMPTY.with_bit(0),
        payload.clone(),
        &mut scratch,
    )
    .unwrap();

    // Bus side: subscriber received exactly one byte payload.
    assert_eq!(stats.dispatch.delivered, 1);
    let got = rx.recv().await.unwrap();
    assert_eq!(&got[..], b"wire-bytes-go-here");

    // Storage side: persisted == true; row reaches cot_router after drain.
    assert!(stats.persisted);
    let drained = store.wait_for_drain(Duration::from_secs(5)).await;
    assert_eq!(drained, 1);
    let row = sqlx::query("SELECT uid, cot_type FROM cot_router WHERE uid = 'VIPER01'")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let uid: String = row.try_get("uid").unwrap();
    let kind: String = row.try_get("cot_type").unwrap();
    assert_eq!(uid, "VIPER01");
    assert_eq!(kind, "a-f-G-U-C");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn dispatch_succeeds_even_when_persistence_drops() {
    let (_container, url) = start_postgis().await;
    // Tiny channel + slow flush so we can fill it.
    let store = Store::connect_and_migrate_with(
        &url,
        4,                                 // channel capacity
        2,                                 // batch_max
        std::time::Duration::from_secs(5), // flush_interval — long enough to never drain in this test
    )
    .await
    .unwrap();

    let bus = Bus::new();
    let (_h, mut rx) = bus.subscribe(Filter {
        group_mask: GroupBitvector::EMPTY.with_bit(0),
        ..Filter::default()
    });

    let mut scratch = DispatchScratch::with_capacity(64);
    let payload = Bytes::from_static(b"x");

    let mut delivered = 0u32;
    let mut persisted_true = 0u32;
    let mut persisted_false = 0u32;
    for i in 0..30 {
        let msg = synthetic_takmessage(&format!("UID-{i}"));
        let s = dispatch_and_persist(
            &bus,
            &store,
            &msg,
            GroupBitvector::EMPTY.with_bit(0),
            payload.clone(),
            &mut scratch,
        )
        .unwrap();
        delivered += s.dispatch.delivered;
        if s.persisted {
            persisted_true += 1;
        } else {
            persisted_false += 1;
        }
    }

    // Dispatch ALWAYS succeeded — the persistence pressure didn't stall it.
    assert_eq!(
        delivered, 30,
        "every dispatch must succeed regardless of persistence pressure"
    );

    // Some persistence calls dropped (channel cap=4, no drain in window).
    assert!(persisted_false > 0, "expected some persistence drops");
    assert!(persisted_true > 0, "expected some persistence successes");

    // Store's dropped_count counter matches our local count.
    assert_eq!(store.dropped_count(), u64::from(persisted_false));

    // Drain the receiver so the test doesn't leave queued messages.
    while rx.try_recv().is_ok() {}
}
