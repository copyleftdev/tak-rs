//! `tak-bus-vopr` — deterministic verification harness for `tak-bus`.
//!
//! # What this is
//!
//! A single binary that:
//!
//! 1. Seeds a ChaCha8 PRNG from a `u64`.
//! 2. Generates a stream of operations against [`tak_bus::Bus`]:
//!    Subscribe (with rich Filter generator), Dispatch (with rich
//!    Inbound generator), DropHandle, DrainReceiver, DropReceiver,
//!    TrySendToStaleId, SnapshotStats.
//! 3. Maintains an abstract `Model` that mirrors the bus's
//!    expected per-subscription state.
//! 4. After every op, asserts the bus's reported state agrees with
//!    the model's prediction. Mismatch = bug, dump op log, exit
//!    non-zero.
//!
//! # Why this exists
//!
//! `tak-bus` carries the H1, H3, H4, H5, H6, ABA, and
//! per-sub-counter invariants from `docs/invariants.md`. Loom
//! covers ≤10k interleavings; proptest covers single-shape
//! properties; this VOPR fills the long tail — millions of ops
//! mixing edge cases that real load wouldn't produce on its
//! own (capacity-1 channels, empty group masks, on-boundary
//! geo points, mid-dispatch unsubscribe + slot reuse, stale
//! `try_send_to`).
//!
//! Same seed = bit-perfect reproducible run. Failures dump the
//! seed + op index + full op log so the failure can be replayed
//! offline under a debugger.
//!
//! # Binary D1 exemption
//!
//! Same as `tak-server` and `taktool`: this is a harness binary
//! at the process boundary. `unwrap`/`expect`/`print*` are the
//! right vocabulary for an "either it works or it screams" CLI.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::panic,
    // The harness is intentionally a single-binary crate; the
    // `pub` items inside each module are only visible inside the
    // binary itself, so the unreachable-pub lint fires en masse.
    // Suppressing here is cleaner than `pub(crate)`-painting the
    // entire surface — this isn't a library.
    unreachable_pub
)]

mod alloc_mode;
mod minimize;
mod model;
mod op;
mod repro;
mod runner;

use clap::Parser;

/// dhat is wired as the global allocator unconditionally so
/// `--alloc-mode` has access to `HeapStats`. With no `Profiler`
/// running it costs a couple of relaxed atomic ops per alloc;
/// default runs still hit ~130k ops/s on the existing 100k-op
/// workload.
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[derive(Parser, Debug)]
#[command(
    name = "tak-bus-vopr",
    version,
    about = "deterministic verification harness for tak-bus",
    long_about = None,
)]
struct Args {
    /// PRNG seed. Same seed reproduces the run bit-for-bit.
    /// Default = a fixed value so a no-arg invocation is also
    /// reproducible (CI runs this without flags).
    #[arg(long, env = "VOPR_SEED", default_value_t = 0xC0FE_BABE)]
    seed: u64,

    /// Number of operations to run.
    #[arg(long, env = "VOPR_OPS", default_value_t = 100_000)]
    ops: u64,

    /// Print every op + outcome. Off by default — millions of ops
    /// produce gigabytes of log noise. Useful for bisecting a
    /// found failure.
    #[arg(long, env = "VOPR_VERBOSE", default_value_t = false)]
    verbose: bool,

    /// Maximum number of live subs the generator allows. Real
    /// production sees thousands; the model's per-op cost grows
    /// with this value, so for the default 100k-op run we cap at
    /// a number that keeps the harness brisk.
    #[arg(long, env = "VOPR_MAX_SUBS", default_value_t = 64)]
    max_subs: usize,

    /// Run the H1 alloc-free invariant check instead of the
    /// regular model-vs-bus campaign. Boots dhat, warms a
    /// diverse subscriber population, snapshots, dispatches the
    /// configured number of mixed-Inbound ops, snapshots, asserts
    /// zero allocation delta. Strictly stronger than the
    /// existing `tak-bus/tests/no_alloc.rs` test (which uses ONE
    /// fixed Inbound shape). A non-zero delta means something
    /// regressed H1 under diverse traffic.
    #[arg(long, env = "VOPR_ALLOC_MODE", default_value_t = false)]
    alloc_mode: bool,

    /// Replay a saved repro artifact (JSON {seed, ops, skip}).
    /// Re-runs the harness with the same seed + skip set; same
    /// output as a regular run with identical determinism.
    /// Mutually exclusive with --minimize.
    #[arg(long, env = "VOPR_REPLAY")]
    replay: Option<std::path::PathBuf>,

    /// Minimize a saved repro artifact (JSON {seed, ops, skip}).
    /// Linearly tries skipping each remaining op; keeps a skip
    /// if the run still fails, restores otherwise. Writes
    /// `<file>.min.json` on completion.
    #[arg(long, env = "VOPR_MINIMIZE")]
    minimize: Option<std::path::PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.alloc_mode {
        return alloc_mode::run(&alloc_mode::Config {
            seed: args.seed,
            measured_ops: args.ops,
            max_subs: args.max_subs,
        });
    }

