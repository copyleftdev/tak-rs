//! Conformance scenarios that need direct access to the in-process
//! `Bus` or `Store` handle, not just the wire socket.
//!
//! The wire-only `Scenario` trait deliberately can't see these
//! handles — that contract is what lets the same trait drive a
//! remote tak-server (or the upstream Java server, for diff runs)
//! over the network. This file is the in-process complement: it
//! pins behaviors that are observable only with privileged access
//! to the system under test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_methods,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("tak_server=debug")),
        )
        .with_test_writer()
        .try_init();
}

use std::time::Duration;

use bytes::Bytes;
use prost::Message;
use tak_conformance::{AtakMockClient, TestServer};
use tak_cot::framing;
use tak_cot::proto::view_to_takmessage;
use tak_cot::xml::decode_xml;

const FIXTURE_PLI: &str = include_str!("../../tak-cot/tests/fixtures/01_pli.xml");

fn bake_pli() -> Bytes {
    let view = decode_xml(FIXTURE_PLI).expect("fixture decode");
    let msg = view_to_takmessage(&view).expect("fixture proto");
    let proto_bytes = msg.encode_to_vec();
    let mut framed = Vec::with_capacity(proto_bytes.len() + 8);
    framing::encode_stream(&proto_bytes, &mut std::io::Cursor::new(&mut framed))
        .expect("fixture frame");
    Bytes::from(framed)
}

/// Connect N clients, assert the bus's subscriber count rises by
/// N. Disconnect them all, assert the count returns to the
/// pre-connect baseline. Pins the bus's subscription-cleanup
/// contract on socket-close — without this, slow client churn
/// leaks `Subscription` slots forever.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn subscription_lifecycle_count() {
    let server = TestServer::start().await.expect("test server start");
    let baseline = server.bus.len();
    println!("baseline subs={baseline}");

    const N: usize = 10;
    let mut clients: Vec<AtakMockClient> = Vec::with_capacity(N);
    for i in 0..N {
        let c = AtakMockClient::connect(server.firehose_addr)
            .await
            .unwrap_or_else(|e| panic!("client {i} connect: {e:?}"));
        clients.push(c);
    }

    // Subscriptions register asynchronously inside the firehose
    // accept handler; give the runtime a beat to wire each one.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while server.bus.len() < baseline + N && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        server.bus.len(),
        baseline + N,
        "all {N} subs should be registered after connect; saw bus.len()={}",
        server.bus.len()
    );

    // Drop all clients (close sockets).
    drop(clients);

    // Wait for the firehose connection-cleanup to drain. The
    // read_loop sees EOF, then the per-conn task exits and the
    // Subscription is detached.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while server.bus.len() > baseline && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        server.bus.len(),
        baseline,
        "all subs should drain back to baseline; saw bus.len()={}",
        server.bus.len()
    );
}

/// Subscribe directly via the bus API with a tiny mpsc cap, never
/// drain the rx, and have a firehose publisher flood frames in.
/// Assert the bus increments `dropped_full` on the slow sub's
/// `SubscriptionStats`. Without this, a stalled subscriber could
/// silently degrade the bus or be exempted from drop accounting.
///
/// Design note: a "slow ATAK client" via the firehose isn't a
/// reliable way to force drops on loopback — Linux auto-tunes TCP
/// recv buffers up to several MB, so the firehose's writer task
/// drains the per-sub mpsc faster than dispatch fills it for any
/// reasonable burst. We bypass the firehose write path here and
/// instead hold the rx directly with a 4-deep capacity. The
/// publisher still goes through the firehose so the dispatch path
/// is exercised end-to-end; only the failure-mode subscriber is
/// synthesized.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn slow_subscriber_drop_accounting() {
    use tak_bus::{Filter, GroupBitvector};

    init_tracing();
    let server = TestServer::start().await.expect("test server start");
    let frame = bake_pli();

    // Slow subscriber: registered directly through the bus with a
    // tiny capacity. We hold the rx but never recv from it, so
    // dispatch fills the mpsc immediately and subsequent try_sends
    // return `Full`.
    let (slow_handle, _slow_rx) = server.bus.subscribe_with_capacity(
        Filter {
            group_mask: GroupBitvector([!0u64; 4]),
            ..Filter::default()
        },
        4,
    );
    let slow_id = slow_handle.id();
    println!("slow sub registered: id={slow_id:?}");

    // Publisher: a real firehose-connected mock client. It also
    // gets a wildcard subscription on connect — that subscription
    // we don't care about, but we drain the publisher's socket so
    // its self-fanout doesn't back-pressure the firehose.
    let mut publisher = AtakMockClient::connect(server.firehose_addr)
        .await
        .expect("publisher connect");
    tokio::time::sleep(Duration::from_millis(100)).await;

    const BURST: usize = 200;
    for i in 0..BURST {
        if let Err(e) = publisher.send_frame(&frame).await {
            panic!("burst send {i}: {e:?}");
        }
        // Drain publisher's self-fanout every few sends so it never
        // stalls the firehose's read path.
        if i.is_multiple_of(8) {
            let _ = publisher
                .recv_frame_with_timeout(Duration::from_millis(5))
                .await;
        }
    }
    println!("publisher sent {BURST} frames");

    // Wait for the slow sub's drop counter to climb above the
    // first few that fit in the 4-cap mpsc.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut slow_dropped = 0u64;
    let mut slow_delivered = 0u64;
    while std::time::Instant::now() < deadline {
        for s in server.bus.subscription_stats() {
            if s.id == slow_id {
                slow_dropped = s.dropped_full;
                slow_delivered = s.delivered;
            }
        }
        if slow_dropped > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("slow sub: delivered={slow_delivered} dropped_full={slow_dropped}");
    assert!(
        slow_dropped > 0,
        "expected slow sub (mpsc cap=4) to drop at least one frame from a {BURST}-frame burst; \
         saw delivered={slow_delivered} dropped_full={slow_dropped}"
    );
    assert!(
        slow_delivered <= 4,
        "slow sub mpsc cap=4 should accept at most 4 frames; saw delivered={slow_delivered}"
    );

    // Keep slow_handle alive until the end so the entry isn't
    // unsubscribed before our final read.
    drop(slow_handle);
}

/// Publish K frames; wait for persistence to drain; assert at
/// least K rows landed in `cot_router`. Pins the persistence
/// side-channel contract — frames published over the firehose
/// also reach durable storage, and the drain-wait is honest.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn persistence_row_lands() {
    let server = TestServer::start().await.expect("test server start");
    let frame = bake_pli();
    let baseline = server.store.inserted_count();

    let mut publisher = AtakMockClient::connect(server.firehose_addr)
        .await
        .expect("publisher connect");
    tokio::time::sleep(Duration::from_millis(100)).await;

    const K: u64 = 25;
    for _ in 0..K {
        publisher.send_frame(&frame).await.expect("send");
    }

    // Drain — wait_for_drain returns when inserted_count is
    // stable for >150 ms (no more pending writes).
    let final_count = server.store.wait_for_drain(Duration::from_secs(10)).await;
    let delta = final_count.saturating_sub(baseline);
    assert!(
        delta >= K,
        "expected ≥{K} new rows, saw delta={delta} (baseline={baseline}, final={final_count})"
    );
    assert_eq!(
        server.store.dropped_count(),
        0,
        "no rows should have been dropped during a 25-frame slow trickle"
    );
}
