//! mTLS server configuration for the production CoT streaming path on 8089.
//!
//! Cipher list and protocol versions are pinned by
//! [docs/decisions/0002-tls-ciphers.md](../../../../docs/decisions/0002-tls-ciphers.md).
//! The configuration negotiates exactly three RFC 6460 ("Suite B") cipher
//! suites — a strict superset of nothing, an exact match for what the
//! upstream Java server offers. ChaCha20 / AES-128-GCM-on-TLS-1.2 are
//! deliberately excluded.
//!
//! Client certs are **required**, not optional, per security invariant
//! **C5**. There is no code path that constructs a [`rustls::ServerConfig`]
//! from this module without a verifier — the type system enforces it via
//! the `with_truststore_pem` step in the builder.
//!
//! # Example
//! ```no_run
//! # use tak_net::tls::ServerConfigBuilder;
//! # fn load() -> Result<(), Box<dyn std::error::Error>> {
//! let cert_pem    = std::fs::read("certs/server-chain.pem")?;
//! let key_pem     = std::fs::read("certs/server.key.pem")?;
//! let ca_roots    = std::fs::read("certs/ca-roots.pem")?;
//!
//! let server_config = ServerConfigBuilder::new()
//!     .with_keystore_pem(&cert_pem, &key_pem)?
//!     .with_truststore_pem(&ca_roots)?
//!     .build()?;
//! # let _ = server_config;
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::version::{TLS12, TLS13};
use rustls::{RootCertStore, ServerConfig, SupportedCipherSuite};

use crate::{Error, Result};

/// The three cipher suites approved by ADR 0002.
///
/// Order is the preference order for negotiation:
/// 1. `TLS13_AES_256_GCM_SHA384` — preferred when client + server both speak TLS 1.3.
/// 2. `TLS13_AES_128_GCM_SHA256` — TLS 1.3 fallback.
/// 3. `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384` — TLS 1.2 fallback for older
///    ATAK clients (RFC 6460 Suite B).
#[must_use]
pub fn approved_cipher_suites() -> Vec<SupportedCipherSuite> {
    vec![
        aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384,
        aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256,
        aws_lc_rs::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    ]
}

/// Build a [`CryptoProvider`] restricted to the approved cipher suites.
///
/// All other rustls crypto (signature schemes, KX groups, RNG) is inherited
/// from `aws_lc_rs::default_provider()`.
#[must_use]
pub fn server_provider() -> Arc<CryptoProvider> {
    Arc::new(CryptoProvider {
        cipher_suites: approved_cipher_suites(),
        ..aws_lc_rs::default_provider()
    })
}

/// Builder for a `rustls::ServerConfig` configured for mTLS streaming TAK CoT.
///
/// Required steps before `build`:
/// 1. [`with_keystore_pem`](Self::with_keystore_pem) — server cert chain + private key.
/// 2. [`with_truststore_pem`](Self::with_truststore_pem) — CA roots used to
///    validate client certs.
///
/// Skipping either yields [`Error::MissingField`].
#[derive(Debug, Default)]
pub struct ServerConfigBuilder {
    cert_chain: Option<Vec<CertificateDer<'static>>>,
    private_key: Option<PrivateKeyDer<'static>>,
    truststore: Option<RootCertStore>,
}

impl ServerConfigBuilder {
    /// Start a new empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the server's certificate chain + private key from PEM bytes.
    ///
    /// `cert_pem` may contain multiple `BEGIN CERTIFICATE` blocks (server cert
    /// followed by intermediates); they are loaded in document order.
    /// `key_pem` must contain exactly one private key in PKCS#8, RSA, or
    /// SEC1 form — whichever the rustls-pki-types parser recognizes first.
    ///
    /// # Errors
    ///
    /// - [`Error::Pem`] on a malformed PEM block.
    /// - [`Error::PemEmpty`] if `cert_pem` contains no certs or `key_pem`
    ///   contains no key.
    pub fn with_keystore_pem(mut self, cert_pem: &[u8], key_pem: &[u8]) -> Result<Self> {
        let cert_chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<core::result::Result<_, _>>()
            .map_err(|e| Error::Pem(e.to_string()))?;
        if cert_chain.is_empty() {
            return Err(Error::PemEmpty("cert chain"));
        }

        let private_key =
            PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| Error::Pem(e.to_string()))?;

