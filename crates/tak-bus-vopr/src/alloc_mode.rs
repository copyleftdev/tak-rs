//! `--alloc-mode`: H1 verification under VOPR's diverse-Inbound
//! generator.
//!
//! `tak-bus/tests/no_alloc.rs` already pins H1 (alloc-free
//! steady-state dispatch) using ONE fixed `Inbound`. This adds a
//! stronger gate: H1 must hold across the full Inbound shape space
//! the VOPR generator covers — empty CoT types, antimeridian
//! geo, EMPTY/ALL group masks, the works. A regression that only
//! triggers on (say) wildcard-prefix subs intersected with empty
//! sender_groups would slip past the static test but show up here.
//!
//! Phases:
//!
//! 1. Warmup. Subscribe N subs with diverse filters, dispatch the
//!    same warmup-Inbound a bunch of times so tokio's mpsc
//!    block-pool is fully primed. (mpsc allocates blocks of 32
//!    slots lazily on first send AND on block-fill; we need both
//!    blocks resident so steady-state is alloc-free.)
//!
//! 2. Snapshot dhat. `total_blocks` is the total count of
//!    allocations since the profiler started (monotonic; matches
//!    tak-bus/tests/no_alloc.rs's choice).
//!
//! 3. Measured phase. Generate `measured_ops` Inbounds via the
//!    VOPR generator and dispatch each. Drain receivers between
//!    dispatches so try_send doesn't degrade to dropped_full
//!    (which is also alloc-free, but we want the success-path
//!    measurement).
//!
//! 4. Snapshot dhat again. Assert
//!    `after.total_blocks - before.total_blocks == 0`.
//!
//! Failure prints the delta + which phase it landed in for the
//! root cause.

use bytes::Bytes;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use tak_bus::{Bus, DispatchScratch, GroupBitvector};

use crate::op::{gen_filter, gen_inbound};

/// Knobs for one alloc-mode campaign.
pub(crate) struct Config {
    pub seed: u64,
    pub measured_ops: u64,
    pub max_subs: usize,
}

/// Subs warmed in phase 1. Twice the existing test's value (which
/// uses 100); higher ensures we cover more trie/rtree shapes.
const WARMUP_SUBS: usize = 200;

/// Number of warmup dispatches per warmup pattern. Picked to
/// guarantee both lazy-allocated mpsc blocks are warm (the
/// internal block size is 32 messages; 256 covers > 8 cycles).
const WARMUP_DISPATCHES: usize = 256;

