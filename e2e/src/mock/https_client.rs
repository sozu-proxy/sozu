use std::{sync::Arc, time::Duration};

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

/// Build an HTTP/1.1 client, plaintext or TLS, whose every connection leaves
/// from the loopback address `local` and is used for one request only.
///
/// For tests that need Sōzu to see distinct client source addresses without
/// a PROXY-protocol header: on Linux the whole of `127.0.0.0/8` is local, so
/// binding `127.0.0.N` before connecting needs no configuration. Pooling is
/// off (`pool_max_idle_per_host(0)`) so each request opens a frontend
/// connection of its own and reaches backend selection, instead of riding a
/// keep-alive connection whose backend was chosen by an earlier request.
pub fn build_https_client_from(local: std::net::IpAddr) -> HttpsClient {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_local_address(Some(local));

    let https = HttpsConnectorBuilder::new()
        .with_tls_config(insecure_tls_config())
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);

    Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(0)
        .build(https)
}

/// [`build_https_client_from`] speaking HTTP/2 only, negotiated over TLS by
/// ALPN. Each request needs a client of its own to open a connection of its
/// own: an H2 client multiplexes every request onto one connection, which Sōzu
/// serves as one session.
pub fn build_h2_client_from(local: std::net::IpAddr) -> HttpsClient {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_local_address(Some(local));

    let https = HttpsConnectorBuilder::new()
        .with_tls_config(insecure_tls_config())
        .https_or_http()
        .enable_http2()
        .wrap_connector(http);

    Client::builder(TokioExecutor::new())
        .http2_only(true)
        .pool_max_idle_per_host(0)
        .build(https)
}

/// Send `request` and return its status and body, under the same 10 s
/// timeout as [`resolve_request`]. For a request that needs more than a URI:
/// a header, a cookie.
pub fn resolve_prepared_request(
    client: &HttpsClient,
    request: hyper::Request<String>,
) -> Option<(StatusCode, String)> {
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    rt.block_on(async {
        let fut = async {
            let response = match client.request(request).await {
                Ok(response) => response,
                Err(error) => {
                    println!("Could not get response: {}", format_error_chain(&error));
                    return None;
                }
            };
            let status = response.status();
            let body = match response.into_body().collect().await {
                Ok(collected) => {
                    String::from_utf8(collected.to_bytes().to_vec()).unwrap_or_default()
                }
                Err(error) => {
                    println!("Could not get body: {}", format_error_chain(&error));
                    String::new()
                }
            };
            Some((status, body))
        };
        match tokio::time::timeout(Duration::from_secs(10), fut).await {
            Ok(result) => result,
            Err(_) => {
                println!("resolve_prepared_request timed out after 10s");
                None
            }
        }
    })
}

/// Send `requests` one after the other through `client` inside ONE runtime,
/// so a pooling client keeps using the connection the first one opened (an
/// H2 client multiplexes them all onto it), and return each status and body.
/// Each request gets the same 10 s timeout as [`resolve_request`].
pub fn resolve_prepared_requests_in_sequence(
    client: &HttpsClient,
    requests: Vec<hyper::Request<String>>,
) -> Vec<Option<(StatusCode, String)>> {
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    rt.block_on(async {
        let mut answers = Vec::with_capacity(requests.len());
        for request in requests {
            let fut = async {
                let response = match client.request(request).await {
                    Ok(response) => response,
                    Err(error) => {
                        println!("Could not get response: {}", format_error_chain(&error));
                        return None;
                    }
                };
                let status = response.status();
                let body = match response.into_body().collect().await {
                    Ok(collected) => {
                        String::from_utf8(collected.to_bytes().to_vec()).unwrap_or_default()
                    }
                    Err(error) => {
                        println!("Could not get body: {}", format_error_chain(&error));
                        String::new()
                    }
                };
                Some((status, body))
            };
            answers.push(
                tokio::time::timeout(Duration::from_secs(10), fut)
                    .await
                    .unwrap_or_else(|_| {
                        println!("resolve_prepared_requests_in_sequence timed out after 10s");
                        None
                    }),
            );
        }
        answers
    })
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

/// Render `error` and every link of its [`std::error::Error::source`]
/// chain on a single line.
///
/// Hyper's top-level `Display` is deliberately opaque: a request whose
/// connection died before a response arrived prints `client error
/// (SendRequest)` and nothing else, while the transport failure that
/// actually ended it — a TLS alert, a `ConnectionReset`, an H2 GOAWAY and
/// its error code — lives one or more `source()` links below and was
/// discarded at every call site in this module. Two CI failures on
/// 2026-09-21 (sozu#1393) reported exactly that eight-word string and
/// stayed undiagnosable:
/// `hsts_tests::test_hsts_on_https_unreachable_503` and
/// `protocol_pair_matrix::basic_auth::test_h2_h1`. Neither was a timeout —
/// both helpers print a distinct timeout message and neither appeared.
///
/// Rendered as `<outer> | caused by [1]: <source> | caused by [2]: ...`;
/// an error with no source renders exactly as `Display` did before, so no
/// existing log line loses information.
///
/// To SEE THIS RED: point any test's URI at a port nothing listens on.
/// Measured 2026-09-21 against `test_hsts_on_https_unreachable_503`:
/// `Could not get response: client error (Connect) | caused by [1]: tcp
/// connect error: Connection refused (os error 111) | caused by [2]:
/// Connection refused (os error 111)` — where the unchained print was the
/// bare `client error (Connect)`.
pub fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    let mut depth = 0usize;
    while let Some(cause) = source {
        depth += 1;
        rendered.push_str(&format!(" | caused by [{depth}]: {cause}"));
        source = cause.source();
    }
    rendered
}

