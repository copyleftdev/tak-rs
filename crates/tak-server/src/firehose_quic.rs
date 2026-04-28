//! QUIC firehose listener.
//!
//! Opt-in via `tak-server --quic`. Binds a `quinn::Endpoint` on UDP
//! and accepts QUIC connections protected by TLS 1.3 (rustls). ALPN
//! identifier is `tak-firehose/1` — clients that don't advertise it
//! get rejected during handshake.
//!
//! # Wire protocol
//!
//! Per-connection: open one bidirectional stream. On that stream
//! the wire is the same framed TakMessage that `stcp` carries —
//! `0xBF <varint length> <protobuf>`. This makes the QUIC path a
//! drop-in replacement for `stcp` from the codec's perspective; only
//! the transport changes.
//!
//! Future evolution (not in this commit):
//! - **Class-of-service streams.** Open separate QUIC streams for
//!   PLI / chat / detail so a slow detail blob can't head-of-line
//!   a PLI burst on the same connection.
//! - **Datagrams for PLI.** QUIC unreliable datagrams match the
//!   semantics of UDP mesh PLI updates (lost frames are harmless —
//!   the next update supersedes them) and avoid the per-stream
//!   overhead.
//!
//! # Why QUIC for TAK
//!
//! Mobile-network reconnect storms are the #1 deployment-pain
//! ATAK admins report. QUIC's connection migration + 0-RTT
//! resume should make those storms vanish — a phone roaming from
//! Wi-Fi to LTE keeps the same QUIC connection alive across the
//! IP change without reauthing.
//!
//! # Cert provisioning
//!
//! By default `tak-server --quic` generates a self-signed RSA
//! cert at startup (via `rcgen`) so the bench harness needs no
//! extra setup. Production should pass `--quic-cert` and
//! `--quic-key` paths to a real chain.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use prost::Message;
use quinn::Endpoint;
use rustls::ServerConfig as RustlsServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tak_bus::{Bus, DispatchScratch, Filter, GroupBitvector};
use tak_cot::framing;
use tak_proto::v1::TakMessage;
use tak_store::Store;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::firehose::PersistMode;
use crate::pipeline;

const ALL_GROUPS: GroupBitvector = GroupBitvector([!0u64; 4]);
const READ_SLOT_CAP: usize = 8192;

/// ALPN protocol identifier negotiated during the QUIC TLS handshake.
/// Bumped on any wire-format change so old clients fail-loud.
pub const ALPN_TAK_FIREHOSE: &[u8] = b"tak-firehose/1";

/// Where to source the QUIC server's cert + private key.
#[derive(Debug, Clone)]
pub enum CertSource {
    /// Read PEM-encoded chain + key from disk.
    PemFiles {
        /// Cert chain (one or more X.509 certs in PEM order).
        cert: std::path::PathBuf,
        /// PKCS#8 or PKCS#1 private key in PEM.
        key: std::path::PathBuf,
    },
    /// Generate a self-signed cert at startup (bench / dev only).
    /// The CN is `tak-firehose-bench`, lifetime 90 days.
    SelfSigned,
}

/// Run the QUIC firehose. Blocks the calling task until the
/// listener errors fatally. Sharded across multiple per-connection
/// tokio tasks; quinn's runtime hooks into the same multi-thread
/// tokio runtime that owns the rest of the binary.
#[allow(clippy::needless_pass_by_value)]
pub async fn run(
    addr: SocketAddr,
    bus: Arc<Bus>,
    store: Store,
    persist: PersistMode,
    cert_source: CertSource,
) -> Result<()> {
    let server_config = build_server_config(&cert_source)?;
    let endpoint =
        Endpoint::server(server_config, addr).with_context(|| format!("quic: bind {addr}"))?;

    info!(
        addr = %endpoint.local_addr().unwrap_or(addr),
        ?persist,
        ?cert_source,
        "firehose-quic: accept loop started"
    );

    let conn_id = Arc::new(AtomicU64::new(0));
    while let Some(incoming) = endpoint.accept().await {
        let bus = bus.clone();
        let store = store.clone();
        let conn_id = conn_id.clone();

        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            let connection = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = ?e, "firehose-quic: handshake failed");
                    return;
                }
            };
            let id = conn_id.fetch_add(1, Ordering::Relaxed);
            let peer = connection.remote_address();
            debug!(conn = id, peer = %peer, "firehose-quic: accepted");
            handle_connection(id, connection, bus, store, persist).await;
            debug!(conn = id, "firehose-quic: closed");
        });
    }
    Ok(())
}

fn build_server_config(source: &CertSource) -> Result<quinn::ServerConfig> {
    // rustls 0.23 requires an explicit CryptoProvider install when
    // both ring and aws_lc_rs are linked. Idempotent — the first
    // call wins, subsequent ones return Err that we discard.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let (chain, key) = match source {
        CertSource::PemFiles { cert, key } => load_pem_chain_and_key(cert, key)?,
        CertSource::SelfSigned => generate_self_signed()?,
    };

    let mut tls_config = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .context("rustls: install single cert")?;
    tls_config.alpn_protocols = vec![ALPN_TAK_FIREHOSE.to_vec()];

    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)
        .context("quinn: wrap rustls config")?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(crypto)))
}

