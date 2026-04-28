//! The op alphabet + a generator that biases toward edge cases.

use bytes::Bytes;
use rand::Rng;
use rand::seq::SliceRandom;
use rand_chacha::ChaCha8Rng;
use tak_bus::{Filter, GeoBbox, GroupBitvector, Inbound};

/// The operations the harness drives. `slot_index` references the
/// model's vector of subscription handles; the model resolves it to
/// a concrete `SubscriptionId` when applying the op.
#[derive(Debug, Clone)]
pub(crate) enum Op {
    /// Register a new subscription.
    Subscribe { filter: Filter, capacity: usize },
    /// Fan out one inbound. The model computes the expected
    /// (delivered, dropped_full) and compares to the bus's report.
    Dispatch { inbound: OwnedInbound },
    /// Drop the SubscriptionHandle at `slot`. Must remove the entry
    /// from the bus and decrement `len()`.
    DropHandle { slot: usize },
    /// Drain up to `max` messages from the receiver at `slot`. The
    /// model's queued count drops by however many the bus actually
    /// produced; we sanity-check that count is non-negative.
    DrainReceiver { slot: usize, max: usize },
    /// Drop the receiver while keeping the handle alive. Future
    /// dispatches to this sub should report `dropped_closed`.
    DropReceiver { slot: usize },
    /// Call `Bus::try_send_to` with the SubscriptionId of a
    /// previously-dropped sub. Must return `false` and not bump
    /// any counters (ABA defense).
    TrySendToStale,
    /// Take a `Bus::subscription_stats()` snapshot and assert
    /// per-sub agreement with the model.
    SnapshotStats,
}

/// `Inbound<'a>` borrows everything; the harness needs an owned
/// version so a single `Op` stays serializable / replayable.
#[derive(Debug, Clone)]
pub(crate) struct OwnedInbound {
    pub payload: Bytes,
    pub sender_groups: GroupBitvector,
    pub cot_type: String,
    pub lat: f64,
    pub lon: f64,
    pub uid: Option<String>,
    pub callsign: Option<String>,
}

impl OwnedInbound {
    pub fn as_borrowed(&self) -> Inbound<'_> {
        Inbound {
            payload: self.payload.clone(),
            sender_groups: self.sender_groups,
            cot_type: &self.cot_type,
            lat: self.lat,
            lon: self.lon,
            uid: self.uid.as_deref(),
            callsign: self.callsign.as_deref(),
        }
    }
}

// ---------------------------------------------------------------------------
// Generator
// ---------------------------------------------------------------------------

/// Hand-picked CoT type strings covering exact, prefix, and
/// extension cases.
const COT_TYPES: &[&str] = &[
    "a-f-G-U-C",
    "a-f-G-U-C-I",
    "a-h-G-U-C",
    "b-m-p-w",
    "t-x-c-t",
    "u-d-r",
    // Edge cases:
    "a",
    "",
];

const COT_TYPE_PREFIXES: &[&str] = &[
    // Wildcard everything (most common in real ATAK):
    "*",
    // Exact-match prefixes (no wildcard suffix — bus treats these
    // as exact matches):
    "a-f-G-U-C",
    "b-m-p-w",
    // Trailing-wildcard prefixes:
    "a-f-*",
    "a-*",
    "b-*",
    // Edge: empty prefix
    "",
];

const UIDS: &[&str] = &["ANDROID-aaaa", "ANDROID-bbbb", "ANDROID-cccc", ""];

const CALLSIGNS: &[&str] = &["VIPER01", "RANGER02", "SHADOW03", ""];

