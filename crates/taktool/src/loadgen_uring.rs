//! io_uring backend for `taktool loadgen --uring`.
//!
//! Linux-only. Drives the same fixture corpus + 70/20/10 mix as the
//! tokio loadgen, but each connection's I/O loop runs on a
//! `tokio-uring` current-thread runtime that submits writes through
//! the kernel's `io_uring` interface — no per-write `write(2)`
//! syscall, and the buffer is owned by the kernel for the duration
//! of the submission.
//!
//! # Buffer ownership
//!
//! `tokio_uring` requires owned buffers (`Vec<u8>`) for I/O — the
//! kernel reads directly from the buffer and returns it on
//! completion. We therefore pre-clone the five framed fixtures into
//! per-connection `Vec<u8>` slots once at startup; each iteration
//! takes a Vec, hands it to `write_all`, receives it back, and
//! stores it for re-use. **No per-message allocation.**
//!
//! # NODELAY
//!
//! `tokio_uring::net::TcpStream` does not expose a typed
//! `set_nodelay` setter. We call `setsockopt(TCP_NODELAY, 1)` via
//! `libc` on the raw fd — without it Nagle's 40ms delay swamps the
//! loadgen at 100 msg/s/conn (10ms inter-message interval).

use std::net::SocketAddr;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use socket2::SockRef;
use tokio::time::interval;
use tokio_uring::net::TcpStream;
use tracing::{info, warn};

use crate::loadgen::{Class, LoadgenArgs, Stats, bake_corpus, emit_json_summary};

/// Set `TCP_NODELAY` via socket2 over a borrowed fd. Without this
/// Nagle's 40ms delay swamps the loadgen at 100 msg/s/conn.
///
/// Reviewed by the unsafe-auditor agent on 2026-04-28; see SAFETY
/// comment for the contract that justifies the `unsafe` block.
#[allow(unsafe_code)]
fn set_nodelay_borrowed(fd: std::os::fd::RawFd) {
    // SAFETY contract for `BorrowedFd::borrow_raw`:
    //   - the fd must be open for the lifetime of the `BorrowedFd`,
    //   - the `BorrowedFd` must not outlive the resource it wraps.
    // Both hold:
    //   - the caller (`drive_connection_uring`) holds the owning
    //     `TcpStream` live across this synchronous call,
    //   - this function contains NO `.await` between `borrow_raw`
    //     and the function return, so no cancel/yield path can drop
    //     the stream and close the fd while `borrowed` is live,
    //   - `borrowed` and `sock` (a `socket2::SockRef` newtype over
    //     `BorrowedFd`) are stack-local and consumed in-frame.
    // Reviewed 2026-04-28; do NOT add an `.await` between borrow_raw
    // and the end of this function without re-validating.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let sock = SockRef::from(&borrowed);
    if let Err(e) = sock.set_nodelay(true) {
        warn!(error = ?e, "uring: set_nodelay failed; continuing");
    }
}

/// Entry point for the io_uring loadgen path. Runs the entire
/// generator on a single-thread `tokio-uring` runtime.
pub(crate) fn run(args: LoadgenArgs) -> Result<()> {
    if args.connections == 0 {
        bail!("--connections must be > 0");
    }
    if args.rate == 0 {
        bail!("--rate must be > 0");
    }

    tokio_uring::start(run_async(args))
}

