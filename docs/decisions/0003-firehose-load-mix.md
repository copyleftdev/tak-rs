# 0003 — Firehose synthetic load mix: 70 / 20 / 10

- **Date:** 2026-04-28
- **Status:** Accepted
- **Issue:** [#3](https://github.com/copyleftdev/tak-rs/issues/3)
- **Open question resolved:** `docs/architecture.md` §9 #5
- **Unblocks:** M5 (perf benches, issues #36-#38)

## Context

The "Rust beats Java" claim in this project is meaningless without a
representative load. A criterion bench that fans `a-f-G-U-C` PLIs to
1000 subscribers tells us how fast the dispatch path is — but a real
ATAK fleet does not send only PLIs. Mixing message types matters
because:

- **PLIs are small (~600 B) and trivial to parse.** They dominate
  message *count*. They stress the per-message overhead in
  `tak_bus::dispatch`.
- **Chat / markers (~1 kB) carry richer `<detail>` blocks.** They
  stress the XML walker (the `Detail.xmlDetail` zero-copy borrow
  invariant C1).
- **Drawings, routes, large detail blobs (~5–15 kB) are rare but
  expensive.** They stress allocator pressure on the persistence
  side-channel and exercise compression / copy paths we cheat on for
  small payloads.

A bench that's 100 % PLI hides regressions in the other two paths.

## Decision

Adopt a **70 / 20 / 10 traffic mix** for the firehose bench harness:

| Class                   | Share | Approx size | Canonical fixture                                |
|-------------------------|-------|-------------|--------------------------------------------------|
| PLI updates             | 70 %  | 600–800 B   | `crates/tak-cot/tests/fixtures/01_pli.xml`       |
| Chat / markers / SA     | 20 %  | 800–1 kB    | `crates/tak-cot/tests/fixtures/02_chat.xml`      |
| Detail blobs (drawings, routes, geofences) | 10 %  | 850 B – 15 kB | `03_geofence.xml`, `04_route.xml`, `05_drawing.xml` |

Concretely, the load generator (issue #36) MUST:

- emit messages drawn from the five canonical fixtures in proportions
  matching the table above (ties broken in favor of more PLI),
- support a configurable per-connection rate (default 1 PLI / 5 s,
  matching ATAK's default `pliReporting` interval),
- preserve message *type* when computing the mix — i.e. don't
  randomly relabel a PLI as chat; pull from the appropriate fixture.

## Why 70 / 20 / 10

### Sources

1. **TAK Product Center documentation.** ATAK's default
   `pliReporting` interval is 5 seconds and chat / marker plotting is
   user-driven, so PLIs swamp the channel by message count. Confirmed
   against the upstream Java metrics emit at
   `.scratch/takserver-java/src/takserver-core/src/main/java/com/bbn/marti/service/MessagingApi.java`,
   which reports `pli` separately from `chat` and `tasking`.
2. **Public CivTAK exercise reports** (Joint Multinational Readiness
   Center after-action notes; OGC TAK domain working group whitepapers)
   put PLIs at "60–80 %" of total CoT volume in active operations.
3. **Internal recon** of the Java schema: `mission_change` and the
   `cot_router` keep PLIs in a separate hot table, suggesting upstream
   already optimizes for the assumption that PLIs dominate.

70 / 20 / 10 sits in the middle of the public band (PLI 70 ±10 %, chat
20 ±5 %, detail blobs 10 ±5 %). It is deliberately *not* tuned to make
us look fast — chat at 20 % and large blobs at 10 % keep the
allocator-heavy paths in the hot loop.

### What we are *not* claiming

- We have no real captured exercise pcap to commit. The "anonymized
  pcap" line in #3's acceptance criteria is downgraded to a future
  refinement: the load mix above ships now, and a real pcap can
  replace the synthetic generator if and when one becomes available.
- These ratios are for **steady-state** firehose. Catastrophic events
  (geofence breach storms, mass-evacuation chat surges) will spike
  chat or detail temporarily; we do not bench those today.

## Consequences

**Positive**

- M5 benches can land without a wait on physical-device pcap capture.
- `01_pli.xml` … `05_drawing.xml` already pass the codec proptest
  round-trip suite, so the bench corpus is known-good.
- The mix is documented and versioned — when we *do* get a real pcap,
  we can compare the two distributions side-by-side and adjust.

**Negative**

- Real exercise traffic likely has a long tail of niche message types
  (`b-d-r` geofences, `b-r-f-h-c` casualty reports, `t-x-c-t` sensor
  feeds) that 0.1 % of fleet actually emits. The 70/20/10 mix
  *misses* these entirely. Mitigation: when we add fixtures for them
  (M5+), bump them into the 10 % "detail" bucket and reweight.
- Per-connection rate is a hardcoded default. Real fleets vary
  (1 s for high-tempo dismounted units, 30 s for vehicles). #36 must
  expose this as a parameter, not hardcode.

## Action

1. ✅ This document.
2. Update `docs/architecture.md` §9 #5 → "Resolved".
3. (Issue #36) Implement load generator in `taktool` honoring this
   mix.
