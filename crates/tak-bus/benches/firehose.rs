//! Firehose hot-path bench — the canonical perf gate.
//!
//! `bench-baseline` agent reads/writes `target/criterion/.../base/` from this
//! suite. `/bench-hot` is the slash command that drives it.
//!
//! Issue #23 adds subscribe / drop / get_filter benches to verify the
//! ≤1μs target stated in the issue acceptance.
//!
//! The full multi-publisher × multi-subscriber × group-filter dispatch
//! bench arrives with #25.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use tak_bus::{Bus, Filter, GeoBbox, GroupBitvector};

fn group_intersect(c: &mut Criterion) {
    let a = GroupBitvector([0xAAAA_AAAA_AAAA_AAAA; 4]);
    let b = GroupBitvector([0x5555_5555_5555_5556; 4]);
    c.bench_function("group_intersect", |b_| {
        b_.iter(|| std::hint::black_box(&a).intersects(std::hint::black_box(&b)))
    });
}

fn group_intersect_disjoint(c: &mut Criterion) {
    let a = GroupBitvector([0xAAAA_AAAA_AAAA_AAAA; 4]);
    let b = GroupBitvector([0x5555_5555_5555_5555; 4]);
    c.bench_function("group_intersect_disjoint", |b_| {
        b_.iter(|| std::hint::black_box(&a).intersects(std::hint::black_box(&b)))
    });
}

fn subscribe_then_drop(c: &mut Criterion) {
    let bus = Bus::new();
    c.bench_function("subscribe_then_drop", |b| {
        b.iter(|| {
            let h = bus.subscribe(Filter::default());
            std::hint::black_box(&h);
            drop(h);
        })
    });
}

fn subscribe_only(c: &mut Criterion) {
    // Measures sub-only latency, with handles accumulating in a Vec we
    // periodically drain. This isolates the insert cost from the remove cost.
    let bus = Bus::new();
    let mut accum: Vec<_> = Vec::with_capacity(1024);
    c.bench_function("subscribe_only", |b| {
        b.iter(|| {
            accum.push(bus.subscribe(Filter::default()));
            if accum.len() >= 1024 {
                accum.clear();
            }
        })
    });
}

fn get_filter_warm(c: &mut Criterion) {
    let bus = Bus::new();
    let h = bus.subscribe(Filter {
        interest_uid: Some("ANDROID-deadbeef".to_owned()),
        ..Filter::default()
    });
    let id = h.id();
    c.bench_function("get_filter_warm", |b| {
        b.iter(|| std::hint::black_box(bus.get_filter(std::hint::black_box(id))))
    });
    drop(h);
}

fn ten_thousand_live_subs_lookup(c: &mut Criterion) {
    // Stress: 10k live subscriptions, measure get_filter on a known id.
    let bus = Bus::new();
    let mut handles = Vec::with_capacity(10_000);
    let mut target_id = None;
    for i in 0..10_000 {
        let h = bus.subscribe(Filter {
            interest_uid: Some(format!("uid-{i}")),
            ..Filter::default()
        });
        if i == 5_000 {
            target_id = Some(h.id());
        }
        handles.push(h);
    }
    let id = target_id.expect("target captured");
    c.bench_function("get_filter_at_10k_subs", |b| {
        b.iter(|| std::hint::black_box(bus.get_filter(std::hint::black_box(id))))
    });
    let _ = Arc::clone(&bus);
}

/// Issue #24 acceptance bench: candidate lookup at 10k subs ≤10μs.
///
/// 10_000 subscriptions: 50% wildcard type filter, 30% prefix wildcard
/// (`a-f-G-*`-style), 20% exact CoT type. Quarter of them have a geo bbox.
/// Query fires for an `a-f-G-U-C` event at LA coordinates.
fn candidate_lookup_at_10k_subs(c: &mut Criterion) {
    let bus = Bus::new();
    let mut handles = Vec::with_capacity(10_000);
    for i in 0..10_000 {
        let type_prefix = match i % 10 {
            0..=4 => None,                         // wildcard
            5..=7 => Some(format!("a-f-G-{i}-*")), // mostly-mismatching prefix wildcard
            _ => Some("a-f-G-U-C".to_owned()),     // exact-match (will all match the query)
        };
        let geo_bbox = if i % 4 == 0 {
            let f = f64::from(i) / 1000.0;
            Some(GeoBbox {
                min_lat: 30.0 + f.sin() * 5.0,
                min_lon: -120.0 + f.cos() * 5.0,
                max_lat: 35.0 + f.sin() * 5.0,
                max_lon: -115.0 + f.cos() * 5.0,
            })
        } else {
            None
        };
        handles.push(bus.subscribe(Filter {
            type_prefix,
            geo_bbox,
            ..Filter::default()
        }));
    }

    let mut buf = Vec::with_capacity(10_000);
    c.bench_function("candidate_lookup_at_10k_subs", |b| {
        b.iter(|| {
            buf.clear();
            bus.extend_candidates(
                std::hint::black_box("a-f-G-U-C"),
                std::hint::black_box(34.05),
                std::hint::black_box(-118.24),
                &mut buf,
            );
            std::hint::black_box(&buf);
        })
    });
    let _ = Arc::clone(&bus);
}

criterion_group!(
    benches,
    group_intersect,
    group_intersect_disjoint,
    subscribe_then_drop,
    subscribe_only,
    get_filter_warm,
    ten_thousand_live_subs_lookup,
    candidate_lookup_at_10k_subs,
);
criterion_main!(benches);
