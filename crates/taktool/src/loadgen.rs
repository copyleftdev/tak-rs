//! `taktool loadgen` — synthetic firehose load against any TAK server.
//!
//! Honors the **70 / 20 / 10** PLI / chat / detail mix locked in
//! `docs/decisions/0003-firehose-load-mix.md`. Used as the input for the
//! M5 Java-vs-Rust comparison harness.
//!
//! # Wire path
//!
//! Each fixture (`crates/tak-cot/tests/fixtures/01..05_*.xml`) is
//! parsed once at startup, converted to a `TakMessage` via
//! [`tak_cot::proto::view_to_takmessage`], prost-encoded, and framed
//! with `0xBF <varint length> <payload>` via
//! [`tak_cot::framing::encode_stream`]. Per-message work in the hot
//! loop is just `socket.write_all(&prebaked_bytes)`.
//!
//! # CLI
//!
//! ```text
//! taktool loadgen \
//!     --target 127.0.0.1:8088 \
//!     --connections 100 \
//!     --rate 5 \
//!     --duration 30 \
//!     --mix realistic
//! ```
//!
//! Output every second:
//!
//! ```text
//! [loadgen] t=  1s conns=100 sent=    500 bytes=   362_000 mix=350/100/50
//! [loadgen] t=  2s conns=100 sent=   1000 bytes=   724_000 mix=700/200/100
//! ```

use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use prost::Message;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::interval;
use tracing::{info, warn};

use tak_cot::framing;
use tak_cot::proto::view_to_takmessage;
use tak_cot::xml::decode_xml;

/// Five canonical fixtures, baked into the binary.
const FIXTURE_PLI: &str = include_str!("../../tak-cot/tests/fixtures/01_pli.xml");
const FIXTURE_CHAT: &str = include_str!("../../tak-cot/tests/fixtures/02_chat.xml");
const FIXTURE_GEOFENCE: &str = include_str!("../../tak-cot/tests/fixtures/03_geofence.xml");
const FIXTURE_ROUTE: &str = include_str!("../../tak-cot/tests/fixtures/04_route.xml");
const FIXTURE_DRAWING: &str = include_str!("../../tak-cot/tests/fixtures/05_drawing.xml");

/// Bench class — informs both fixture pool and the per-class metric counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// Position Location Information — small, periodic.
    Pli,
    /// Chat or marker — medium-sized, user-driven.
    Chat,
    /// Drawing / route / geofence — large detail blob.
    Detail,
}

/// Mix profiles. See `docs/decisions/0003-firehose-load-mix.md`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum MixProfile {
    /// 70 % PLI / 20 % chat / 10 % detail. The locked v0 default.
    Realistic,
    /// 100 % PLI — micro-bench dispatch path on smallest message.
    PliOnly,
    /// 33 / 33 / 34 — exercises every code path equally; useful for
    /// flushing out class-specific regressions before tuning for the
    /// realistic mix.
    Uniform,
}

impl MixProfile {
    /// Returns a 100-element table mapping `[0, 100)` → [`Class`]. The
    /// load generator picks a class by `counter % 100`, giving a
    /// strictly deterministic mix without needing an RNG.
    pub(crate) fn lookup_table(self) -> [Class; 100] {
        let mut table = [Class::Pli; 100];
        let (pli, chat, _detail) = match self {
            Self::Realistic => (70, 20, 10),
            Self::PliOnly => (100, 0, 0),
            Self::Uniform => (34, 33, 33),
        };
        for (i, slot) in table.iter_mut().enumerate() {
            *slot = if i < pli {
                Class::Pli
            } else if i < pli + chat {
                Class::Chat
            } else {
                Class::Detail
            };
        }
        table
    }
}

/// `taktool loadgen` arguments.
#[derive(Args, Debug)]
pub(crate) struct LoadgenArgs {
    /// `host:port` to dial. Plain TCP for now; mTLS on 8089 lands later.
    #[arg(long, default_value = "127.0.0.1:8088")]
    pub(crate) target: String,

    /// Number of concurrent connections.
    #[arg(long, short = 'c', default_value_t = 10)]
    pub(crate) connections: usize,

