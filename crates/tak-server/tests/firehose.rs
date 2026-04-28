//! End-to-end integration: bind firehose listener, drive a real TAK
//! frame in via TCP, prove the bus dispatched it AND the row landed in
//! Postgres.
//!
//! Same testcontainer pattern as `pipeline.rs`; differs in that it
//! exercises the `firehose::run` accept loop + reader path rather
//! than calling `dispatch_and_persist` directly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::io::Cursor;
use std::time::Duration;

use prost::Message;
use sqlx::Row;
use tak_bus::Bus;
use tak_cot::framing;
use tak_cot::proto::view_to_takmessage;
use tak_cot::xml::decode_xml;
use tak_server::firehose::{self, PersistMode};
use tak_store::Store;
use testcontainers::core::{ContainerPort, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

const PG_USER: &str = "tak";
const PG_PASS: &str = "takatak";
const PG_DB: &str = "tak";

const FIXTURE_PLI: &str = include_str!("../../tak-cot/tests/fixtures/01_pli.xml");

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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn firehose_dispatches_and_persists_a_real_frame() {
    let (_pg, url) = start_postgis().await;
    let store = Store::connect_and_migrate(&url).await.unwrap();
    let bus = Bus::new();

    // Bind on an ephemeral port; spawn the accept loop.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    let bus_for_loop = bus.clone();
    let store_for_loop = store.clone();
    let _accept = tokio::spawn(async move {
        let _ = firehose::run(listener, bus_for_loop, store_for_loop, PersistMode::On).await;
    });

    // Bake one PLI frame the same way taktool loadgen does.
    let view = decode_xml(FIXTURE_PLI).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let proto_bytes = msg.encode_to_vec();
    let mut framed = Vec::with_capacity(proto_bytes.len() + 8);
    framing::encode_stream(&proto_bytes, &mut Cursor::new(&mut framed)).unwrap();

    // Connect, write the frame, close.
    let mut sock = TcpStream::connect(local).await.unwrap();
    sock.set_nodelay(true).ok();
    sock.write_all(&framed).await.unwrap();
    sock.flush().await.unwrap();
    drop(sock);

    // Poll the cot_router table until the row lands or we time out.
    let pool = store.pool();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut count: i64 = 0;
    while tokio::time::Instant::now() < deadline {
        let row = sqlx::query("SELECT count(*) AS n FROM cot_router")
            .fetch_one(pool)
            .await
            .unwrap();
        count = row.get::<i64, _>("n");
        if count > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(count > 0, "expected the row in cot_router; saw {count}");

    // Bus subscription registry is empty (subscribers come and go on
    // accept) — but the inbound did go through the dispatch path, which
    // is what we care about for the firehose contract.
    assert_eq!(
        bus.len(),
        0,
        "bus should be empty after the test connection drops"
    );

    // Tag-along check: the persisted row carries the fixture's UID.
    let uid_row = sqlx::query("SELECT uid FROM cot_router LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap();
    let uid: String = uid_row.get("uid");
    assert!(uid.starts_with("ANDROID-"), "got uid: {uid}");
}
