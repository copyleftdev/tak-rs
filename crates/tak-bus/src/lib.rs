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
//!     group_mask: GroupBitvector::EMPTY.with_bit(3),
//!     ..Filter::default()
//! };
//! let (handle, _rx) = bus.subscribe(filter);
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

use bytes::Bytes;
use parking_lot::RwLock;
use sharded_slab::Slab;
use tokio::sync::mpsc;

pub mod dispatch;
pub mod index;
pub use dispatch::{DispatchScratch, DispatchStats, Inbound};
pub use index::{GeoBbox, GeoIndex, TypeIndex};

/// Default capacity for per-subscription outbound channels (invariant H5).
pub const DEFAULT_SUBSCRIBER_CAPACITY: usize = 1024;

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

/// Snapshot of one subscription's delivery counters, returned by
/// [`Bus::subscription_stats`].
///
/// Both counters are monotonic since the subscription was registered.
/// The `id.slab_key` field is **not** populated by
/// `subscription_stats` (sharded-slab's `unique_iter` doesn't expose
/// the key); only `generation` is preserved. Callers comparing
/// two snapshots should match on generation, not slab_key.
#[derive(Debug, Clone, Copy)]
pub struct SubscriptionStats {
    /// The subscription's id. `slab_key` is unset; see struct docs.
    pub id: SubscriptionId,
    /// Total successful deliveries since the subscription registered.
    pub delivered: u64,
    /// Total `try_send`-Full drops since the subscription registered.
    pub dropped_full: u64,
}

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
/// - `interest_uid` — direct UID match (overrides type/geo when set, M3).
/// - `interest_callsign` — direct callsign match.
/// - `group_mask` — the subscriber's group membership; messages whose
///   sender is in any of these groups are candidates for delivery.
/// - `type_prefix` — CoT-type pattern. `None` or `Some("*")` matches all
///   types. Otherwise a hyphen-separated CoT type with optional terminal
///   `*` wildcard (e.g. `a-f-G-*`).
/// - `geo_bbox` — geographic bounding box. `None` matches any location.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Direct interest in messages whose `event.uid` matches this string.
    pub interest_uid: Option<String>,
    /// Direct interest in messages whose `contact.callsign` matches.
    pub interest_callsign: Option<String>,
    /// Subscriber's group bitvector — gated by `intersects` against the
    /// sender's group bitvector at dispatch time.
    pub group_mask: GroupBitvector,
    /// CoT-type pattern. `None` matches any type.
    pub type_prefix: Option<String>,
    /// Geographic bounding box. `None` matches any location.
    pub geo_bbox: Option<GeoBbox>,
}

/// Stored per-subscription state. Internals only — public API hands out
/// [`SubscriptionHandle`] which references this opaquely.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) filter: Filter,
    /// Generation tag, set on insert; used to make [`SubscriptionId`]
    /// unable to alias a freshly-reused slab slot.
    pub(crate) generation: u64,
    /// Per-subscription outbound channel. Bounded per invariant H5;
    /// dispatch uses `try_send` and drops on full.
    pub(crate) sender: mpsc::Sender<Bytes>,
    /// Total successful `try_send`s into this subscription's channel.
    /// Hot-path counter — local AtomicU64 so dispatch threads only
    /// touch the cache line for the subscription they're delivering
    /// to, not a shared global atomic. Read out via
    /// [`Bus::subscription_stats`] for observability.
    pub(crate) delivered: AtomicU64,
    /// Total `try_send`s that hit `Full`. Same hot-path / cache-line
    /// rationale as `delivered`.
    pub(crate) dropped_full: AtomicU64,
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
    type_index: RwLock<TypeIndex>,
    geo_index: RwLock<GeoIndex>,
    next_generation: AtomicU64,
    live: AtomicUsize,
    /// Side-table of live SubscriptionIds. Touched only on
    /// subscribe/unsubscribe (rare relative to dispatch); read by
    /// [`Self::subscription_stats`] for the slow observability
    /// path. Dispatch never touches this — it walks the
    /// type/geo indices instead, so this lock is never on the
    /// H1 hot path.
    live_ids: RwLock<Vec<SubscriptionId>>,
}

