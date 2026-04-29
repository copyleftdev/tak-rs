//! `taktool latency` — single-connection round-trip timing probe.
//!
//! Each TAK server connection is both publisher AND subscriber: the
//! firehose installs a wildcard subscription per connect, so any
//! frame this client publishes also fans back to it. We exploit
//! that to measure end-to-end RTT through the dispatch path:
//!
//! 1. Open one TCP connection to the firehose.
//! 2. Writer task: every `1/--rate` second, send the canonical PLI
//!    fixture; record `Instant::now()` into a queue.
//! 3. Reader task: each frame received, pop the front of the queue
//!    and compute `Instant::now() - send_at = RTT`. Push to a Vec.
//! 4. After `--duration`, sort the RTT samples and report
//!    p50 / p95 / p99 / p99.9 / max.
//!
//! TCP is FIFO; with rate ≤ a few hundred Hz, the in-flight queue
//! depth stays small (the order-of-receive matches order-of-send).
//! The probe is intended for `--rate 5..50`; the soak runs it
//! alongside the real loadgen to answer "is dispatch latency
//! stable under load."
//!
//! The fixture frame's `send_time` is left unchanged — that's a
//! u64 ms-since-epoch the upstream Java server expects on
//! ingest, and re-encoding the proto on every send would add
//! microseconds the histogram doesn't need (we measure wall-
//! clock RTT, not in-protocol latency). The send-time queue
//! tracks our own monotonic clock instead.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use clap::Args;
use prost::Message;
use tak_cot::framing;
use tak_cot::proto::view_to_takmessage;
use tak_cot::xml::decode_xml;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::info;

const FIXTURE_PLI: &str = include_str!("../../tak-cot/tests/fixtures/01_pli.xml");

#[derive(Args, Debug, Clone)]
pub(crate) struct LatencyArgs {
    /// `host:port` to dial.
    #[arg(long, default_value = "127.0.0.1:8088")]
    pub target: String,

    /// Frames per second to send. Keep low (5-50) so the in-flight
    /// queue depth stays under the per-sub mpsc capacity (otherwise
    /// the server's bus dispatch will start dropping our frames as
    /// `dropped_full` and the probe under-counts).
    #[arg(short, long, default_value_t = 10)]
    pub rate: u32,

    /// How long to run before reporting (seconds).
    #[arg(short, long, default_value_t = 30)]
    pub duration: u64,

