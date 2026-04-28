//! Abstract model of the bus.
//!
//! For each registered subscription we track:
//! - The handle (or `None` after `DropHandle`).
//! - The receiver (or `None` after `DropReceiver` / drain-to-empty
//!   wouldn't drop it; only the explicit `DropReceiver` op).
//! - The bus-supplied `SubscriptionId`.
//! - The filter (cloned at register time so we don't follow the
//!   bus's internal state).
//! - The channel capacity.
//! - `expected_delivered` — every dispatch where we predicted
//!   delivery would succeed (i.e. queue had room and channel was
//!   alive).
//! - `expected_dropped_full` — every dispatch where we predicted
//!   the queue was full.
//! - `queued` — the model's running count of messages in the
//!   bus-side mpsc that haven't been drained yet. Bumped on
//!   delivery, decremented by `DrainReceiver`.
//! - `handle_dropped` / `receiver_dropped` — observed flags.
//!
//! After applying each op, the comparator asserts:
//! - `bus.dispatch()` aggregate counts match the per-sub sums we
//!   just predicted.
//! - `bus.subscription_stats()` per-sub `delivered` /
//!   `dropped_full` match the model's running totals.
//! - `bus.len()` matches the count of subs whose handle hasn't
//!   been dropped.

use bytes::Bytes;
use tak_bus::{Bus, Filter, GeoBbox, Inbound, SubscriptionHandle, SubscriptionId};
use tokio::sync::mpsc;

#[derive(Debug)]
pub(crate) struct ModelSub {
    pub handle: Option<SubscriptionHandle>,
    pub rx: Option<mpsc::Receiver<Bytes>>,
    pub id: SubscriptionId,
    pub filter: Filter,
    pub capacity: usize,
    pub expected_delivered: u64,
    pub expected_dropped_full: u64,
    pub queued: usize,
    pub handle_dropped: bool,
    pub receiver_dropped: bool,
}

impl ModelSub {
    pub fn handle_alive(&self) -> bool {
        !self.handle_dropped
    }

    /// Predicted outcome for this sub on a single dispatch with the
    /// given Inbound. Returns `Some(true)` for a successful delivery,
    /// `Some(false)` for a dropped_full, `None` if the sub doesn't
    /// match the inbound at all.
    pub fn predict(&self, inbound: &Inbound<'_>) -> Option<bool> {
        if !self.handle_alive() {
            return None; // unsubscribed; bus shouldn't see us
        }
        if !filter_matches(&self.filter, inbound) {
            return None;
        }
        if self.receiver_dropped {
            // try_send returns Err(Closed) — counts as dropped_closed,
            // not dropped_full. Returning None here means the model
            // doesn't expect this sub's per-sub `delivered` /
            // `dropped_full` to change.
            return None;
        }
        if self.queued >= self.capacity {
            Some(false)
        } else {
            Some(true)
        }
    }
}

/// Replicates [`tak_bus::dispatch`]'s exact sub-matching logic so
/// the model and the bus agree on which subs receive what.
pub(crate) fn filter_matches(filter: &Filter, inbound: &Inbound<'_>) -> bool {
    if !type_prefix_matches(filter.type_prefix.as_deref(), inbound.cot_type) {
        return false;
    }
    if !filter.group_mask.intersects(&inbound.sender_groups) {
        return false;
    }
    if let Some(bbox) = &filter.geo_bbox
        && !bbox_contains(bbox, inbound.lat, inbound.lon)
    {
        return false;
    }
    true
}

