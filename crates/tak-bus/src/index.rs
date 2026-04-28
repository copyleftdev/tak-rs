//! Filter indices: type-prefix trie + geo R-tree.
//!
//! Both indices share the contract: a candidate set returned from them is a
//! **superset** of the actual matches. The caller still applies the group
//! bitvector test (and direct UID/callsign overrides) — these indices just
//! narrow the search space cheaply.
//!
//! # Type-prefix trie
//!
//! CoT type codes are hyphen-separated tokens (`a-f-G-U-C`, `b-t-f`,
//! `u-d-r`). Subscribers may register either an exact pattern
//! (`a-f-G-U-C`) or a terminal-wildcard pattern (`a-f-G-*`). At query
//! time, walking the trie level-by-level we accumulate any subscribers
//! whose terminal `*` falls at or before the current depth, plus exact
//! matches when we reach the final token.
//!
//! Mid-string wildcards (`a-*-G`) are intentionally not supported — they
//! would force a fan-out across all children at every wildcard level,
//! defeating the index's purpose. CoT in practice uses prefix wildcards
//! only.
//!
//! # Geo R-tree
//!
//! Subscribers with a geographic interest register a `(min_lat, min_lon)
//! / (max_lat, max_lon)` bounding box. At dispatch time we ask the
//! R-tree which boxes contain the message's `(lat, lon)` point —
//! `rstar`'s `locate_all_at_point` returns these in O(log N) average.
//!
//! Subscribers with NO geographic interest are not in the R-tree at all;
//! the [`super::Bus::extend_candidates`] caller treats them as
//! always-matching for the geo dimension.

use std::collections::HashMap;

use rstar::{AABB, RTree, RTreeObject};

use crate::SubscriptionId;

// ---------------------------------------------------------------------------
// Type-prefix trie
// ---------------------------------------------------------------------------

/// A trie indexing subscription IDs by their CoT-type pattern.
///
/// A subscription is registered with [`insert`](Self::insert) under either
/// an exact pattern (`a-f-G-U-C`) or a terminal-wildcard pattern
/// (`a-f-G-*`). Empty pattern (`""`) and bare `*` both register at the
/// root as match-everything.
#[derive(Debug, Default)]
pub struct TypeIndex {
    root: TypeNode,
}

#[derive(Debug, Default)]
struct TypeNode {
    children: HashMap<String, TypeNode>,
    /// IDs whose pattern terminated at this node without a wildcard
    /// (i.e., exact match at this depth).
    exact: Vec<SubscriptionId>,
    /// IDs whose pattern was `<this prefix>-*` — match anything at or below.
    wildcard: Vec<SubscriptionId>,
}

impl TypeIndex {
    /// Insert `id` against `pattern`.
    ///
    /// `pattern` is `""` or `"*"` for "match all", otherwise hyphen-separated
    /// tokens optionally ending in `"*"` for terminal-wildcard.
    pub fn insert(&mut self, pattern: &str, id: SubscriptionId) {
        let pat = pattern.trim();
        if pat.is_empty() || pat == "*" {
            self.root.wildcard.push(id);
            return;
        }
        let mut node = &mut self.root;
        let mut tokens = pat.split('-').peekable();
        while let Some(tok) = tokens.next() {
            let last = tokens.peek().is_none();
            if last && tok == "*" {
                node.wildcard.push(id);
                return;
            }
            node = node.children.entry(tok.to_owned()).or_default();
            if last {
                node.exact.push(id);
                return;
            }
        }
    }

    /// Remove `id` from `pattern`. No-op if not present.
    pub fn remove(&mut self, pattern: &str, id: SubscriptionId) {
        let pat = pattern.trim();
        if pat.is_empty() || pat == "*" {
            self.root.wildcard.retain(|&x| x != id);
            return;
        }
        let mut node = &mut self.root;
        let mut tokens = pat.split('-').peekable();
        while let Some(tok) = tokens.next() {
            let last = tokens.peek().is_none();
            if last && tok == "*" {
                node.wildcard.retain(|&x| x != id);
                return;
            }
            match node.children.get_mut(tok) {
                Some(child) => node = child,
                None => return, // pattern not present
            }
            if last {
                node.exact.retain(|&x| x != id);
                return;
            }
        }
    }

