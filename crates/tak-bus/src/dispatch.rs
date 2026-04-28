//! Per-message dispatch: candidate-set lookup → filter → fan-out.
//!
//! The dispatch path is the firehose. At 50k msg/s × 100 subscribers the
//! throughput target lives or dies in this function. Hot-path invariants
//! `H1` (alloc-free in steady state) and `H3` (fan-out is `Bytes::clone`)
//! both ride here.
//!
//! [`Bus::dispatch`] is alloc-free per invocation by design:
//!
//! 1. The candidate-set buffer is caller-owned via [`DispatchScratch`].
//! 2. Per-candidate work is pointer-swap and arithmetic only — no
//!    `Vec::push` outside `scratch.candidates`, no `String` ops.
//! 3. Fan-out is `Bytes::clone` (Arc bump) per delivered subscriber.
//!
//! `DispatchScratch::candidates` is reused across calls by clearing
//! (which retains capacity); the dhat alloc test in #26 verifies the
//! steady-state allocation count is 0.

use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::sync::mpsc::error::TrySendError;

use crate::{Bus, GroupBitvector, SubscriptionId};

/// Inbound message metadata + payload, as fed to [`Bus::dispatch`].
///
/// All fields are borrowed; the dispatch loop holds the [`Inbound`] for
/// the duration of one call only.
#[derive(Debug, Clone)]
pub struct Inbound<'a> {
    /// The wire payload to fan out. `Bytes` is ref-counted; subscribers
    /// receive `Arc`-bump clones (invariant H3).
    pub payload: Bytes,
    /// Group bitvector of the SENDER. Used to gate delivery via
    /// [`GroupBitvector::intersects`] against each subscriber's mask.
    pub sender_groups: GroupBitvector,
    /// CoT type code (e.g. `"a-f-G-U-C"`). Used to walk the type trie.
    pub cot_type: &'a str,
    /// Latitude of the event in WGS-84 decimal degrees.
    pub lat: f64,
    /// Longitude of the event in WGS-84 decimal degrees.
    pub lon: f64,
    /// Optional UID of the event (`event.uid`). Reserved for direct-route
    /// optimization in M3.
    pub uid: Option<&'a str>,
    /// Optional callsign of the event (`contact.callsign`). Reserved for
    /// direct-route optimization in M3.
    pub callsign: Option<&'a str>,
}

/// Caller-owned scratch buffer for [`Bus::dispatch`].
///
/// Reuse the same scratch across calls — `dispatch` calls
/// `candidates.clear()` at the start (capacity preserved), then fills it
/// without reallocating in steady state.
#[derive(Debug, Default)]
pub struct DispatchScratch {
    /// Candidate IDs collected from the type/geo indices. Caller-owned so
    /// dispatch doesn't allocate per call once warm.
    pub candidates: Vec<SubscriptionId>,
}

impl DispatchScratch {
    /// Allocate a new scratch with the given pre-reserved capacity. Use
    /// sized to the typical candidate-set count for the workload (e.g.
    /// 1024 for medium deployments, 16384 for large).
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            candidates: Vec::with_capacity(cap),
        }
    }
}

/// Result of one [`Bus::dispatch`] invocation.
///
/// Counts the four outcomes a candidate can have. Caller threads these
/// into Prometheus counters via the `metrics` crate at whatever cadence
/// is appropriate (typically a periodic batch update — incrementing per
/// dispatch is doable but costs a contended atomic per counter).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DispatchStats {
    /// Successfully `try_send`'d to the subscriber's mpsc.
    pub delivered: u32,
    /// Filtered out by group bitvector intersect.
    pub filtered_groups: u32,
    /// Filtered out by the geo bbox check (sub had a bbox; event was outside).
    pub filtered_geo: u32,
    /// Subscriber's mpsc was full — dropped per the H1-respecting
    /// "drop persistence not delivery" invariant inverted: when the
    /// subscriber can't keep up, we drop FOR THIS subscriber. That's
    /// correct behavior — slow subscribers don't stall fast ones.
    pub dropped_full: u32,
    /// Subscriber's mpsc receiver was already dropped. The slab entry
    /// may still be there briefly until the SubscriptionHandle is dropped.
    pub dropped_closed: u32,
}

impl DispatchStats {
    /// Total number of candidates considered (sum of all outcomes).
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.delivered as u64
            + self.filtered_groups as u64
            + self.filtered_geo as u64
            + self.dropped_full as u64
            + self.dropped_closed as u64
    }
}

