//! Codec micro-benches — placeholders until decoders are implemented (M0).
//! `bench-baseline` agent reads results from this file.
#![allow(missing_docs)]

use criterion::{Criterion, criterion_group, criterion_main};

fn framing_magic_check(c: &mut Criterion) {
    let buf = [0xBFu8, 0x01, 0xBF, 0xDE, 0xAD, 0xBE, 0xEF];
    c.bench_function("framing_magic_check", |b| {
        b.iter(|| std::hint::black_box(buf[0]) == tak_cot::framing::MAGIC)
    });
}

criterion_group!(benches, framing_magic_check);
criterion_main!(benches);