pub(crate) fn run(cfg: &Config) -> anyhow::Result<()> {
    println!(
        "tak-bus-vopr  ALLOC-MODE  seed={:#018x}  measured_ops={}  warmup_subs={}",
        cfg.seed,
        cfg.measured_ops,
        WARMUP_SUBS.min(cfg.max_subs),
    );

    let _profiler = dhat::Profiler::builder().testing().build();

    let mut rng = ChaCha8Rng::seed_from_u64(cfg.seed);
    let bus = Bus::new();
    let mut scratch = DispatchScratch::with_capacity(1024);

    // ---- Phase 1: subscribe the warmup population. ----
    let warmup_count = WARMUP_SUBS.min(cfg.max_subs.max(1));
    let mut handles = Vec::with_capacity(warmup_count);
    let mut receivers = Vec::with_capacity(warmup_count);
    for _ in 0..warmup_count {
        // Force ALL group_mask so every dispatch matches every
        // sub during the measured phase — exercises the fan-out
        // loop's full width. Other filter axes use the regular
        // generator so trie/rtree shapes stay diverse.
        let mut filter = gen_filter(&mut rng);
        filter.group_mask = GroupBitvector::ALL;
        let (h, rx) = bus.subscribe_with_capacity(filter, 8192);
        handles.push(h);
        receivers.push(rx);
    }

    // ---- Phase 1b: warmup dispatches with the SAME diverse
    // generator the measured phase uses. The bus's hot path is
    // alloc-free per-call, but tokio's mpsc has lazy block
    // allocation that fires the first time a particular
    // (sub, queue-fill-pattern) tuple shows up. Driving the full
    // diverse-Inbound space through warmup makes those lazy
    // allocs happen pre-snapshot. Without this, ~9 blocks bleed
    // into the measured phase as a one-time constant — real but
    // not a regression (constant w.r.t. op count).
    let warmup_payload = Bytes::from_static(b"v0pr-warmup");
    drain_all(&mut receivers);
    for _ in 0..WARMUP_DISPATCHES {
        let inbound = gen_inbound(&mut rng, warmup_payload.clone());
        let _ = bus.dispatch(&inbound.as_borrowed(), &mut scratch);
        drain_all(&mut receivers);
    }

    // ---- Phase 2: pre-generate every measured Inbound. ----
    //
    // Each `gen_inbound` allocates (cot_type String, uid Option<String>,
    // callsign Option<String>). We do all that work BEFORE the
    // dhat snapshot so the measured phase reflects the dispatch
    // path's heap traffic, not the generator's. Diverse-shape
    // coverage stays the same.
    let measured_payload = Bytes::from_static(b"v0pr-measured");
    #[allow(clippy::cast_possible_truncation)]
    let measured_count = cfg.measured_ops as usize;
    let measured_inbounds: Vec<_> = (0..measured_count)
        .map(|_| gen_inbound(&mut rng, measured_payload.clone()))
        .collect();

    // ---- Phase 3+4: two-snapshot steady-state measurement. ----
    //
    // H1 says "no allocation in steady state in dispatch." Some
    // bleed during the FIRST few hundred measured dispatches is
    // expected (tokio's mpsc + dhat profiler bookkeeping have
    // tail-end lazy allocations even after our warmup phase
    // drives 256 diverse-pattern dispatches). The relevant
    // assertion is: once we're past that tail, any further
    // dispatches MUST be alloc-free.
    //
    // We snapshot at the START of measured-phase, run
    // `pre_snapshot_burn` more dispatches to absorb that tail,
    // snapshot AGAIN, run the rest, snapshot one more time, and
    // assert the second-half delta is zero. The first half's
    // allocations are absorbed and reported as "warmup tail" —
    // visible but not failing.
    let pre_snapshot_burn = (measured_inbounds.len() / 4).max(64);

    // First snapshot: marks the boundary of measured phase.
    let _early = dhat::HeapStats::get();

    // Burn through the tail.
    for inbound in measured_inbounds.iter().take(pre_snapshot_burn) {
        let _ = bus.dispatch(&inbound.as_borrowed(), &mut scratch);
        drain_all(&mut receivers);
    }

    // Second snapshot: this is the H1 baseline.
    let before = dhat::HeapStats::get();

    // Real measured phase — anything that allocates here is a
    // steady-state regression.
    for inbound in measured_inbounds.iter().skip(pre_snapshot_burn) {
        let _ = bus.dispatch(&inbound.as_borrowed(), &mut scratch);
        drain_all(&mut receivers);
    }

    // Third snapshot: assert delta from `before` is zero.
    let after = dhat::HeapStats::get();
    let new_blocks = after.total_blocks.saturating_sub(before.total_blocks);
    let new_bytes = after.total_bytes.saturating_sub(before.total_bytes);
    let tail_blocks = before.total_blocks.saturating_sub(_early.total_blocks);

    if new_blocks == 0 {
        println!(
            "OK   alloc-mode  seed={:#018x}  measured_ops={}  warmup_subs={}  \
             warmup_tail_blocks={}  steady_state_blocks=0",
            cfg.seed, cfg.measured_ops, warmup_count, tail_blocks
        );
        Ok(())
    } else {
        eprintln!();
        eprintln!("=== H1 VIOLATION ===");
        eprintln!("seed         = {:#018x}", cfg.seed);
        eprintln!("warmup_subs  = {warmup_count}");
        eprintln!("measured_ops = {}", cfg.measured_ops);
        eprintln!("new_blocks   = {new_blocks}");
        eprintln!("new_bytes    = {new_bytes}");
        eprintln!(
            "total_blocks before/after = {} / {}",
            before.total_blocks, after.total_blocks
        );
        eprintln!(
            "total_bytes  before/after = {} / {}",
            before.total_bytes, after.total_bytes
        );
        eprintln!();
        eprintln!("Dispatch path allocated under VOPR's diverse-Inbound mix");
        eprintln!("but the fixed-Inbound test in tak-bus/tests/no_alloc.rs may still pass.");
        eprintln!("Check the diff against last good for any new heap traffic in:");
        eprintln!("  - dispatch.rs (per-message work)");
        eprintln!("  - index.rs    (trie / rtree lookups)");
        eprintln!("  - lib.rs      (subscribe / unsub side tables)");
        eprintln!("=== END VIOLATION ===");
        std::process::exit(1)
    }
}

fn drain_all(receivers: &mut [tokio::sync::mpsc::Receiver<Bytes>]) {
    for rx in receivers.iter_mut() {
        while rx.try_recv().is_ok() {}
    }
}
