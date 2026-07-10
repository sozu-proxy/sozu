//! TLS-terminating mock backend for the TCP passthrough SNI+ALPN preread
//! e2e coverage (sozu-proxy/sozu#1279, `e2e/src/tests/tcp_sni_tests.rs`).
//!
//! Every other mock backend in this crate (`sync_backend`, `async_backend`,
//! `h2_backend`, ...) either speaks plaintext HTTP or is fronted by Sōzu's
//! OWN TLS termination. The TCP-passthrough SNI preread is different in
//! kind: Sōzu decides a route from the ClientHello's SNI/ALPN and then
//! forwards the RAW bytes verbatim -- it never terminates TLS. Proving
//! that end-to-end requires a backend that actually completes a TLS
//! handshake of its own (this module), so an in-test client can observe:
//!
//! - which backend's OWN certificate it received (proving Sōzu didn't
//!   substitute a single shared front cert, which is what a terminating
//!   proxy would do), and
//! - whether its OWN client certificate reached the backend intact
//!   (mTLS), which a terminating proxy would break by presenting its own
//!   identity to the backend instead of relaying the client's.
//!
//! Every other V8/V9/V10/V15 case in `tcp_sni_tests.rs` only needs to
//! observe RAW bytes (Sōzu forwards them unchanged regardless of what
//! they contain), so those use a plain raw-byte-capture backend instead
//! of this module -- see `tcp_sni_tests.rs`'s `spawn_raw_capture_backend`.

use std::{
    io::{Read, Write},
    net::SocketAddr,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use rustls::{
    RootCertStore, ServerConfig, ServerConnection, StreamOwned,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::WebPkiClientVerifier,
};

use crate::port_registry::bind_std_listener;

/// What one accepted connection observed, captured for test assertions.
#[derive(Debug, Default, Clone)]
pub struct TlsBackendCapture {
    /// Plaintext bytes read from the established TLS session -- the
    /// "request" the client sent once the handshake completed.
    pub received: Vec<u8>,
    /// The SNI hostname the CLIENT presented in its ClientHello, as
    /// observed server-side by rustls once the handshake completes.
    /// `None` would mean the client sent no SNI at all -- every test
    /// driving this mock sends one, so this is always `Some` in
    /// practice, but the accessor stays honest about rustls's own
    /// `Option`.
    pub sni_seen: Option<String>,
    /// DER bytes of the client's leaf certificate. `Some` only when the
    /// backend requires client auth (mTLS mode,
    /// [`server_config_requiring_client_cert`]) AND the client actually
    /// presented one.
    pub client_cert_der: Option<Vec<u8>>,
}

/// Build a single-cert `ServerConfig` (no client auth) from a PEM cert +
/// key pair -- the plain SNI-routing backends (V7 backend A / backend B).
pub fn server_config_single_cert(cert_pem: &[u8], key_pem: &[u8]) -> Arc<ServerConfig> {
    let cert = CertificateDer::from_pem_slice(cert_pem).expect("parse TLS backend certificate PEM");
    let key = PrivateKeyDer::from_pem_slice(key_pem).expect("parse TLS backend key PEM");
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("build single-cert rustls ServerConfig");
    Arc::new(config)
}

/// Build a `ServerConfig` that REQUIRES a client certificate, verified
/// against `ca_cert_pem` as the sole trust anchor (V7 mTLS backend, the
/// Kubernetes-API-server-style case). A client that presents no
/// certificate, or one not signed by this CA, fails the handshake --
/// [`spawn_accept_one`] surfaces that as a `None` capture.
pub fn server_config_requiring_client_cert(
    cert_pem: &[u8],
    key_pem: &[u8],
    ca_cert_pem: &[u8],
) -> Arc<ServerConfig> {
    let cert =
        CertificateDer::from_pem_slice(cert_pem).expect("parse mTLS backend certificate PEM");
    let key = PrivateKeyDer::from_pem_slice(key_pem).expect("parse mTLS backend key PEM");
    let ca = CertificateDer::from_pem_slice(ca_cert_pem).expect("parse mTLS CA certificate PEM");

    let mut roots = RootCertStore::empty();
    roots.add(ca).expect("add throwaway test CA to root store");
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .expect("build WebPkiClientVerifier requiring a client cert");

    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![cert], key)
        .expect("build client-auth rustls ServerConfig");
    Arc::new(config)
}

/// Spawn a thread that binds `address`, accepts EXACTLY ONE TCP
/// connection, completes a TLS handshake using `server_config`, reads
/// whatever plaintext bytes the client sends (one `read` -- every caller
/// controls its own request framing so a single read is enough to
/// observe "the client's request"), writes `response` back over the same
/// TLS session, and returns the resulting [`TlsBackendCapture`].
///
/// Returns `None` (the join handle yields `None`) when the accept, the
/// handshake, or the read/write fails -- in particular, the mTLS
/// negative-space case (a client with no certificate against a verifier
/// that requires one) fails INSIDE the first `read` (rustls drives
/// `complete_io` there), so a `None` capture is this function's positive
/// signal that the backend itself rejected the client, not Sōzu.
pub fn spawn_accept_one(
    name: &'static str,
    address: SocketAddr,
    server_config: Arc<ServerConfig>,
    response: &'static [u8],
) -> JoinHandle<Option<TlsBackendCapture>> {
    thread::spawn(move || {
        let listener = bind_std_listener(address, name);
        listener
            .set_nonblocking(false)
            .expect("could not set blocking on TLS backend listener");

        let (stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) => {
                println!("{name}: accept failed: {error}");
                return None;
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set TLS backend read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set TLS backend write timeout");

        let conn = match ServerConnection::new(server_config) {
            Ok(conn) => conn,
            Err(error) => {
                println!("{name}: could not build ServerConnection: {error}");
                return None;
            }
        };
        let mut tls = StreamOwned::new(conn, stream);

        // The first `read` is where the handshake actually happens: rustls'
        // blanket `Read for StreamOwned` impl drives `complete_io` before
        // returning plaintext. A client-cert verification failure (mTLS
        // reject case) or any other handshake error surfaces here as an
        // `Err`, never as a corrupted/partial plaintext read.
        let mut buf = [0u8; 4096];
        let n = match tls.read(&mut buf) {
            Ok(n) => n,
            Err(error) => {
                println!(
                    "{name}: handshake/read failed (expected for the mTLS-no-client-cert case): {error}"
                );
                return None;
            }
        };

        let sni_seen = tls.conn.server_name().map(str::to_owned);
        let client_cert_der = tls
            .conn
            .peer_certificates()
            .and_then(|certs| certs.first())
            .map(|cert| cert.as_ref().to_vec());

        if let Err(error) = tls.write_all(response) {
            println!("{name}: write failed: {error}");
        }
        let _ = tls.flush();

        Some(TlsBackendCapture {
            received: buf[..n].to_vec(),
            sni_seen,
            client_cert_der,
        })
    })
}