    /// Per-connection emit rate in messages per second.
    #[arg(long, short = 'r', default_value_t = 5)]
    pub(crate) rate: u32,

    /// How long to run before stopping (seconds). 0 = run forever.
    #[arg(long, short = 'd', default_value_t = 30)]
    pub(crate) duration: u64,

    /// Traffic mix profile.
    #[arg(long, short = 'm', default_value = "realistic", value_enum)]
    pub(crate) mix: MixProfile,

    /// On exit, print a single line of JSON to stdout summarizing the
    /// run. Consumed by `scripts/bench-baseline.sh`.
    #[arg(long)]
    pub(crate) json: bool,

    /// Tag to embed in the JSON output (e.g. "rust", "java-baseline").
    /// Lets a comparison harness merge runs from different targets.
    #[arg(long, default_value = "")]
    pub(crate) tag: String,

    /// Use the io_uring driver instead of the default tokio (epoll)
    /// driver. Linux-only — on other platforms this flag is rejected
    /// at startup. Single-threaded uring runtime.
    #[arg(long)]
    pub(crate) uring: bool,
}

/// Build the wire payload (TakMessage proto, length-prefixed) from one
/// fixture XML.
fn bake(xml: &str) -> Result<Vec<u8>> {
    let view = decode_xml(xml).context("decode fixture xml")?;
    let msg = view_to_takmessage(&view).context("convert to TakMessage")?;
    let proto_bytes = msg.encode_to_vec();
    let mut framed = Vec::with_capacity(proto_bytes.len() + 8);
    framing::encode_stream(&proto_bytes, &mut Cursor::new(&mut framed)).context("frame payload")?;
    Ok(framed)
}

/// All baked fixtures for a single loadgen run. The `detail` array
/// rotates across geofence / route / drawing inside the 10 % bucket.
pub(crate) struct Corpus {
    pub(crate) pli: Vec<u8>,
    pub(crate) chat: Vec<u8>,
    pub(crate) detail: [Vec<u8>; 3],
}

/// Pre-bake all five fixtures into wire frames.
pub(crate) fn bake_corpus() -> Result<Corpus> {
    Ok(Corpus {
        pli: bake(FIXTURE_PLI)?,
        chat: bake(FIXTURE_CHAT)?,
        detail: [
            bake(FIXTURE_GEOFENCE)?,
            bake(FIXTURE_ROUTE)?,
            bake(FIXTURE_DRAWING)?,
        ],
    })
}

/// Per-second telemetry counters.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    pub(crate) sent_total: AtomicU64,
    pub(crate) bytes_total: AtomicU64,
    pub(crate) sent_pli: AtomicU64,
    pub(crate) sent_chat: AtomicU64,
    pub(crate) sent_detail: AtomicU64,
    pub(crate) write_errors: AtomicU64,
}

impl Stats {
    pub(crate) fn record(&self, class: Class, bytes: usize) {
        self.sent_total.fetch_add(1, Ordering::Relaxed);
        self.bytes_total.fetch_add(bytes as u64, Ordering::Relaxed);
        match class {
            Class::Pli => self.sent_pli.fetch_add(1, Ordering::Relaxed),
            Class::Chat => self.sent_chat.fetch_add(1, Ordering::Relaxed),
            Class::Detail => self.sent_detail.fetch_add(1, Ordering::Relaxed),
        };
    }
}

/// Read-only handle the per-connection driver borrows.
#[derive(Clone)]
struct DriverCtx {
    target: String,
    rate: u32,
    deadline: Option<Instant>,
    table: [Class; 100],
    pli: Arc<Vec<u8>>,
    chat: Arc<Vec<u8>>,
    detail: Arc<[Vec<u8>; 3]>,
    stats: Arc<Stats>,
}

