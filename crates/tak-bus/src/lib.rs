//! Subscription registry and message fan-out — the firehose core.
//!
//! Pure routing: `(message)` in, `(subscriber_id, message)*` out. No sockets,
//! no storage. Architecture in `docs/architecture.md` §5.3.
//!
//! Hot-path invariants H1-H6 (`docs/invariants.md`) live here:
//! - **H1** dispatch is alloc-free in steady state (dhat test enforces; #26).
//! - **H3** fan-out is `Bytes::clone` (Arc bump), never `Vec::clone`.
//! - **H4** group AND is `[u64; 4]`, not arbitrary bigint.
//! - **H5** per-subscription mpsc is bounded.
//!
//! # Layout (per-issue scope)
//!
//! - **#23 (this issue):** `Bus` + `SubscriptionHandle` registry, drop-on-handle
//!   unsubscribe. Filters carry `uid` / `callsign` / `group_mask` only.
//! - **#24:** type-prefix trie + geo R-tree filter indices on top of the registry.
//! - **#25:** per-subscription bounded mpsc + dispatch loop.
//! - **#26:** `dhat` alloc-free dispatch verification.
//! - **#27:** `loom` model-check on concurrent subscribe + dispatch + unsub.
//!
//! # Example
//! ```
//! use std::sync::Arc;
//! use tak_bus::{Bus, Filter, GroupBitvector};
//!
//! let bus = Bus::new();
//! let filter = Filter {
//!     interest_uid: Some("ANDROID-deadbeef".to_owned()),
//!     interest_callsign: None,
//!     group_mask: GroupBitvector::EMPTY.with_bit(3),
//! };
//! let handle = bus.subscribe(filter);
//! assert_eq!(bus.len(), 1);
//! drop(handle);
//! assert_eq!(bus.len(), 0);
//! ```
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented
    )
)]
#![warn(missing_docs, missing_debug_implementations)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use sharded_slab::Slab;

// ---------------------------------------------------------------------------
// GroupBitvector — invariant H4
// ---------------------------------------------------------------------------

/// Group-membership bitvector. Fixed-width `[u64; 4]` = 256 groups.
///
/// See invariant **H4** in `docs/invariants.md`. Per-message dispatch performs
/// one `intersects` per candidate subscription on the hot path; the const-width
/// `[u64; 4]` lets this lower to ~4 instructions vs Java's `BigInteger.and()`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupBitvector(pub [u64; 4]);

impl GroupBitvector {
    /// Empty bitvector — matches no groups.
    pub const EMPTY: Self = Self([0; 4]);

    /// All groups set — matches anything (use sparingly; this is "admin").
    pub const ALL: Self = Self([u64::MAX; 4]);

    /// Returns `true` if any group is in common between `self` and `other`.
    /// Hot-path predicate; intentionally branch-free.
    #[inline]
    #[must_use]
    pub const fn intersects(&self, other: &Self) -> bool {
        let lo = (self.0[0] & other.0[0]) | (self.0[1] & other.0[1]);
        let hi = (self.0[2] & other.0[2]) | (self.0[3] & other.0[3]);
        (lo | hi) != 0
    }

    /// Set bit `n` (0..256). Out-of-range bits are ignored.
    #[inline]
    pub const fn with_bit(mut self, n: usize) -> Self {
        if n < 256 {
            self.0[n / 64] |= 1u64 << (n % 64);
        }
        self
    }
}

// ---------------------------------------------------------------------------
// Subscription identity + filter
// ---------------------------------------------------------------------------

/// Stable handle for a registered subscription, returned by [`Bus::subscribe`].
///
/// Uniquely identifies a subscription within a single `Bus`. The internal
/// shape is opaque; treat it as an opaque token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriptionId {
    /// Slab key.
    slab_key: usize,
    /// Generation counter — defends against ABA on slab-key reuse.
    generation: u64,
}

