//! Firehose hot-path bench — the canonical perf gate.
//!
//! `bench-baseline` agent reads/writes `target/criterion/.../base/` from this
//! suite. `/bench-hot` is the slash command that drives it.
//!
//! Stub: real bench (publishers × subscribers × group-filter) lands once
//! `tak_bus::Bus` is implemented (M2 milestone).
#![allow(missing_docs)]

use criterion::{Criterion, criterion_group, criterion_main};
use tak_bus::GroupBitvector;

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

criterion_group!(benches, group_intersect, group_intersect_disjoint);
criterion_main!(benches);
