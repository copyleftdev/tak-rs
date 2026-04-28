//! `UserAuthenticationFile.xml` parser and [`UserStore`] lookup table.
//!
//! Schema source: upstream
//! `.scratch/takserver-java/src/takserver-common/src/main/xsd/UserAuthenticationFile.xsd`.
//! Schema verbatim:
//!
//! ```xml
//! <UserAuthenticationFile xmlns="http://bbn.com/marti/xml/bindings">
//!   <User identifier="..."     <!-- required: cert CN, username, etc. -->
//!         fingerprint="..."    <!-- optional: SHA-256 of leaf cert DER -->
//!         password="..."       <!-- optional: hash if passwordHashed=true -->
//!         passwordHashed="..."
//!         role="...">          <!-- ROLE_{ADMIN,READONLY,ANONYMOUS,NON_ADMIN_UI,WEBTAK,NONEXISTENT} -->
//!     <groupList>...</groupList>      <!-- bidirectional group memberships -->
//!     <groupListIN>...</groupListIN>  <!-- groups whose traffic the user receives -->
//!     <groupListOUT>...</groupListOUT> <!-- groups the user can send to -->
//!   </User>
//!   ...
//! </UserAuthenticationFile>
//! ```
//!
//! [`UserStore`] indexes the parsed users by `identifier` and (when present)
//! `fingerprint` for O(1) cert→user lookup at handshake time.
//!
//! Group bitvectors are computed by the [`Authenticator`] further down,
//! which combines a [`UserStore`] with a [`GroupRegistry`] that interns
//! group names into bit positions and produces a `tak_bus::GroupBitvector`
//! per resolved user (issue #22).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use dashmap::DashMap;
use rustls::pki_types::CertificateDer;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tak_bus::GroupBitvector;

use crate::{Error, Result};

/// User role per upstream schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize)]
pub enum Role {
    /// Full admin.
    #[serde(rename = "ROLE_ADMIN")]
    Admin,
    /// Read-only — receives messages, can't publish.
    #[serde(rename = "ROLE_READONLY")]
    ReadOnly,
    /// Default role; full client capabilities but not admin.
    #[default]
    #[serde(rename = "ROLE_ANONYMOUS")]
    Anonymous,
    /// Non-admin UI access (web only).
    #[serde(rename = "ROLE_NON_ADMIN_UI")]
    NonAdminUi,
    /// WebTAK browser client.
    #[serde(rename = "ROLE_WEBTAK")]
    WebTak,
    /// Sentinel for missing user.
    #[serde(rename = "ROLE_NONEXISTENT")]
    Nonexistent,
}

/// One `<User>` entry from the auth file.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    /// The user's primary identifier (cert CN, username, etc.). Required.
    #[serde(rename = "@identifier")]
    pub identifier: String,

    /// SHA-256 fingerprint of the user's leaf certificate (DER), if any.
    /// Used for O(1) cert→user lookup at TLS handshake.
    #[serde(rename = "@fingerprint", default)]
    pub fingerprint: Option<String>,

    /// Plaintext or hashed password (per [`Self::password_hashed`]).
    #[serde(rename = "@password", default)]
    pub password: Option<String>,

    /// `true` if [`Self::password`] is a stored hash (bcrypt typically).
    #[serde(rename = "@passwordHashed", default)]
    pub password_hashed: bool,

    /// User role; defaults to [`Role::Anonymous`] per schema.
    #[serde(rename = "@role", default)]
    pub role: Role,

    /// Bidirectional group memberships.
    #[serde(rename = "groupList", default)]
    pub group_list: Vec<String>,

    /// Inbound (receive-only) group memberships.
    #[serde(rename = "groupListIN", default)]
    pub group_list_in: Vec<String>,

    /// Outbound (send-only) group memberships.
    #[serde(rename = "groupListOUT", default)]
    pub group_list_out: Vec<String>,
}

impl User {
    /// All groups the user can receive from (`groupList ∪ groupListIN`).
    pub fn all_inbound_groups(&self) -> impl Iterator<Item = &str> {
        self.group_list
            .iter()
            .chain(self.group_list_in.iter())
            .map(String::as_str)
    }

