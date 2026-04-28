//! The op loop. Generate → apply to bus + model → compare → log.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bytes::Bytes;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use tak_bus::{Bus, DispatchScratch, SubscriptionId};

use crate::model::{Model, filter_matches};
use crate::op::{Op, gen_capacity, gen_filter, gen_inbound};

/// Knobs for one VOPR campaign.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub seed: u64,
    pub ops: u64,
    pub verbose: bool,
    pub max_subs: usize,
}

/// Result of one campaign.
pub(crate) enum Outcome {
    Ok {
        ops_run: u64,
        elapsed: Duration,
    },
    Failed {
        seed: u64,
        op_index: u64,
        reason: String,
        log: Vec<String>,
    },
}

/// Ring of recent op descriptions for the failure dump. We don't
/// keep every op — that's gigabytes for a 100k-op run — but the
/// last N is enough to root-cause.
const OP_LOG_TAIL: usize = 64;

pub(crate) fn run(cfg: &Config) -> Outcome {
    let mut rng = ChaCha8Rng::seed_from_u64(cfg.seed);
    let bus = Bus::new();
    let mut model = Model::default();
    let mut scratch = DispatchScratch::default();
    let mut log: VecDeque<String> = VecDeque::with_capacity(OP_LOG_TAIL);

    let payload = Bytes::from_static(b"v0pr-payload-shared-arc");
    let started = Instant::now();

    for i in 0..cfg.ops {
        let op = pick_op(&mut rng, &model, cfg);
        if cfg.verbose {
            println!("[{i:>6}] {op:?}");
        }
        if log.len() == OP_LOG_TAIL {
            log.pop_front();
        }
        log.push_back(format!("{op:?}"));

        match apply_op(&bus, &mut model, &mut scratch, &payload, op) {
            Ok(()) => {}
            Err(reason) => {
                return Outcome::Failed {
                    seed: cfg.seed,
                    op_index: i,
                    reason,
                    log: log.into_iter().collect(),
                };
            }
        }

        // Cheap per-op invariant: bus.len() matches model.
        let bus_len = bus.len();
        let model_len = model.live_count();
        if bus_len != model_len {
            return Outcome::Failed {
                seed: cfg.seed,
                op_index: i,
                reason: format!("bus.len() = {bus_len} but model live_count = {model_len}"),
                log: log.into_iter().collect(),
            };
        }
    }

    // Final sweep: full subscription_stats agreement.
    if let Err(reason) = compare_stats_full(&bus, &model) {
        return Outcome::Failed {
            seed: cfg.seed,
            op_index: cfg.ops,
            reason: format!("post-run stats divergence: {reason}"),
            log: log.into_iter().collect(),
        };
    }

    Outcome::Ok {
        ops_run: cfg.ops,
        elapsed: started.elapsed(),
    }
}

/// Pick the next op given the current model state. Weighted to
/// keep the live-sub population around `max_subs` and to spend
/// most of the time dispatching (where the interesting state
/// transitions happen).
#[allow(clippy::too_many_lines)]
fn pick_op(rng: &mut ChaCha8Rng, model: &Model, cfg: &Config) -> Op {
    let live = model.live_count();

    // If we have no live subs and TrySendToStale wouldn't have a
    // partner, force a Subscribe.
    if live == 0 && model.dropped_ids.is_empty() {
        return Op::Subscribe {
            filter: gen_filter(rng),
            capacity: gen_capacity(rng),
        };
    }

    let r = rng.gen_range(0..100);
    match r {
        // Subscribe: more likely when below max_subs.
        0..=14 if live < cfg.max_subs => Op::Subscribe {
            filter: gen_filter(rng),
            capacity: gen_capacity(rng),
        },
        // DropHandle: ~5% — rare so the live population grows,
        // and recently-dropped ids remain in the ring for stale
        // tests.
        15..=19 if live > 0 => {
            let slot = pick_live_slot(rng, model);
            Op::DropHandle {
                slot: slot.unwrap_or(0),
            }
        }
        // DrainReceiver: 30%. Most subs need draining or they'll
        // hit Full and dropped_full counters become the only
        // mover.
        20..=49 if live > 0 => {
            let slot = pick_live_slot(rng, model);
            Op::DrainReceiver {
                slot: slot.unwrap_or(0),
                max: rng.gen_range(1..=8),
            }
        }
        // DropReceiver: rare (~2%) — exercises the closed-channel
        // path. We skip the per-sub agreement check for closed
        // subs since dropped_closed isn't per-sub-tracked.
        50..=51 if live > 0 => {
            let slot = pick_live_slot(rng, model);
            Op::DropReceiver {
                slot: slot.unwrap_or(0),
            }
        }
        // TrySendToStale: ~3% when we have something to test against.
        52..=54 if !model.dropped_ids.is_empty() => Op::TrySendToStale,
        // SnapshotStats: ~2%. Periodic full-state comparison.
        55..=56 => Op::SnapshotStats,
        // Default: Dispatch.
        _ => Op::Dispatch {
            inbound: gen_inbound(rng, Bytes::from_static(b"v0pr-payload-shared-arc")),
        },
    }
}

fn pick_live_slot(rng: &mut ChaCha8Rng, model: &Model) -> Option<usize> {
    let live: Vec<usize> = (0..model.subs.len())
        .filter(|&i| model.subs[i].handle_alive())
        .collect();
    if live.is_empty() {
        return None;
    }
    Some(live[rng.gen_range(0..live.len())])
}