    if args.replay.is_some() && args.minimize.is_some() {
        anyhow::bail!("--replay and --minimize are mutually exclusive");
    }
    if let Some(path) = &args.replay {
        return run_replay(path, args.max_subs, args.verbose);
    }
    if let Some(path) = &args.minimize {
        return run_minimize(path, args.max_subs, args.verbose);
    }

    println!(
        "tak-bus-vopr  seed={:#018x}  ops={}  max_subs={}",
        args.seed, args.ops, args.max_subs
    );

    let outcome = runner::run(&runner::Config {
        seed: args.seed,
        ops: args.ops,
        verbose: args.verbose,
        max_subs: args.max_subs,
    });

    match outcome {
        runner::Outcome::Ok { ops_run, elapsed } => {
            #[allow(clippy::cast_precision_loss)]
            let ops_per_s = ops_run as f64 / elapsed.as_secs_f64();
            println!(
                "OK   seed={:#018x}  ops_run={}  elapsed_s={:.3}  ops/s={:.0}",
                args.seed,
                ops_run,
                elapsed.as_secs_f64(),
                ops_per_s
            );
            Ok(())
        }
        runner::Outcome::Failed {
            seed,
            op_index,
            reason,
            log,
        } => {
            // Dump the minimal repro artifact alongside the
            // failure so the user can replay or minimize without
            // needing the original CLI invocation.
            let repro = repro::Repro {
                seed,
                ops: args.ops,
                skip: Vec::new(),
            };
            let repro_path = repro::Repro::default_failure_path(seed);
            let dump_msg = match repro.write_json(&repro_path) {
                Ok(()) => format!(
                    "repro     = {} (run with --replay or --minimize)",
                    repro_path.display()
                ),
                Err(e) => format!(
                    "repro     = (write failed: {e}; re-run with --seed {seed:#018x} --ops {})",
                    args.ops
                ),
            };

            eprintln!();
            eprintln!("=== VOPR FAILURE ===");
            eprintln!("seed       = {seed:#018x}");
            eprintln!("op_index   = {op_index}");
            eprintln!("reason     = {reason}");
            eprintln!("{dump_msg}");
            eprintln!("--- op log (last {} ops) ---", log.len());
            for (i, line) in log.iter().enumerate() {
                eprintln!("[{i:>4}] {line}");
            }
            eprintln!("=== END FAILURE ===");
            std::process::exit(1);
        }
    }
}

fn run_replay(path: &std::path::Path, max_subs: usize, verbose: bool) -> anyhow::Result<()> {
    let repro = repro::Repro::read_json(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    println!(
        "REPLAY   {}  seed={:#018x}  ops={}  skip_count={}",
        path.display(),
        repro.seed,
        repro.ops,
        repro.skip.len()
    );
    let outcome = runner::run_with_skip(
        &runner::Config {
            seed: repro.seed,
            ops: repro.ops,
            verbose,
            max_subs,
        },
        &repro.skip_set(),
    );
    match outcome {
        runner::Outcome::Ok { ops_run, elapsed } => {
            println!(
                "PASS  ops_run={ops_run}  elapsed_s={:.3}",
                elapsed.as_secs_f64()
            );
            Ok(())
        }
        runner::Outcome::Failed {
            op_index, reason, ..
        } => {
            eprintln!("FAIL  op_index={op_index}  reason={reason}");
            std::process::exit(1);
        }
    }
}

fn run_minimize(path: &std::path::Path, max_subs: usize, verbose: bool) -> anyhow::Result<()> {
    let input = repro::Repro::read_json(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let original_skip_count = input.skip.len();
    let original_ops = input.ops;
    println!(
        "MINIMIZE {}  seed={:#018x}  ops={}  starting_skips={}",
        path.display(),
        input.seed,
        input.ops,
        original_skip_count
    );

    let started = std::time::Instant::now();
    let result = minimize::minimize(input, max_subs, verbose);
    let elapsed = started.elapsed();

    let out_path = repro::Repro::default_min_path(path);
    result
        .repro
        .write_json(&out_path)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", out_path.display()))?;

    let final_skip = result.repro.skip.len();
    let kept_ops = original_ops.saturating_sub(final_skip as u64);
    println!(
        "DONE   final_skips={final_skip}  kept_ops={kept_ops}  attempts={}  elapsed_s={:.3}",
        result.passes_attempted,
        elapsed.as_secs_f64()
    );
    println!("       written: {}", out_path.display());
    if matches!(result.final_outcome, runner::Outcome::Ok { .. }) {
        eprintln!(
            "WARNING: minimized repro now PASSES; the input artifact may not be reproducible. \
             Re-run the original failing seed to confirm."
        );
    }
    Ok(())
}
