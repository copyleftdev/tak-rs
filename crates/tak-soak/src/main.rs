//! `tak-soak` — wall-clock soak harness for tak-server.
//!
//! # What this is
//!
//! VOPR is logical-time: millions of ops in seconds, catches
//! state-machine bugs. **This** is wall-clock real-time:
//! sustained load against a real Postgres + the tak-server
//! release binary for hours, catches things VOPR can't see —
//! memory leaks, allocator fragmentation, mpsc block-pool
//! drift, TLS resumption-cache leaks, Postgres connection
//! churn, plugin worker accumulation.
//!
//! # How it runs
//!
//! 1. Boot a `postgis/postgis:16-3.4` testcontainer.
//! 2. Spawn the local `target/release/tak-server` as a real
//!    subprocess against that postgres. We measure THAT
//!    binary's RSS, not the harness's.
//! 3. Wait for the metrics endpoint to respond (= server ready).
//! 4. Spawn `target/release/taktool loadgen` as a subprocess to
//!    drive the configured publisher load.
//! 5. Every `--sample-interval-secs` (default 1 s), sample:
//!    - tak-server's RSS from `/proc/<pid>/status`.
//!    - tak-server's prom metrics: `tak_bus_*`,
//!      `tak.persistence.*`. Parsed inline, no client lib.
//!    - Wall-clock timestamp.
//!    Push to a Vec for end-of-run analysis.
//! 6. After `--duration-secs`, kill loadgen + tak-server +
//!    teardown postgres.
//! 7. Compute linear regression of RSS vs time. Slope =
//!    kb/min growth rate. Fail if greater than
//!    `--max-rss-drift-kb-per-min`.
//! 8. Optionally write CSV for offline analysis.
//!
//! # Default thresholds
//!
//! - `--max-rss-drift-kb-per-min = 1024` (1 MB/min). At 60 MB/h,
//!   any real leak shows up well within an hour-long soak.
//! - `--duration-secs = 300` (5 min) — enough wall-clock for the
//!   regression to be statistically meaningful, short enough to
//!   gate a nightly job.
//!
//! # Binary D1 exemption
//!
//! Same as the rest of the harness binaries.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::doc_lazy_continuation,
    unreachable_pub
)]

mod analyze;
mod metrics;
mod sampler;
mod server;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::process::Command;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "tak-soak",
    version,
    about = "wall-clock soak harness for tak-server",
    long_about = None,
)]
struct Args {
    /// How long to drive load before stopping.
    #[arg(long, env = "SOAK_DURATION_SECS", default_value_t = 300)]
    duration_secs: u64,

    /// Sampling cadence. 1 s = 300 samples per default 5-min run,
    /// plenty for a linear regression.
    #[arg(long, env = "SOAK_SAMPLE_INTERVAL_SECS", default_value_t = 1)]
    sample_interval_secs: u64,

    /// Loadgen connection count.
    #[arg(long, env = "SOAK_CONNS", default_value_t = 50)]
    conns: usize,

    /// Loadgen per-connection emit rate (msg/s).
    #[arg(long, env = "SOAK_RATE", default_value_t = 200)]
    rate: u32,

    /// RSS-drift threshold. Run fails if linear regression of
    /// RSS over time has a slope greater than this. 1 MB/min
    /// catches any real leak inside a single soak window
    /// while tolerating allocator-pool warmup at run start.
    #[arg(long, env = "SOAK_MAX_RSS_DRIFT", default_value_t = 1024.0)]
    max_rss_drift_kb_per_min: f64,

