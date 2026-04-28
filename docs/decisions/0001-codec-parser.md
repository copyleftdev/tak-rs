# 0001 — XML codec parser: quick-xml borrowed mode

- **Date:** 2026-04-27
- **Status:** Accepted
- **Issue:** [#2](https://github.com/copyleftdev/tak-rs/issues/2)
- **Open question resolved:** `docs/architecture.md` §9 #4

## Context

`tak-cot` decodes CoT XML into typed views on the hot path. The two
candidate parsers were:

- **`quick-xml`** (borrowed mode) — streaming SAX-style; attribute values are
  `&[u8]` / `&str` slices into the input. Verbose API.
- **`roxmltree`** — DOM-style; allocates a tree at parse time; access is
  ergonomic via `Node` traversal.

Invariant **H2** in `docs/invariants.md` requires decoders to borrow from
input; `roxmltree` allocates internally for the tree, even though its
public `&str` returns slice into the source.

## Experiment

Five canonical CoT samples were committed at
`crates/tak-cot/tests/fixtures/`:

| File | Description | Bytes |
|---|---|---|
| `01_pli.xml`      | Position Location Information (`a-f-G-U-C`)      | ~640 |
| `02_chat.xml`     | GeoChat (`b-t-f`) with chatgrp + remarks         | ~890 |
| `03_geofence.xml` | Drawing rectangle (`u-d-r`) with 4 corners       | ~870 |
| `04_route.xml`    | Multi-waypoint route (`b-m-r`) with 5 link items | ~1180 |
| `05_drawing.xml`  | Free shape (`u-d-f`) with 7 link points          | ~1010 |

Each parser does the same realistic work per fixture: parse the document,
read `<event>` attributes, read `<point>` attributes, walk `<detail>`
children and count them.

Bench harness: `crates/tak-cot/benches/codec.rs` (criterion, 50-sample, 3s
measurement, single-core via `taskset -c 0`).

## Result

| Fixture       | quick-xml | roxmltree | Ratio (rox / qx) |
|---------------|-----------|-----------|-------------------|
| `01_pli`      | 940 ns    | 3.12 µs   | **3.32×**         |
| `02_chat`     | 1.13 µs   | 3.68 µs   | **3.26×**         |
| `03_geofence` | 1.11 µs   | 3.83 µs   | **3.44×**         |
| `04_route`    | 1.29 µs   | 5.32 µs   | **4.13×**         |
| `05_drawing`  | 1.23 µs   | 4.14 µs   | **3.37×**         |

`quick-xml` is **3.3-4.1× faster** on every fixture, with the gap widening
on larger documents (the 5-waypoint route is worst). At 50k msg/s × 100
subscribers, that's the difference between the firehose hitting the perf
thesis and missing it.

## Decision

**`quick-xml` borrowed mode, exclusively.**

`roxmltree` is added to `deny.toml` bans so a future contributor can't
quietly reach for the ergonomic option.

## Consequences

- `tak-cot::Codec::decode_xml` will use `quick_xml::Reader` with
  `read_event_into` for streaming, attribute access via `e.attributes()`.
- `Detail::xmlDetail` will be a `&'a str` slice into the original input,
  satisfying invariant H2 + the lossless XML round-trip requirement (C1).
- The verbose API surface is the cost. We accept it — it keeps the hot
  path honest. A small ergonomic helper (`tak_cot::xml::cursor`) can wrap
  the most common operations if friction becomes real.
- `roxmltree` is **banned** in `deny.toml`. Re-evaluate only if quick-xml
  ever drops borrowed mode (it won't — that's their main feature).

## Reproducing

```sh
git checkout <commit-with-this-bench>
taskset -c 0 cargo bench --bench codec -- parse_xml \
  --warm-up-time 1 --measurement-time 3 --sample-size 50
```

Raw run is in `bench/history/2026-04-27-parser-shootout.txt`.
