//! Boot a `tak-server` in-process for conformance scenarios.
//!
//! The harness needs:
//! - A real Postgres+PostGIS (so persistence + mission API work).
//! - A real bus (so fan-out + drop accounting work).
//! - A real firehose listener (so the wire protocol is exercised
//!   end-to-end, not synthesized).
//!
//! [`TestServer::start`] spins up a `postgis/postgis:16-3.4`
//! testcontainer, runs migrations, binds an ephemeral firehose
//! port, and returns a handle the test can dial.

use std::sync::Arc;
use std::time::Duration;

use tak_bus::Bus;
use tak_server::firehose::{self, PersistMode};
use tak_store::Store;
use testcontainers::core::{ContainerPort, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const PG_USER: &str = "tak";
const PG_PASS: &str = "takatak";
const PG_DB: &str = "tak";

/// Errors raised while bringing up the conformance server.
#[derive(Debug, thiserror::Error)]
pub enum TestServerError {
    /// Postgres testcontainer failed to come up. Usually means
    /// Docker is not running on the host.
    #[error("postgres testcontainer: {0}")]
    Postgres(#[source] anyhow::Error),
    /// `Store::connect_and_migrate` failed.
    #[error("store connect+migrate: {0}")]
    Store(#[source] anyhow::Error),
    /// Couldn't bind the ephemeral firehose port.
    #[error("firehose bind: {0}")]
    Bind(#[source] std::io::Error),
}

/// Live test server. Drop ends the firehose loop and tears down the
/// Postgres container.
pub struct TestServer {
    /// Address the firehose listener is bound to. Hand to
    /// [`crate::AtakMockClient::connect`].
    pub firehose_addr: std::net::SocketAddr,
    /// Bus handle — caller can `subscription_stats()` on it for
    /// scenarios that assert per-sub drop behavior.
    pub bus: Arc<Bus>,
    /// Store handle — caller can run SQL against `cot_router`
    /// to verify persistence.
    pub store: Store,
    /// Keep the container alive for the duration of the test.
    _pg: ContainerAsync<GenericImage>,
    /// Firehose accept-loop handle. Dropped on `TestServer` drop;
    /// the loop tears itself down.
    _accept: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for TestServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestServer")
            .field("firehose_addr", &self.firehose_addr)
            .field("bus_subs", &self.bus.len())
            .finish_non_exhaustive()
    }
}

impl TestServer {
    /// Boot a conformance test server with default config (no
    /// replay-on-reconnect). Equivalent to `start_with(None)`.
    ///
    /// # Errors
    ///
    /// See [`TestServerError`].
    pub async fn start() -> Result<Self, TestServerError> {
        Self::start_with(None).await
    }

    /// Boot a conformance test server with an optional replay
    /// window. Pass `Some(d)` for scenarios that exercise the
    /// replay-on-reconnect path; `None` for scenarios that need
    /// the legacy "no replay" behavior (e.g. the PLI byte-identity
    /// test, which would race a phantom replay against live
    /// dispatch otherwise).
    ///
    /// # Errors
    ///
    /// See [`TestServerError`].
    pub async fn start_with(replay_window: Option<Duration>) -> Result<Self, TestServerError> {
        let (pg, url) = start_postgis().await?;
        let store = Store::connect_and_migrate(&url)
            .await
            .map_err(|e| TestServerError::Store(anyhow::Error::new(e)))?;
        let bus = Bus::new();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(TestServerError::Bind)?;
        let firehose_addr = listener.local_addr().map_err(TestServerError::Bind)?;

        let bus_for_loop = bus.clone();
        let store_for_loop = store.clone();
        #[allow(clippy::disallowed_methods)]
        let accept = tokio::spawn(async move {
            if let Err(e) = firehose::run(
                listener,
                bus_for_loop,
                store_for_loop,
                PersistMode::On,
                None,
                replay_window,
                // TestServer drops at end of scenario; the
                // listener task gets aborted via JoinHandle.
                // The CancellationToken here is unused but the
                // run() signature requires it.
                CancellationToken::new(),
            )
            .await
            {
                warn!(error = ?e, "conformance: firehose loop exited");
            }
        });

        Ok(Self {
            firehose_addr,
            bus,
            store,
            _pg: pg,
            _accept: accept,
        })
    }
}

async fn start_postgis() -> Result<(ContainerAsync<GenericImage>, String), TestServerError> {
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
        .map_err(|e| TestServerError::Postgres(anyhow::Error::new(e)))?;
    let host = container
        .get_host()
        .await
        .map_err(|e| TestServerError::Postgres(anyhow::Error::new(e)))?;
    let port = container
        .get_host_port_ipv4(5432.tcp())
        .await
        .map_err(|e| TestServerError::Postgres(anyhow::Error::new(e)))?;
    let url = format!("postgres://{PG_USER}:{PG_PASS}@{host}:{port}/{PG_DB}");
    Ok((container, url))
}
