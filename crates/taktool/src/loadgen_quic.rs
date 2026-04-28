//! QUIC backend for `taktool loadgen --quic`.
//!
//! Targets a `tak-server --quic` listener (`quinn` over rustls,
//! ALPN `tak-firehose/1`). For the bench we accept a self-signed
//! server cert (the server uses `rcgen` at startup unless an
//! operator pins a real chain), so the client TLS verifier is
//! "trust everything".
//!
//! Wire shape per connection:
//!   1. quinn::Endpoint::connect (TLS 1.3, ALPN match)
//!   2. open_bi() → (send, recv) streams
//!   3. write framed TakMessage bytes on `send` at the configured
//!      rate, picking from the same 5-fixture corpus + mix table
//!      as the tokio loadgen
//!   4. recv stream is left to drain server output (we don't read
//!      it for bench purposes; readers would tip the server's
//!      back-pressure path)

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use quinn::{ClientConfig, Endpoint};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::time::interval;
use tracing::{info, warn};

use crate::loadgen::{Class, LoadgenArgs, Stats, bake_corpus, emit_json_summary};

const ALPN_TAK_FIREHOSE: &[u8] = b"tak-firehose/1";

/// Trust-everything TLS verifier. Bench-only; production deployments
/// pin a real CA chain.
#[derive(Debug)]
struct InsecureCertVerifier;

impl ServerCertVerifier for InsecureCertVerifier {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
        ]
    }
}

fn build_client_config() -> Result<ClientConfig> {
    // Install the rustls aws_lc_rs provider as the default for this
    // process. Idempotent — first call wins, subsequent calls return
    // an Err that we ignore.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let mut tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(InsecureCertVerifier))
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![ALPN_TAK_FIREHOSE.to_vec()];

    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
        .context("quinn: wrap rustls client config")?;
    Ok(ClientConfig::new(Arc::new(crypto)))
}

/// Entry point for `taktool loadgen --quic`. Runs on the same
/// tokio runtime as the default tokio loadgen.
pub(crate) async fn run(args: LoadgenArgs) -> Result<()> {
    if args.connections == 0 {
        bail!("--connections must be > 0");
    }
    if args.rate == 0 {
        bail!("--rate must be > 0");
    }

    let target: SocketAddr = args
        .target
        .parse()
        .with_context(|| format!("parse target {}", &args.target))?;

    let corpus = bake_corpus()?;
    info!(
        pli_bytes = corpus.pli.len(),
        chat_bytes = corpus.chat.len(),
        geofence_bytes = corpus.detail[0].len(),
        route_bytes = corpus.detail[1].len(),
        drawing_bytes = corpus.detail[2].len(),
        "fixtures baked (quic)"
    );

    let pli_buf = Arc::new(corpus.pli);
    let chat_buf = Arc::new(corpus.chat);
    let detail_bufs: Arc<[Vec<u8>; 3]> = Arc::new(corpus.detail);

    let table = args.mix.lookup_table();
    let stats = Arc::new(Stats::default());
    let started = Instant::now();
    let deadline = if args.duration == 0 {
        None
    } else {
        Some(started + Duration::from_secs(args.duration))
    };

    info!(
        target = %args.target,
        connections = args.connections,
        rate = args.rate,
        duration = args.duration,
        mix = ?args.mix,
        driver = "quic",
        "loadgen starting"
    );

    let client_config = build_client_config()?;

    let mut handles = Vec::with_capacity(args.connections);
    for id in 0..args.connections {
        let stats = stats.clone();
        let pli = pli_buf.clone();
        let chat = chat_buf.clone();
        let detail = detail_bufs.clone();
        let cc = client_config.clone();
        #[allow(clippy::disallowed_methods)]
        let h = tokio::spawn(async move {
            if let Err(e) = drive_connection_quic(
                id, target, args.rate, deadline, table, pli, chat, detail, stats, cc,
            )
            .await
            {
                warn!(conn = id, error = ?e, "quic conn driver exited");
            }
        });
        handles.push(h);
    }

    let report_stats = stats.clone();
    #[allow(clippy::disallowed_methods)]
    let reporter = tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(1));
        let mut prev_sent = 0u64;
        let mut prev_bytes = 0u64;
        loop {
            tick.tick().await;
            let elapsed = started.elapsed().as_secs();
            let sent = report_stats.sent_total.load(Ordering::Relaxed);
            let bytes = report_stats.bytes_total.load(Ordering::Relaxed);
            info!(
                t_s = elapsed,
                sent_delta = sent - prev_sent,
                bytes_delta = bytes - prev_bytes,
                sent_total = sent,
                pli = report_stats.sent_pli.load(Ordering::Relaxed),
                chat = report_stats.sent_chat.load(Ordering::Relaxed),
                detail = report_stats.sent_detail.load(Ordering::Relaxed),
                errs = report_stats.write_errors.load(Ordering::Relaxed),
                driver = "quic",
                "loadgen tick"
            );
            prev_sent = sent;
            prev_bytes = bytes;
            if let Some(d) = deadline
                && Instant::now() >= d
            {
                return;
            }
        }
    });

    for h in handles {
        let _ = h.await;
    }
    reporter.abort();

    if args.json {
        emit_json_summary(&args, &stats, started.elapsed());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn drive_connection_quic(
    id: usize,
    target: SocketAddr,
    rate: u32,
    deadline: Option<Instant>,
    table: [Class; 100],
    pli: Arc<Vec<u8>>,
    chat: Arc<Vec<u8>>,
    detail: Arc<[Vec<u8>; 3]>,
    stats: Arc<Stats>,
    client_config: ClientConfig,
) -> Result<()> {
    // Each loadgen "connection" gets its own QUIC client endpoint
    // bound to a fresh ephemeral UDP port. quinn::Endpoint maps 1:1
    // to a UDP socket; reusing one across connections would
    // multiplex them on a single socket which is fine but harder to
    // reason about under load.
    let mut endpoint =
        Endpoint::client("0.0.0.0:0".parse().unwrap()).context("quic: client endpoint")?;
    endpoint.set_default_client_config(client_config);

    let conn = endpoint
        .connect(target, "tak-firehose-bench")
        .with_context(|| format!("conn {id}: quic connect {target}"))?
        .await
        .with_context(|| format!("conn {id}: quic handshake"))?;

    let (mut send, _recv) = conn
        .open_bi()
        .await
        .with_context(|| format!("conn {id}: open_bi"))?;

    let period = Duration::from_secs_f64(1.0 / f64::from(rate.max(1)));
    let mut tick = interval(period);
    let mut counter: u64 = id as u64;

    loop {
        tick.tick().await;
        if let Some(d) = deadline
            && Instant::now() >= d
        {
            let _ = send.finish();
            conn.close(0u32.into(), b"done");
            endpoint.wait_idle().await;
            return Ok(());
        }

        let class = table[usize::try_from(counter % 100).unwrap_or(0)];
        let bytes: &[u8] = match class {
            Class::Pli => &pli,
            Class::Chat => &chat,
            Class::Detail => {
                let cycle = usize::try_from(counter / 100).unwrap_or(0) % 3;
                &detail[cycle]
            }
        };

        match send.write_all(bytes).await {
            Ok(()) => stats.record(class, bytes.len()),
            Err(e) => {
                stats.write_errors.fetch_add(1, Ordering::Relaxed);
                warn!(conn = id, error = ?e, "quic: write failed; closing connection");
                conn.close(1u32.into(), b"write-error");
                endpoint.wait_idle().await;
                return Ok(());
            }
        }
        counter = counter.wrapping_add(1);
    }
}
