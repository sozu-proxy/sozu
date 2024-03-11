use std::{sync::Arc, time::SystemTime};

use hyper::{
    client::{connect::dns::GaiResolver, HttpConnector, ResponseFuture},
    StatusCode,
};
use hyper_rustls::HttpsConnector;
use rustls::{
    client::{ClientConfig, ServerCertVerified, ServerCertVerifier},
    Certificate, ServerName,
};

// We implement our own verifier to allow self-signed certificates
#[derive(PartialEq, Eq, Clone, Debug)]
pub struct Verifier;

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        _end_entity: &Certificate,
        _intermediates: &[Certificate],
        _server_name: &ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: SystemTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
}

/// Build a Hyper HTTP Client that supports TLS and self signed certificates
pub fn build_https_client() -> hyper::Client<HttpsConnector<HttpConnector<GaiResolver>>, hyper::Body>
{
    let config = ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(Arc::new(Verifier))
        .with_no_client_auth();

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_http1()
        .build();

    hyper::Client::builder().build::<_, hyper::Body>(https)
}

/// Sends the request, awaits the response,
/// returns the status code and body in case of success
pub fn resolve_request(request: ResponseFuture) -> Option<(StatusCode, String)> {
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    rt.block_on(async {
        let response = match request.await {
            Ok(response) => response,
            Err(error) => {
                println!("Could not get response: {error}");
                return None;
            }
        };
        println!("Response: {response:?}");
        let status = response.status();
        let body_bytes = hyper::body::to_bytes(response.into_body())
            .await
            .expect("Could not get body");
        let body = String::from_utf8(body_bytes.to_vec()).expect("Invalid UTF-8 body");
        Some((status, body))
    })
}
