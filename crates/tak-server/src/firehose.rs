//! Plain-TCP CoT firehose: accept loop + per-connection reader/writer.
//!
//! Drives the bus from real sockets. Each connected client is both a
//! publisher (their PLI / chat / detail blobs flow into
//! [`pipeline::dispatch_and_persist`]) and a subscriber (every message
//! the bus dispatches gets framed back onto their socket).
//!
//! # v0 simplifications
//!
//! - Plain TCP only. mTLS on port 8089 is post-M5 — TLS doesn't change
//!   the dispatch story, but cert provisioning bloats the smoke
//!   harness.
//! - Wildcard subscription per connection: every client sees every
//!   message. Group bitvectors are forced all-ones at both subscribe
//!   and dispatch time, so [`GroupBitvector::intersects`] always
//!   succeeds.
//! - No backpressure on the publisher. Reader reads as fast as the
//!   socket delivers; per-subscription mpsc is bounded (H5) and
//!   dispatch drops on a full queue, never blocks fan-out.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use prost::Message;
use tak_bus::{Bus, DispatchScratch, Filter, GroupBitvector};
use tak_cot::framing;
use tak_plugin_host::{PluginEvent, PluginHost};
use tak_proto::v1::TakMessage;
use tak_store::Store;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::pipeline;

/// Whether the firehose should write each inbound CoT event to the
/// `cot_router` table. `--no-persist` on the binary turns this off so
/// the dispatch path can be benched apples-to-apples against an
/// upstream Java server with persistence disabled or off-box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistMode {
    /// Full pipeline: dispatch + best-effort persistence.
    On,
    /// Bus dispatch only; the persistence side-channel is skipped
    /// entirely (no `CotInsert` allocations, no mpsc try_send).
    Off,
}

/// Group bitvector with every bit set — used as the v0 "no auth, no
/// filter" mask. Both the per-connection [`Filter::group_mask`] and
/// the inbound `sender_groups` use this so [`GroupBitvector::intersects`]
/// always returns true.
const ALL_GROUPS: GroupBitvector = GroupBitvector([!0u64; 4]);

/// Per-connection read buffer initial capacity. Sized for ~10 PLI
/// frames worth of fixture data.
const READ_BUF_CAPACITY: usize = 8192;

/// Run the firehose accept loop. Blocks until `listener` errors fatally.
///
/// Each accepted connection spawns two tasks:
/// - **reader** — drains framed CoT messages from the socket and feeds
///   them into [`pipeline::dispatch_and_persist`] (or
///   [`pipeline::dispatch_only`] when `persist == PersistMode::Off`).
/// - **writer** — drains the subscription mpsc and writes each
///   delivered frame back to the same socket.
///
/// On socket close the reader returns; its drop releases the
/// [`tak_bus::SubscriptionHandle`], which removes the entry from the
/// bus and lets the writer task observe a closed channel and exit.
///
/// # Errors
///
/// Returns when `TcpListener::accept` returns an unrecoverable error
/// (e.g. listener was closed). Per-connection errors are logged via
/// `tracing` but never propagated up.
pub async fn run(
    listener: TcpListener,
    bus: Arc<Bus>,
    store: Store,
    persist: PersistMode,
    plugin_host: Option<Arc<PluginHost>>,
) -> Result<()> {
    let connection_id = Arc::new(AtomicU64::new(0));
    info!(
        addr = ?listener.local_addr().ok(),
        ?persist,
        plugins = plugin_host.as_deref().map(PluginHost::len).unwrap_or(0),
        "firehose: accept loop started"
    );

    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                warn!(error = ?e, "firehose: accept error; continuing");
                continue;
            }
        };
        sock.set_nodelay(true).ok();

        let id = connection_id.fetch_add(1, Ordering::Relaxed);
        let bus = bus.clone();
        let store = store.clone();
        let plugin_host = plugin_host.clone();
        // Per-connection task. Discipline: lib-side enforces the N3
        // named-spawn rule via clippy::disallowed_methods, so we
        // suppress at the call site.
        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            debug!(conn = id, peer = %peer, "firehose: accepted");
            handle_connection(id, sock, bus, store, persist, plugin_host).await;
            debug!(conn = id, "firehose: closed");
        });
    }
}

async fn handle_connection(
    id: u64,
    sock: TcpStream,
    bus: Arc<Bus>,
    store: Store,
    persist: PersistMode,
    plugin_host: Option<Arc<PluginHost>>,
) {
    let (read, write) = sock.into_split();

    let filter = Filter {
        group_mask: ALL_GROUPS,
        ..Filter::default()
    };
    let (handle, rx) = bus.subscribe(filter);

    #[allow(clippy::disallowed_methods)]
    let writer = tokio::spawn(write_loop(id, write, rx));

    if let Err(e) = read_loop(id, read, &bus, &store, persist, plugin_host.as_deref()).await {
        debug!(conn = id, error = ?e, "firehose: reader exit");
    }

    drop(handle);
    writer.abort();
}