        self.cert_chain = Some(cert_chain);
        self.private_key = Some(private_key);
        Ok(self)
    }

    /// Load the truststore (CA roots used to verify client certs) from PEM bytes.
    ///
    /// Multiple `BEGIN CERTIFICATE` blocks are accepted; each becomes a trusted
    /// root anchor.
    ///
    /// # Errors
    ///
    /// - [`Error::Pem`] on a malformed PEM block.
    /// - [`Error::PemEmpty`] if `ca_pem` contains no certs.
    /// - [`Error::Rustls`] if rustls rejects an anchor (malformed DER, etc.).
    pub fn with_truststore_pem(mut self, ca_pem: &[u8]) -> Result<Self> {
        let cas: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(ca_pem)
            .collect::<core::result::Result<_, _>>()
            .map_err(|e| Error::Pem(e.to_string()))?;
        if cas.is_empty() {
            return Err(Error::PemEmpty("truststore"));
        }

        let mut store = RootCertStore::empty();
        for ca in cas {
            store.add(ca)?;
        }
        self.truststore = Some(store);
        Ok(self)
    }

    /// Finalize the builder into a `rustls::ServerConfig`.
    ///
    /// The result is configured with:
    /// - The three approved cipher suites in preference order (ADR 0002).
    /// - TLS 1.3 + TLS 1.2 protocol versions.
    /// - A required `WebPkiClientVerifier` against the truststore.
    /// - A single-cert (chain + key) responder.
    ///
    /// # Errors
    ///
    /// - [`Error::MissingField`] if the keystore or truststore wasn't set.
    /// - [`Error::Rustls`] for any rustls-side configuration error
    ///   (mismatched key type, invalid client verifier, etc.).
    pub fn build(self) -> Result<ServerConfig> {
        let cert_chain = self.cert_chain.ok_or(Error::MissingField("keystore"))?;
        let private_key = self.private_key.ok_or(Error::MissingField("keystore"))?;
        let truststore = self.truststore.ok_or(Error::MissingField("truststore"))?;

        let provider = server_provider();
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(truststore), provider.clone())
                .build()
                .map_err(|e| Error::Rustls(rustls::Error::General(e.to_string())))?;

        let config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&TLS13, &TLS12])
            .map_err(Error::Rustls)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(cert_chain, private_key)
            .map_err(Error::Rustls)?;

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Generate a self-signed cert + matching key pair for tests.
    /// Both server-cert and client-truststore in the tests below use this —
    /// we're testing the BUILDER, not the actual cert chain semantics
    /// (that's a handshake test, scope of issue #17).
    fn self_signed() -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let cert_pem = cert.cert.pem();
        let key_pem = cert.key_pair.serialize_pem();
        (cert_pem, key_pem)
    }

    #[test]
    fn approved_suites_are_exactly_three() {
        let suites = approved_cipher_suites();
        assert_eq!(suites.len(), 3, "ADR 0002 mandates 3 suites");
    }

    #[test]
    fn server_provider_carries_only_approved_suites() {
        let provider = server_provider();
        assert_eq!(provider.cipher_suites.len(), 3);
        // Spot-check one TLS 1.3 + the TLS 1.2 fallback.
        let names: Vec<_> = provider
            .cipher_suites
            .iter()
            .map(|s| format!("{s:?}"))
            .collect();
        assert!(
            names.iter().any(|n| n.contains("TLS13_AES_256_GCM_SHA384")),
            "missing TLS13_AES_256_GCM_SHA384, got {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.contains("TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384")),
            "missing TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384, got {names:?}"
        );
    }

    #[test]
    fn builder_with_full_config_produces_server_config() {
        let (cert_pem, key_pem) = self_signed();
        let config = ServerConfigBuilder::new()
            .with_keystore_pem(cert_pem.as_bytes(), key_pem.as_bytes())
            .unwrap()
            .with_truststore_pem(cert_pem.as_bytes())
            .unwrap()
            .build()
            .unwrap();
        // Config is opaque post-build; the act of building without error is
        // the test signal. Sanity-check it carries our cipher list.
        assert_eq!(config.crypto_provider().cipher_suites.len(), 3);
    }

    #[test]
    fn builder_missing_keystore_errors() {
        let (cert_pem, _) = self_signed();
        let err = ServerConfigBuilder::new()
            .with_truststore_pem(cert_pem.as_bytes())
            .unwrap()
            .build()
            .unwrap_err();
        assert!(matches!(err, Error::MissingField("keystore")), "got {err}");
    }

    #[test]
    fn builder_missing_truststore_errors() {
        let (cert_pem, key_pem) = self_signed();
        let err = ServerConfigBuilder::new()
            .with_keystore_pem(cert_pem.as_bytes(), key_pem.as_bytes())
            .unwrap()
            .build()
            .unwrap_err();
        assert!(
            matches!(err, Error::MissingField("truststore")),
            "got {err}"
        );
    }

    #[test]
    fn empty_keystore_pem_rejected() {
        let (_, key_pem) = self_signed();
        let err = ServerConfigBuilder::new()
            .with_keystore_pem(b"", key_pem.as_bytes())
            .unwrap_err();
        assert!(matches!(err, Error::PemEmpty(_)), "got {err}");
    }

    #[test]
    fn empty_truststore_pem_rejected() {
        let err = ServerConfigBuilder::new()
            .with_truststore_pem(b"")
            .unwrap_err();
        assert!(matches!(err, Error::PemEmpty(_)), "got {err}");
    }

    #[test]
    fn malformed_pem_rejected() {
        let (cert_pem, _) = self_signed();
        let err = ServerConfigBuilder::new()
            .with_keystore_pem(cert_pem.as_bytes(), b"not a real key")
            .unwrap_err();
        assert!(matches!(err, Error::Pem(_)), "got {err}");
    }
}
