//! Codec benches.
//!
//! Historical context: this file previously held the quick-xml-vs-roxmltree
//! shootout for issue #2 (`docs/decisions/0001-codec-parser.md`). quick-xml
//! won decisively (3.3-4.1× faster) and roxmltree is now banned. The
//! shootout code was removed; the raw numbers live in
//! `bench/history/2026-04-27-parser-shootout.txt`.
//!
//! What stays: a parse bench using only quick-xml borrowed mode, against
//! the canonical CoT fixtures. This is the path tak-cot::Codec::decode_xml
//! will take, and the `bench-baseline` agent diffs against it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const FIXTURES: &[(&str, &str)] = &[
    ("01_pli", include_str!("../tests/fixtures/01_pli.xml")),
    ("02_chat", include_str!("../tests/fixtures/02_chat.xml")),
    (
        "03_geofence",
        include_str!("../tests/fixtures/03_geofence.xml"),
    ),
    ("04_route", include_str!("../tests/fixtures/04_route.xml")),
    (
        "05_drawing",
        include_str!("../tests/fixtures/05_drawing.xml"),
    ),
];

#[derive(Debug, Default)]
struct ParseSummary {
    event_attrs: usize,
    point_attrs: usize,
    detail_children: usize,
    type_len: usize,
}

/// quick-xml borrowed-mode parse — what tak-cot::Codec::decode_xml will do.
fn parse_quick_xml(src: &str) -> ParseSummary {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(src);
    reader.config_mut().trim_text(false);
    let mut summary = ParseSummary::default();
    let mut depth = 0u32;
    let mut in_detail = false;
    let mut buf = Vec::with_capacity(256);

    loop {
        match reader.read_event_into(&mut buf) {
            Err(_) => break,
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                depth += 1;
                let name = e.name();
                let local = name.as_ref();
                if local == b"event" {
                    for a in e.attributes().flatten() {
                        summary.event_attrs += 1;
                        if a.key.as_ref() == b"type" {
                            summary.type_len = a.value.len();
                        }
                    }
                } else if local == b"detail" {
                    in_detail = true;
                } else if in_detail && depth == 3 {
                    summary.detail_children += 1;
                }
            }
            Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = name.as_ref();
                if local == b"point" {
                    summary.point_attrs += e.attributes().count();
                } else if local == b"event" {
                    for a in e.attributes().flatten() {
                        summary.event_attrs += 1;
                        if a.key.as_ref() == b"type" {
                            summary.type_len = a.value.len();
                        }
                    }
                } else if in_detail && depth == 2 {
                    summary.detail_children += 1;
                }
            }
            Ok(Event::End(_)) => {
                depth = depth.saturating_sub(1);
                if depth < 2 {
                    in_detail = false;
                }
            }
            _ => {}
        }
        buf.clear();
    }

    summary
}

fn bench_parse(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse_xml");
    for (name, src) in FIXTURES {
        g.throughput(Throughput::Bytes(src.len() as u64));
        g.bench_with_input(BenchmarkId::new("quick_xml", name), src, |b, src| {
            b.iter(|| parse_quick_xml(std::hint::black_box(src)))
        });
    }
    g.finish();
}

fn framing_magic_check(c: &mut Criterion) {
    let buf = [0xBFu8, 0x01, 0xBF, 0xDE, 0xAD, 0xBE, 0xEF];
    c.bench_function("framing_magic_check", |b| {
        b.iter(|| std::hint::black_box(buf[0]) == tak_cot::framing::MAGIC)
    });
}

criterion_group!(benches, bench_parse, framing_magic_check);
criterion_main!(benches);
