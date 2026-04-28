//! Invariant **H1** verification: the dispatch loop is allocation-free in
//! steady state.
//!
//! This is the load-bearing perf invariant. At 50k msg/s × 100 subscribers
//! the firehose can't afford a single allocation per message — every alloc
//! is a cache-line bounce, an mimalloc lock contention point, and a
//! potential GC-style hiccup at p99. dhat measures actual heap traffic,
//! so this test fails loudly the moment we regress.
//!
//! Pattern: dhat as the test-binary's global allocator, warm the path,
//! snapshot, run the hot loop, snapshot, assert `total_blocks` unchanged.
//!
//! `#[ignore]`'d so it doesn't run on every `cargo test`. Run via:
//!
//! ```sh
//! cargo test -p tak-bus --test no_alloc -- --ignored
//! ```
//!
//! Or the gauntlet variant: `/check-invariants`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use tak_bus::{Bus, DispatchScratch, Filter, GroupBitvector, Inbound};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
#[ignore = "dhat: run via `cargo test -p tak-bus --test no_alloc -- --ignored`"]
fn dispatch_is_alloc_free_in_steady_state() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let bus = Bus::new();

    // Subscribers: 100 with mixed filter shapes. group_mask=bit-0 so they
    // all match the inbound's sender_groups; type_prefix alternates between
    // None (root wildcard) and `a-f-G-*` so the type trie walks more than
    // one node.
    let mut handles = Vec::with_capacity(100);
    let mut receivers = Vec::with_capacity(100);
    for i in 0..100 {
        let (h, rx) = bus.subscribe_with_capacity(
            Filter {
                group_mask: GroupBitvector::EMPTY.with_bit(0),
                type_prefix: if i % 2 == 0 {
                    None
                } else {
                    Some("a-f-G-*".to_owned())
                },
                ..Filter::default()
            },
            8192,
        );
        handles.push(h);
        receivers.push(rx);
    }

    // Pre-allocate scratch large enough to hold all candidates without
    // growing — Vec::extend_from_slice on adequate capacity is alloc-free.
    let mut scratch = DispatchScratch::with_capacity(256);

    // `Bytes::from_static` is the alloc-free constructor: the Bytes carries
    // a `&'static [u8]` rather than an Arc-owned heap buffer, so cloning
    // it is a pointer copy, no atomic, no malloc. (`Bytes::from(Vec<u8>)`
    // would heap-allocate; this test would catch that.)
    let payload = Bytes::from_static(b"hello-cot");
    let inbound = Inbound {
        payload: payload.clone(),
        sender_groups: GroupBitvector::EMPTY.with_bit(0),
        cot_type: "a-f-G-U-C",
        lat: 34.0,
        lon: -118.0,
        uid: None,
        callsign: None,
    };

    // Warm-up: tokio's mpsc allocates internal "blocks" of 32 message slots
    // lazily. The first allocation per channel happens on first send; the
    // second when the first block fills (at message #33) and a new block is
    // needed alongside the freelist. After both are around, send/drain
    // cycles reuse them. Warm with > BLOCK_CAP × 2 messages per channel to
    // ensure both are warmed.
    drain_all(&mut receivers);
    for _ in 0..128 {
        bus.dispatch(&inbound, &mut scratch);
        drain_all(&mut receivers);
    }

    // Snapshot the allocator.
    let before = dhat::HeapStats::get();

    // Steady-state hot loop. drain_all between dispatches so every send
    // succeeds (try_send into an empty bounded queue is alloc-free; into
    // a full one degrades to the dropped_full branch which is also
    // alloc-free, but we want to measure the SUCCESS path).
    let iters: u64 = 1024;
    for _ in 0..iters {
        bus.dispatch(&inbound, &mut scratch);
        drain_all(&mut receivers);
    }

    let after = dhat::HeapStats::get();

    let new_blocks = after.total_blocks - before.total_blocks;
    let new_bytes = after.total_bytes - before.total_bytes;
    assert_eq!(
        new_blocks, 0,
        "H1 VIOLATED: dispatch allocated {new_blocks} blocks ({new_bytes} bytes) \
         over {iters} iterations × 100 subscribers. \
         Final state: max_blocks={}, max_bytes={}.",
        after.max_blocks, after.max_bytes
    );
}

/// Drain all receivers so the next dispatch's `try_send` finds an empty queue.
fn drain_all(receivers: &mut [tokio::sync::mpsc::Receiver<Bytes>]) {
    for rx in receivers.iter_mut() {
        while rx.try_recv().is_ok() {}
    }
}