/// Read framed CoT from the socket, decode, dispatch, optionally persist.
///
/// **Zero-copy frame extraction:** for each complete frame in the
/// buffer we call [`framing::decode_stream`] only to learn the frame's
/// total length, then `BytesMut::split_to(total).freeze()` hands the
/// front of the buffer over to the dispatch path as a `Bytes` with no
/// memcpy. The proto payload slice is re-derived from the now-detached
/// frame and decoded in place.
async fn read_loop(
    id: u64,
    mut read: tokio::net::tcp::OwnedReadHalf,
    bus: &Arc<Bus>,
    store: &Store,
    persist: PersistMode,
    plugin_host: Option<&PluginHost>,
) -> Result<()> {
    let mut buf = BytesMut::with_capacity(READ_BUF_CAPACITY);
    let mut scratch = DispatchScratch::default();
    let mut decoded = 0u64;

    loop {
        let n = read
            .read_buf(&mut buf)
            .await
            .with_context(|| format!("conn {id}: read"))?;
        if n == 0 {
            debug!(conn = id, decoded, "firehose: peer EOF");
            return Ok(());
        }

        // Drain as many complete frames as the buffer holds. decode_stream
        // returns Err when the buffer is short; that's the loop terminator.
        while let Ok((total, _)) = framing::decode_stream(&buf[..]) {
            // Hand the frame off to the bus path with zero memcpy.
            // BytesMut::split_to + freeze ⇒ ref-counted ownership
            // transfer; the underlying allocation is shared across all
            // subscriber clones (Bytes::clone is an Arc bump, H3).
            let framed: Bytes = buf.split_to(total).freeze();

            // Re-derive the proto slice from the detached frame. The
            // borrow's lifetime ends once TakMessage::decode returns.
            let proto_payload = match framing::decode_stream(&framed[..]) {
                Ok((_, payload)) => payload,
                Err(e) => {
                    // Should not happen: we just decoded the same bytes
                    // out of `buf` successfully a moment ago.
                    warn!(conn = id, error = ?e, "firehose: re-decode of detached frame failed");
                    continue;
                }
            };

            match TakMessage::decode(proto_payload) {
                Ok(msg) => {
                    // Plugin host fan-out runs BEFORE moving `framed`
                    // into the pipeline. Bytes::clone is an Arc bump
                    // (H3); the plugin worker pool drops on full
                    // queue without back-pressuring dispatch.
                    if let Some(host) = plugin_host
                        && let Some(event) = build_plugin_event(&msg, framed.clone(), ALL_GROUPS)
                    {
                        let _ = host.publish(event);
                    }
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
                    warn!(
                        conn = id,
                        error = ?e,
                        "firehose: TakMessage decode failed; frame dropped"
                    );
                }
            }
        }
    }
}

/// Build a [`PluginEvent`] from a decoded `TakMessage`. Returns
/// `None` if the message has no `cot_event` payload (which means
/// it's a TAK control frame with no app-level CoT — plugins don't
/// see those in v0).
fn build_plugin_event(
    msg: &TakMessage,
    payload: Bytes,
    sender_groups: GroupBitvector,
) -> Option<PluginEvent> {
    let cot = msg.cot_event.as_ref()?;
    Some(PluginEvent {
        payload,
        cot_type: cot.r#type.clone(),
        uid: cot.uid.clone(),
        callsign: None, // detail-block contact extraction lands later
        lat: cot.lat,
        lon: cot.lon,
        hae: cot.hae,
        send_time_ms: cot.send_time,
        sender_groups_low: sender_groups.0[0],
    })
}

/// Drain the per-connection mpsc and write each frame to the socket.
///
/// Each item on `rx` is a complete on-wire frame produced by
/// [`Bytes::clone`] from the read path — invariant H3 (Arc bump, no
/// memcpy) all the way out to the socket.
async fn write_loop(
    id: u64,
    mut write: tokio::net::tcp::OwnedWriteHalf,
    mut rx: mpsc::Receiver<Bytes>,
) {
    let mut sent = 0u64;
    while let Some(b) = rx.recv().await {
        if let Err(e) = write.write_all(&b).await {
            debug!(conn = id, sent, error = ?e, "firehose: writer exit");
            return;
        }
        sent += 1;
    }
    debug!(conn = id, sent, "firehose: writer drained");
}
