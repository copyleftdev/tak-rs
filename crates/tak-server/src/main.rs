//! tak-rs server binary.
//!
//! Binds two listeners:
//! - **firehose** (plain TCP, default `0.0.0.0:8088`) — TAK Protocol v1
//!   over the streaming framing (`0xBF <varint length> <payload>`).
//! - **mission API** (axum HTTP, default `0.0.0.0:8080`) — `/missions`,
//!   `/missions/:name`, `/missions/:name/subscription`,
//!   `/missions/:name/changes`.
//!
//! Both share the same [`tak_store::Store`] handle and the firehose
//! also shares its [`tak_bus::Bus`] with future M5+ wiring (mission
//! mutation handlers will publish into the same `ChangeBroker`).
//!
//! Binary exception to invariant D1: `unwrap`/`expect` and `print*`
//! are allowed here since this is the process boundary that owns
//! argv parsing and bootstrap logging.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::Parser;
use mimalloc::MiMalloc;
use tak_bus::Bus;
use tak_server::firehose;
use tak_store::Store;
use tokio::net::TcpListener;
use tracing::{info, warn};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[command(name = "tak-server", version, about = "tak-rs single-node server", long_about = None)]
struct Args {
    /// Postgres URL (must be reachable; migrations run at boot).
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Plain-TCP CoT firehose listen address.
    #[arg(long, env = "TAK_LISTEN_COT", default_value = "0.0.0.0:8088")]
    listen_cot: SocketAddr,

    /// Mission API listen address.
    #[arg(long, env = "TAK_LISTEN_API", default_value = "0.0.0.0:8080")]
    listen_api: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,tak_=debug")),
        )
        .json()
        .init();

    let args = Args::parse();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        listen_cot = %args.listen_cot,
        listen_api = %args.listen_api,
        "tak-server starting"
    );

    let store = Store::connect_and_migrate(&args.database_url)
        .await
        .with_context(|| format!("connect+migrate {}", args.database_url))?;
    info!("postgres ready, migrations applied");

    let bus = Bus::new();

    let cot_listener = TcpListener::bind(args.listen_cot)
        .await
        .with_context(|| format!("bind {}", args.listen_cot))?;
    let api_listener = TcpListener::bind(args.listen_api)
        .await
        .with_context(|| format!("bind {}", args.listen_api))?;

    let api_router = tak_mission::MissionRouter::build(store.clone());

    info!(
        cot = %args.listen_cot,
        api = %args.listen_api,
        "listeners bound"
    );

    // Both top-level listener tasks are conceptually "named" — they
    // are the two pillars holding the process up. We use raw
    // tokio::spawn since the entire process exits if either dies, so
    // the supervision discipline N3 guards (silent task disappearance)
    // does not apply.
    #[allow(clippy::disallowed_methods)]
    let firehose_handle = {
        let bus = bus.clone();
        let store = store.clone();
        tokio::spawn(async move {
            if let Err(e) = firehose::run(cot_listener, bus, store).await {
                warn!(error = ?e, "firehose loop exited");
            }
        })
    };

    #[allow(clippy::disallowed_methods)]
    let api_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(api_listener, api_router).await {
            warn!(error = ?e, "mission api exited");
        }
    });

    // Either listener exiting takes the process down — they're co-equal
    // and the operator should restart on either crash.
    tokio::select! {
        res = firehose_handle => {
            if let Err(e) = res {
                warn!(error = ?e, "firehose join error");
            }
        }
        res = api_handle => {
            if let Err(e) = res {
                warn!(error = ?e, "api join error");
            }
        }
    }

    Ok(())
}