    /// All groups the user can send to (`groupList ∪ groupListOUT`).
    pub fn all_outbound_groups(&self) -> impl Iterator<Item = &str> {
        self.group_list
            .iter()
            .chain(self.group_list_out.iter())
            .map(String::as_str)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename = "UserAuthenticationFile")]
struct UafFile {
    #[serde(rename = "User", default)]
    users: Vec<User>,
}

/// In-memory lookup table for parsed users.
///
/// Built once at server startup; queried at every TLS handshake. Lookup by
/// [`User::identifier`] and by [`User::fingerprint`] is O(1) via an
/// internal `HashMap`. Iteration over all users preserves document order.
#[derive(Debug, Default)]
pub struct UserStore {
    users: Vec<User>,
    by_identifier: HashMap<String, usize>,
    by_fingerprint: HashMap<String, usize>,
}

impl UserStore {
    /// Parse a `UserAuthenticationFile.xml` blob.
    ///
    /// # Errors
    ///
    /// - [`Error::Xml`] for any XML parse / schema-shape failure.
    pub fn from_xml(xml: &str) -> Result<Self> {
        let parsed: UafFile = quick_xml::de::from_str(xml)?;
        Ok(Self::from_users(parsed.users))
    }

    /// Read and parse from a path.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] if the file is unreadable.
    /// - [`Error::Xml`] for any XML parse failure.
    pub fn from_path(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let xml = std::fs::read_to_string(path)?;
        Self::from_xml(&xml)
    }

    /// Construct from a pre-built user list (test convenience).
    #[must_use]
    pub fn from_users(users: Vec<User>) -> Self {
        let mut store = Self::default();
        for user in users {
            let idx = store.users.len();
            store.by_identifier.insert(user.identifier.clone(), idx);
            if let Some(fp) = &user.fingerprint {
                store.by_fingerprint.insert(fp.clone(), idx);
            }
            store.users.push(user);
        }
        store
    }

    /// Lookup by primary identifier.
    #[must_use]
    pub fn by_identifier(&self, identifier: &str) -> Option<&User> {
        self.by_identifier
            .get(identifier)
            .and_then(|&i| self.users.get(i))
    }

    /// Lookup by leaf-cert SHA-256 fingerprint (lowercase hex without separators).
    #[must_use]
    pub fn by_fingerprint(&self, fingerprint: &str) -> Option<&User> {
        self.by_fingerprint
            .get(fingerprint)
            .and_then(|&i| self.users.get(i))
    }

    /// Total user count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.users.len()
    }

    /// True iff there are no users.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// Iterate users in document order.
    pub fn iter(&self) -> impl Iterator<Item = &User> {
        self.users.iter()
    }
}

// ===========================================================================
// Resolver — issue #22.
// ===========================================================================

/// A resolved user identifier (the upstream `User.identifier` field).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UserId(pub String);

impl UserId {
    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for UserId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Maps group name → bit position in a `GroupBitvector`. New names get the
/// next free bit; bits are stable for the lifetime of the registry.
///
/// Capacity is 256 bits (matches `tak_bus::GroupBitvector` width per
/// invariant H4). The 257th distinct group seen saturates at bit 255 and
/// emits a tracing::warn — operators should reduce group count.
#[derive(Debug, Default)]
pub struct GroupRegistry {
    bits: DashMap<String, u8>,
    next: AtomicU16,
}

impl GroupRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or assign a bit position for `name`. Concurrent-safe.
    #[must_use]
    pub fn intern(&self, name: &str) -> u8 {
        if let Some(bit) = self.bits.get(name) {
            return *bit;
        }
        let entry = self.bits.entry(name.to_owned()).or_insert_with(|| {
            let next = self.next.fetch_add(1, Ordering::Relaxed);
            if next >= 256 {
                tracing::warn!(
                    group = name,
                    "GroupRegistry saturated: more than 256 distinct groups; widen GroupBitvector or reduce groups"
                );
                255u8
            } else {
                #[allow(clippy::cast_possible_truncation)] // checked above
                {
                    next as u8
                }
            }
        });
        *entry
    }

    /// Build a `GroupBitvector` from an iterator of group names.
    #[must_use]
    pub fn bitvector_for<'a, I>(&self, names: I) -> GroupBitvector
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut bv = GroupBitvector::EMPTY;
        for name in names {
            let bit = self.intern(name);
            bv = bv.with_bit(bit as usize);
        }
        bv
    }

    /// Distinct group count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bits.len()
    }

    /// True iff no groups have been interned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }
}

