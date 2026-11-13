//! The registry's certificate authority and TLS listener.
//!
//! Each registry generates a CA and a server certificate signed by it when it starts (rcgen,
//! ECDSA P-256 over aws-lc-rs), so no key ever exists outside a running test and nothing is
//! committed. The client trusts the CA, never the server certificate itself: the data plane's
//! verifier (webpki under rustls-platform-verifier) refuses a CA certificate used as an end
//! entity, so the chain is the shape a real registry presents.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// A CA and the server certificate it signed for `127.0.0.1` and `localhost`.
pub(crate) struct Authority {
    pub(crate) ca_pem: String,
    pub(crate) ca_der: CertificateDer<'static>,
    leaf_der: CertificateDer<'static>,
    leaf_key: PrivateKeyDer<'static>,
}

fn other(err: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(err)
}

fn named(common_name: &str) -> DistinguishedName {
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, common_name);
    name
}

impl Authority {
    /// A fresh CA and server certificate.
    pub(crate) fn generate() -> io::Result<Self> {
        let ca_key = KeyPair::generate().map_err(other)?;
        let mut ca = CertificateParams::new(Vec::<String>::new()).map_err(other)?;
        ca.distinguished_name = named("mayfly fake registry CA");
        ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_cert = ca.self_signed(&ca_key).map_err(other)?;
        let issuer = Issuer::new(ca, ca_key);

        let leaf_key = KeyPair::generate().map_err(other)?;
        let mut leaf = CertificateParams::new(vec!["127.0.0.1".to_owned(), "localhost".to_owned()])
            .map_err(other)?;
        leaf.distinguished_name = named("localhost");
        leaf.is_ca = IsCa::ExplicitNoCa;
        leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        leaf.use_authority_key_identifier_extension = true;
        let leaf_cert = leaf.signed_by(&leaf_key, &issuer).map_err(other)?;
        Ok(Self {
            ca_pem: ca_cert.pem(),
            ca_der: ca_cert.der().clone(),
            leaf_der: leaf_cert.der().clone(),
            leaf_key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
        })
    }

    /// The server's TLS configuration: the server certificate and its CA as the chain, HTTP/1.1
    /// by ALPN (the server speaks nothing else).
    pub(crate) fn server_config(&self) -> io::Result<ServerConfig> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(other)?
            .with_no_client_auth()
            .with_single_cert(
                vec![self.leaf_der.clone(), self.ca_der.clone()],
                self.leaf_key.clone_key(),
            )
            .map_err(other)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }
}

/// A TCP listener whose connections are TLS, for `axum::serve`.
///
/// Limitations, stated rather than engineered away, because its only clients are the tests'
/// own:
/// - Handshakes run one at a time inside `accept`, with no deadline, so a client that connects
///   and never handshakes holds every later connection back. Acceptable for a test's single
///   client, never for a server facing strangers.
/// - A failed handshake is dropped and the next connection taken (a client that gives up is not
///   the server's fault).
/// - A failed accept is retried at once, with no backoff: a persistent error (out of file
///   descriptors) spins this thread until the registry is stopped. axum's own listener sleeps a
///   second between failed accepts; a real-time wait in a test-only crate needs ADR 0023's lint
///   exception, which a loopback test registry does not earn.
pub(crate) struct TlsListener {
    pub(crate) tcp: TcpListener,
    pub(crate) acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok((tcp, addr)) = self.tcp.accept().await else {
                continue;
            };
            if let Ok(tls) = self.acceptor.accept(tcp).await {
                return (tls, addr);
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}