    /// Path to the tak-server binary. Defaults to the workspace's
    /// release build.
    #[arg(
        long,
        env = "SOAK_TAK_SERVER",
        default_value = "target/release/tak-server"
    )]
    tak_server_bin: PathBuf,

    /// Path to taktool. Defaults to the workspace's release build.
    #[arg(long, env = "SOAK_TAKTOOL", default_value = "target/release/taktool")]
    taktool_bin: PathBuf,

    /// CSV output path. If unset, no CSV is written.
    #[arg(long, env = "SOAK_OUT_CSV")]
    out_csv: Option<PathBuf>,

    /// Use this firehose port instead of the default 18088.
    /// Needed if 18088 is already taken on the host.
    #[arg(long, env = "SOAK_FIREHOSE_PORT", default_value_t = 18088)]
    firehose_port: u16,

    /// Metrics port (corresponds to tak-server --listen-metrics).
    #[arg(long, env = "SOAK_METRICS_PORT", default_value_t = 19091)]
    metrics_port: u16,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,tak_soak=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    info!(
        duration_s = args.duration_secs,
        conns = args.conns,
        rate = args.rate,
        firehose_port = args.firehose_port,
        metrics_port = args.metrics_port,
        "tak-soak starting"
    );

    if !args.tak_server_bin.exists() {
        anyhow::bail!(
            "tak-server binary not found at {} — run `cargo build --release -p tak-server` first \
             or pass --tak-server-bin",
            args.tak_server_bin.display()
        );
    }
    if !args.taktool_bin.exists() {
        anyhow::bail!(
            "taktool binary not found at {} — run `cargo build --release -p taktool` first \
             or pass --taktool-bin",
            args.taktool_bin.display()
        );
    }

    // Postgres testcontainer.
    let (_pg_handle, db_url) = server::start_postgis()
        .await
        .context("start postgres testcontainer")?;
    info!(db_url, "postgres ready");

    // tak-server subprocess.
    let mut server_child = server::start_tak_server(
        &args.tak_server_bin,
        &db_url,
        args.firehose_port,
        args.metrics_port,
    )
    .await
    .context("start tak-server subprocess")?;
    let server_pid = server_child
        .id()
        .ok_or_else(|| anyhow::anyhow!("tak-server child has no PID"))?;
    info!(pid = server_pid, "tak-server spawned");

    let metrics_url = format!("http://127.0.0.1:{}/metrics", args.metrics_port);
    let firehose_addr = format!("127.0.0.1:{}", args.firehose_port);
    server::wait_for_ready(&metrics_url, &firehose_addr, Duration::from_secs(30))
        .await
        .context("wait for tak-server (metrics + firehose) ready")?;
    info!(metrics_url, firehose_addr, "tak-server ready");

    // taktool loadgen subprocess.
    let target = format!("127.0.0.1:{}", args.firehose_port);
    let mut loadgen_child = Command::new(&args.taktool_bin)
        .args([
            "loadgen",
            "--target",
            &target,
            "-c",
            &args.conns.to_string(),
            "-r",
            &args.rate.to_string(),
            "-d",
            // Loadgen's own duration is the soak duration plus a
            // small head-room so it doesn't tear down before the
            // sampler reads the final metrics tick.
            &(args.duration_secs + 5).to_string(),
            "-m",
            "realistic",
        ])
        // Pipe loadgen output to a side log so soak debugging
        // works the same way as for tak-server.
        .stdout(
            std::fs::File::create("/tmp/tak-soak-loadgen.log")
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::null()),
        )
        .stderr(
            std::fs::File::create("/tmp/tak-soak-loadgen.err.log")
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::null()),
        )
        .spawn()
        .context("spawn taktool loadgen")?;
    info!(
        conns = args.conns,
        rate = args.rate,
        "loadgen spawned (msg/s offered = {})",
        args.conns as u32 * args.rate
    );

    // Sample loop runs for the soak duration.
    let samples = sampler::run(
        server_pid,
        &metrics_url,
        Duration::from_secs(args.duration_secs),
        Duration::from_secs(args.sample_interval_secs),
    )
    .await;
    info!(
        sample_count = samples.len(),
        "sampling complete; tearing down"
    );

    // Cleanup. Best-effort kill; if these fail the testcontainer
    // teardown still runs on _pg_handle drop.
    let _ = loadgen_child.kill().await;
    let _ = loadgen_child.wait().await;
    let _ = server_child.kill().await;
    let _ = server_child.wait().await;

    // Analyze + report.
    let report = analyze::analyze(&samples, args.max_rss_drift_kb_per_min);
    analyze::print_report(&report);

    if let Some(path) = &args.out_csv {
        sampler::write_csv(path, &samples).with_context(|| format!("write {}", path.display()))?;
        info!(path = %path.display(), "csv written");
    }

    if report.failed {
        warn!("soak FAILED — see report above");
        std::process::exit(1);
    }
    info!("soak OK");
    Ok(())
}