    /// Append all subscription IDs matching `cot_type` to `out`.
    ///
    /// Caller-owned buffer per the alloc-free hot-path discipline.
    pub fn extend_matches(&self, cot_type: &str, out: &mut Vec<SubscriptionId>) {
        // Root wildcard ("*") matches every type.
        out.extend_from_slice(&self.root.wildcard);

        let mut node = &self.root;
        let tokens: smallvec::SmallVec<[&str; 8]> = cot_type.split('-').collect();
        for (i, tok) in tokens.iter().enumerate() {
            match node.children.get(*tok) {
                Some(child) => {
                    node = child;
                    out.extend_from_slice(&node.wildcard);
                    if i + 1 == tokens.len() {
                        out.extend_from_slice(&node.exact);
                    }
                }
                None => break,
            }
        }
    }

    /// Total number of indexed entries. O(N) — for tests/diagnostics only.
    #[must_use]
    pub fn len(&self) -> usize {
        fn walk(n: &TypeNode) -> usize {
            n.exact.len() + n.wildcard.len() + n.children.values().map(walk).sum::<usize>()
        }
        walk(&self.root)
    }

    /// True iff no entries are indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// Geo R-tree
// ---------------------------------------------------------------------------

/// A geographic bounding box (`(min_lat, min_lon)` to `(max_lat, max_lon)`).
///
/// Coordinates are WGS-84 decimal degrees. The R-tree treats `lon` as the
/// X axis and `lat` as the Y axis; the box is half-open in the standard
/// rstar convention.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoBbox {
    /// Minimum latitude (south edge).
    pub min_lat: f64,
    /// Minimum longitude (west edge).
    pub min_lon: f64,
    /// Maximum latitude (north edge).
    pub max_lat: f64,
    /// Maximum longitude (east edge).
    pub max_lon: f64,
}

impl GeoBbox {
    /// True iff the bbox contains the point `(lat, lon)`.
    #[must_use]
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
    }
}

/// One R-tree-stored entry: bbox + which subscription owns it.
#[derive(Debug, Clone, Copy)]
struct GeoEntry {
    bbox: GeoBbox,
    id: SubscriptionId,
}

impl PartialEq for GeoEntry {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.bbox == other.bbox
    }
}

impl RTreeObject for GeoEntry {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(
            [self.bbox.min_lon, self.bbox.min_lat],
            [self.bbox.max_lon, self.bbox.max_lat],
        )
    }
}

/// R-tree-backed geographic interest index.
#[derive(Debug, Default)]
pub struct GeoIndex {
    tree: RTree<GeoEntry>,
}

impl GeoIndex {
    /// Insert `id` against `bbox`.
    pub fn insert(&mut self, bbox: GeoBbox, id: SubscriptionId) {
        self.tree.insert(GeoEntry { bbox, id });
    }

    /// Remove `(bbox, id)`. No-op if not present.
    pub fn remove(&mut self, bbox: GeoBbox, id: SubscriptionId) {
        let _ = self.tree.remove(&GeoEntry { bbox, id });
    }

    /// Append all subscription IDs whose bbox contains `(lat, lon)` to `out`.
    pub fn extend_matches(&self, lat: f64, lon: f64, out: &mut Vec<SubscriptionId>) {
        // rstar's API for "all rects containing this point" is to query
        // with a zero-area envelope intersecting the point.
        let pt = AABB::from_corners([lon, lat], [lon, lat]);
        for entry in self.tree.locate_in_envelope_intersecting(&pt) {
            out.push(entry.id);
        }
    }