/// Generate a Filter that may include any combination of: type
/// prefix, group mask (sometimes empty — H4 edge), geo bbox
/// (sometimes containing the canonical (0,0) test point, sometimes
/// not), interest_uid, interest_callsign.
pub(crate) fn gen_filter(rng: &mut ChaCha8Rng) -> Filter {
    let group_mask = if rng.gen_bool(0.05) {
        // Empty group_mask — sub never matches anything. Tests
        // the H4 short-circuit + ensures we don't deliver to it.
        GroupBitvector::EMPTY
    } else if rng.gen_bool(0.05) {
        // ALL — matches everyone.
        GroupBitvector::ALL
    } else {
        // Random sparse.
        let mut bv = GroupBitvector::EMPTY;
        let bits = rng.gen_range(1..=4);
        for _ in 0..bits {
            bv = bv.with_bit(rng.gen_range(0..256));
        }
        bv
    };

    let type_prefix = if rng.gen_bool(0.4) {
        None
    } else {
        Some((*COT_TYPE_PREFIXES.choose(rng).unwrap()).to_owned())
    };

    let geo_bbox = if rng.gen_bool(0.7) {
        None
    } else {
        // Build a bbox; sometimes degenerate (min == max).
        if rng.gen_bool(0.1) {
            Some(GeoBbox {
                min_lat: 0.0,
                max_lat: 0.0,
                min_lon: 0.0,
                max_lon: 0.0,
            })
        } else {
            let lat = rng.gen_range(-89.0..89.0);
            let lon = rng.gen_range(-179.0..179.0);
            let dlat = rng.gen_range(0.0..1.0);
            let dlon = rng.gen_range(0.0..1.0);
            Some(GeoBbox {
                min_lat: lat,
                max_lat: lat + dlat,
                min_lon: lon,
                max_lon: lon + dlon,
            })
        }
    };

    let interest_uid = if rng.gen_bool(0.1) {
        Some((*UIDS.choose(rng).unwrap()).to_owned())
    } else {
        None
    };
    let interest_callsign = if rng.gen_bool(0.1) {
        Some((*CALLSIGNS.choose(rng).unwrap()).to_owned())
    } else {
        None
    };

    Filter {
        interest_uid,
        interest_callsign,
        group_mask,
        type_prefix,
        geo_bbox,
    }
}

/// Generate an Inbound. Includes the boundary points (0,0), poles,
/// equator. CoT type drawn from the corpus. Sender groups
/// occasionally EMPTY (matches no subs) and ALL (matches all).
pub(crate) fn gen_inbound(rng: &mut ChaCha8Rng, payload: Bytes) -> OwnedInbound {
    let sender_groups = if rng.gen_bool(0.05) {
        GroupBitvector::EMPTY
    } else if rng.gen_bool(0.1) {
        GroupBitvector::ALL
    } else {
        let mut bv = GroupBitvector::EMPTY;
        for _ in 0..rng.gen_range(1..=8) {
            bv = bv.with_bit(rng.gen_range(0..256));
        }
        bv
    };

    let cot_type = (*COT_TYPES.choose(rng).unwrap()).to_owned();

    // Mix: random points, the (0,0) boundary, north pole.
    let (lat, lon) = match rng.gen_range(0..10) {
        0 => (0.0, 0.0),
        1 => (90.0, 0.0),
        2 => (-90.0, 0.0),
        3 => (0.0, 180.0),
        4 => (0.0, -180.0),
        _ => (
            rng.gen_range(-89.999..89.999),
            rng.gen_range(-179.999..179.999),
        ),
    };

    let uid = if rng.gen_bool(0.5) {
        Some((*UIDS.choose(rng).unwrap()).to_owned())
    } else {
        None
    };
    let callsign = if rng.gen_bool(0.3) {
        Some((*CALLSIGNS.choose(rng).unwrap()).to_owned())
    } else {
        None
    };

    OwnedInbound {
        payload,
        sender_groups,
        cot_type,
        lat,
        lon,
        uid,
        callsign,
    }
}

/// Capacity drawn from a distribution that includes the pathological
/// `1` (forces Full immediately on a non-draining receiver) more
/// often than uniform random would.
pub(crate) fn gen_capacity(rng: &mut ChaCha8Rng) -> usize {
    match rng.gen_range(0..10) {
        0 => 1,
        1 => 2,
        2 => 4,
        3 | 4 => 16,
        _ => 1024,
    }
}
