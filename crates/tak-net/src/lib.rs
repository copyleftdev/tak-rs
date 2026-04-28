//! Network listeners for tak-rs.
//!
//! - TCP plain on 8087 (legacy "open-squirt-close") and 8088 (streaming)
//! - TLS streaming on 8089 (production path; mTLS via rustls)
//! - QUIC on 8090 (deferred; behind `quic` feature)
//! - UDP unicast/multicast on `239.2.3.1:6969` (SA mesh)
//!
//! Architecture: see `docs/architecture.md` §5.1.
//! Security invariant **C5**: no code path passes an unverified peer cert
//! to `tak-auth`. The `unsafe-auditor` agent gates changes here. The
//! [`tls`] module's [`ServerConfigBuilder`] enforces a required client cert
//! verifier — the `WebPkiClientVerifier` is the only entry point.
//!
//! TLS cipher choice is locked by `docs/decisions/0002-tls-ciphers.md`:
//! exactly the three RFC 6460 "Suite B" suites the upstream Java server
//! offers, served via rustls 0.23 + aws_lc_rs + tls12.
//!
//! # Example
//! ```
//! use tak_net::ports::{PLAIN_TCP, STREAM_TCP, TLS, QUIC};
//! assert_eq!((PLAIN_TCP, STREAM_TCP, TLS, QUIC), (8087, 8088, 8089, 8090));
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

pub mod ports {
    //! Default port assignments matching upstream TAK Server defaults.

    /// Plain CoT TCP (legacy, "open-squirt-close").
    pub const PLAIN_TCP: u16 = 8087;
    /// Plain streaming TCP.
    pub const STREAM_TCP: u16 = 8088;
    /// TLS streaming (production).
    pub const TLS: u16 = 8089;
    /// QUIC (deferred).
    pub const QUIC: u16 = 8090;
    /// Federation v2 (gRPC).
    pub const FED_V2: u16 = 9001;
}

pub mod auth;
pub mod conn;
pub mod listener;
pub mod tasks;
pub mod tls;

/// Errors returned by `tak-net`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// PEM parse failure (cert chain or private key).
    #[error("pem: {0}")]
    Pem(String),

    /// Required PEM input was missing (no certs, no key, etc.).
    #[error("pem: empty or missing — {0}")]
    PemEmpty(&'static str),

    /// Underlying rustls config error.
    #[error("rustls: {0}")]
    Rustls(#[from] rustls::Error),

    /// Required builder field was not set before [`build`](tls::ServerConfigBuilder::build).
    #[error("config: required field not set: {0}")]
    MissingField(&'static str),

    /// XML parse failure (UserAuthenticationFile.xml or similar).
    #[error("xml: {0}")]
    Xml(String),

    /// The peer cert chain has no leaf certificate.
    #[error("auth: empty peer certificate chain")]
    EmptyCertChain,

    /// X.509 leaf cert failed to parse.
    #[error("auth: x509 parse: {0}")]
    X509Parse(String),

    /// No user matches the presented certificate (by fingerprint or by CN).
    /// Identifier shown is whichever lookup-key was tried first.
    #[error("auth: unknown user (fingerprint={fingerprint}, cn={cn:?})")]
    UnknownUser {
        /// SHA-256 fingerprint of the leaf cert (upstream format: `XX:XX:...:XX`).
        fingerprint: String,
        /// Common Name extracted from the leaf cert subject DN, if available.
        cn: Option<String>,
    },

    /// Underlying I/O failure (file read on the convenience helpers).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<quick_xml::DeError> for Error {
    fn from(e: quick_xml::DeError) -> Self {
        Self::Xml(e.to_string())
    }
}

/// Convenience result type used across the crate.
pub type Result<T> = core::result::Result<T, Error>;