/// Compiled subscription filter.
///
/// At #23 scope this carries the unconditional fields:
///
/// - `interest_uid` — direct UID match (overrides type/geo when set, M3).
/// - `interest_callsign` — direct callsign match.
/// - `group_mask` — the subscriber's group membership; messages whose
///   sender is in any of these groups are candidates for delivery.
///
/// Issue #24 adds:
/// - `type_prefix` — CoT-type glob (e.g. `a-f-G-*`).
/// - `geo_bbox` — geographic bounding box.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Direct interest in messages whose `event.uid` matches this string.
    pub interest_uid: Option<String>,
    /// Direct interest in messages whose `contact.callsign` matches.
    pub interest_callsign: Option<String>,
    /// Subscriber's group bitvector — gated by `intersects` against the
    /// sender's group bitvector at dispatch time.
    pub group_mask: GroupBitvector,
}

/// Stored per-subscription state. Internals only — public API hands out
/// [`SubscriptionHandle`] which references this opaquely.
#[derive(Debug)]
struct Entry {
    filter: Filter,
    /// Generation tag, set on insert; used to make [`SubscriptionId`]
    /// unable to alias a freshly-reused slab slot.
    generation: u64,
}

// ---------------------------------------------------------------------------
// Bus
// ---------------------------------------------------------------------------

/// The subscription registry.
///
/// Storage is a [`sharded_slab::Slab`] keyed by `usize` — lock-free
/// append-only with concurrent reads, perfect for "many readers + occasional
/// writers" which is the live-connection pattern at scale. A monotonic
/// generation counter is bumped on every insert; the [`SubscriptionId`]
/// includes this so a stale handle from a since-released slot doesn't
/// accidentally match a new subscription that reused the slot.
///
/// The bus is `Arc`'d at construction so handles can hold a `Weak<Bus>`
/// without a reference cycle.
#[derive(Debug)]
pub struct Bus {
    subs: Slab<Entry>,
    next_generation: AtomicU64,
    live: AtomicUsize,
}

