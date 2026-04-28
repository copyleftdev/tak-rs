//! Connection lifecycle type-state.
//!
//! A TAK client connection moves through three states; the type system
//! enforces that operations only valid in one state can't be called in
//! another. Per the Gjengset persona's guidance — illegal state
//! transitions should be unrepresentable.
//!
//! ```text
//!         new()                  promote_to_authed()
//!  ╭────────────────╮        ╭─────────────────────╮        ╭─────────╮
//!  │ Handshaking    │ ─────▶ │   Authed            │ ─────▶ │ Streaming
//!  │ • id           │        │   • id              │        │ • id    │
//!  │ • peer_addr    │        │   • peer_addr       │        │ • addr  │
//!  ╰────────────────╯        │   • peer_certs      │        │ • certs │
//!                            ╰─────────────────────╯        ╰─────────╯
//!                                start_streaming()
//! ```
//!
//! Each transition is a value-consuming method — once you've authed, the
//! `Handshaking` value is gone, so there's no way to "rewind" or call a
//! handshake-only method on an authed connection. Compile errors do the
//! work runtime checks would otherwise have to.
//!
//! The state-marker types ([`Handshaking`], [`Authed`], [`Streaming`])
//! are sealed via [`ConnState`]'s private bound — third-party crates
//! cannot define their own state markers, preserving the invariant that
//! the lifecycle has exactly three phases.
//!
//! # Example
//! ```
//! # use tak_net::conn::{ConnectionState, Handshaking};
//! # use std::net::SocketAddr;
//! let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//! let conn = ConnectionState::<Handshaking>::new(addr);
//! // Transition only available on Handshaking:
//! let authed = conn.promote_to_authed(Vec::new());
//! let streaming = authed.start_streaming();
//! assert!(streaming.peer_certs().is_empty());
//! ```
//!
//! # Compile-fail demos
//!
//! Calling `peer_certs()` on a `Handshaking` connection won't compile:
//!
//! ```compile_fail
//! # use tak_net::conn::{ConnectionState, Handshaking};
//! # use std::net::SocketAddr;
//! let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//! let conn = ConnectionState::<Handshaking>::new(addr);
//! let _ = conn.peer_certs(); // ERROR: method `peer_certs` not found
//! ```
//!
//! Calling `promote_to_authed` on a `Streaming` connection won't compile:
//!
//! ```compile_fail
//! # use tak_net::conn::{ConnectionState, Handshaking};
//! # use std::net::SocketAddr;
//! let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//! let conn = ConnectionState::<Handshaking>::new(addr)
//!     .promote_to_authed(Vec::new())
//!     .start_streaming();
//! let _ = conn.promote_to_authed(Vec::new()); // ERROR: not on Streaming
//! ```
//!
//! Returning a connection in the wrong state from a function won't compile:
//!
//! ```compile_fail
//! # use tak_net::conn::{ConnectionState, Authed};
//! # use std::net::SocketAddr;
//! fn need_authed() -> ConnectionState<Authed> { unimplemented!() }
//! let _wrong: ConnectionState<tak_net::conn::Streaming> = need_authed();
//! ```

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use rustls::pki_types::CertificateDer;

/// Monotonic per-process connection identifier.
///
/// Allocated at the moment a TCP/TLS accept yields a new socket, before
/// any handshake. IDs are not reused for the lifetime of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(u64);

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

