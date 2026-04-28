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
use tak_conformance::scenarios::pli_dispatch_byte_identity::PliDispatchByteIdentity;
use tak_conformance::scenarios::replay_on_reconnect::ReplayOnReconnect;
use tak_conformance::{Outcome, Scenario, TestServer};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn conformance_suite() {
    // 60 s replay window is enough for the replay scenario; the
    // PLI byte-identity scenario opens its subscriber BEFORE any
    // event is persisted, so its replay query finds 0 rows and
    // doesn't perturb the assertion.
    let server = TestServer::start_with(Some(std::time::Duration::from_secs(60)))
        .await
        .expect("test server start");

    let scenarios: Vec<Box<dyn Scenario>> = vec![
        Box::new(PliDispatchByteIdentity),
        Box::new(ChatXmlLossless),
        Box::new(ReplayOnReconnect),
    ];

    let mut any_failed = false;
    let mut report: Vec<(String, String, String)> = Vec::with_capacity(scenarios.len());

    for sc in &scenarios {
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

    println!("\n=== conformance report ===");
    for (name, desc, outcome) in &report {
        println!("  {outcome:<12}  {name}  -- {desc}");
    }
    println!();

    assert!(!any_failed, "at least one conformance scenario FAILED");
}