/// Drive a single connection: dial, then emit at `ctx.rate` msg/s
/// until the deadline. Picks fixtures via `counter % 100` against
/// `ctx.table`.
async fn drive_connection(id: usize, ctx: DriverCtx) -> Result<()> {
    let mut sock = TcpStream::connect(&ctx.target)
        .await
        .with_context(|| format!("conn {id}: connect {}", ctx.target))?;
    sock.set_nodelay(true).ok();

    let period = Duration::from_secs_f64(1.0 / f64::from(ctx.rate.max(1)));
    let mut tick = interval(period);
    let mut counter: u64 = id as u64;

    loop {
        tick.tick().await;
        if let Some(d) = ctx.deadline
            && Instant::now() >= d
        {
            return Ok(());
        }

        let class = ctx.table[usize::try_from(counter % 100).unwrap_or(0)];
        let bytes: &[u8] = match class {
            Class::Pli => &ctx.pli,
            Class::Chat => &ctx.chat,
            // detail rotates across the three sub-fixtures
            Class::Detail => {
                let cycle = usize::try_from(counter / 100).unwrap_or(0);
                &ctx.detail[cycle % 3]
            }
        };

        match sock.write_all(bytes).await {
            Ok(()) => ctx.stats.record(class, bytes.len()),
            Err(e) => {
                ctx.stats.write_errors.fetch_add(1, Ordering::Relaxed);
                warn!(conn = id, error = ?e, "write failed; closing connection");
                return Ok(());
            }
        }
        counter = counter.wrapping_add(1);
    }
}

