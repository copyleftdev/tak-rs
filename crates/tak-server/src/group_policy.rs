//! Cert-to-group mapping policy.
//!
//! ATAK deployments encode team / role membership in client cert
//! subject fields — historically `OU=Cyan` for the Cyan team, etc.
//! This module turns those strings into [`tak_bus::GroupBitvector`]
//! bits using an operator-supplied TOML table:
//!
//! ```toml
//! # /etc/tak/group-policy.toml
//! [ou_to_bit]
//! Cyan  = 5
//! Red   = 6
//! Green = 7
//! Admin = 0
//! ```
//!
//! Resolution rules (applied in [`resolve_groups`]):
//! - Walk the leaf cert's OU RDN values.
//! - For each OU present in `ou_to_bit`, set the corresponding bit
//!   in the resulting bitvector.
//! - Multiple matching OUs OR together — a cert with `OU=Cyan,
//!   OU=Admin` gets bits 5 and 0 set.
//! - **An OU that isn't in the table is silently ignored.**
//! - **A cert with no matching OUs ends up with an empty
//!   bitvector and sees no events.** This is the secure default:
//!   an unmapped cert is a misconfiguration the operator must
//!   notice, not a "fail open" hole.
//!
//! For deployments without mTLS (plain TCP), the firehose passes
//! `GroupBitvector::ALL` so existing bench / lab setups keep
//! working unchanged. Production should run mTLS only.
//!
//! # What's deliberately not here
//!
//! - **CN-based bonus bits.** Could be added (`cn_to_bits`); ATAK
//!   convention puts identity in CN and team in OU, so OU is the
//!   primary lever. Will land if a real deployment needs it.
//! - **Replay-side filtering.** [`tak_bus::Bus::try_send_to`]
//!   delivers replay frames by subscription id, not by group
//!   intersection. A cert that reconnects with a smaller
//!   bitvector might still see replayed events from a wider
//!   group. Closing this needs `cot_router` to remember the
//!   sender's bitvector at insert time. Documented as a gap;
//!   tracked in the conformance scaffold.

use std::collections::HashMap;
use std::path::Path;

use rustls::pki_types::CertificateDer;
use serde::Deserialize;
use tak_bus::GroupBitvector;

/// Loaded group-mapping policy.
///
/// Default = empty mapping. Combined with the resolution rules
/// above, a default policy applied to a TLS connection results in
/// an empty bitvector → connection sees nothing. That's the
/// correct fail-secure behavior; operators must supply a real
/// policy to enable any traffic.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GroupPolicy {
    /// `OU=<key>` → bit index in the GroupBitvector.
    ///
    /// Indices outside `0..256` are clamped at load time
    /// (see [`Self::load_from_path`]).
    pub ou_to_bit: HashMap<String, u16>,
}

impl GroupPolicy {
    /// Load the policy from a TOML file.
    ///
    /// # Errors
    ///
    /// - I/O error reading the file.
    /// - Malformed TOML.
    /// - A bit index >= 256 — those would need a wider
    ///   GroupBitvector than the H4 contract permits, so the
    ///   loader fails loudly rather than silently dropping bits.
    pub fn load_from_path(path: &Path) -> Result<Self, GroupPolicyError> {
        let raw = std::fs::read_to_string(path).map_err(|e| GroupPolicyError::Read {
            path: path.to_path_buf(),
            source: e,
        })?;
        let policy: Self = toml::from_str(&raw).map_err(|e| GroupPolicyError::Parse {
            path: path.to_path_buf(),
            source: Box::new(e),
        })?;
        for (ou, &bit) in &policy.ou_to_bit {
            if bit >= 256 {
                return Err(GroupPolicyError::BitOutOfRange {
                    ou: ou.clone(),
                    bit,
                });
            }
        }
        Ok(policy)
    }
}

/// Resolve a connected peer's [`GroupBitvector`] from their cert
/// chain and the policy.
///
/// Off the H1 hot path — called once per accept. Runs through the
/// leaf cert's Subject and walks the OU RDNs.
///
/// Returns [`GroupBitvector::EMPTY`] if no OUs match the policy
/// (including the case where the chain is empty or unparseable).
#[must_use]
pub fn resolve_groups(certs: &[CertificateDer<'_>], policy: &GroupPolicy) -> GroupBitvector {
    let Some(leaf) = certs.first() else {
        return GroupBitvector::EMPTY;
    };
    let Ok((_, parsed)) = x509_parser::parse_x509_certificate(leaf) else {
        return GroupBitvector::EMPTY;
    };
    let mut groups = GroupBitvector::EMPTY;
    for ou_attr in parsed.subject().iter_organizational_unit() {
        let Ok(ou_str) = ou_attr.as_str() else {
            continue;
        };
        if let Some(&bit) = policy.ou_to_bit.get(ou_str) {
            // bit < 256 enforced at policy load time; the
            // `usize::from` is widening-only.
            groups = groups.with_bit(usize::from(bit));
        }
    }
    groups
}

/// Errors raised by [`GroupPolicy::load_from_path`].
#[derive(Debug, thiserror::Error)]
pub enum GroupPolicyError {
    /// I/O error reading the TOML file.
    #[error("read {path}: {source}")]
    Read {
        /// Path that failed to read.
        path: std::path::PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// TOML parse error.
    #[error("parse {path}: {source}")]
    Parse {
        /// Path that failed to parse.
        path: std::path::PathBuf,
        /// Underlying parse error.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// A bit index in the policy exceeds the GroupBitvector
    /// width (256 bits).
    #[error("OU={ou:?}: bit {bit} >= 256 — GroupBitvector is fixed-width [u64; 4]")]
    BitOutOfRange {
        /// OU whose mapping was out of range.
        ou: String,
        /// Offending bit index as parsed.
        bit: u16,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn empty_policy_yields_empty_bitvector() {
        let policy = GroupPolicy::default();
        let bv = resolve_groups(&[], &policy);
        assert_eq!(bv, GroupBitvector::EMPTY);
    }

    #[test]
    fn parses_ou_table() {
        let raw = r#"
            [ou_to_bit]
            Cyan = 5
            Red = 6
        "#;
        let p: GroupPolicy = toml::from_str(raw).unwrap();
        assert_eq!(p.ou_to_bit.get("Cyan"), Some(&5));
        assert_eq!(p.ou_to_bit.get("Red"), Some(&6));
    }

    #[test]
    fn out_of_range_bit_rejected_at_load() {
        let dir = std::env::temp_dir().join(format!("tak-grouppolicy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.toml");
        std::fs::write(&path, "[ou_to_bit]\nWide = 999\n").unwrap();
        let err = GroupPolicy::load_from_path(&path).unwrap_err();
        match err {
            GroupPolicyError::BitOutOfRange { bit, .. } => assert_eq!(bit, 999),
            e => panic!("wrong error: {e:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
