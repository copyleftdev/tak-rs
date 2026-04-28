//! Subprocess + testcontainer orchestration for the soak harness.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use testcontainers::core::{ContainerPort, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::process::{Child, Command};

const PG_USER: &str = "tak";
const PG_PASS: &str = "takatak";
const PG_DB: &str = "tak";

pub async fn start_postgis() -> Result<(ContainerAsync<GenericImage>, String)> {
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
        .context("postgis container start")?;
    let host = container.get_host().await.context("container host")?;
    let port = container
        .get_host_port_ipv4(5432.tcp())
        .await
        .context("host port mapping")?;
    let url = format!("postgres://{PG_USER}:{PG_PASS}@{host}:{port}/{PG_DB}");
    Ok((container, url))
}

pub async fn start_tak_server(
    bin: &Path,
    db_url: &str,
    firehose_port: u16,
    metrics_port: u16,
) -> Result<Child> {
    let listen_cot = format!("127.0.0.1:{firehose_port}");
    let listen_metrics = format!("127.0.0.1:{metrics_port}");
    Command::new(bin)
        .args([
            "--database-url",
            db_url,
            "--listen-cot",
            &listen_cot,
            // The Mission API is unused by the soak; bind it to a
            // distinct ephemeral-ish port so we don't collide with
            // anything else on the host.
            "--listen-api",
            "127.0.0.1:0",
            "--listen-metrics",
            &listen_metrics,
            // Replay window 0 so reconnecting test clients don't
            // see persistent backlog (which would skew throughput
            // measurement).
            "--replay-window-secs",
            "0",
        ])
        // Pipe stdout/stderr to a side log so soak failures are
        // debuggable. Operators can override RUST_LOG to crank
        // verbosity. The path is fixed so it shows up where
        // operators look first.
        .stdout(
            std::fs::File::create("/tmp/tak-soak-server.log")
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::null()),
        )
        .stderr(
            std::fs::File::create("/tmp/tak-soak-server.err.log")
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::null()),
        )
        .spawn()
        .context("spawn tak-server")
}

/// Wait for tak-server to be FULLY ready: both the metrics
/// endpoint AND the firehose TCP port accepting.
///
/// tak-server's startup order is metrics-first → Postgres migrate
/// → firehose bind. If we wait only on /metrics we hit a race
/// where loadgen connects before the firehose listener is up
/// and every connection fails instantly. Probing both gates that.
pub async fn wait_for_ready(
    metrics_url: &str,
    firehose_addr: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut attempt = 0u32;
    loop {
        if Instant::now() > deadline {
            anyhow::bail!("tak-server didn't become ready within {timeout:?}");
        }
        attempt += 1;

        let metrics_ok = read_url_text(metrics_url, Duration::from_secs(1))
            .await
            .is_ok();
        let firehose_ok = matches!(
            tokio::time::timeout(
                Duration::from_secs(1),
                tokio::net::TcpStream::connect(firehose_addr),
            )
            .await,
            Ok(Ok(_))
        );

        if metrics_ok && firehose_ok {
            return Ok(());
        }

        if attempt >= 120 {
            anyhow::bail!("ready probe timeout: metrics_ok={metrics_ok} firehose_ok={firehose_ok}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Tiny GET that doesn't pull a full HTTP client crate. Hits
/// 127.0.0.1, parses status line + body. Sufficient for prom
/// scraping; fails loudly on anything fancy (chunked, redirects,
/// etc.) that the prom exporter doesn't produce.
pub async fn read_url_text(url: &str, timeout: Duration) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let parsed = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("only http:// supported, got {url}"))?;
    let (host_port, path) = match parsed.find('/') {
        Some(i) => (&parsed[..i], &parsed[i..]),
        None => (parsed, "/"),
    };
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");

    let connect = tokio::net::TcpStream::connect(host_port);
    let mut stream = tokio::time::timeout(timeout, connect)
        .await
        .map_err(|_| anyhow::anyhow!("tcp connect timeout"))??;
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::with_capacity(8192);
    tokio::time::timeout(timeout, stream.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("read timeout"))??;

    // Body starts after the first \r\n\r\n.
    let raw = String::from_utf8_lossy(&buf).into_owned();
    let body_start = raw
        .find("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("no http header terminator"))?
        + 4;
    Ok(raw[body_start..].to_owned())
}
