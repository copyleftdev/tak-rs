//! tak-rs server binary.
//!
//! Scaffold only — listeners, bus, persistence, mission API are wired in
//! subsequent milestones. See `docs/architecture.md` §11 for the M0-M5 plan.
//!
//! Binary exception to invariant D1: `unwrap`/`expect` and `print*` are
//! allowed here since this is the process boundary that owns argv parsing
//! and bootstrap logging.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,tak_=debug")),
        )
        .json()
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "tak-server starting"
    );
    tracing::warn!("scaffold only; no listeners are bound yet — see docs/architecture.md §11");

    Ok(())
}