/// Mirrors the bus's `TypeIndex::insert` + `extend_matches` exactly
/// (`crates/tak-bus/src/index.rs`):
///
/// - `None` matches everything.
/// - `Some(s)` is `.trim()`'d. After trimming, empty string OR
///   `"*"` matches everything (root wildcard slot).
/// - Pattern `a-f-*` is split on `-` to tokens `["a", "f", "*"]`,
///   stored at node `a → f`'s wildcard slot. Lookup walks
///   `cot_type`'s tokens; at each visited node's wildcard slot
///   it collects matches. So `a-*` matches **`a`, `a-f`, `a-f-G`**
///   — anything in or below the `a` subtree, NOT just things
///   starting with `"a-"`.
/// - Exact pattern `a-f-G-U-C` (no trailing `*`) matches only
///   when cot_type's tokens equal it.
///
/// Twice-burned-by-VOPR semantics now pinned down: the original
/// "starts_with(prefix)" was wrong because it missed the
/// "subtree root" case (pattern `a-*` should match `a`).
fn type_prefix_matches(pattern: Option<&str>, cot_type: &str) -> bool {
    let p = match pattern {
        None => return true,
        Some(p) => p.trim(),
    };
    if p.is_empty() || p == "*" {
        return true;
    }
    let pat_tokens: Vec<&str> = p.split('-').collect();
    let cot_tokens: Vec<&str> = cot_type.split('-').collect();

    // Match the bus's per-token walk:
    //
    //   for i in 0..cot_tokens.len():
    //       node = node.children[cot_tokens[i]]   // bail on miss
    //       collect node.wildcard
    //       if i+1 == cot_tokens.len():
    //           collect node.exact
    //
    // A pattern `a-f-*` lives at depth 2 in the wildcard slot;
    // it matches when the walk visits that node, which is true
    // for cot_type `a-f`, `a-f-anything`, etc. — i.e. when
    // pat_tokens[..-1] is a prefix of cot_tokens.
    //
    // A pattern `a-f-G-U-C` (exact, no `*`) lives at depth 5 in
    // the exact slot; it matches when cot_tokens == pat_tokens.
    if pat_tokens.last() == Some(&"*") {
        let pat_prefix = &pat_tokens[..pat_tokens.len() - 1];
        cot_tokens.len() >= pat_prefix.len() && cot_tokens.starts_with(pat_prefix)
    } else {
        cot_tokens == pat_tokens
    }
}

/// Closed-interval containment. `tak_bus`'s GeoBbox::contains uses
/// the same semantics; we replicate it inline so the model doesn't
/// need access to the (possibly internal) helper.
fn bbox_contains(bbox: &GeoBbox, lat: f64, lon: f64) -> bool {
    lat >= bbox.min_lat && lat <= bbox.max_lat && lon >= bbox.min_lon && lon <= bbox.max_lon
}

#[derive(Debug, Default)]
pub(crate) struct Model {
    pub subs: Vec<ModelSub>,
    /// SubscriptionIds we've explicitly dropped, kept around so the
    /// `TrySendToStale` op can pick one and the model can predict
    /// `false`. Bounded; we only keep the most recent N.
    pub dropped_ids: Vec<SubscriptionId>,
}

impl Model {
    pub const STALE_ID_RING_CAP: usize = 32;

    pub fn add_sub(
        &mut self,
        bus: &std::sync::Arc<Bus>,
        filter: Filter,
        capacity: usize,
    ) -> SubscriptionId {
        let (handle, rx) = bus.subscribe_with_capacity(filter.clone(), capacity);
        let id = handle.id();
        self.subs.push(ModelSub {
            handle: Some(handle),
            rx: Some(rx),
            id,
            filter,
            capacity,
            expected_delivered: 0,
            expected_dropped_full: 0,
            queued: 0,
            handle_dropped: false,
            receiver_dropped: false,
        });
        id
    }

    pub fn drop_handle(&mut self, slot: usize) -> Option<SubscriptionId> {
        let s = self.subs.get_mut(slot)?;
        if s.handle_dropped {
            return None;
        }
        s.handle_dropped = true;
        let id = s.id;
        // Hand the SubscriptionHandle to drop here so the bus
        // observes the drop synchronously (Bus::unsubscribe runs
        // inside Drop).
        s.handle.take();
        // Track for stale-id testing.
        self.dropped_ids.push(id);
        if self.dropped_ids.len() > Self::STALE_ID_RING_CAP {
            self.dropped_ids.remove(0);
        }
        Some(id)
    }

    pub fn drop_receiver(&mut self, slot: usize) -> bool {
        let Some(s) = self.subs.get_mut(slot) else {
            return false;
        };
        if s.receiver_dropped {
            return false;
        }
        s.receiver_dropped = true;
        s.rx.take();
        true
    }

    /// Drain up to `max` messages from the receiver. Returns the
    /// actual number drained.
    pub fn drain(&mut self, slot: usize, max: usize) -> usize {
        let Some(s) = self.subs.get_mut(slot) else {
            return 0;
        };
        let Some(rx) = s.rx.as_mut() else {
            return 0;
        };
        let mut drained = 0;
        for _ in 0..max {
            match rx.try_recv() {
                Ok(_) => {
                    drained += 1;
                }
                Err(_) => break,
            }
        }
        s.queued = s.queued.saturating_sub(drained);
        drained
    }

    /// Count of subscriptions whose handle is still alive (i.e.
    /// what `Bus::len()` should report).
    pub fn live_count(&self) -> usize {
        self.subs.iter().filter(|s| s.handle_alive()).count()
    }
}