/// Cached resolution: identity + inbound bitvector.
#[derive(Debug, Clone)]
struct ResolvedIdentity {
    user_id: UserId,
    bitvector: GroupBitvector,
}

/// Resolves an X.509 peer certificate to a [`UserId`] + [`GroupBitvector`].
///
/// Combines a [`UserStore`] (the `UserAuthenticationFile.xml` contents) with
/// a shared [`GroupRegistry`] (group-name → bit-position map). The result
/// is cached by SHA-256(cert DER), so repeat resolutions for the same
/// certificate are an O(1) `dashmap` lookup. Cache hits and misses emit
/// `tak_auth.cache.{hit,miss}` counters via the [`metrics`] crate.
///
/// Lookup order on a cache miss:
///
/// 1. Upstream-compatible fingerprint (`XX:XX:...:XX` uppercase hex with
///    colons every 2 chars) → `UserStore::by_fingerprint`.
/// 2. Common Name from the cert's Subject DN → `UserStore::by_identifier`.
/// 3. Otherwise [`Error::UnknownUser`].
///
/// The bitvector built includes the user's `groupList` ∪ `groupListIN`
/// (everything inbound). The OUT side is deferred — issue #22 specifies a
/// single bitvector return; OUT-side dispatch is M4 territory.
#[derive(Debug)]
pub struct Authenticator {
    users: Arc<UserStore>,
    groups: Arc<GroupRegistry>,
    cache: DashMap<[u8; 32], ResolvedIdentity>,
}

impl Authenticator {
    /// Build a new resolver over `users`. The [`GroupRegistry`] starts empty
    /// and grows lazily as new group names are seen.
    #[must_use]
    pub fn new(users: Arc<UserStore>) -> Self {
        Self {
            users,
            groups: Arc::new(GroupRegistry::new()),
            cache: DashMap::new(),
        }
    }

    /// Use an existing shared registry (e.g. when other components also
    /// need to map group names → bits, like `tak-bus` itself).
    #[must_use]
    pub fn with_registry(users: Arc<UserStore>, groups: Arc<GroupRegistry>) -> Self {
        Self {
            users,
            groups,
            cache: DashMap::new(),
        }
    }

    /// Borrow the shared group registry.
    #[must_use]
    pub fn registry(&self) -> &Arc<GroupRegistry> {
        &self.groups
    }

    /// Resolve a peer certificate to the matching user + group bitvector.
    ///
    /// # Errors
    ///
    /// - [`Error::EmptyCertChain`] if `cert` is empty.
    /// - [`Error::X509Parse`] if the leaf cert is malformed.
    /// - [`Error::UnknownUser`] if neither fingerprint nor CN match a user.
    pub fn resolve(&self, cert: &CertificateDer<'_>) -> Result<(UserId, GroupBitvector)> {
        let der = cert.as_ref();
        if der.is_empty() {
            return Err(Error::EmptyCertChain);
        }

        let digest = Sha256::digest(der);
        let mut key = [0u8; 32];
        key.copy_from_slice(&digest);

        if let Some(entry) = self.cache.get(&key) {
            metrics::counter!("tak_auth.cache.hit").increment(1);
            return Ok((entry.user_id.clone(), entry.bitvector));
        }
        metrics::counter!("tak_auth.cache.miss").increment(1);

        let fingerprint = format_fingerprint(&digest);

        // Try by-fingerprint first (the upstream convention).
        let user = if let Some(u) = self.users.by_fingerprint(&fingerprint) {
            u
        } else {
            // Fall back to CN extracted from the leaf cert's Subject DN.
            let cn = parse_leaf_cn(der)?;
            match cn.as_deref().and_then(|c| self.users.by_identifier(c)) {
                Some(u) => u,
                None => {
                    metrics::counter!("tak_auth.resolve.unknown").increment(1);
                    return Err(Error::UnknownUser { fingerprint, cn });
                }
            }
        };

        let bitvector = self.groups.bitvector_for(user.all_inbound_groups());
        let user_id = UserId(user.identifier.clone());

        self.cache.insert(
            key,
            ResolvedIdentity {
                user_id: user_id.clone(),
                bitvector,
            },
        );

        Ok((user_id, bitvector))
    }
}