impl Bus {
    /// Fan out one inbound message to all matching subscribers.
    ///
    /// The candidate set comes from the type/geo indices (#24). For each
    /// candidate we:
    /// 1. Re-fetch the entry by id (ABA-safe via generation tag).
    /// 2. Intersect the subscriber's group mask against the sender's.
    /// 3. If the subscriber has a geo bbox, check it contains the event's
    ///    location — the index pre-filter is coarse, this is the exact
    ///    test.
    /// 4. `try_send` an `Arc`-bump clone of the payload onto the
    ///    subscriber's bounded channel.
    ///
    /// Returns per-call [`DispatchStats`] for the caller to aggregate
    /// into Prometheus counters at whatever cadence makes sense.
    ///
    /// # Allocation discipline
    ///
    /// In steady state — `scratch.candidates` already has capacity, no
    /// new subscriptions, no banner-throughput cliff — this function
    /// performs **zero** heap allocations. Verified by the dhat test in
    /// #26.
    pub fn dispatch(&self, msg: &Inbound<'_>, scratch: &mut DispatchScratch) -> DispatchStats {
        scratch.candidates.clear();
        self.extend_candidates(msg.cot_type, msg.lat, msg.lon, &mut scratch.candidates);

        let mut stats = DispatchStats::default();
        for &id in &scratch.candidates {
            let Some(entry) = self.subs.get(id.slab_key) else {
                continue;
            };
            // ABA defense: stale candidate from before a recent unsubscribe.
            if entry.generation != id.generation {
                continue;
            }

            // Group bitvector — invariant H4 hot predicate.
            if !entry.filter.group_mask.intersects(&msg.sender_groups) {
                stats.filtered_groups = stats.filtered_groups.saturating_add(1);
                continue;
            }

            // Per-candidate geo check (the index is coarse — it returns
            // any sub whose pattern matched, not just those whose bbox
            // contains the point).
            if let Some(bbox) = &entry.filter.geo_bbox {
                if !bbox.contains(msg.lat, msg.lon) {
                    stats.filtered_geo = stats.filtered_geo.saturating_add(1);
                    continue;
                }
            }

            // Fan-out: Bytes::clone is an Arc bump (H3). try_send is
            // non-blocking; drop policy is "drop for this subscriber"
            // (slow subscribers don't stall fast ones).
            match entry.sender.try_send(msg.payload.clone()) {
                Ok(()) => {
                    stats.delivered = stats.delivered.saturating_add(1);
                    // Per-sub counter: relaxed because the read side
                    // (Bus::subscription_stats) is best-effort and
                    // doesn't need cross-thread happens-before. One
                    // atomic add per delivery, on a cache line owned
                    // by this subscription only.
                    entry.delivered.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(_)) => {
                    stats.dropped_full = stats.dropped_full.saturating_add(1);
                    entry.dropped_full.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Closed(_)) => {
                    stats.dropped_closed = stats.dropped_closed.saturating_add(1);
                }
            }
        }

        stats
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::{Bus, Filter, GeoBbox, GroupBitvector};

    fn payload() -> Bytes {
        Bytes::from_static(b"hello-cot")
    }

    #[tokio::test]
    async fn dispatch_delivers_to_matching_subscriber() {
        let bus = Bus::new();
        let filter = Filter {
            group_mask: GroupBitvector::EMPTY.with_bit(0),
            ..Filter::default()
        };
        let (_h, mut rx) = bus.subscribe(filter);

        let stats = bus.dispatch(
            &Inbound {
                payload: payload(),
                sender_groups: GroupBitvector::EMPTY.with_bit(0),
                cot_type: "a-f-G-U-C",
                lat: 34.0,
                lon: -118.0,
                uid: None,
                callsign: None,
            },
            &mut DispatchScratch::default(),
        );
        assert_eq!(stats.delivered, 1);
        assert_eq!(stats.total(), 1);
        let got = rx.recv().await.unwrap();
        assert_eq!(&got[..], b"hello-cot");
    }

    #[tokio::test]
    async fn dispatch_filters_by_disjoint_groups() {
        let bus = Bus::new();
        let filter = Filter {
            group_mask: GroupBitvector::EMPTY.with_bit(0), // sub in group 0
            ..Filter::default()
        };
        let (_h, mut rx) = bus.subscribe(filter);

        let stats = bus.dispatch(
            &Inbound {
                payload: payload(),
                sender_groups: GroupBitvector::EMPTY.with_bit(7), // sender in group 7 (disjoint)
                cot_type: "a-f-G-U-C",
                lat: 0.0,
                lon: 0.0,
                uid: None,
                callsign: None,
            },
            &mut DispatchScratch::default(),
        );
        assert_eq!(stats.delivered, 0);
        assert_eq!(stats.filtered_groups, 1);
        assert!(rx.try_recv().is_err(), "no message should have arrived");
    }

    #[tokio::test]
    async fn dispatch_filters_by_geo_bbox_outside() {
        let bus = Bus::new();
        let filter = Filter {
            group_mask: GroupBitvector::EMPTY.with_bit(0),
            geo_bbox: Some(GeoBbox {
                min_lat: 33.0,
                min_lon: -119.0,
                max_lat: 35.0,
                max_lon: -117.0,
            }),
            ..Filter::default()
        };
        let (_h, mut rx) = bus.subscribe(filter);

        let stats = bus.dispatch(
            &Inbound {
                payload: payload(),
                sender_groups: GroupBitvector::EMPTY.with_bit(0),
                cot_type: "a-f-G-U-C",
                lat: 40.0, // NYC; outside LA bbox
                lon: -74.0,
                uid: None,
                callsign: None,
            },
            &mut DispatchScratch::default(),
        );
        assert_eq!(stats.filtered_geo, 1);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_drops_when_channel_full() {
        let bus = Bus::new();
        // Capacity 2 — easy to fill.
        let (_h, _rx) = bus.subscribe_with_capacity(
            Filter {
                group_mask: GroupBitvector::EMPTY.with_bit(0),
                ..Filter::default()
            },
            2,
        );

        let inbound = Inbound {
            payload: payload(),
            sender_groups: GroupBitvector::EMPTY.with_bit(0),
            cot_type: "a-f-G-U-C",
            lat: 0.0,
            lon: 0.0,
            uid: None,
            callsign: None,
        };
        let mut scratch = DispatchScratch::default();

        let s1 = bus.dispatch(&inbound, &mut scratch);
        let s2 = bus.dispatch(&inbound, &mut scratch);
        let _s3 = bus.dispatch(&inbound, &mut scratch);
        let s4 = bus.dispatch(&inbound, &mut scratch);

        assert_eq!(s1.delivered, 1);
        assert_eq!(s2.delivered, 1);
        // 3rd may or may not deliver depending on whether the rx pulled
        // anything yet (we never read), but with capacity 2 and no rx
        // reads, by 4th the channel is definitely full.
        assert_eq!(s4.delivered, 0);
        assert_eq!(s4.dropped_full, 1);
    }

    #[tokio::test]
    async fn dispatch_drops_when_receiver_closed() {
        let bus = Bus::new();
        let (h, rx) = bus.subscribe(Filter {
            group_mask: GroupBitvector::EMPTY.with_bit(0),
            ..Filter::default()
        });
        // Drop the receiver — channel is now closed but the entry's
        // still in the slab until h is dropped.
        drop(rx);

        let stats = bus.dispatch(
            &Inbound {
                payload: payload(),
                sender_groups: GroupBitvector::EMPTY.with_bit(0),
                cot_type: "a-f-G-U-C",
                lat: 0.0,
                lon: 0.0,
                uid: None,
                callsign: None,
            },
            &mut DispatchScratch::default(),
        );
        assert_eq!(stats.dropped_closed, 1);
        assert_eq!(stats.delivered, 0);
        drop(h);
    }

    #[tokio::test]
    async fn dispatch_to_multiple_subs_with_mixed_filters() {
        let bus = Bus::new();
        // Sub A: matches all types, group 0
        let (_ha, mut ra) = bus.subscribe(Filter {
            group_mask: GroupBitvector::EMPTY.with_bit(0),
            ..Filter::default()
        });
        // Sub B: only `a-f-G-*`, group 0
        let (_hb, mut rb) = bus.subscribe(Filter {
            group_mask: GroupBitvector::EMPTY.with_bit(0),
            type_prefix: Some("a-f-G-*".to_owned()),
            ..Filter::default()
        });
        // Sub C: matches all types, group 5 (no overlap with sender)
        let (_hc, mut rc) = bus.subscribe(Filter {
            group_mask: GroupBitvector::EMPTY.with_bit(5),
            ..Filter::default()
        });

        let stats = bus.dispatch(
            &Inbound {
                payload: payload(),
                sender_groups: GroupBitvector::EMPTY.with_bit(0),
                cot_type: "a-f-G-U-C",
                lat: 0.0,
                lon: 0.0,
                uid: None,
                callsign: None,
            },
            &mut DispatchScratch::default(),
        );

        // A and B should both receive; C is filtered by groups.
        assert_eq!(stats.delivered, 2);
        assert_eq!(stats.filtered_groups, 1);

        assert!(ra.try_recv().is_ok());
        assert!(rb.try_recv().is_ok());
        assert!(rc.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_scratch_is_reusable_across_calls() {
        let bus = Bus::new();
        let (_h, _rx) = bus.subscribe(Filter {
            group_mask: GroupBitvector::EMPTY.with_bit(0),
            ..Filter::default()
        });

        let mut scratch = DispatchScratch::with_capacity(64);
        let inbound = Inbound {
            payload: payload(),
            sender_groups: GroupBitvector::EMPTY.with_bit(0),
            cot_type: "a-f-G-U-C",
            lat: 0.0,
            lon: 0.0,
            uid: None,
            callsign: None,
        };

        // Many dispatches; capacity preserved across .clear() calls.
        for _ in 0..100 {
            let _ = bus.dispatch(&inbound, &mut scratch);
        }
        assert!(scratch.candidates.capacity() >= 1);
    }
}