impl Bus {
    /// Construct a new empty bus, wrapped in `Arc`.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            subs: Slab::new(),
            next_generation: AtomicU64::new(0),
            live: AtomicUsize::new(0),
        })
    }

    /// Register a subscription. Returns a [`SubscriptionHandle`] whose
    /// `Drop` unsubscribes; keep it alive for the lifetime of the connection.
    #[must_use]
    pub fn subscribe(self: &Arc<Self>, filter: Filter) -> SubscriptionHandle {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);

        let entry = Entry { filter, generation };
        let key = match self.subs.insert(entry) {
            Some(k) => k,
            None => {
                // The default sharded_slab config caps at usize::MAX which is
                // effectively unreachable; if we ever hit this in practice it
                // means a custom Config was used and is exhausted. Return a
                // sentinel handle that's a no-op on Drop — better than panic
                // in lib code (D1).
                return SubscriptionHandle {
                    id: SubscriptionId {
                        slab_key: usize::MAX,
                        generation,
                    },
                    bus: Weak::new(),
                };
            }
        };

        self.live.fetch_add(1, Ordering::Relaxed);

        SubscriptionHandle {
            id: SubscriptionId {
                slab_key: key,
                generation,
            },
            bus: Arc::downgrade(self),
        }
    }

    /// Look up a subscription's filter by id, if still live. Returns `None`
    /// if the subscription was dropped or the id is stale (slot reused).
    #[must_use]
    pub fn get_filter(&self, id: SubscriptionId) -> Option<Filter> {
        let entry = self.subs.get(id.slab_key)?;
        if entry.generation != id.generation {
            return None; // stale id; slot was reused by a newer subscription
        }
        Some(entry.filter.clone())
    }

    /// Number of live subscriptions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// True iff there are no live subscriptions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn unsubscribe(&self, id: SubscriptionId) {
        // Verify generation before removing — defensively guards against
        // a doubled-Drop or stale handle from a since-reused slot.
        let still_ours = self
            .subs
            .get(id.slab_key)
            .is_some_and(|e| e.generation == id.generation);
        if still_ours && self.subs.remove(id.slab_key) {
            self.live.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// RAII guard for a registered subscription. Drop unregisters the
/// subscription from its `Bus`; the bus's `live` count decrements.
///
/// `SubscriptionHandle` holds a `Weak<Bus>` so a handle outliving its bus
/// (e.g., during shutdown) is a no-op rather than a use-after-free.
#[derive(Debug)]
pub struct SubscriptionHandle {
    id: SubscriptionId,
    bus: Weak<Bus>,
}

impl SubscriptionHandle {
    /// The opaque id assigned to this subscription.
    #[must_use]
    pub fn id(&self) -> SubscriptionId {
        self.id
    }

    /// True if the bus is still alive (i.e., dropping the handle will
    /// actually unsubscribe).
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.bus.strong_count() > 0
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        if let Some(bus) = self.bus.upgrade() {
            bus.unsubscribe(self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn flt(uid: &str) -> Filter {
        Filter {
            interest_uid: Some(uid.to_owned()),
            interest_callsign: None,
            group_mask: GroupBitvector::EMPTY,
        }
    }

    #[test]
    fn subscribe_then_drop_removes_entry() {
        let bus = Bus::new();
        assert_eq!(bus.len(), 0);
        let h = bus.subscribe(flt("ANDROID-1"));
        assert_eq!(bus.len(), 1);
        drop(h);
        assert_eq!(bus.len(), 0);
    }

    #[test]
    fn multiple_subs_get_distinct_ids() {
        let bus = Bus::new();
        let h1 = bus.subscribe(flt("a"));
        let h2 = bus.subscribe(flt("b"));
        let h3 = bus.subscribe(flt("c"));
        assert_ne!(h1.id(), h2.id());
        assert_ne!(h2.id(), h3.id());
        assert_ne!(h1.id(), h3.id());
        assert_eq!(bus.len(), 3);
    }

    #[test]
    fn get_filter_returns_some_for_live_id() {
        let bus = Bus::new();
        let h = bus.subscribe(flt("alpha"));
        let f = bus.get_filter(h.id()).unwrap();
        assert_eq!(f.interest_uid.as_deref(), Some("alpha"));
    }

    #[test]
    fn get_filter_none_for_dropped_handle() {
        let bus = Bus::new();
        let h = bus.subscribe(flt("alpha"));
        let id = h.id();
        drop(h);
        assert!(bus.get_filter(id).is_none());
    }

    #[test]
    fn stale_id_after_slot_reuse_returns_none() {
        // Subscribe, capture id, drop. Now subscribe again — sharded-slab
        // may reuse the same slab_key, but the gen tag should differ, so
        // the OLD id should still resolve to None.
        let bus = Bus::new();
        let h1 = bus.subscribe(flt("first"));
        let stale = h1.id();
        drop(h1);

        // Force enough churn that slab is likely to reuse the slot.
        let _h2 = bus.subscribe(flt("second"));
        // Whether or not slab_key matches, the gen has advanced.
        assert!(
            bus.get_filter(stale).is_none(),
            "stale id must NOT alias a fresh subscription"
        );
    }

    #[test]
    fn handle_after_bus_dropped_is_a_no_op() {
        // Construct the bus, subscribe, drop the bus (forcing strong_count
        // to 0), then drop the handle. Should not crash; is_attached() is
        // false.
        let bus = Bus::new();
        let h = bus.subscribe(flt("orphan"));
        drop(bus);
        assert!(!h.is_attached());
        // Drop happens at end of scope — should be silent.
    }

    #[test]
    fn group_intersect_bitvector() {
        let red = GroupBitvector([0b0001, 0, 0, 0]);
        let blue = GroupBitvector([0b0010, 0, 0, 0]);
        let red_blue = GroupBitvector([0b0011, 0, 0, 0]);
        assert!(red.intersects(&red_blue));
        assert!(blue.intersects(&red_blue));
        assert!(!red.intersects(&blue));
    }
}
