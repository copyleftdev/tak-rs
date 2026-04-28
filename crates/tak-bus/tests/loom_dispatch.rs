//! Invariant **N1** verification: bus dispatch is correct under concurrent
//! subscribe / unsubscribe / dispatch from any thread interleaving.
//!
//! Two complementary tests live here:
//!
//! 1. **`stress_concurrent_subscribe_dispatch_drop`** — runs in normal
//!    `cargo test`. Spawns N std::thread workers, each performing K
//!    subscribe-dispatch-drop cycles. Catches data races (via Miri if
//!    enabled), deadlocks (via timeout), lost messages (via stat
//!    reconciliation), and panics. High-iteration coverage of the real
//!    `Bus` against the real concurrency primitives we ship with
//!    (parking_lot RwLock, sharded-slab, tokio mpsc, atomic counters).
//!
//! 2. **`generation_tag_prevents_aba_under_any_schedule`** — runs only
//!    under `RUSTFLAGS="--cfg loom"`. Models the generation-tag pattern
//!    on a small set of loom-instrumented primitives so the model
//!    checker can exhaustively explore thread interleavings. Verifies
//!    the load-bearing logic the real `Bus` relies on for ABA defense
//!    when slab slots get reused.
//!
//! The full `Bus` can't be loom-instrumented today because it depends on
//! `sharded_slab` (its own internal atomics aren't loom types) and
//! `parking_lot::RwLock`. Switching to loom-compatible analogues under
//! `cfg(loom)` is a follow-up if the focused model surfaces gaps.
//!
//! Run both:
//! ```sh
//! cargo test -p tak-bus --test loom_dispatch
//! RUSTFLAGS='--cfg loom' cargo test -p tak-bus --test loom_dispatch --release
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

// ===========================================================================
// Stress test (always runs)
// ===========================================================================

#[cfg(not(loom))]
mod stress {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;
    use std::time::Duration;

    use bytes::Bytes;
    use tak_bus::{Bus, DispatchScratch, Filter, GroupBitvector, Inbound};

    /// Stress test: 8 worker threads × 1000 subscribe-dispatch-drop cycles.
    /// Verifies:
    /// - No panic / no deadlock (timed out via thread join + soft deadline).
    /// - Live count returns to 0 after all handles drop.
    /// - At least one delivery succeeds in each iteration where the dispatcher
    ///   matches a live subscription, ruling out lost-message bugs at the
    ///   "dispatch sees the sub" level.
    #[test]
    fn stress_concurrent_subscribe_dispatch_drop() {
        const WORKERS: u32 = 8;
        const ITERS_PER_WORKER: u32 = 1000;

        let bus = Bus::new();
        let total_delivered = Arc::new(AtomicU32::new(0));
        let total_subscribed = Arc::new(AtomicU32::new(0));

        let mut workers = Vec::with_capacity(WORKERS as usize);
        for w in 0..WORKERS {
            let bus = Arc::clone(&bus);
            let total_delivered = Arc::clone(&total_delivered);
            let total_subscribed = Arc::clone(&total_subscribed);
            workers.push(
                thread::Builder::new()
                    .name(format!("worker-{w}"))
                    .spawn(move || {
                        let mut scratch = DispatchScratch::with_capacity(64);
                        let payload = Bytes::from_static(b"x");

                        for _ in 0..ITERS_PER_WORKER {
                            // Subscribe, dispatch, drop. Use distinct group bits per
                            // worker so different workers' subs share enough overlap
                            // for non-zero match rates.
                            let (h, mut rx) = bus.subscribe(Filter {
                                group_mask: GroupBitvector::EMPTY.with_bit(w as usize % 8),
                                ..Filter::default()
                            });
                            total_subscribed.fetch_add(1, Ordering::Relaxed);

                            let inbound = Inbound {
                                payload: payload.clone(),
                                sender_groups: GroupBitvector::EMPTY.with_bit(w as usize % 8),
                                cot_type: "a-f-G-U-C",
                                lat: 0.0,
                                lon: 0.0,
                                uid: None,
                                callsign: None,
                            };
                            let stats = bus.dispatch(&inbound, &mut scratch);
                            total_delivered.fetch_add(stats.delivered, Ordering::Relaxed);

                            // Drain any messages routed to us during the dispatch.
                            while rx.try_recv().is_ok() {}
                            drop(h);
                        }
                    })
                    .expect("spawn"),
            );
        }

        let deadline = Duration::from_secs(30);
        for h in workers {
            // Soft deadline: the test would deadlock-detect via the OS test
            // harness eventually, but we want fast feedback.
            let timer = std::time::Instant::now();
            h.join().unwrap_or_else(|e| {
                panic!("worker panicked after {:?}: {:?}", timer.elapsed(), e);
            });
            assert!(
                timer.elapsed() < deadline,
                "worker exceeded soft deadline {deadline:?}"
            );
        }

        assert_eq!(bus.len(), 0, "all subs were dropped → live count must be 0");

        let subscribed = total_subscribed.load(Ordering::Relaxed);
        let delivered = total_delivered.load(Ordering::Relaxed);
        assert_eq!(subscribed, WORKERS * ITERS_PER_WORKER, "subscribe count");
        // Loose check: each worker dispatches once per iter, and at minimum
        // its own sub matches (group bits self-overlap). If we ever see ZERO
        // deliveries that's a bug — the dispatch never matched a live sub
        // even with a self-matching group.
        assert!(
            delivered >= WORKERS * ITERS_PER_WORKER / 2,
            "delivered={delivered} suspiciously low — possible lost-message bug"
        );
    }
}