/// Format a SHA-256 digest as the upstream-compatible `XX:XX:...:XX`
/// (uppercase hex, colon every 2 chars). Matches the format produced by
/// `RemoteUtil.getCertSHA256Fingerprint(cert)` in upstream Java.
fn format_fingerprint(digest: &[u8]) -> String {
    let mut out = String::with_capacity(digest.len() * 3);
    for (i, byte) in digest.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

/// Extract the Common Name from a leaf cert's Subject DN, if present.
fn parse_leaf_cn(der: &[u8]) -> Result<Option<String>> {
    let (_, cert) =
        x509_parser::parse_x509_certificate(der).map_err(|e| Error::X509Parse(e.to_string()))?;
    Ok(cert
        .subject()
        .iter_common_name()
        .filter_map(|cn| cn.as_str().ok().map(str::to_owned))
        .next())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<UserAuthenticationFile xmlns="http://bbn.com/marti/xml/bindings">
  <User identifier="VIPER01" fingerprint="abc123" role="ROLE_ANONYMOUS">
    <groupList>Cyan</groupList>
    <groupList>Blue</groupList>
    <groupListIN>Spectator</groupListIN>
  </User>
  <User identifier="ADMIN01" fingerprint="deadbeef" role="ROLE_ADMIN" password="hash" passwordHashed="true">
    <groupList>__ANON__</groupList>
  </User>
  <User identifier="GUEST01"/>
</UserAuthenticationFile>"#;

    #[test]
    fn parse_three_users() {
        let store = UserStore::from_xml(SAMPLE).expect("parses");
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn lookup_by_identifier_finds_the_right_user() {
        let store = UserStore::from_xml(SAMPLE).unwrap();
        let viper = store.by_identifier("VIPER01").expect("VIPER01 present");
        assert_eq!(viper.role, Role::Anonymous);
        assert_eq!(viper.fingerprint.as_deref(), Some("abc123"));
        assert_eq!(viper.group_list, vec!["Cyan".to_owned(), "Blue".to_owned()]);
        assert_eq!(viper.group_list_in, vec!["Spectator".to_owned()]);
        assert!(viper.group_list_out.is_empty());
    }

    #[test]
    fn lookup_by_fingerprint() {
        let store = UserStore::from_xml(SAMPLE).unwrap();
        let admin = store.by_fingerprint("deadbeef").expect("by fingerprint");
        assert_eq!(admin.identifier, "ADMIN01");
        assert_eq!(admin.role, Role::Admin);
        assert!(admin.password_hashed);
    }

    #[test]
    fn missing_role_defaults_to_anonymous() {
        let store = UserStore::from_xml(SAMPLE).unwrap();
        let guest = store.by_identifier("GUEST01").unwrap();
        assert_eq!(guest.role, Role::Anonymous);
        assert!(guest.fingerprint.is_none());
        assert!(guest.password.is_none());
        assert!(!guest.password_hashed);
        assert!(guest.group_list.is_empty());
    }

    #[test]
    fn lookup_by_fingerprint_returns_none_for_user_without_one() {
        // GUEST01 has no fingerprint so it's not in the by_fingerprint index.
        let store = UserStore::from_xml(SAMPLE).unwrap();
        assert!(store.by_fingerprint("does-not-exist").is_none());
    }

    #[test]
    fn empty_file_yields_empty_store() {
        let xml = r#"<?xml version="1.0"?>
<UserAuthenticationFile xmlns="http://bbn.com/marti/xml/bindings"/>"#;
        let store = UserStore::from_xml(xml).unwrap();
        assert_eq!(store.len(), 0);
        assert!(store.is_empty());
    }

    #[test]
    fn malformed_xml_errors() {
        let store = UserStore::from_xml("<UserAuthenticationFile><User</UserAuthenticationFile>");
        assert!(store.is_err(), "malformed XML must error");
    }

    #[test]
    fn iterator_preserves_document_order() {
        let store = UserStore::from_xml(SAMPLE).unwrap();
        let ids: Vec<&str> = store.iter().map(|u| u.identifier.as_str()).collect();
        assert_eq!(ids, vec!["VIPER01", "ADMIN01", "GUEST01"]);
    }

    #[test]
    fn group_helpers_compose_directional_lists() {
        let user = User {
            identifier: "x".into(),
            fingerprint: None,
            password: None,
            password_hashed: false,
            role: Role::Anonymous,
            group_list: vec!["both".into()],
            group_list_in: vec!["in_only".into()],
            group_list_out: vec!["out_only".into()],
        };
        let inbound: Vec<&str> = user.all_inbound_groups().collect();
        let outbound: Vec<&str> = user.all_outbound_groups().collect();
        assert_eq!(inbound, vec!["both", "in_only"]);
        assert_eq!(outbound, vec!["both", "out_only"]);
    }

    #[test]
    fn from_path_reads_file_and_parses() {
        // Round-trip: write to a tempfile, read back, verify.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("uaf-{}.xml", std::process::id()));
        std::fs::write(&path, SAMPLE).unwrap();
        let store = UserStore::from_path(&path).unwrap();
        assert_eq!(store.len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    // -----------------------------------------------------------------
    // Resolver tests (issue #22)
    // -----------------------------------------------------------------

    use rcgen::{CertificateParams, DnType, KeyPair};

    /// Build a cert with the given CN, return (DER, fingerprint).
    fn cert_with_cn(cn: &str) -> (CertificateDer<'static>, String) {
        let mut params = CertificateParams::new(vec![cn.to_owned()]).unwrap();
        params.distinguished_name.push(DnType::CommonName, cn);
        let kp = KeyPair::generate().unwrap();
        let cert = params.self_signed(&kp).unwrap();
        let der = CertificateDer::from(cert.der().to_vec());
        let digest = Sha256::digest(der.as_ref());
        (der, format_fingerprint(&digest))
    }

    fn store_with_user(identifier: &str, fingerprint: Option<&str>, groups: &[&str]) -> UserStore {
        UserStore::from_users(vec![User {
            identifier: identifier.to_owned(),
            fingerprint: fingerprint.map(str::to_owned),
            password: None,
            password_hashed: false,
            role: Role::Anonymous,
            group_list: groups.iter().map(|s| (*s).to_owned()).collect(),
            group_list_in: vec![],
            group_list_out: vec![],
        }])
    }

    #[test]
    fn fingerprint_format_matches_upstream() {
        // Upstream: uppercase hex, colon every 2 chars, 32 bytes → 95 chars.
        let digest = [0xDE, 0xAD, 0xBE, 0xEF].repeat(8);
        let fp = format_fingerprint(&digest);
        assert_eq!(fp.len(), 32 * 2 + 31, "32 bytes × 2 hex + 31 colons");
        assert!(fp.starts_with("DE:AD:BE:EF"), "got {fp}");
        assert!(fp.ends_with("DE:AD:BE:EF"), "got {fp}");
        assert_eq!(fp.matches(':').count(), 31);
    }

    #[test]
    fn resolve_via_fingerprint() {
        let (der, fp) = cert_with_cn("VIPER01");
        let users = Arc::new(store_with_user("VIPER01", Some(&fp), &["Cyan", "Field"]));
        let auth = Authenticator::new(users);

        let (user_id, bv) = auth.resolve(&der).unwrap();
        assert_eq!(user_id.as_str(), "VIPER01");
        // Two distinct groups → exactly two bits set.
        let bits_set: u32 = bv.0.iter().map(|w| w.count_ones()).sum();
        assert_eq!(bits_set, 2, "expected 2 bits, got {bits_set}");
    }

    #[test]
    fn resolve_via_cn_fallback() {
        let (der, _fp) = cert_with_cn("VIPER02");
        // No fingerprint set on the user — must fall back to CN match.
        let users = Arc::new(store_with_user("VIPER02", None, &["Cyan"]));
        let auth = Authenticator::new(users);

        let (user_id, _) = auth.resolve(&der).unwrap();
        assert_eq!(user_id.as_str(), "VIPER02");
    }

    #[test]
    fn resolve_unknown_returns_error_with_fingerprint() {
        let (der, fp) = cert_with_cn("STRANGER");
        let users = Arc::new(UserStore::default()); // empty
        let auth = Authenticator::new(users);

        let err = auth.resolve(&der).unwrap_err();
        match err {
            Error::UnknownUser { fingerprint, cn } => {
                assert_eq!(fingerprint, fp);
                assert_eq!(cn.as_deref(), Some("STRANGER"));
            }
            other => panic!("expected UnknownUser, got {other:?}"),
        }
    }

    #[test]
    fn resolve_caches_repeat_lookups() {
        let (der, fp) = cert_with_cn("VIPER03");
        let users = Arc::new(store_with_user("VIPER03", Some(&fp), &["Cyan"]));
        let auth = Authenticator::new(users);

        // First call populates cache.
        let (id1, bv1) = auth.resolve(&der).unwrap();
        assert_eq!(auth.cache.len(), 1);

        // Second call hits cache.
        let (id2, bv2) = auth.resolve(&der).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(bv1, bv2);
        assert_eq!(auth.cache.len(), 1, "cache should not grow on hit");
    }

    #[test]
    fn empty_cert_is_rejected() {
        let der = CertificateDer::from(Vec::<u8>::new());
        let users = Arc::new(UserStore::default());
        let auth = Authenticator::new(users);
        assert!(matches!(auth.resolve(&der), Err(Error::EmptyCertChain)));
    }

    #[test]
    fn group_registry_assigns_distinct_bits_consistently() {
        let reg = GroupRegistry::new();
        let a1 = reg.intern("Cyan");
        let b1 = reg.intern("Magenta");
        let a2 = reg.intern("Cyan"); // already interned
        assert_eq!(a1, a2, "same group → same bit");
        assert_ne!(a1, b1, "distinct groups → distinct bits");
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn bitvector_for_combines_all_named_groups() {
        let reg = GroupRegistry::new();
        let bv = reg.bitvector_for(["A", "B", "C"]);
        let bits_set: u32 = bv.0.iter().map(|w| w.count_ones()).sum();
        assert_eq!(bits_set, 3);
    }

    #[test]
    fn registry_shared_across_authenticators() {
        let registry = Arc::new(GroupRegistry::new());

        let (der1, fp1) = cert_with_cn("U1");
        let (der2, fp2) = cert_with_cn("U2");
        let users = Arc::new(UserStore::from_users(vec![
            User {
                identifier: "U1".into(),
                fingerprint: Some(fp1),
                password: None,
                password_hashed: false,
                role: Role::Anonymous,
                group_list: vec!["GroupX".into()],
                group_list_in: vec![],
                group_list_out: vec![],
            },
            User {
                identifier: "U2".into(),
                fingerprint: Some(fp2),
                password: None,
                password_hashed: false,
                role: Role::Anonymous,
                group_list: vec!["GroupX".into()], // SAME group as U1
                group_list_in: vec![],
                group_list_out: vec![],
            },
        ]));

        let auth = Authenticator::with_registry(users, registry.clone());
        let (_, bv1) = auth.resolve(&der1).unwrap();
        let (_, bv2) = auth.resolve(&der2).unwrap();

        // Both users share GroupX → their bitvectors must intersect.
        assert!(
            bv1.intersects(&bv2),
            "users in same group must intersect: bv1={:?} bv2={:?}",
            bv1,
            bv2
        );
        assert_eq!(
            registry.len(),
            1,
            "GroupX should be the only interned group"
        );
    }

    #[test]
    fn inbound_bitvector_includes_group_list_in() {
        let (der, fp) = cert_with_cn("VIPER04");
        let users = Arc::new(UserStore::from_users(vec![User {
            identifier: "VIPER04".into(),
            fingerprint: Some(fp),
            password: None,
            password_hashed: false,
            role: Role::Anonymous,
            group_list: vec!["Bidir".into()],
            group_list_in: vec!["InOnly1".into(), "InOnly2".into()],
            group_list_out: vec!["OutOnly".into()], // must NOT appear in inbound bv
        }]));
        let auth = Authenticator::new(users);

        let (_, bv) = auth.resolve(&der).unwrap();
        let bits_set: u32 = bv.0.iter().map(|w| w.count_ones()).sum();
        // Bidir + InOnly1 + InOnly2 = 3 bits. OutOnly is excluded.
        assert_eq!(
            bits_set,
            3,
            "got {bits_set} bits, registry={:?}",
            auth.registry()
        );
    }
}
