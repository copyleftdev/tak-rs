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
use tak_server::firehose::{self, PersistMode};
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

    /// Prometheus metrics scrape endpoint. Exposes
    /// `GET /metrics` with the standard text format. Counters
    /// emitted by `tak-bus`, `tak-store`, `tak-net::auth`, and the
    /// pipeline accumulate here. Empty string disables the
    /// exporter.
    #[arg(long, env = "TAK_LISTEN_METRICS", default_value = "0.0.0.0:9091")]
    listen_metrics: String,

    /// Skip the persistence side-channel for every CoT event.
    /// Used for apples-to-apples bus dispatch benchmarks against an
    /// upstream Java server with persistence disabled or off-box.
    #[arg(long, env = "TAK_NO_PERSIST", default_value_t = false)]
    no_persist: bool,

    /// Run the firehose on a multi-threaded compio (io_uring)
    /// runtime instead of the default tokio reactor. Linux-only.
    /// The mission API stays on tokio either way.
    #[arg(long, env = "TAK_COMPIO", default_value_t = false)]
    compio: bool,

    /// Number of compio worker threads. Each owns one io_uring
    /// instance and binds the firehose port with `SO_REUSEPORT`.
    /// Ignored unless `--compio` is set.
    #[arg(long, env = "TAK_COMPIO_THREADS", default_value_t = 4)]
    compio_threads: usize,

    /// Also bind a QUIC firehose listener (UDP, TLS 1.3, ALPN
    /// `tak-firehose/1`). Independent of --compio; both can run.
    #[arg(long, env = "TAK_QUIC", default_value_t = false)]
    quic: bool,

    /// QUIC listen address (UDP). Default :8090, one above the TCP
    /// firehose. Ignored unless `--quic` is set.
    #[arg(long, env = "TAK_LISTEN_QUIC", default_value = "0.0.0.0:8090")]
    listen_quic: SocketAddr,

    /// PEM cert chain for the QUIC TLS handshake. If unset, a
    /// self-signed cert is generated at startup (bench / dev only).
    #[arg(long, env = "TAK_QUIC_CERT")]
    quic_cert: Option<std::path::PathBuf>,

    /// PEM private key for the QUIC TLS handshake.
    #[arg(long, env = "TAK_QUIC_KEY")]
    quic_key: Option<std::path::PathBuf>,

    /// Directory of `*.wasm` plugin components to load at startup
    /// (per `docs/decisions/0004-wasm-plugins.md`). Plugins run on
    /// a separate worker pool, off the H1 hot path; their queues
    /// drop on full instead of stalling dispatch. Unset = no
    /// plugins.
    #[arg(long, env = "TAK_PLUGIN_DIR")]
    plugin_dir: Option<std::path::PathBuf>,
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
        no_persist = args.no_persist,
        "tak-server starting"
    );
    let persist = if args.no_persist {
        PersistMode::Off
    } else {
        PersistMode::On
    };

    // Install the Prometheus exporter early so any metric emitted
    // by Store::connect_and_migrate() is captured. Empty
    // --listen-metrics disables the exporter entirely; the
    // metrics::counter!() calls scattered through the codebase
    // become silent no-ops. Failure to bind is logged but
    // non-fatal — the server still serves data, you just can't
    // scrape it.
    if !args.listen_metrics.is_empty() {
        match args.listen_metrics.parse::<SocketAddr>() {
            Ok(addr) => match metrics_exporter_prometheus::PrometheusBuilder::new()
                .with_http_listener(addr)
                .install()
            {
                Ok(()) => info!(addr = %addr, "metrics: prometheus exporter listening"),
                Err(e) => {
                    warn!(error = ?e, addr = %addr, "metrics: exporter install failed; continuing")
                }
            },
            Err(e) => {
                warn!(error = ?e, value = %args.listen_metrics, "metrics: --listen-metrics parse failed; metrics disabled")
            }
        }
    } else {
        info!("metrics: --listen-metrics empty; exporter disabled");
    }

    let store = Store::connect_and_migrate(&args.database_url)
        .await
        .with_context(|| format!("connect+migrate {}", args.database_url))?;
    info!("postgres ready, migrations applied");

    let bus = Bus::new();

    // Optional wasm plugin host. Loaded once at startup; hot-reload
    // is future work. Failure to load any single plugin is logged
    // but doesn't abort startup.
    //
    // The outbound receiver is taken before wrapping the host in
    // an Arc — `take_outbound` needs `&mut`, and Arc-wrapped values
    // can't be mutated. The receiver is then handed to the plugin
    // replay drainer (`firehose::run_plugin_replay`), which feeds
    // `Action::Replace` frames back through the pipeline.
    let (plugin_host, plugin_outbound): (
        Option<std::sync::Arc<tak_plugin_host::PluginHost>>,
        Option<tokio::sync::mpsc::Receiver<bytes::Bytes>>,
    ) = if let Some(dir) = args.plugin_dir.clone() {
        let cfg = tak_plugin_host::PluginHostConfig {
            plugin_dir: dir,
            ..Default::default()
        };
        match tak_plugin_host::PluginHost::new(cfg).await {
            Ok(mut host) => {
                let outbound = host.take_outbound();
                info!(loaded = host.len(), "plugin host ready");
                (Some(std::sync::Arc::new(host)), outbound)
            }
            Err(e) => {
                warn!(error = ?e, "plugin host failed to start; continuing without plugins");
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    let api_listener = TcpListener::bind(args.listen_api)
        .await
        .with_context(|| format!("bind {}", args.listen_api))?;

    let api_router = tak_mission::MissionRouter::build(store.clone());

    info!(
        cot = %args.listen_cot,
        api = %args.listen_api,
        compio = args.compio,
        compio_threads = if args.compio { args.compio_threads } else { 0 },
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
        let cot_addr = args.listen_cot;
        let use_compio = args.compio;
        let compio_threads = args.compio_threads;
        let plugin_host_for_firehose = plugin_host.clone();
        if use_compio {
            // compio's blocking runtime needs a blocking thread of
            // its own — spawn_blocking parks on tokio's pool.
            // Plugin host wiring on the compio path is future work;
            // for now plugins only run when --compio is off.
            tokio::task::spawn(async move {
                let res = tokio::task::spawn_blocking(move || {
                    firehose_compio_run(cot_addr, bus, store, compio_threads, persist)
                })
                .await;
                match res {
                    Ok(Err(e)) => warn!(error = ?e, "firehose-compio loop exited"),
                    Err(e) => warn!(error = ?e, "firehose-compio panic"),
                    Ok(Ok(())) => {}
                }
            })
        } else {
            tokio::spawn(async move {
                let cot_listener = match TcpListener::bind(cot_addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        warn!(error = ?e, "firehose bind failed");
                        return;
                    }
                };
                if let Err(e) =
                    firehose::run(cot_listener, bus, store, persist, plugin_host_for_firehose).await
                {
                    warn!(error = ?e, "firehose loop exited");
                }
            })
        }
    };

    #[allow(clippy::disallowed_methods)]
    let api_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(api_listener, api_router).await {
            warn!(error = ?e, "mission api exited");
        }
    });

    // Plugin replay drainer: feeds `Action::Replace` frames from
    // the wasm worker pool back through the dispatch pipeline. The
    // task lives for the whole process lifetime; it exits only
    // when the plugin host drops, which only happens at shutdown.
    if let Some(rx) = plugin_outbound {
        let bus = bus.clone();
        let store = store.clone();
        #[allow(clippy::disallowed_methods)]
        tokio::spawn(firehose::run_plugin_replay(rx, bus, store, persist));
    }

    // Optional QUIC firehose listener. Bound to its own UDP port
    // alongside the TCP firehose; both can be live at once.
    let quic_handle = if args.quic {
        use tak_server::firehose_quic::{self, CertSource};
        let cert_source = match (args.quic_cert.clone(), args.quic_key.clone()) {
            (Some(cert), Some(key)) => CertSource::PemFiles { cert, key },
            (None, None) => CertSource::SelfSigned,
            _ => {
                warn!(
                    "--quic-cert and --quic-key must be supplied together; falling back to self-signed"
                );
                CertSource::SelfSigned
            }
        };
        let bus = bus.clone();
        let store = store.clone();
        let quic_addr = args.listen_quic;
        info!(addr = %quic_addr, "firehose-quic: bound");
        #[allow(clippy::disallowed_methods)]
        Some(tokio::spawn(async move {
            if let Err(e) = firehose_quic::run(quic_addr, bus, store, persist, cert_source).await {
                warn!(error = ?e, "firehose-quic loop exited");
            }
        }))
    } else {
        None
    };

    // Either listener exiting takes the process down — they're co-equal
    // and the operator should restart on either crash.
    if let Some(quic) = quic_handle {
        tokio::select! {
            res = firehose_handle => log_join("firehose", res),
            res = api_handle => log_join("api", res),
            res = quic => log_join("firehose-quic", res),
        }
    } else {
        tokio::select! {
            res = firehose_handle => log_join("firehose", res),
            res = api_handle => log_join("api", res),
        }
    }

    Ok(())
}

fn log_join(name: &str, res: Result<(), tokio::task::JoinError>) {
    if let Err(e) = res {
        warn!(name, error = ?e, "join error");
    }
}

/// Linux-only shim for the compio firehose runtime. On non-Linux
/// targets this returns a fail-loud error so the binary still builds
/// (the `--compio` flag itself is universally accepted; only its
/// activation is gated).
#[cfg(target_os = "linux")]
fn firehose_compio_run(
    addr: SocketAddr,
    bus: std::sync::Arc<Bus>,
    store: Store,
    threads: usize,
    persist: PersistMode,
) -> Result<()> {
    tak_server::firehose_compio::run(addr, bus, store, threads, persist)
}

#[cfg(not(target_os = "linux"))]
fn firehose_compio_run(
    _addr: SocketAddr,
    _bus: std::sync::Arc<Bus>,
    _store: Store,
    _threads: usize,
    _persist: PersistMode,
) -> Result<()> {
    anyhow::bail!("--compio is Linux-only; rebuild on Linux or omit the flag")
}
