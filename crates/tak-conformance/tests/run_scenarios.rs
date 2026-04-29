//! Boot a test server and run every conformance scenario against
//! it. PASS / FAIL / SKIPPED is reported per-scenario; the suite as
//! a whole fails if any FAIL is present.
//!
//! Skipped scenarios do not gate the build. They're a backlog
//! readout — the scenario name + reason makes it clear what needs
//! to land for the suite to graduate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_methods
)]

use tak_conformance::scenarios::chat_xml_lossless::ChatXmlLossless;
use tak_conformance::scenarios::dispatch_under_burst::DispatchUnderBurst;
use tak_conformance::scenarios::multi_publisher_no_crosstalk::MultiPublisherNoCrosstalk;
use tak_conformance::scenarios::multi_subscriber_fanout::MultiSubscriberFanout;
use tak_conformance::scenarios::pli_dispatch_byte_identity::PliDispatchByteIdentity;
use tak_conformance::scenarios::replay_on_reconnect::ReplayOnReconnect;
use tak_conformance::{Outcome, Scenario, TestServer};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn conformance_suite() {
    // Byte-identity scenarios assume "I see exactly what was
    // just published" — prior scenarios' persisted frames must
    // not bleed in via replay. Boot the server with replay
    // disabled for these.
    let server = TestServer::start().await.expect("test server start");

    let byte_identity: Vec<Box<dyn Scenario>> = vec![
        Box::new(PliDispatchByteIdentity),
        Box::new(ChatXmlLossless),
        Box::new(MultiSubscriberFanout),
        Box::new(MultiPublisherNoCrosstalk),
        Box::new(DispatchUnderBurst),
    ];

    let mut any_failed = false;
    let mut report: Vec<(String, String, String)> = Vec::with_capacity(byte_identity.len() + 1);

    for sc in &byte_identity {
        let outcome = sc.run(server.firehose_addr).await;
        if matches!(outcome, Outcome::Fail(_)) {
            any_failed = true;
        }
        report.push((
            sc.name().to_owned(),
            sc.description().to_owned(),
            outcome.to_string(),
        ));
    }
    drop(server);

    // Replay scenario needs replay enabled. A fresh server
    // ensures the only persisted events are the ones it
    // publishes itself.
    let replay_server = TestServer::start_with(Some(std::time::Duration::from_secs(60)))
        .await
        .expect("replay test server start");
    let replay_sc = ReplayOnReconnect;
    let outcome = replay_sc.run(replay_server.firehose_addr).await;
    if matches!(outcome, Outcome::Fail(_)) {
        any_failed = true;
    }
    report.push((
        replay_sc.name().to_owned(),
        replay_sc.description().to_owned(),
        outcome.to_string(),
    ));

    println!("\n=== conformance report ===");
    for (name, desc, outcome) in &report {
        println!("  {outcome:<12}  {name}  -- {desc}");
    }
    println!();

    assert!(!any_failed, "at least one conformance scenario FAILED");
}