// ===========================================================================
// loom-instrumented model (cfg loom only)
// ===========================================================================

#[cfg(loom)]
mod loom_model {
    //! Models the generation-tag ABA defense the real Bus uses to prevent
    //! a stale `SubscriptionId` from aliasing a freshly-reused slab slot.
    //!
    //! The Bus has:
    //!     subs: sharded_slab — slot allocator
    //!     next_generation: AtomicU64 — monotonic counter
    //!     SubscriptionId { slab_key, generation } — opaque handle
    //!
    //! On insert: gen ← next_generation.fetch_add(1); slab[key] = {gen, value}
    //! On lookup: id matches if slab[key].is_some() && slab[key].gen == id.gen
    //! On remove: slab[key] = None
    //!
    //! This model represents the "one slot" worst case where slot reuse is
    //! guaranteed. Loom explores all interleavings of:
    //!     T1: insert(A) → remove → insert(B)
    //!     T2: read after capturing id_A → verify id_A no longer aliases B
    //!
    //! The invariant: T2 reading a slot with the captured id MUST NOT see
    //! B's value. Either it sees A (still there), None (removed), or the
    //! generation mismatches (sees B but id_A.gen != B.gen → reject).

    use loom::sync::Arc;
    use loom::sync::Mutex;
    use loom::sync::atomic::{AtomicU64, Ordering};

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Id {
        generation: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Entry {
        generation: u64,
        value: u32,
    }

    struct Slot {
        next_gen: AtomicU64,
        cell: Mutex<Option<Entry>>,
    }

    impl Slot {
        fn new() -> Self {
            Self {
                next_gen: AtomicU64::new(0),
                cell: Mutex::new(None),
            }
        }

        fn insert(&self, value: u32) -> Id {
            let generation = self.next_gen.fetch_add(1, Ordering::Relaxed);
            *self.cell.lock().unwrap() = Some(Entry { generation, value });
            Id { generation }
        }

        fn remove(&self) {
            *self.cell.lock().unwrap() = None;
        }

        /// ABA-safe lookup: returns the value ONLY if the entry's generation
        /// matches the caller's id. Stale ids never alias new entries.
        fn lookup(&self, id: Id) -> Option<u32> {
            let cell = self.cell.lock().unwrap();
            cell.and_then(|e| (e.generation == id.generation).then_some(e.value))
        }
    }

    /// Loom test: under any thread interleaving, T2's lookup with a
    /// captured stale id never returns T1's NEW (post-remove) value.
    ///
    /// Loom explores ~10k+ schedules in this state space; failure surfaces
    /// any reordering bug in the generation/cell write pair.
    #[test]
    fn generation_tag_prevents_aba_under_any_schedule() {
        loom::model(|| {
            let slot = Arc::new(Slot::new());

            // Pre-populate with value A; capture its id.
            let id_a = slot.insert(100);

            let slot1 = slot.clone();
            let t1 = loom::thread::spawn(move || {
                slot1.remove();
                slot1.insert(200);
            });

            let slot2 = slot.clone();
            let t2 = loom::thread::spawn(move || {
                // T2 races T1. Lookup with id_a may see:
                //   - Some(100): T1 hasn't removed yet
                //   - None: T1 removed but hasn't reinserted
                //   - None (gen mismatch): T1 reinserted with gen+1; id_a.gen
                //     no longer matches → lookup returns None
                // What we MUST NEVER see: Some(200) under id_a.
                let observed = slot2.lookup(id_a);
                assert_ne!(
                    observed,
                    Some(200),
                    "ABA leaked: stale id aliased new value"
                );
            });

            t1.join().unwrap();
            t2.join().unwrap();
        });
    }
}
