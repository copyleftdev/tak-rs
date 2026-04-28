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
use tak_proto::v1::TakMessage;
use tak_store::Store;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::pipeline;

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
///   them into [`pipeline::dispatch_and_persist`].
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
pub async fn run(listener: TcpListener, bus: Arc<Bus>, store: Store) -> Result<()> {
    let connection_id = Arc::new(AtomicU64::new(0));
    info!(addr = ?listener.local_addr().ok(), "firehose: accept loop started");

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
        // Per-connection task. Discipline: lib-side enforces the N3
        // named-spawn rule via clippy::disallowed_methods, so we
        // suppress at the call site.
        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            debug!(conn = id, peer = %peer, "firehose: accepted");
            handle_connection(id, sock, bus, store).await;
            debug!(conn = id, "firehose: closed");
        });
    }
}

async fn handle_connection(id: u64, sock: TcpStream, bus: Arc<Bus>, store: Store) {
    let (read, write) = sock.into_split();

    let filter = Filter {
        group_mask: ALL_GROUPS,
        ..Filter::default()
    };
    let (handle, rx) = bus.subscribe(filter);

    #[allow(clippy::disallowed_methods)]
    let writer = tokio::spawn(write_loop(id, write, rx));

    if let Err(e) = read_loop(id, read, &bus, &store).await {
        debug!(conn = id, error = ?e, "firehose: reader exit");
    }

    drop(handle);
    writer.abort();
}

/// Read framed CoT from the socket, decode, dispatch, persist.
async fn read_loop(
    id: u64,
    mut read: tokio::net::tcp::OwnedReadHalf,
    bus: &Arc<Bus>,
    store: &Store,
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
        while let Ok((consumed, payload)) = framing::decode_stream(&buf[..]) {
            // Keep both views: the framed slice (what we re-broadcast,
            // since subscribers are TAK clients that expect framing on
            // the wire) and the inner protobuf (what we decode for
            // dispatch metadata).
            let framed_len = consumed;
            let proto_payload = payload;

            match TakMessage::decode(proto_payload) {
                Ok(msg) => {
                    let framed_bytes = Bytes::copy_from_slice(&buf[..framed_len]);
                    let _ = pipeline::dispatch_and_persist(
                        bus,
                        store,
                        &msg,
                        ALL_GROUPS,
                        framed_bytes,
                        &mut scratch,
                    );
                    decoded += 1;
                }
                Err(e) => {
                    warn!(
                        conn = id,
                        error = ?e,
                        "firehose: TakMessage decode failed; advancing past frame"
                    );
                }
            }

            // Drop the consumed bytes from the front of the buffer.
            // BytesMut::advance is the in-place cursor move.
            let _ = buf.split_to(framed_len);
        }
    }
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