/// Entry point for `taktool loadgen`.
pub(crate) async fn run(args: LoadgenArgs) -> Result<()> {
    if args.connections == 0 {
        bail!("--connections must be > 0");
    }
    if args.rate == 0 {
        bail!("--rate must be > 0");
    }

    let corpus = bake_corpus()?;
    info!(
        pli_bytes = corpus.pli.len(),
        chat_bytes = corpus.chat.len(),
        geofence_bytes = corpus.detail[0].len(),
        route_bytes = corpus.detail[1].len(),
        drawing_bytes = corpus.detail[2].len(),
        "fixtures baked"
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
        "loadgen starting"
    );

    // Spawn N connection tasks. We do NOT use the `tasks::spawn`
    // helper here because that lives in tak-server (binary) — for a
    // CLI tool we accept naked `tokio::spawn` and silence N3.
    let ctx = DriverCtx {
        target: args.target.clone(),
        rate: args.rate,
        deadline,
        table,
        pli: pli_buf,
        chat: chat_buf,
        detail: detail_bufs,
        stats: stats.clone(),
    };
    let mut handles = Vec::with_capacity(args.connections);
    for id in 0..args.connections {
        let ctx = ctx.clone();
        #[allow(clippy::disallowed_methods)]
        let h = tokio::spawn(async move { drive_connection(id, ctx).await });
        handles.push(h);
    }
    drop(ctx);

    // Reporter task — every second prints rolling counters.
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
            let pli = report_stats.sent_pli.load(Ordering::Relaxed);
            let chat = report_stats.sent_chat.load(Ordering::Relaxed);
            let detail = report_stats.sent_detail.load(Ordering::Relaxed);
            let errs = report_stats.write_errors.load(Ordering::Relaxed);
            info!(
                t_s = elapsed,
                sent_delta = sent - prev_sent,
                bytes_delta = bytes - prev_bytes,
                sent_total = sent,
                pli,
                chat,
                detail,
                errs,
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

    let elapsed = started.elapsed().as_secs_f64();
    let sent = stats.sent_total.load(Ordering::Relaxed);
    let bytes = stats.bytes_total.load(Ordering::Relaxed);
    let pli = stats.sent_pli.load(Ordering::Relaxed);
    let chat = stats.sent_chat.load(Ordering::Relaxed);
    let detail = stats.sent_detail.load(Ordering::Relaxed);
    let errs = stats.write_errors.load(Ordering::Relaxed);
    let msg_per_s = sent as f64 / elapsed;
    let mb_per_s = (bytes as f64 / elapsed) / 1_048_576.0;
    info!(
        elapsed_s = elapsed,
        sent_total = sent,
        bytes_total = bytes,
        msg_per_s,
        mb_per_s,
        pli,
        chat,
        detail,
        errs,
        "loadgen done"
    );
    if args.json {
        emit_json_summary(&args, &stats, started.elapsed());
    }
    Ok(())
}

/// One-line JSON record consumed by `scripts/bench-baseline.sh`.
/// Hand-rolled rather than via serde_json to keep the dependency
/// surface tight; numeric formatting matches Rust's debug repr.
pub(crate) fn emit_json_summary(args: &LoadgenArgs, stats: &Stats, elapsed: Duration) {
    let elapsed_s = elapsed.as_secs_f64();
    let sent = stats.sent_total.load(Ordering::Relaxed);
    let bytes = stats.bytes_total.load(Ordering::Relaxed);
    let pli = stats.sent_pli.load(Ordering::Relaxed);
    let chat = stats.sent_chat.load(Ordering::Relaxed);
    let detail = stats.sent_detail.load(Ordering::Relaxed);
    let errs = stats.write_errors.load(Ordering::Relaxed);
    let msg_per_s = sent as f64 / elapsed_s;
    let mb_per_s = (bytes as f64 / elapsed_s) / 1_048_576.0;
    let driver = if args.uring { "io_uring" } else { "tokio" };
    println!(
        r#"{{"tag":"{tag}","target":"{target}","driver":"{driver}","connections":{conns},"rate":{rate},"duration":{dur},"mix":"{mix:?}","elapsed_s":{elapsed_s},"sent_total":{sent},"bytes_total":{bytes},"msg_per_s":{mps},"mb_per_s":{bps},"pli":{pli},"chat":{chat},"detail":{detail},"errors":{errs}}}"#,
        tag = args.tag,
        target = args.target,
        driver = driver,
        conns = args.connections,
        rate = args.rate,
        dur = args.duration,
        mix = args.mix,
        elapsed_s = elapsed_s,
        sent = sent,
        bytes = bytes,
        mps = msg_per_s,
        bps = mb_per_s,
        pli = pli,
        chat = chat,
        detail = detail,
        errs = errs,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mix table sums match the documented ratios.
    #[test]
    fn realistic_mix_is_70_20_10() {
        let table = MixProfile::Realistic.lookup_table();
        let pli = table.iter().filter(|c| **c == Class::Pli).count();
        let chat = table.iter().filter(|c| **c == Class::Chat).count();
        let detail = table.iter().filter(|c| **c == Class::Detail).count();
        assert_eq!((pli, chat, detail), (70, 20, 10));
    }

    #[test]
    fn pli_only_mix_is_100_0_0() {
        let table = MixProfile::PliOnly.lookup_table();
        let pli = table.iter().filter(|c| **c == Class::Pli).count();
        assert_eq!(pli, 100);
    }

    #[test]
    fn uniform_mix_is_balanced() {
        let table = MixProfile::Uniform.lookup_table();
        let pli = table.iter().filter(|c| **c == Class::Pli).count();
        let chat = table.iter().filter(|c| **c == Class::Chat).count();
        let detail = table.iter().filter(|c| **c == Class::Detail).count();
        assert_eq!(pli + chat + detail, 100);
        // Each bucket within ±1 of 33 — balanced enough for the
        // intended micro-class regression sweep.
        for n in [pli, chat, detail] {
            assert!((33..=34).contains(&n), "bucket out of band: {n}");
        }
    }

    /// Every fixture round-trips through XML→TakMessage→prost→framed.
    /// Boundary check: confirms the "bake" step is non-empty for each
    /// canonical fixture so a future fixture rename doesn't silently
    /// break the corpus.
    #[test]
    fn corpus_bakes_all_five_fixtures() {
        let c = bake_corpus().expect("bake corpus");
        assert!(c.pli.len() > 50, "PLI frame too small");
        assert!(c.chat.len() > 50, "chat frame too small");
        for (i, d) in c.detail.iter().enumerate() {
            assert!(d.len() > 50, "detail[{i}] frame too small");
        }
    }

    /// First byte of every framed payload is 0xBF (per
    /// `docs/architecture.md` §3 — the streaming framing magic).
    #[test]
    fn framed_payloads_start_with_magic() {
        let c = bake_corpus().expect("bake corpus");
        assert_eq!(c.pli[0], 0xBF);
        assert_eq!(c.chat[0], 0xBF);
        for d in &c.detail {
            assert_eq!(d[0], 0xBF);
        }
    }
}
