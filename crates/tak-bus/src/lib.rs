//! Subscription registry and message fan-out — the firehose core.
//!
//! Pure routing: `(message)` in, `(subscriber_id, message)*` out. No sockets,
//! no storage. Architecture in `docs/architecture.md` §5.3.
//!
//! Hot-path invariants H1-H6 (`docs/invariants.md`) live here:
//! - **H1** dispatch is alloc-free in steady state (dhat test enforces).
//! - **H3** fan-out is `Bytes::clone` (Arc bump), never `Vec::clone`.
//! - **H4** group AND is `[u64; 4]`, not arbitrary bigint.
//! - **H5** per-subscription mpsc is bounded.
//!
//! # Example
//! ```
//! use tak_bus::GroupBitvector;
//! let red  = GroupBitvector([0b0001, 0, 0, 0]);
//! let blue = GroupBitvector([0b0010, 0, 0, 0]);
//! let red_blue = GroupBitvector([0b0011, 0, 0, 0]);
//! assert!(red.intersects(&red_blue));
//! assert!(blue.intersects(&red_blue));
//! assert!(!red.intersects(&blue));
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