impl ConnectionId {
    /// Allocate the next ConnectionId.
    ///
    /// `Relaxed` is the right ordering — we don't synchronize *anything*
    /// with the act of incrementing the counter; we just want a unique
    /// monotonic value. Bus dispatch code never inspects another
    /// connection's id atomically.
    #[inline]
    #[must_use]
    pub fn next() -> Self {
        Self(NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// The raw u64 value.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "conn#{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Sealed state markers.
// ---------------------------------------------------------------------------

mod sealed {
    pub trait Sealed {}
}

/// Marker trait for the three connection-lifecycle states.
///
/// Sealed: outside this crate, no further states can be added. The TAK
/// protocol has exactly the three phases [`Handshaking`], [`Authed`],
/// [`Streaming`] — nothing else has a meaningful place in the lifecycle.
pub trait ConnState: sealed::Sealed {}

/// Initial state: TCP socket accepted, TLS handshake in progress, no
/// cert chain extracted yet.
#[derive(Debug)]
pub struct Handshaking;

/// Post-TLS-handshake: peer cert chain extracted; group resolution
/// (tak-auth) hasn't necessarily happened yet but is permitted to run.
#[derive(Debug)]
pub struct Authed {
    peer_certs: Vec<CertificateDer<'static>>,
}

/// Subscribed to the bus; the connection is producing/consuming CoT
/// events on the firehose.
#[derive(Debug)]
pub struct Streaming {
    peer_certs: Vec<CertificateDer<'static>>,
}

impl sealed::Sealed for Handshaking {}
impl sealed::Sealed for Authed {}
impl sealed::Sealed for Streaming {}

impl ConnState for Handshaking {}
impl ConnState for Authed {}
impl ConnState for Streaming {}

// ---------------------------------------------------------------------------
// ConnectionState — generic over the lifecycle phase.
// ---------------------------------------------------------------------------

/// A connection in lifecycle state `S`.
///
/// `id` and `peer_addr` are stable across all transitions; per-state data
/// (`peer_certs` etc.) lives on the state marker itself.
#[derive(Debug)]
pub struct ConnectionState<S: ConnState> {
    /// Monotonic per-process ID.
    pub id: ConnectionId,
    /// Remote socket address (filled at accept-time, immutable).
    pub peer_addr: SocketAddr,
    state: S,
}

impl ConnectionState<Handshaking> {
    /// Allocate a new connection in the `Handshaking` state.
    #[must_use]
    pub fn new(peer_addr: SocketAddr) -> Self {
        Self {
            id: ConnectionId::next(),
            peer_addr,
            state: Handshaking,
        }
    }

    /// Consume the handshaking connection and produce an authed one,
    /// recording the peer's cert chain extracted at TLS handshake.
    ///
    /// `peer_certs` may be empty for plain-TCP transports (8087 / 8088),
    /// but those listeners don't pass through this state-machine in v1 —
    /// they short-circuit straight to `Streaming` once we wire them up
    /// in #19. For TLS (8089), the chain is non-empty by construction
    /// (the verifier in [`super::tls::ServerConfigBuilder`] requires a
    /// client cert).
    #[must_use]
    pub fn promote_to_authed(
        self,
        peer_certs: Vec<CertificateDer<'static>>,
    ) -> ConnectionState<Authed> {
        ConnectionState {
            id: self.id,
            peer_addr: self.peer_addr,
            state: Authed { peer_certs },
        }
    }
}

impl ConnectionState<Authed> {
    /// The peer's certificate chain (server cert + intermediates), as
    /// extracted from the rustls connection.
    #[must_use]
    pub fn peer_certs(&self) -> &[CertificateDer<'static>] {
        &self.state.peer_certs
    }

    /// The peer's leaf-cert Subject Distinguished Name (RFC 4514 form).
    ///
    /// Lazily parsed from the leaf cert; not cached. Callers that need
    /// the DN repeatedly should hold the returned `String`. Returns an
    /// error if the chain is empty or the leaf cert can't be parsed.
    pub fn peer_dn(&self) -> Result<String, PeerCertError> {
        peer_subject(self.peer_certs(), |s| Ok(s.to_string()))
    }

    /// The Organizational Unit (`OU`) RDN values from the peer leaf cert,
    /// in the order they appear in the Subject DN. Useful for the upstream
    /// "OU=group" convention some TAK deployments use as a poor man's
    /// authorization model.
    pub fn peer_ous(&self) -> Result<Vec<String>, PeerCertError> {
        peer_subject(self.peer_certs(), |s| {
            Ok(s.iter_organizational_unit()
                .filter_map(|ou| ou.as_str().ok().map(String::from))
                .collect())
        })
    }

    /// Consume the authed connection and produce a streaming one.
    /// Called once the bus has accepted a subscription for this client.
    #[must_use]
    pub fn start_streaming(self) -> ConnectionState<Streaming> {
        ConnectionState {
            id: self.id,
            peer_addr: self.peer_addr,
            state: Streaming {
                peer_certs: self.state.peer_certs,
            },
        }
    }
}

impl ConnectionState<Streaming> {
    /// The peer's certificate chain — same data as in [`Authed`], moved
    /// across the transition so we don't lose it.
    #[must_use]
    pub fn peer_certs(&self) -> &[CertificateDer<'static>] {
        &self.state.peer_certs
    }

    /// The peer's leaf-cert Subject DN. See [`ConnectionState::peer_dn`].
    pub fn peer_dn(&self) -> Result<String, PeerCertError> {
        peer_subject(self.peer_certs(), |s| Ok(s.to_string()))
    }

    /// The peer's Organizational Unit values. See [`ConnectionState::peer_ous`].
    pub fn peer_ous(&self) -> Result<Vec<String>, PeerCertError> {
        peer_subject(self.peer_certs(), |s| {
            Ok(s.iter_organizational_unit()
                .filter_map(|ou| ou.as_str().ok().map(String::from))
                .collect())
        })
    }
}

// ---------------------------------------------------------------------------
// Cert-chain inspection helpers (issue #18).
// ---------------------------------------------------------------------------

/// Errors arising from inspecting a peer's certificate chain.
#[derive(Debug, thiserror::Error)]
pub enum PeerCertError {
    /// No leaf cert was presented (empty chain). For TLS connections this
    /// shouldn't happen — the verifier in [`super::tls::ServerConfigBuilder`]
    /// requires a client cert; if you see this on TLS it's a bug.
    #[error("peer-cert: chain is empty")]
    EmptyChain,
    /// The leaf cert failed to parse as X.509.
    #[error("peer-cert: x509 parse: {0}")]
    Parse(String),
}

fn peer_subject<T, F>(chain: &[CertificateDer<'static>], f: F) -> Result<T, PeerCertError>
where
    F: FnOnce(&x509_parser::x509::X509Name<'_>) -> Result<T, PeerCertError>,
{
    let leaf = chain.first().ok_or(PeerCertError::EmptyChain)?;
    let (_, cert) = x509_parser::parse_x509_certificate(leaf)
        .map_err(|e| PeerCertError::Parse(e.to_string()))?;
    f(cert.subject())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    #[test]
    fn ids_are_monotonic_and_unique() {
        let a = ConnectionId::next();
        let b = ConnectionId::next();
        let c = ConnectionId::next();
        assert!(a.get() < b.get());
        assert!(b.get() < c.get());
        assert_ne!(a, b);
    }

    #[test]
    fn display_is_conn_hash_form() {
        let id = ConnectionId(42);
        assert_eq!(id.to_string(), "conn#42");
    }

    #[test]
    fn happy_path_three_state_progression() {
        let conn = ConnectionState::<Handshaking>::new(loopback());
        let id = conn.id;
        let addr = conn.peer_addr;

        let authed = conn.promote_to_authed(Vec::new());
        assert_eq!(authed.id, id, "id stable across handshake transition");
        assert_eq!(authed.peer_addr, addr);
        assert!(authed.peer_certs().is_empty());

        let streaming = authed.start_streaming();
        assert_eq!(streaming.id, id, "id stable across stream transition");
        assert_eq!(streaming.peer_addr, addr);
        assert!(streaming.peer_certs().is_empty());
    }

    #[test]
    fn cert_chain_carries_through_transitions() {
        let cert = CertificateDer::from(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        let conn = ConnectionState::<Handshaking>::new(loopback())
            .promote_to_authed(vec![cert.clone()])
            .start_streaming();
        assert_eq!(conn.peer_certs(), &[cert]);
    }
}