async fn run_async(args: LoadgenArgs) -> Result<()> {
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
        "fixtures baked (uring)"
    );

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
        driver = "io_uring",
        "loadgen starting"
    );

    let mut handles = Vec::with_capacity(args.connections);
    for id in 0..args.connections {
        let stats = stats.clone();
        let pli = corpus.pli.clone();
        let chat = corpus.chat.clone();
        let detail0 = corpus.detail[0].clone();
        let detail1 = corpus.detail[1].clone();
        let detail2 = corpus.detail[2].clone();
        // tokio_uring::spawn schedules onto the same current-thread
        // runtime; no Send bound (the uring fd itself is !Send).
        let h = tokio_uring::spawn(async move {
            drive_connection_uring(
                id,
                target,
                args.rate,
                deadline,
                table,
                pli,
                chat,
                [detail0, detail1, detail2],
                stats,
            )
            .await
        });
        handles.push(h);
    }

    // Reporter task — same per-second cadence as the tokio path.
    let report_stats = stats.clone();
    let _reporter = tokio_uring::spawn(async move {
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
                driver = "io_uring",
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

    emit_json_summary(&args, &stats, started.elapsed());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn drive_connection_uring(
    id: usize,
    target: SocketAddr,
    rate: u32,
    deadline: Option<Instant>,
    table: [Class; 100],
    pli: Vec<u8>,
    chat: Vec<u8>,
    detail: [Vec<u8>; 3],
    stats: Arc<Stats>,
) -> Result<()> {
    let sock = TcpStream::connect(target)
        .await
        .with_context(|| format!("conn {id}: uring connect {target}"))?;
    set_nodelay_borrowed(sock.as_raw_fd());

    // Per-connection owned buffers: 5 slots (PLI, chat, geo, route,
    // drawing). io_uring requires we hand the kernel an owned Vec for
    // each write; we rotate slots so the same allocation is reused
    // across iterations.
    let mut slot_pli = pli;
    let mut slot_chat = chat;
    let [mut slot_geo, mut slot_route, mut slot_draw] = detail;

    let period = Duration::from_secs_f64(1.0 / f64::from(rate.max(1)));
    let mut tick = interval(period);
    let mut counter: u64 = id as u64;

    loop {
        tick.tick().await;
        if let Some(d) = deadline
            && Instant::now() >= d
        {
            return Ok(());
        }

        let class = table[usize::try_from(counter % 100).unwrap_or(0)];
        // Take the matching slot's Vec by value, write it, take the
        // returned Vec back. No memcpy, no per-iteration allocation.
        let (res, returned, len) = match class {
            Class::Pli => {
                let len = slot_pli.len();
                let buf = std::mem::take(&mut slot_pli);
                let (res, buf) = sock.write_all(buf).await;
                (res, buf, len)
            }
            Class::Chat => {
                let len = slot_chat.len();
                let buf = std::mem::take(&mut slot_chat);
                let (res, buf) = sock.write_all(buf).await;
                (res, buf, len)
            }
            Class::Detail => {
                let cycle = usize::try_from(counter / 100).unwrap_or(0) % 3;
                match cycle {
                    0 => {
                        let len = slot_geo.len();
                        let buf = std::mem::take(&mut slot_geo);
                        let (res, buf) = sock.write_all(buf).await;
                        (res, buf, len)
                    }
                    1 => {
                        let len = slot_route.len();
                        let buf = std::mem::take(&mut slot_route);
                        let (res, buf) = sock.write_all(buf).await;
                        (res, buf, len)
                    }
                    _ => {
                        let len = slot_draw.len();
                        let buf = std::mem::take(&mut slot_draw);
                        let (res, buf) = sock.write_all(buf).await;
                        (res, buf, len)
                    }
                }
            }
        };

        // Restore the slot regardless of write result so we don't leak
        // the allocation and stay alloc-free in the steady state.
        match class {
            Class::Pli => slot_pli = returned,
            Class::Chat => slot_chat = returned,
            Class::Detail => {
                let cycle = usize::try_from(counter / 100).unwrap_or(0) % 3;
                match cycle {
                    0 => slot_geo = returned,
                    1 => slot_route = returned,
                    _ => slot_draw = returned,
                }
            }
        }

        match res {
            Ok(()) => stats.record(class, len),
            Err(e) => {
                stats.write_errors.fetch_add(1, Ordering::Relaxed);
                warn!(conn = id, error = ?e, "uring: write failed; closing connection");
                return Ok(());
            }
        }
        counter = counter.wrapping_add(1);
    }
}
