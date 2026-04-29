//! taktool — CLI for tak-rs.
//!
//! Subcommands today: `wire`, `loadgen`. Planned: `pub`, `sub`,
//! `replay`, `fuzz`, `proto-decode`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

mod latency;
mod loadgen;
mod loadgen_quic;

#[cfg(target_os = "linux")]
mod loadgen_uring;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "taktool", version, about = "TAK Cursor-on-Target CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Print TAK Protocol v1 wire constants for sanity check.
    Wire,

    /// Generate synthetic firehose load against any TAK server.
    Loadgen(loadgen::LoadgenArgs),

    /// Single-connection RTT probe: send PLIs, read your own
    /// frames back via the wildcard subscription, report
    /// p50/p95/p99/p99.9 dispatch latency. Run alongside loadgen
    /// during a soak to measure latency under load.
    Latency(latency::LatencyArgs),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Wire => {
            use tak_cot::framing::{MAGIC, MESH_HEADER, MULTICAST_GROUP, MULTICAST_PORT};
            println!("TAK Protocol v1 wire constants:");
            println!("  magic byte:     0x{MAGIC:02X}");
            println!("  mesh header:    {MESH_HEADER:02X?}");
            println!("  multicast:      {MULTICAST_GROUP}:{MULTICAST_PORT}");
            Ok(())
        }
        Cmd::Loadgen(args) => run_loadgen(args),
        Cmd::Latency(args) => run_latency(args),
    }
}

fn run_latency(args: latency::LatencyArgs) -> anyhow::Result<()> {
    init_loadgen_tracing();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(latency::run(args))
}

#[cfg(target_os = "linux")]
fn run_loadgen(args: loadgen::LoadgenArgs) -> anyhow::Result<()> {
    init_loadgen_tracing();
    if args.quic && args.uring {
        anyhow::bail!("--quic and --uring are mutually exclusive");
    }
    if args.uring {
        return loadgen_uring::run(args);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if args.quic {
        rt.block_on(loadgen_quic::run(args))
    } else {
        rt.block_on(loadgen::run(args))
    }
}

#[cfg(not(target_os = "linux"))]
fn run_loadgen(args: loadgen::LoadgenArgs) -> anyhow::Result<()> {
    init_loadgen_tracing();
    if args.uring {
        anyhow::bail!("--uring is Linux-only; rebuild on Linux or omit the flag");
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if args.quic {
        rt.block_on(loadgen_quic::run(args))
    } else {
        rt.block_on(loadgen::run(args))
    }
}

fn init_loadgen_tracing() {
    // Logs go to stderr so the optional --json line on stdout can be
    // captured cleanly by `scripts/bench-baseline.sh`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}
