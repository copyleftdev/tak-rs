//! TLS streaming listener for port 8089 (and any other configured TLS port).
//!
//! [`accept_tls`] is the building block: one accept + TLS handshake + cert
//! extraction, returning an [`Authed`](crate::conn::Authed) connection
//! ready for whoever wants to drive bytes through it. Higher-level loops
//! (one task per accepted connection) are composed by callers via
//! [`crate::tasks::spawn`].
//!
//! Security invariant **C5**: this module never produces a connection in
//! [`Authed`](crate::conn::Authed) without the underlying rustls
//! handshake having succeeded; rustls's `WebPkiClientVerifier` (configured
//! by [`crate::tls::ServerConfigBuilder`]) gates that.

use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::CertificateDer;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::conn::{Authed, ConnectionState, Handshaking};

/// Accept one TCP connection, run the TLS handshake, and produce an
/// [`Authed`](ConnectionState) connection paired with the live stream.
///
/// This is the primitive — it does NOT loop and does NOT spawn a task.
/// Callers compose those concerns themselves. The intent is that
/// `tak-server` will call this in a `loop` and dispatch each result to a
/// `tasks::spawn` for the per-connection work.
///
/// # Errors
///
/// - I/O errors propagate from `TcpListener::accept` and from the rustls
///   handshake. Both yield [`AcceptError`]; the error variant tells you
///   whether to retry the listener (transient) or stop (fatal).
pub async fn accept_tls(
    listener: &TcpListener,
    acceptor: &TlsAcceptor,
) -> Result<(ConnectionState<Authed>, TlsStream<TcpStream>), AcceptError> {
    let (tcp, peer_addr) = listener.accept().await.map_err(AcceptError::Accept)?;
    let conn = ConnectionState::<Handshaking>::new(peer_addr);

    let tls_stream = acceptor
        .accept(tcp)
        .await
        .map_err(|err| AcceptError::Handshake {
            id: conn.id,
            err: err.to_string(),
        })?;

    // Extract the peer cert chain from the just-completed handshake.
    let (_, server_conn) = tls_stream.get_ref();
    let peer_certs: Vec<CertificateDer<'static>> = server_conn
        .peer_certificates()
        .map(<[CertificateDer<'static>]>::to_vec)
        .unwrap_or_default();

    let authed = conn.promote_to_authed(peer_certs);
    Ok((authed, tls_stream))
}

/// Convenience: build a [`TlsAcceptor`] from a `rustls::ServerConfig`.
#[must_use]
pub fn acceptor(config: ServerConfig) -> TlsAcceptor {
    TlsAcceptor::from(Arc::new(config))
}

/// Errors raised by [`accept_tls`].
#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    /// The underlying TCP accept failed. Usually transient on a listener
    /// that's still bound — caller should log and retry.
    #[error("tcp accept: {0}")]
    Accept(std::io::Error),
    /// The TLS handshake failed for a connection that successfully
    /// reached us via TCP. The connection's id is included so logs can
    /// correlate the failure with the accept event that produced it.
    #[error("tls handshake [{id}]: {err}")]
    Handshake {
        /// The id we allocated for this would-be connection.
        id: crate::conn::ConnectionId,
        /// Stringified rustls error; the original `rustls::Error` is
        /// rich but we collapse to a string here so AcceptError stays
        /// `Send + 'static` for use across task boundaries.
        err: String,
    },
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::disallowed_methods // tokio::spawn in tests is fine; production uses tasks::spawn
    )]

    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::time::Duration;

    use rcgen::{CertificateParams, DnType, KeyPair};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
    use rustls::{ClientConfig, RootCertStore};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::TlsConnector;

    use super::*;

    /// Generate (CA cert, CA key, server cert, server key, client cert, client key)
    /// for a full mTLS test rig. CA signs both server and client certs.
    fn mtls_certs() -> (
        CertificateDer<'static>,
        PrivateKeyDer<'static>,
        Vec<CertificateDer<'static>>,
        PrivateKeyDer<'static>,
        Vec<CertificateDer<'static>>,
        PrivateKeyDer<'static>,
    ) {
        // CA
        let mut ca_params = CertificateParams::new(vec!["TAK Test CA".to_owned()]).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "TAK Test CA");
        let ca_kp = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_kp).unwrap();

        // Server cert signed by CA
        let mut server_params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        server_params
            .distinguished_name
            .push(DnType::CommonName, "test.tak.server");
        let server_kp = KeyPair::generate().unwrap();
        let server_cert = server_params
            .signed_by(&server_kp, &ca_cert, &ca_kp)
            .unwrap();

        // Client cert signed by CA, with an OU we'll later assert on.
        // (rcgen's DistinguishedName collapses duplicate DnType pushes to
        // the last value, so we keep this to a single OU. Multi-OU support
        // is a real-world concern for some TAK deployments — covered by
        // the bytes-level x509 extraction, not this test.)
        let mut client_params = CertificateParams::new(vec!["TAK Test Client".to_owned()]).unwrap();
        client_params
            .distinguished_name
            .push(DnType::CommonName, "VIPER01");
        client_params
            .distinguished_name
            .push(DnType::OrganizationalUnitName, "Cyan");
        let client_kp = KeyPair::generate().unwrap();
        let client_cert = client_params
            .signed_by(&client_kp, &ca_cert, &ca_kp)
            .unwrap();

        let der_of = |c: &rcgen::Certificate| CertificateDer::from(c.der().to_vec());
        let key_of = |kp: &KeyPair| PrivateKeyDer::try_from(kp.serialize_der()).expect("key der");

        (
            der_of(&ca_cert),
            key_of(&ca_kp),
            vec![der_of(&server_cert), der_of(&ca_cert)],
            key_of(&server_kp),
            vec![der_of(&client_cert), der_of(&ca_cert)],
            key_of(&client_kp),
        )
    }

    fn build_server_config(
        ca: &CertificateDer<'static>,
        server_chain: Vec<CertificateDer<'static>>,
        server_key: PrivateKeyDer<'static>,
    ) -> rustls::ServerConfig {
        // ServerConfigBuilder takes PEM, but we already have DER from rcgen.
        // Build directly using rustls APIs to keep the test light.
        use rustls::server::WebPkiClientVerifier;
        use rustls::version::{TLS12, TLS13};

        let mut roots = RootCertStore::empty();
        roots.add(ca.clone()).unwrap();
        let provider = crate::tls::server_provider();
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .build()
                .unwrap();
        rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&TLS13, &TLS12])
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(server_chain, server_key)
            .unwrap()
    }

    fn build_client_config(
        ca: &CertificateDer<'static>,
        client_chain: Vec<CertificateDer<'static>>,
        client_key: PrivateKeyDer<'static>,
    ) -> rustls::ClientConfig {
        use rustls::version::{TLS12, TLS13};
        let mut roots = RootCertStore::empty();
        roots.add(ca.clone()).unwrap();
        let provider = crate::tls::server_provider();
        ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&TLS13, &TLS12])
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(client_chain, client_key)
            .unwrap()
    }

    #[tokio::test]
    async fn end_to_end_mtls_handshake_extracts_client_cert() {
        let (ca, _, server_chain, server_key, client_chain, client_key) = mtls_certs();

        let server_cfg = build_server_config(&ca, server_chain, server_key);
        let client_cfg = build_client_config(&ca, client_chain.clone(), client_key);

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let bound = listener.local_addr().unwrap();
        let acceptor = acceptor(server_cfg);

        // Server task: accept one connection, return the Authed conn.
        let server = tokio::spawn(async move { accept_tls(&listener, &acceptor).await });

        // Client side: connect, complete handshake, send a byte.
        let connector = TlsConnector::from(Arc::new(client_cfg));
        let tcp = TcpStream::connect(bound).await.unwrap();
        let server_name = ServerName::try_from("localhost").unwrap();
        let mut tls = connector.connect(server_name, tcp).await.unwrap();
        tls.write_all(b"hi").await.unwrap();
        tls.shutdown().await.unwrap();

        // Server side: handshake completed, cert chain extracted.
        let (authed, mut server_stream) = tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("server timed out")
            .expect("server task panicked")
            .expect("accept_tls failed");

        // Read the 2-byte hello to verify the stream is live.
        let mut buf = [0u8; 2];
        server_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");

        // Cert extraction: client presented its chain (leaf + ca).
        assert_eq!(authed.peer_certs().len(), 2);

        // Subject DN parsing.
        let dn = authed.peer_dn().expect("peer_dn parses");
        assert!(dn.contains("CN=VIPER01"), "got DN: {dn}");

        // OU extraction.
        let ous = authed.peer_ous().expect("peer_ous parses");
        assert_eq!(ous, vec!["Cyan".to_owned()], "got {ous:?}");
    }
}
