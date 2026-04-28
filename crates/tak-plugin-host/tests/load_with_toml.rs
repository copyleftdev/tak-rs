//! Loads the bundled `geofence-redact` example end-to-end through
//! [`PluginHost::new`] and proves the per-plugin `<stem>.toml`
//! flows through into the worker. Doesn't need Postgres or the
//! firehose — exercises just the host runtime.
//!
//! Skipped if the example wasm hasn't been built yet (so `cargo
//! nextest run` on a fresh clone doesn't fail with a confusing
//! file-not-found).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::disallowed_methods
)]

use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use tak_plugin_host::{PluginEvent, PluginHost, PluginHostConfig};

fn example_wasm_path() -> PathBuf {
    // Tests run from the crate dir; the example lives at workspace
    // root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../examples/plugins/geofence-redact/target/wasm32-wasip2/release/geofence_redact.wasm",
    )
}

fn example_toml_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins/geofence-redact/geofence_redact.toml")
}

#[tokio::test]
async fn host_loads_example_plugin_with_bundled_toml() {
    let wasm = example_wasm_path();
    let toml = example_toml_path();
    if !wasm.exists() {
        eprintln!(
            "skipping: example wasm not built — run `cargo build --release --target wasm32-wasip2` in examples/plugins/geofence-redact"
        );
        return;
    }
    assert!(toml.exists(), "example toml missing: {}", toml.display());

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::copy(&wasm, dir.path().join("geofence_redact.wasm")).expect("copy wasm");
    std::fs::copy(&toml, dir.path().join("geofence_redact.toml")).expect("copy toml");

    let cfg = PluginHostConfig {
        plugin_dir: dir.path().to_path_buf(),
        queue_capacity: 16,
    };
    let host = PluginHost::new(cfg).await.expect("plugin host comes up");
    assert_eq!(host.len(), 1, "geofence_redact should be loaded");

    // Push a couple of events through the queue. Below the configured
    // threshold (36.0) so the plugin's drop-counter ticks; above the
    // threshold so it passes. The host doesn't surface the plugin's
    // internal state directly; this test confirms the queue isn't
    // stalled and the publish path works after init.
    let event_below = PluginEvent {
        payload: Bytes::from_static(b""),
        cot_type: "a-f-G-U-C".to_owned(),
        uid: "TEST-1".to_owned(),
        callsign: Some("BELOW".to_owned()),
        lat: 35.0,
        lon: -80.0,
        hae: 0.0,
        send_time_ms: 0,
        sender_groups_low: 0,
    };
    let event_above = PluginEvent {
        lat: 40.0,
        callsign: Some("ABOVE".to_owned()),
        uid: "TEST-2".to_owned(),
        ..event_below.clone()
    };
    assert_eq!(host.publish(event_below), 1);
    assert_eq!(host.publish(event_above), 1);

    // Give the worker a beat to drain. We're not asserting on
    // plugin-internal counters here — just that the publish-then-
    // drain pipeline doesn't deadlock or panic. The previous
    // 25 k-event firehose smoke (commit 59af584) covers the deeper
    // contract.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn tight_cpu_budget_eventually_traps_and_unloads_worker() {
    // The geofence-redact plugin's heartbeat path (every 1000
    // events) formats a string + crosses a host import — that's
    // the work that exceeds a 1 ms epoch budget on an idle box.
    // Setting `max-cpu-ms-per-msg = 1` should let several thousand
    // pure-compare calls succeed, then the heartbeat traps and
    // the worker exits permanently.
    //
    // This test is the load-bearing assertion that the budget
    // enforcement is wired and not a no-op.
    let wasm = example_wasm_path();
    if !wasm.exists() {
        eprintln!("skipping: example wasm not built");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::copy(&wasm, dir.path().join("geofence_redact.wasm")).expect("copy wasm");
    std::fs::write(
        dir.path().join("geofence_redact.toml"),
        // Same plugin-config as the bundled toml so the plugin
        // does its full heartbeat work; budget squeezed to 1 ms.
        r#"
            [limits]
            max-cpu-ms-per-msg = 1

            [capabilities]
            plugin-config = '{ "drop_below_lat": 36.0 }'
        "#,
    )
    .expect("write toml");

    let cfg = PluginHostConfig {
        plugin_dir: dir.path().to_path_buf(),
        queue_capacity: 256,
    };
    let host = PluginHost::new(cfg).await.expect("host comes up");
    assert_eq!(host.len(), 1);

    let event = PluginEvent {
        payload: Bytes::from_static(b""),
        cot_type: "a-f-G-U-C".to_owned(),
        uid: "TEST-OVERRUN".to_owned(),
        callsign: Some("X".to_owned()),
        lat: 35.0, // below threshold so plugin tries to drop+log
        lon: -80.0,
        hae: 0.0,
        send_time_ms: 0,
        sender_groups_low: 0,
    };

    // Pump events with a small inter-batch sleep so the worker
    // can drain. After the heartbeat traps the worker, the rx
    // closes and `publish` returns 0 forever after.
    let mut saw_unload = false;
    for batch in 0..50u32 {
        for _ in 0..256 {
            let _ = host.publish(event.clone());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        if host.publish(event.clone()) == 0 {
            saw_unload = true;
            // Once unloaded, every subsequent publish must also
            // be 0 (the channel doesn't recover).
            for _ in 0..10 {
                assert_eq!(host.publish(event.clone()), 0);
            }
            break;
        }
        // Bail-out so the test isn't open-ended if something
        // changes that prevents the trap.
        if batch > 30 && !saw_unload {
            panic!("worker did not unload after {batch} batches; budget enforcement broken");
        }
    }
    assert!(
        saw_unload,
        "expected the 1 ms budget to trap + unload the worker"
    );
}

#[tokio::test]
async fn disabled_plugin_is_skipped() {
    let wasm = example_wasm_path();
    if !wasm.exists() {
        eprintln!("skipping: example wasm not built");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::copy(&wasm, dir.path().join("geofence_redact.wasm")).expect("copy wasm");
    std::fs::write(
        dir.path().join("geofence_redact.toml"),
        "[plugin]\nenabled = false\n",
    )
    .expect("write toml");

    let cfg = PluginHostConfig {
        plugin_dir: dir.path().to_path_buf(),
        queue_capacity: 16,
    };
    let host = PluginHost::new(cfg).await.expect("plugin host comes up");
    assert_eq!(
        host.len(),
        0,
        "plugin with enabled=false should not be loaded"
    );
}