/// Sends a GET request to the given URI using the provided client,
/// awaits the response, returns the status code and body in case of success
pub fn resolve_request(client: &HttpsClient, uri: hyper::Uri) -> Option<(StatusCode, String)> {
    resolve_request_timeout(client, uri, Duration::from_secs(10))
}

/// Variant of [`resolve_request`] that also returns the response
/// `HeaderMap`. Used by the HSTS e2e tests to assert on
/// `Strict-Transport-Security` (and any other response-side header
/// emitted by sozu) rather than relying on the body. Same 10 s
/// timeout as [`resolve_request`]; the inner future does not follow
/// redirects so 3xx default answers stay as-is for the assertion.
pub fn resolve_request_with_headers(
    client: &HttpsClient,
    uri: hyper::Uri,
) -> Option<(StatusCode, hyper::HeaderMap, String)> {
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    rt.block_on(async {
        let fut = async {
            let response = match client.get(uri).await {
                Ok(response) => response,
                Err(error) => {
                    println!("Could not get response: {}", format_error_chain(&error));
                    return None;
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            let body_bytes = match response.into_body().collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(error) => {
                    println!("Could not get body: {}", format_error_chain(&error));
                    return Some((status, headers, String::new()));
                }
            };
            let body = String::from_utf8(body_bytes.to_vec()).unwrap_or_default();
            Some((status, headers, body))
        };
        match tokio::time::timeout(Duration::from_secs(10), fut).await {
            Ok(result) => result,
            Err(_) => {
                println!("resolve_request_with_headers timed out after 10s");
                None
            }
        }
    })
}

pub fn resolve_request_timeout(
    client: &HttpsClient,
    uri: hyper::Uri,
    timeout: Duration,
) -> Option<(StatusCode, String)> {
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    rt.block_on(async {
        let fut = async {
            let response = match client.get(uri).await {
                Ok(response) => response,
                Err(error) => {
                    println!("Could not get response: {}", format_error_chain(&error));
                    return None;
                }
            };
            println!("Response: {response:?}");
            let status = response.status();
            let body_bytes = match response.into_body().collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(error) => {
                    println!("Could not get body: {}", format_error_chain(&error));
                    return Some((status, String::new()));
                }
            };
            let body = String::from_utf8(body_bytes.to_vec()).expect("Invalid UTF-8 body");
            Some((status, body))
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(result) => result,
            Err(_) => {
                println!("resolve_request timed out after {timeout:?}");
                None
            }
        }
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
        let fut = async {
            let request = hyper::Request::builder()
                .method(hyper::Method::POST)
                .uri(uri)
                .header("content-type", "application/octet-stream")
                .body(body)
                .expect("Could not build request");
            let response = match client.request(request).await {
                Ok(response) => response,
                Err(error) => {
                    println!("Could not get response: {}", format_error_chain(&error));
                    return None;
                }
            };
            println!("Response: {response:?}");
            let status = response.status();
            let body_bytes = match response.into_body().collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(error) => {
                    println!("Could not get body: {}", format_error_chain(&error));
                    return Some((status, String::new()));
                }
            };
            let body = String::from_utf8(body_bytes.to_vec()).expect("Invalid UTF-8 body");
            Some((status, body))
        };
        match tokio::time::timeout(Duration::from_secs(10), fut).await {
            Ok(result) => result,
            Err(_) => {
                println!("resolve_post_request timed out after 10s");
                None
            }
        }
    })
}

/// Sends multiple concurrent GET requests over a single H2 connection
pub fn resolve_concurrent_requests(
    client: &HttpsClient,
    uris: Vec<hyper::Uri>,
) -> Vec<Option<(StatusCode, String)>> {
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    rt.block_on(async {
        let fut = async {
            let futures: Vec<_> = uris
                .into_iter()
                .map(|uri| {
                    let client = client.clone();
                    async move {
                        let response = match client.get(uri).await {
                            Ok(response) => response,
                            Err(error) => {
                                println!("Could not get response: {}", format_error_chain(&error));
                                return None;
                            }
                        };
                        let status = response.status();
                        let body_bytes = match response.into_body().collect().await {
                            Ok(collected) => collected.to_bytes(),
                            Err(error) => {
                                println!("Could not get body: {}", format_error_chain(&error));
                                return Some((status, String::new()));
                            }
                        };
                        let body =
                            String::from_utf8(body_bytes.to_vec()).expect("Invalid UTF-8 body");
                        Some((status, body))
                    }
                })
                .collect();
            futures::future::join_all(futures).await
        };
        match tokio::time::timeout(Duration::from_secs(30), fut).await {
            Ok(result) => result,
            Err(_) => {
                println!("resolve_concurrent_requests timed out after 30s");
                Vec::new()
            }
        }
    })
}