fn load_pem_chain_and_key(
    cert: &Path,
    key: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_pem = std::fs::read(cert).with_context(|| format!("read cert {cert:?}"))?;
    let key_pem = std::fs::read(key).with_context(|| format!("read key {key:?}"))?;
    let chain = rustls_pemfile_chain(&cert_pem)?;
    let key = rustls_pemfile_key(&key_pem)?;
    Ok((chain, key))
}

fn rustls_pemfile_chain(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let mut out = Vec::new();
    for item in CertificateDer::pem_slice_iter(pem) {
        out.push(item.context("pem: parse cert")?);
    }
    if out.is_empty() {
        anyhow::bail!("no PEM-encoded certificates found in chain");
    }
    Ok(out)
}

fn rustls_pemfile_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem).context("pem: parse private key")
}

fn generate_self_signed() -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    // rcgen 0.13: CertifiedKey { cert: Certificate, signing_key: KeyPair }
    let subject_alt_names = vec!["localhost".to_owned(), "tak-firehose-bench".to_owned()];
    let cert = rcgen::generate_simple_self_signed(subject_alt_names)
        .context("rcgen: self-signed cert generation")?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der())
        .map_err(|e| anyhow::anyhow!("rcgen key → PrivateKeyDer: {e}"))?;
    Ok((vec![cert_der], key_der))
}

async fn handle_connection(
    id: u64,
    connection: quinn::Connection,
    bus: Arc<Bus>,
    store: Store,
    persist: PersistMode,
) {
    // Wire shape: one bidirectional stream carrying framed
    // TakMessages in both directions. Future commits split into
    // multiple class-of-service streams.
    let (send_stream, recv_stream) = match connection.accept_bi().await {
        Ok(pair) => pair,
        Err(e) => {
            debug!(conn = id, error = ?e, "firehose-quic: accept_bi failed");
            return;
        }
    };

    let filter = Filter {
        group_mask: ALL_GROUPS,
        ..Filter::default()
    };
    let (handle, rx) = bus.subscribe(filter);

    #[allow(clippy::disallowed_methods)]
    let writer = tokio::spawn(write_loop(id, send_stream, rx));

    if let Err(e) = read_loop(id, recv_stream, &bus, &store, persist).await {
        debug!(conn = id, error = ?e, "firehose-quic: reader exit");
    }

    drop(handle);
    writer.abort();
    connection.close(0u32.into(), b"bye");
}

async fn read_loop(
    id: u64,
    mut recv: quinn::RecvStream,
    bus: &Arc<Bus>,
    store: &Store,
    persist: PersistMode,
) -> Result<()> {
    let mut acc = BytesMut::with_capacity(READ_SLOT_CAP * 2);
    let mut scratch = DispatchScratch::default();
    let mut decoded = 0u64;
    let mut chunk = vec![0u8; READ_SLOT_CAP];

    loop {
        let n = match recv.read(&mut chunk).await {
            Ok(Some(n)) => n,
            Ok(None) => {
                debug!(conn = id, decoded, "firehose-quic: peer FIN");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        acc.extend_from_slice(&chunk[..n]);

        while let Ok((total, _)) = framing::decode_stream(&acc[..]) {
            let framed: Bytes = acc.split_to(total).freeze();
            let proto_payload = match framing::decode_stream(&framed[..]) {
                Ok((_, p)) => p,
                Err(e) => {
                    warn!(conn = id, error = ?e, "quic: re-decode failed");
                    continue;
                }
            };
            match TakMessage::decode(proto_payload) {
                Ok(msg) => {
                    match persist {
                        PersistMode::On => {
                            let _ = pipeline::dispatch_and_persist(
                                bus,
                                store,
                                &msg,
                                ALL_GROUPS,
                                framed,
                                &mut scratch,
                            );
                        }
                        PersistMode::Off => {
                            let _ = pipeline::dispatch_only(
                                bus,
                                &msg,
                                ALL_GROUPS,
                                framed,
                                &mut scratch,
                            );
                        }
                    }
                    decoded += 1;
                }
                Err(e) => {
                    warn!(conn = id, error = ?e, "quic: TakMessage decode failed");
                }
            }
        }
    }
}

async fn write_loop(id: u64, mut send: quinn::SendStream, mut rx: mpsc::Receiver<Bytes>) {
    let mut sent = 0u64;
    while let Some(b) = rx.recv().await {
        if let Err(e) = send.write_all(&b).await {
            debug!(conn = id, sent, error = ?e, "firehose-quic: writer exit");
            return;
        }
        sent += 1;
    }
    let _ = send.finish();
    debug!(conn = id, sent, "firehose-quic: writer drained");
}
