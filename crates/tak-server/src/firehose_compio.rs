//! Multi-threaded io_uring firehose, on top of `compio`.
//!
//! Linux-only. Same semantic shape as [`crate::firehose`] (TCP accept
//! → per-connection read/write loops feeding the bus) but the
//! per-connection task runs on a `compio` thread-per-core runtime
//! with one `io_uring` instance per worker.
//!
//! # Architecture
//!
//! Compio's `TcpStream` is `!Send` by design — connections registered
//! with one io_uring instance cannot be moved to another. That rules
//! out the "main thread accepts, dispatch to worker" pattern. We use
//! the canonical thread-per-core listener pattern instead:
//!
//! - Spawn N OS threads (one per worker).
//! - Each thread builds its own compio runtime and binds the listen
//!   socket with `SO_REUSEPORT`. The kernel load-balances incoming
//!   connections across the N sockets — no userspace handoff.
//! - Connections accepted on worker `i` live on worker `i`'s runtime
//!   for their entire lifetime. Read + write halves are spawned as
//!   separate tasks on the same runtime.
//!
//! # Cross-runtime channels
//!
//! Subscriber writer tasks receive `Bytes` from `tak-bus` via
//! `tokio::sync::mpsc::Receiver`. The receiver future is poll-only —
//! it does NOT require a tokio runtime context, so polling it from a
//! compio task works. The sender is invoked from `Bus::dispatch` on
//! whichever worker thread publishes; `tokio::sync::mpsc::Sender::try_send`
//! is also runtime-agnostic.
//!
//! # Cross-runtime persistence bridge
//!
//! The `Store` writer task (sqlx, batched INSERTs) lives on the
//! tokio runtime — sqlx requires it. But `Store::try_insert_event`
//! is a sync call that just `try_send`s onto a `tokio::sync::mpsc`
//! — no await, no runtime context, no syscall. We invoke it
//! directly from the compio worker thread via
//! [`pipeline::dispatch_and_persist`], which crosses the
//! runtime boundary safely:
//!
//! ```text
//!   compio worker thread             tokio runtime
//!   ──────────────────────           ─────────────
//!   bus.dispatch       ┐             ┌ writer task
//!                      ├── Bytes ──→ │   recv().await
//!                      │             │   sqlx insert batch
//!   try_send(CotInsert)┘             └
//! ```
//!
//! `mpsc::Sender::try_send` is lock-free + atomic; polling a
//! `mpsc::Receiver::recv()` future from tokio uses standard
//! `Waker`s and does not care which thread/runtime woke it.
//!
//! # What's currently NOT supported
//!
//! - **mTLS.** Plain TCP only for v0.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use prost::Message;
use socket2::{Domain, Protocol, Socket, Type};
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
const LISTEN_BACKLOG: i32 = 1024;

