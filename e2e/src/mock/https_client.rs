use std::sync::Arc;

use http_body_util::BodyExt;
use hyper::StatusCode;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use rustls::{
    ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};

// We implement our own verifier to allow self-signed certificates
#[derive(Debug)]
pub struct Verifier;

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}

pub type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, String>;

fn insecure_tls_config() -> ClientConfig {
    ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier))
        .with_no_client_auth()
}

/// Build a Hyper HTTP Client that supports TLS and self signed certificates
pub fn build_https_client() -> HttpsClient {
    let config = insecure_tls_config();

    let https = HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_http1()
        .build();

    Client::builder(TokioExecutor::new()).build(https)
}

/// Build a Hyper HTTP Client that negotiates H2 via ALPN over TLS.
/// The connector advertises only "h2" in ALPN and the client is forced to HTTP/2.
pub fn build_h2_client() -> HttpsClient {
    let config = insecure_tls_config();

    let https = HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_http2()
        .build();

    Client::builder(TokioExecutor::new())
        .http2_only(true)
        .build(https)
}

/// Build a Hyper HTTP Client that advertises both h2 and http/1.1 via ALPN,
/// letting the server choose the protocol.
pub fn build_h2_or_h1_client() -> HttpsClient {
    let config = insecure_tls_config();

    let https = HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_all_versions()
        .build();

    Client::builder(TokioExecutor::new()).build(https)
}

/// Sends a GET request to the given URI using the provided client,
/// awaits the response, returns the status code and body in case of success
pub fn resolve_request(client: &HttpsClient, uri: hyper::Uri) -> Option<(StatusCode, String)> {
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    rt.block_on(async {
        let response = match client.get(uri).await {
            Ok(response) => response,
            Err(error) => {
                println!("Could not get response: {error}");
                return None;
            }
        };
        println!("Response: {response:?}");
        let status = response.status();
        let body_bytes = response
            .into_body()
            .collect()
            .await
            .expect("Could not get body")
            .to_bytes();
        let body = String::from_utf8(body_bytes.to_vec()).expect("Invalid UTF-8 body");
        Some((status, body))
    })
}

/// Sends a POST request with the given body, returns status code and response body
pub fn resolve_post_request(
    client: &HttpsClient,
    uri: hyper::Uri,
    body: String,
) -> Option<(StatusCode, String)> {
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    rt.block_on(async {
        let request = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(uri)
            .header("content-type", "application/octet-stream")
            .body(body)
            .expect("Could not build request");
        let response = match client.request(request).await {
            Ok(response) => response,
            Err(error) => {
                println!("Could not get response: {error}");
                return None;
            }
        };
        println!("Response: {response:?}");
        let status = response.status();
        let body_bytes = response
            .into_body()
            .collect()
            .await
            .expect("Could not get body")
            .to_bytes();
        let body = String::from_utf8(body_bytes.to_vec()).expect("Invalid UTF-8 body");
        Some((status, body))
    })
}

/// Sends multiple concurrent GET requests over a single H2 connection
pub fn resolve_concurrent_requests(
    client: &HttpsClient,
    uris: Vec<hyper::Uri>,
) -> Vec<Option<(StatusCode, String)>> {
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    rt.block_on(async {
        let futures: Vec<_> = uris
            .into_iter()
            .map(|uri| {
                let client = client.clone();
                async move {
                    let response = match client.get(uri).await {
                        Ok(response) => response,
                        Err(error) => {
                            println!("Could not get response: {error}");
                            return None;
                        }
                    };
                    let status = response.status();
                    let body_bytes = response
                        .into_body()
                        .collect()
                        .await
                        .expect("Could not get body")
                        .to_bytes();
                    let body = String::from_utf8(body_bytes.to_vec()).expect("Invalid UTF-8 body");
                    Some((status, body))
                }
            })
            .collect();
        futures::future::join_all(futures).await
    })
}