impl Bus {
    /// Construct a new empty bus, wrapped in `Arc`.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            subs: Slab::new(),
            type_index: RwLock::new(TypeIndex::default()),
            geo_index: RwLock::new(GeoIndex::default()),
            next_generation: AtomicU64::new(0),
            live: AtomicUsize::new(0),
            live_ids: RwLock::new(Vec::new()),
        })
    }

    /// Register a subscription with the default channel capacity
    /// ([`DEFAULT_SUBSCRIBER_CAPACITY`]). Convenience wrapper for
    /// [`Self::subscribe_with_capacity`].
    #[must_use]
    #[allow(clippy::needless_pass_by_value)] // by-value matches caller intent (we own it now)
    pub fn subscribe(
        self: &Arc<Self>,
        filter: Filter,
    ) -> (SubscriptionHandle, mpsc::Receiver<Bytes>) {
        self.subscribe_with_capacity(filter, DEFAULT_SUBSCRIBER_CAPACITY)
    }

    /// Register a subscription with an explicit per-channel capacity.
    /// Returns a [`SubscriptionHandle`] (Drop unsubscribes) and the
    /// `Receiver` end of the per-subscription bounded mpsc.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn subscribe_with_capacity(
        self: &Arc<Self>,
        filter: Filter,
        capacity: usize,
    ) -> (SubscriptionHandle, mpsc::Receiver<Bytes>) {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(capacity);

        let entry = Entry {
            filter: filter.clone(),
            generation,
            sender: tx,
            delivered: AtomicU64::new(0),
            dropped_full: AtomicU64::new(0),
        };
        let key = match self.subs.insert(entry) {
            Some(k) => k,
            None => {
                // Slab full (only reachable on a custom-cap config that's
                // exhausted). Lib-deny-panic discipline (D1) — return a
                // sentinel handle whose Weak is dead.
                return (
                    SubscriptionHandle {
                        id: SubscriptionId {
                            slab_key: usize::MAX,
                            generation,
                        },
                        bus: Weak::new(),
                    },
                    rx,
                );
            }
        };
        let id = SubscriptionId {
            slab_key: key,
            generation,
        };

        // Index by type — every subscription is in the type trie. A None
        // pattern goes into the root wildcard so it matches every type.
        {
            let pattern = filter.type_prefix.as_deref().unwrap_or("*");
            self.type_index.write().insert(pattern, id);
        }
        // Index by geo — only subscriptions WITH a bbox are in the rtree.
        if let Some(bbox) = filter.geo_bbox {
            self.geo_index.write().insert(bbox, id);
        }

        self.live.fetch_add(1, Ordering::Relaxed);
        self.live_ids.write().push(id);

        (
            SubscriptionHandle {
                id,
                bus: Arc::downgrade(self),
            },
            rx,
        )
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

    /// Append candidate subscription IDs that *may* match an event with the
    /// given CoT type at `(lat, lon)`. The candidate set is a SUPERSET of
    /// the actual matches — the caller still applies the group bitvector
    /// test and any direct UID/callsign override.
    ///
    /// `out` is caller-owned to satisfy invariant **H1** (alloc-free
    /// dispatch in steady state). The caller is responsible for clearing
    /// `out` between dispatches; this function only appends.
    pub fn extend_candidates(
        &self,
        cot_type: &str,
        lat: f64,
        lon: f64,
        out: &mut Vec<SubscriptionId>,
    ) {
        // Type matches include subs with a `None` type filter (under the
        // root wildcard). We walk the trie under a read lock, so the
        // dispatch path takes only one read lock per call.
        self.type_index.read().extend_matches(cot_type, out);

        // Geo: subs with no bbox are NOT in the geo index. We need to
        // include them as candidates regardless of location, so we DON'T
        // intersect with geo here — instead, the caller applies the per-
        // candidate geo test via `Filter::geo_bbox` at dispatch time.
        // Subs WITH a bbox that happens to contain (lat, lon) come in
        // via the type index already; the geo index is consulted as a
        // *narrower* index for queries that originated geographically
        // (e.g., "who's interested in events near here?") rather than
        // for type-based dispatch.
        //
        // For type-based dispatch (the firehose path), the type index
        // alone is sufficient as a candidate-superset producer; the
        // alloc-free per-candidate filter check by the caller is fast.

        // Mark `lat`/`lon` as deliberately not-yet-used here to suppress
        // the unused-variable lint without weakening the API. They become
        // load-bearing in #25 when dispatch wires this in with the
        // alternative geo-keyed candidate path.
        let _ = (lat, lon);
    }

    /// Append candidate subscription IDs whose geo bbox contains
    /// `(lat, lon)`. Subscriptions without a bbox are NOT included by
    /// this function (they're not in the geo index). Used by callers
    /// that want a geo-narrowed candidate set — currently called only
    /// by tests and benches; #25's dispatch will combine with type
    /// matches as appropriate.
    pub fn extend_geo_candidates(&self, lat: f64, lon: f64, out: &mut Vec<SubscriptionId>) {
        self.geo_index.read().extend_matches(lat, lon, out);
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

    /// Push one payload to a single subscription's mpsc, bypassing
    /// the fan-out dispatch path.
    ///
    /// Used by the firehose's replay-on-reconnect path: when a new
    /// connection subscribes, the server queries `cot_router` for
    /// recent events and unicasts each frame to *only* that new
    /// subscription. The bus's normal dispatch fans out events to
    /// every matching sub, which is the wrong behavior here — we
    /// want the replay to reach exactly one client.
    ///
    /// Returns:
    /// - `true` if the frame was accepted by the channel.
    /// - `false` if the subscription is gone, the channel is full
    ///   (per-sub `dropped_full` is bumped), or the channel was
    ///   already closed.
    ///
    /// Off the H1 hot path — replay queries are per-connection, run
    /// inside the accept handler, not the per-message dispatch loop.
    pub fn try_send_to(&self, id: SubscriptionId, payload: Bytes) -> bool {
        let Some(entry) = self.subs.get(id.slab_key) else {
            return false;
        };
        if entry.generation != id.generation {
            return false;
        }
        match entry.sender.try_send(payload) {
            Ok(()) => {
                entry.delivered.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                entry.dropped_full.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Snapshot per-subscription delivery + drop counts.
    ///
    /// Walks the side-table of live ids once, reading two relaxed
    /// atomics per subscription. Off the dispatch hot path — call
    /// from a periodic observability tick (every few seconds), not
    /// from per-message work.
    ///
    /// Counts are monotonic since the subscription was registered;
    /// callers compute deltas across two snapshots if they want
    /// rate-style numbers.
    #[must_use]
    pub fn subscription_stats(&self) -> Vec<SubscriptionStats> {
        let ids = self.live_ids.read();
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids.iter() {
            let Some(entry) = self.subs.get(id.slab_key) else {
                continue;
            };
            // Stale id race: subscription dropped + slot reused
            // between our `live_ids` snapshot and the slab read.
            if entry.generation != id.generation {
                continue;
            }
            out.push(SubscriptionStats {
                id,
                delivered: entry.delivered.load(Ordering::Relaxed),
                dropped_full: entry.dropped_full.load(Ordering::Relaxed),
            });
        }
        out
    }

    fn unsubscribe(&self, id: SubscriptionId) {
        // Verify generation and copy out the filter fields we need to
        // remove from the indices, then remove from slab.
        let (still_ours, type_pattern, geo_bbox) = {
            let Some(entry) = self.subs.get(id.slab_key) else {
                return;
            };
            if entry.generation != id.generation {
                return;
            }
            let pat = entry
                .filter
                .type_prefix
                .clone()
                .unwrap_or_else(|| "*".to_owned());
            (true, pat, entry.filter.geo_bbox)
        };
        if !still_ours {
            return;
        }

        // De-index BEFORE removing from slab so a concurrent dispatch can't
        // resolve a candidate id whose slab slot has been emptied.
        self.type_index.write().remove(&type_pattern, id);
        if let Some(bbox) = geo_bbox {
            self.geo_index.write().remove(bbox, id);
        }

        if self.subs.remove(id.slab_key) {
            self.live.fetch_sub(1, Ordering::Relaxed);
            // Best-effort drop from the live-ids side table. O(N)
            // but only fires on unsubscribe (rare), and N is the
            // live subscription count.
            let mut ids = self.live_ids.write();
            if let Some(pos) = ids.iter().position(|&i| i == id) {
                ids.swap_remove(pos);
            }
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
            ..Filter::default()
        }
    }

    #[test]
    fn subscribe_then_drop_removes_entry() {
        let bus = Bus::new();
        assert_eq!(bus.len(), 0);
        let (h, _rx) = bus.subscribe(flt("ANDROID-1"));
        assert_eq!(bus.len(), 1);
        drop(h);
        assert_eq!(bus.len(), 0);
    }

    #[test]
    fn multiple_subs_get_distinct_ids() {
        let bus = Bus::new();
        let (h1, _r1) = bus.subscribe(flt("a"));
        let (h2, _r2) = bus.subscribe(flt("b"));
        let (h3, _r3) = bus.subscribe(flt("c"));
        assert_ne!(h1.id(), h2.id());
        assert_ne!(h2.id(), h3.id());
        assert_ne!(h1.id(), h3.id());
        assert_eq!(bus.len(), 3);
    }

    #[test]
    fn get_filter_returns_some_for_live_id() {
        let bus = Bus::new();
        let (h, _rx) = bus.subscribe(flt("alpha"));
        let f = bus.get_filter(h.id()).unwrap();
        assert_eq!(f.interest_uid.as_deref(), Some("alpha"));
    }

    #[test]
    fn get_filter_none_for_dropped_handle() {
        let bus = Bus::new();
        let (h, _rx) = bus.subscribe(flt("alpha"));
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
        let (h1, _r1) = bus.subscribe(flt("first"));
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
        let (h, _rx) = bus.subscribe(flt("orphan"));
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