    /// Total entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tree.size()
    }

    /// True iff no entries are indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tree.size() == 0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn id(slab_key: usize, generation: u64) -> SubscriptionId {
        SubscriptionId {
            slab_key,
            generation,
        }
    }

    // -----------------------------------------------------------------
    // TypeIndex
    // -----------------------------------------------------------------

    #[test]
    fn type_exact_match() {
        let mut t = TypeIndex::default();
        t.insert("a-f-G-U-C", id(1, 0));
        let mut out = Vec::new();
        t.extend_matches("a-f-G-U-C", &mut out);
        assert_eq!(out, vec![id(1, 0)]);
    }

    #[test]
    fn type_exact_no_partial_match() {
        let mut t = TypeIndex::default();
        t.insert("a-f-G-U-C", id(1, 0));
        let mut out = Vec::new();
        t.extend_matches("a-f-G-U", &mut out); // shorter
        assert!(out.is_empty(), "shorter type must NOT match exact pattern");
    }

    #[test]
    fn type_terminal_wildcard_matches_descendants() {
        let mut t = TypeIndex::default();
        t.insert("a-f-G-*", id(1, 0));
        let mut out = Vec::new();

        t.extend_matches("a-f-G-U-C", &mut out);
        assert_eq!(out, vec![id(1, 0)]);

        out.clear();
        t.extend_matches("a-f-G-X", &mut out);
        assert_eq!(out, vec![id(1, 0)]);

        out.clear();
        t.extend_matches("a-f-X-Y", &mut out);
        assert!(out.is_empty(), "different prefix must not match");
    }

    #[test]
    fn type_root_wildcard_matches_everything() {
        let mut t = TypeIndex::default();
        t.insert("*", id(1, 0));
        t.insert("", id(2, 0));

        for ty in ["a-f-G-U-C", "b-t-f", "u-d-r"] {
            let mut out = Vec::new();
            t.extend_matches(ty, &mut out);
            assert!(out.contains(&id(1, 0)), "{ty}: missing wildcard sub 1");
            assert!(out.contains(&id(2, 0)), "{ty}: missing wildcard sub 2");
        }
    }

    #[test]
    fn type_multiple_subscribers_at_same_pattern() {
        let mut t = TypeIndex::default();
        t.insert("a-f-G-*", id(1, 0));
        t.insert("a-f-G-*", id(2, 0));
        let mut out = Vec::new();
        t.extend_matches("a-f-G-U", &mut out);
        out.sort_unstable();
        assert_eq!(out, vec![id(1, 0), id(2, 0)]);
    }

    #[test]
    fn type_remove_drops_only_named_id() {
        let mut t = TypeIndex::default();
        t.insert("a-f-G-*", id(1, 0));
        t.insert("a-f-G-*", id(2, 0));
        t.remove("a-f-G-*", id(1, 0));

        let mut out = Vec::new();
        t.extend_matches("a-f-G-U", &mut out);
        assert_eq!(out, vec![id(2, 0)]);
    }

    #[test]
    fn type_layered_wildcards_collected_at_each_depth() {
        let mut t = TypeIndex::default();
        t.insert("a-*", id(1, 0));
        t.insert("a-f-*", id(2, 0));
        t.insert("a-f-G-*", id(3, 0));
        t.insert("a-f-G-U-C", id(4, 0));

        let mut out = Vec::new();
        t.extend_matches("a-f-G-U-C", &mut out);
        out.sort_unstable();
        assert_eq!(
            out,
            vec![id(1, 0), id(2, 0), id(3, 0), id(4, 0)],
            "all four layered patterns must match"
        );
    }

    // -----------------------------------------------------------------
    // GeoIndex
    // -----------------------------------------------------------------

    fn la_bbox() -> GeoBbox {
        GeoBbox {
            min_lat: 33.0,
            min_lon: -119.0,
            max_lat: 35.0,
            max_lon: -117.0,
        }
    }

    #[test]
    fn geo_point_inside_bbox_matches() {
        let mut g = GeoIndex::default();
        g.insert(la_bbox(), id(1, 0));
        let mut out = Vec::new();
        g.extend_matches(34.0, -118.0, &mut out);
        assert_eq!(out, vec![id(1, 0)]);
    }

    #[test]
    fn geo_point_outside_bbox_no_match() {
        let mut g = GeoIndex::default();
        g.insert(la_bbox(), id(1, 0));
        let mut out = Vec::new();
        g.extend_matches(40.0, -74.0, &mut out); // NYC
        assert!(out.is_empty());
    }

    #[test]
    fn geo_overlapping_bboxes_both_match() {
        let mut g = GeoIndex::default();
        g.insert(
            GeoBbox {
                min_lat: 33.0,
                min_lon: -119.0,
                max_lat: 34.5,
                max_lon: -117.5,
            },
            id(1, 0),
        );
        g.insert(
            GeoBbox {
                min_lat: 34.0,
                min_lon: -118.5,
                max_lat: 35.0,
                max_lon: -117.0,
            },
            id(2, 0),
        );
        let mut out = Vec::new();
        g.extend_matches(34.2, -118.0, &mut out); // in both
        out.sort_unstable();
        assert_eq!(out, vec![id(1, 0), id(2, 0)]);
    }

    #[test]
    fn geo_remove_drops_only_named_entry() {
        let mut g = GeoIndex::default();
        g.insert(la_bbox(), id(1, 0));
        g.insert(la_bbox(), id(2, 0));
        g.remove(la_bbox(), id(1, 0));

        let mut out = Vec::new();
        g.extend_matches(34.0, -118.0, &mut out);
        assert_eq!(out, vec![id(2, 0)]);
    }

    #[test]
    fn bbox_contains_predicate() {
        let bb = la_bbox();
        assert!(bb.contains(34.0, -118.0));
        assert!(!bb.contains(40.0, -74.0));
        // Boundary inclusivity (bbox is closed in our convention).
        assert!(bb.contains(33.0, -119.0));
        assert!(bb.contains(35.0, -117.0));
    }
}
