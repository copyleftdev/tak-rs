//! Trace minimization for VOPR failures.
//!
//! Strategy: linear ddmin. Try removing each op in (random) order;
//! if the run still fails, keep the skip; if the run now passes,
//! restore. Stop after one full pass with no successful new
//! removals. The result is a 1-minimal failing trace — removing
//! any single remaining op makes it pass.
//!
//! Why linear and not full ddmin's hierarchical halving:
//! - Linear is O(N) replays. Hierarchical halving is faster on
//!   "many redundant ops" traces (O(N log N) — ish) but more
//!   complex.
//! - Most VOPR failures concentrate in ≤ a few hundred ops,
//!   where N is small enough that linear's wall-clock cost is
//!   already trivial.
//! - The op cost is dominated by replaying, which is already
//!   ~133k ops/s on the regular harness path.
//!
//! When VOPR finds a real bug we'll revisit; for now linear is
//! the right "simplest thing that could possibly work."
//!
//! Determinism: skip set order doesn't affect the verdict (replay
//! is bit-identical for the same skip set), but it DOES affect
//! the SHAPE of the minimal trace we converge on. We use a
//! seeded ChaCha8 derived from the original seed so the
//! minimization is reproducible too.

use std::collections::HashSet;

use rand::SeedableRng;
use rand::seq::SliceRandom;
use rand_chacha::ChaCha8Rng;

use crate::repro::Repro;
use crate::runner::{Config, Outcome, run_with_skip};

pub(crate) struct Minimized {
    pub repro: Repro,
    pub passes_attempted: u64,
    pub final_outcome: Outcome,
}

pub(crate) fn minimize(input: Repro, max_subs: usize, verbose: bool) -> Minimized {
    let baseline_cfg = Config {
        seed: input.seed,
        ops: input.ops,
        verbose: false,
        max_subs,
    };
    let mut skip: HashSet<u64> = input.skip_set();

    // Confirm the input still fails with the seed it was saved
    // for. If it doesn't, minimization has nothing to do.
    let baseline = run_with_skip(&baseline_cfg, &skip);
    let Outcome::Failed {
        op_index, reason, ..
    } = &baseline
    else {
        // The repro doesn't reproduce. Caller should treat as a
        // "no work" case; we return the input unchanged so an
        // operator can see they shouldn't be minimizing this.
        return Minimized {
            repro: input,
            passes_attempted: 0,
            final_outcome: baseline,
        };
    };
    let original_op_index = *op_index;
    let original_reason = reason.clone();
    if verbose {
        println!(
            "minimize: baseline FAIL at op_index={original_op_index} reason={original_reason}"
        );
    }

    // Indices we'll attempt to skip, in shuffled order. Skipping
    // an already-skipped index is a no-op so we don't bother
    // re-trying those.
    let mut candidates: Vec<u64> = (0..input.ops).filter(|i| !skip.contains(i)).collect();
    // Derived shuffler seed so minimization is reproducible too.
    // Magic constant is "MINFY" letters as ASCII bytes.
    let mut shuffler = ChaCha8Rng::seed_from_u64(input.seed.wrapping_add(0x4D49_4E46_5900));
    candidates.shuffle(&mut shuffler);

    let mut passes_attempted = 0u64;
    let mut final_outcome = baseline;

    for cand in candidates {
        passes_attempted += 1;
        skip.insert(cand);
        let outcome = run_with_skip(&baseline_cfg, &skip);
        match &outcome {
            Outcome::Failed { .. } => {
                // Still fails — keep the skip.
                final_outcome = outcome;
                if verbose && passes_attempted % 50 == 0 {
                    println!(
                        "minimize: {} attempts done, {} skips kept",
                        passes_attempted,
                        skip.len()
                    );
                }
            }
            Outcome::Ok { .. } => {
                // Removing this op made it pass — restore.
                skip.remove(&cand);
            }
        }
    }

    let mut sorted: Vec<u64> = skip.iter().copied().collect();
    sorted.sort_unstable();
    Minimized {
        repro: Repro {
            seed: input.seed,
            ops: input.ops,
            skip: sorted,
        },
        passes_attempted,
        final_outcome,
    }
}