    /// Emit one JSON line summary on stdout in addition to the
    /// human-readable text. Same shape pattern as `taktool loadgen
    /// --json`.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

pub(crate) async fn run(args: LatencyArgs) -> Result<()> {
    let frame = bake_pli_frame()?;
    info!(target = %args.target, rate = args.rate, duration = args.duration, "latency probe starting");

    let sock = TcpStream::connect(&args.target)
        .await
        .with_context(|| format!("connect {}", args.target))?;
    sock.set_nodelay(true).ok();
    let (read, mut write) = sock.into_split();

    // Shared state between writer and reader: a FIFO of
    // (send_at) timestamps. Mutex because tokio::io::split makes
    // halves Send + 'static but the queue itself isn't atomic;
    // contention is trivial at probe rates.
    let in_flight: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::with_capacity(
        usize::try_from(args.rate * 2).unwrap_or(64),
    )));
    let rtts_us: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::with_capacity(
        usize::try_from(u64::from(args.rate) * args.duration).unwrap_or(1024),
    )));

    let deadline = Instant::now() + Duration::from_secs(args.duration);
    let period = Duration::from_secs_f64(1.0 / f64::from(args.rate.max(1)));

    // Writer task.
    let in_flight_w = in_flight.clone();
    let frame_w = frame.clone();
    #[allow(clippy::disallowed_methods)]
    let writer = tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        let mut sends = 0u64;
        loop {
            tick.tick().await;
            if Instant::now() >= deadline {
                break;
            }
            // Push send_at BEFORE the write so the corresponding
            // receive (which can land before write_all returns
            // on a fast loopback) doesn't pop an empty queue.
            in_flight_w.lock().await.push_back(Instant::now());
            if let Err(e) = write.write_all(&frame_w).await {
                tracing::warn!(error = ?e, "writer: write_all failed");
                break;
            }
            sends += 1;
        }
        sends
    });

    // Reader task. Drains framed bytes from the socket, pops
    // in_flight front per frame, computes RTT.
    let in_flight_r = in_flight.clone();
    let rtts_r = rtts_us.clone();
    #[allow(clippy::disallowed_methods)]
    let reader = tokio::spawn(async move {
        let mut buf = BytesMut::with_capacity(8192);
        let mut read = read;
        let mut recvs = 0u64;
        loop {
            // Bounded read budget: tokio::time::timeout breaks
            // the read so we exit on deadline even if nothing's
            // arriving.
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let n = match tokio::time::timeout(remaining, read.read_buf(&mut buf)).await {
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break,
                Err(_) => break, // deadline
            };
            if n == 0 {
                break;
            }
            // Drain as many complete frames as the buffer holds.
            while let Ok((total, _)) = framing::decode_stream(&buf[..]) {
                let _frame: Bytes = buf.split_to(total).freeze();
                let rtt_us = match in_flight_r.lock().await.pop_front() {
                    Some(send_at) => {
                        let rtt = Instant::now().saturating_duration_since(send_at);
                        u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX)
                    }
                    None => continue, // stray frame; not one of ours
                };
                rtts_r.lock().await.push(rtt_us);
                recvs += 1;
            }
        }
        recvs
    });

    // Drain.
    let sends = writer.await.unwrap_or(0);
    let recvs = reader.await.unwrap_or(0);

    let mut rtts = std::mem::take(&mut *rtts_us.lock().await);
    rtts.sort_unstable();

    let p = |q: f64| -> u64 {
        if rtts.is_empty() {
            return 0;
        }
        // q = 0.50 => index = floor(0.50 * (N-1))
        let n = rtts.len();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = (q * (n as f64 - 1.0)).round() as usize;
        rtts[idx.min(n - 1)]
    };

    let p50 = p(0.50);
    let p95 = p(0.95);
    let p99 = p(0.99);
    let p999 = p(0.999);
    let pmax = rtts.last().copied().unwrap_or(0);

    println!();
    println!("=== latency probe ===");
    println!("target          {}", args.target);
    println!("rate            {} Hz", args.rate);
    println!("duration        {} s", args.duration);
    println!("sends           {sends}");
    println!("recvs           {recvs}");
    println!("samples         {}", rtts.len());
    println!("p50  (us)       {p50}");
    println!("p95  (us)       {p95}");
    println!("p99  (us)       {p99}");
    println!("p999 (us)       {p999}");
    println!("max  (us)       {pmax}");
    println!("=====================");

    if args.json {
        // Single line so a soak harness can grep + parse.
        println!(
            "{{\"target\":\"{}\",\"rate\":{},\"duration\":{},\"sends\":{sends},\"recvs\":{recvs},\"samples\":{},\"p50_us\":{p50},\"p95_us\":{p95},\"p99_us\":{p99},\"p999_us\":{p999},\"max_us\":{pmax}}}",
            args.target,
            args.rate,
            args.duration,
            rtts.len()
        );
    }

    Ok(())
}

fn bake_pli_frame() -> Result<Bytes> {
    let view = decode_xml(FIXTURE_PLI).context("fixture decode")?;
    let msg = view_to_takmessage(&view).context("fixture proto")?;
    let proto_bytes = msg.encode_to_vec();
    let mut framed = Vec::with_capacity(proto_bytes.len() + 8);
    framing::encode_stream(&proto_bytes, &mut std::io::Cursor::new(&mut framed))
        .context("fixture frame")?;
    Ok(Bytes::from(framed))
}
