//! Network listeners for tak-rs.
//!
//! - TCP plain on 8087 (legacy "open-squirt-close") and 8088 (streaming)
//! - TLS streaming on 8089 (production path; mTLS via rustls)
//! - QUIC on 8090 (deferred; behind `quic` feature)
//! - UDP unicast/multicast on `239.2.3.1:6969` (SA mesh)
//!
//! Architecture: see `docs/architecture.md` §5.1.
//! Security invariant C5: no code path passes an unverified peer cert
//! to `tak-auth`. The `unsafe-auditor` agent gates changes here.
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
