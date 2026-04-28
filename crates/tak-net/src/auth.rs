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
//! Group bitvectors aren't computed here — that's the next layer
//! (issue #22). This module's job is just file → typed data.

use std::collections::HashMap;

use serde::Deserialize;

use crate::Result;

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
}