/// Run the multi-threaded compio firehose. Spawns `threads` OS
/// threads, each running its own compio runtime + listener with
/// `SO_REUSEPORT`. Blocks the calling thread until every worker
/// thread exits (which they don't, in normal operation — the loop is
/// infinite).
#[allow(clippy::needless_pass_by_value)] // by-value matches firehose::run signature
pub fn run(
    addr: SocketAddr,
    bus: Arc<Bus>,
    store: Store,
    threads: usize,
    persist: PersistMode,
) -> Result<()> {
    if threads == 0 {
        anyhow::bail!("compio firehose: --compio-threads must be > 0");
    }
    info!(addr = %addr, threads, ?persist, "firehose-compio: spawning workers");

    let mut handles = Vec::with_capacity(threads);
    for i in 0..threads {
        let bus = bus.clone();
        let store = store.clone();
        let h = std::thread::Builder::new()
            .name(format!("compio-fh-{i}"))
            .spawn(move || {
                if let Err(e) = worker_main(i, addr, bus, store, persist) {
                    warn!(worker = i, error = ?e, "compio worker exited");
                }
            })
            .context("spawn compio worker thread")?;
        handles.push(h);
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn worker_main(
    id: usize,
    addr: SocketAddr,
    bus: Arc<Bus>,
    store: Store,
    persist: PersistMode,
) -> Result<()> {
    let runtime = compio::runtime::Runtime::new()
        .with_context(|| format!("worker {id}: build compio runtime"))?;
    runtime.block_on(async move {
        let listener = bind_reuseport(addr)
            .with_context(|| format!("worker {id}: SO_REUSEPORT bind {addr}"))?;
        info!(
            worker = id,
            addr = ?listener.local_addr().ok(),
            "firehose-compio: worker accept loop started"
        );

        let conn_id = AtomicU64::new((id as u64) << 48);
        loop {
            let (sock, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(worker = id, error = ?e, "firehose-compio: accept error");
                    continue;
                }
            };
            sock.set_nodelay(true).ok();
            let cid = conn_id.fetch_add(1, Ordering::Relaxed);
            let bus_for_task = bus.clone();
            let store_for_task = store.clone();
            // Spawn on the SAME (this worker's) runtime — TcpStream
            // is !Send and stays here for its entire lifetime.
            compio::runtime::spawn(async move {
                debug!(conn = cid, peer = %peer, "firehose-compio: accepted");
                handle_connection(cid, sock, bus_for_task, store_for_task, persist).await;
                debug!(conn = cid, "firehose-compio: closed");
            })
            .detach();
        }
    })
}

/// Build a compio `TcpListener` with `SO_REUSEPORT` so multiple
/// workers can bind the same port and let the kernel load-balance.
fn bind_reuseport(addr: SocketAddr) -> Result<TcpListener> {
    let domain = match addr {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    let std_listener: std::net::TcpListener = socket.into();
    let listener = TcpListener::from_std(std_listener)?;
    Ok(listener)
}

async fn handle_connection(
    id: u64,
    sock: TcpStream,
    bus: Arc<Bus>,
    store: Store,
    persist: PersistMode,
) {
    let filter = Filter {
        group_mask: ALL_GROUPS,
        ..Filter::default()
    };
    let (handle, rx) = bus.subscribe(filter);

    let (read_half, write_half) = sock.into_split();

    let writer = compio::runtime::spawn(write_loop(id, write_half, rx));

    if let Err(e) = read_loop(id, read_half, &bus, &store, persist).await {
        debug!(conn = id, error = ?e, "firehose-compio: reader exit");
    }

    drop(handle);
    let _ = writer.cancel().await;
}

async fn read_loop(
    id: u64,
    mut read: compio::net::OwnedReadHalf<TcpStream>,
    bus: &Arc<Bus>,
    store: &Store,
    persist: PersistMode,
) -> Result<()> {
    let mut acc = BytesMut::with_capacity(READ_SLOT_CAP * 2);
    let mut slot: Vec<u8> = Vec::with_capacity(READ_SLOT_CAP);
    let mut scratch = DispatchScratch::default();
    let mut decoded = 0u64;

    loop {
        // compio reads into the Vec's spare capacity. We pass the
        // slot by value, get it back with len = bytes read.
        slot.clear();
        let compio::BufResult(res, returned) = read.read(slot).await;
        slot = returned;

        let n = match res {
            Ok(0) => {
                debug!(conn = id, decoded, "firehose-compio: peer EOF");
                return Ok(());
            }
            Ok(n) => n,
            Err(e) => return Err(e.into()),
        };
        acc.extend_from_slice(&slot[..n]);

        while let Ok((total, _)) = framing::decode_stream(&acc[..]) {
            let framed: Bytes = acc.split_to(total).freeze();
            let proto_payload = match framing::decode_stream(&framed[..]) {
                Ok((_, p)) => p,
                Err(e) => {
                    warn!(conn = id, error = ?e, "compio: re-decode failed");
                    continue;
                }
            };
            match TakMessage::decode(proto_payload) {
                Ok(msg) => {
                    match persist {
                        PersistMode::On => {
                            // Cross-runtime: try_insert_event is sync
                            // + runtime-agnostic; the Store writer
                            // task on tokio drains the mpsc.
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
                    warn!(conn = id, error = ?e, "compio: TakMessage decode failed");
                }
            }
        }
    }
}

/// Drain the per-subscription mpsc and write each frame to the
/// socket via compio. The `Bytes` buffer is moved into `write_all`
/// (zero-copy via compio's `IoBuf for bytes::Bytes`).
async fn write_loop(
    id: u64,
    mut write: compio::net::OwnedWriteHalf<TcpStream>,
    mut rx: mpsc::Receiver<Bytes>,
) {
    let mut sent = 0u64;
    while let Some(b) = rx.recv().await {
        let compio::BufResult(res, _b) = write.write_all(b).await;
        if let Err(e) = res {
            debug!(conn = id, sent, error = ?e, "firehose-compio: writer exit");
            return;
        }
        sent += 1;
    }
    debug!(conn = id, sent, "firehose-compio: writer drained");
}