#[allow(clippy::too_many_lines)]
fn apply_op(
    bus: &std::sync::Arc<Bus>,
    model: &mut Model,
    scratch: &mut DispatchScratch,
    payload: &Bytes,
    op: Op,
) -> Result<(), String> {
    match op {
        Op::Subscribe { filter, capacity } => {
            model.add_sub(bus, filter, capacity);
            Ok(())
        }
        Op::Dispatch { inbound } => {
            let borrow = inbound.as_borrowed();
            // Predict per-sub outcomes BEFORE calling dispatch —
            // we'll compare aggregate + per-sub afterwards.
            let mut expected_delivered = 0u32;
            let mut expected_dropped_full = 0u32;
            let mut per_sub_changes: Vec<(usize, bool)> = Vec::new();
            for (i, sub) in model.subs.iter().enumerate() {
                if let Some(delivered) = sub.predict(&borrow) {
                    if delivered {
                        expected_delivered = expected_delivered.saturating_add(1);
                    } else {
                        expected_dropped_full = expected_dropped_full.saturating_add(1);
                    }
                    per_sub_changes.push((i, delivered));
                }
            }

            let _ = payload; // we use the inbound's owned payload directly
            let stats = bus.dispatch(&borrow, scratch);

            // Aggregate agreement: delivered + dropped_full must
            // match. We do NOT compare filtered_groups /
            // filtered_geo because those depend on the candidate
            // set (which is a SUPERSET of true matches), and
            // mirroring the trie/rtree exactly in the model is
            // out of scope. Per-sub counters are the load-bearing
            // assertion.
            if stats.delivered != expected_delivered {
                return Err(format!(
                    "Dispatch delivered mismatch: bus={} model={} (inbound={:?})",
                    stats.delivered, expected_delivered, borrow
                ));
            }
            if stats.dropped_full != expected_dropped_full {
                return Err(format!(
                    "Dispatch dropped_full mismatch: bus={} model={} (inbound={:?})",
                    stats.dropped_full, expected_dropped_full, borrow
                ));
            }

            // Update model's per-sub counters.
            for (i, delivered) in per_sub_changes {
                let s = &mut model.subs[i];
                if delivered {
                    s.expected_delivered += 1;
                    s.queued += 1;
                } else {
                    s.expected_dropped_full += 1;
                }
            }
            Ok(())
        }
        Op::DropHandle { slot } => {
            let _ = model.drop_handle(slot);
            Ok(())
        }
        Op::DrainReceiver { slot, max } => {
            let _ = model.drain(slot, max);
            Ok(())
        }
        Op::DropReceiver { slot } => {
            model.drop_receiver(slot);
            Ok(())
        }
        Op::TrySendToStale => {
            // Pick a stale id; bus.try_send_to MUST return false
            // and MUST NOT bump any counters.
            let Some(&stale_id) = model.dropped_ids.first() else {
                return Ok(());
            };
            // Snapshot every sub's pre-stats.
            let pre = bus.subscription_stats();
            let ok = bus.try_send_to(stale_id, payload.clone());
            if ok {
                return Err(format!(
                    "try_send_to returned true for stale id {stale_id:?}"
                ));
            }
            // No live sub's counter should have changed.
            let post = bus.subscription_stats();
            if pre.len() != post.len() {
                return Err(format!(
                    "try_send_to to stale id changed live-sub count: {} -> {}",
                    pre.len(),
                    post.len()
                ));
            }
            for (a, b) in pre.iter().zip(post.iter()) {
                if a.delivered != b.delivered || a.dropped_full != b.dropped_full {
                    return Err(format!(
                        "try_send_to to stale id mutated stats for live sub {:?}: \
                         delivered {} -> {}, dropped_full {} -> {}",
                        a.id, a.delivered, b.delivered, a.dropped_full, b.dropped_full
                    ));
                }
            }
            Ok(())
        }
        Op::SnapshotStats => compare_stats_full(bus, model),
    }
}

/// Compare every live sub's `delivered` / `dropped_full` between
/// the bus and the model. Off the per-op path; called either at
/// SnapshotStats time or end-of-run.
fn compare_stats_full(bus: &std::sync::Arc<Bus>, model: &Model) -> Result<(), String> {
    let snapshot = bus.subscription_stats();
    let bus_by_id: std::collections::HashMap<SubscriptionId, _> =
        snapshot.into_iter().map(|s| (s.id, s)).collect();

    let mut live_in_model = 0usize;
    for sub in &model.subs {
        if !sub.handle_alive() {
            // Bus must NOT have this id any more.
            if bus_by_id.contains_key(&sub.id) {
                return Err(format!("bus still has dropped sub {:?}", sub.id));
            }
            continue;
        }
        live_in_model += 1;
        let Some(b) = bus_by_id.get(&sub.id) else {
            return Err(format!("bus missing stats for live sub {:?}", sub.id));
        };
        if b.delivered != sub.expected_delivered {
            return Err(format!(
                "sub {:?}: delivered bus={} model={}",
                sub.id, b.delivered, sub.expected_delivered
            ));
        }
        if b.dropped_full != sub.expected_dropped_full {
            return Err(format!(
                "sub {:?}: dropped_full bus={} model={}",
                sub.id, b.dropped_full, sub.expected_dropped_full
            ));
        }
    }

    // Bus shouldn't have stats for anything beyond what the model
    // expects.
    if bus_by_id.len() != live_in_model {
        return Err(format!(
            "bus reported {} live subs in stats, model expected {}",
            bus_by_id.len(),
            live_in_model
        ));
    }
    Ok(())
}

// Optional unused-but-future helpers that the runner doesn't yet
// call but the model exposes; reference here so dead-code lint
// stays quiet.
#[allow(dead_code)]
fn _shape_check(filter: &tak_bus::Filter, inbound: &tak_bus::Inbound<'_>) -> bool {
    filter_matches(filter, inbound)
}
