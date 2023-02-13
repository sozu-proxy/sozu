use hyper::{
    self,
    client::{connect::dns::GaiResolver, HttpConnector, ResponseFuture},
    StatusCode,
};
use hyper_tls::HttpsConnector;

/// Build a Hyper HTTP Client that supports TLS and self signed certificates
pub fn build_https_client() -> hyper::Client<HttpsConnector<HttpConnector<GaiResolver>>, hyper::Body>
{
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    let tls = hyper_tls::native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("Could not build TlsConnector");
    let https = HttpsConnector::from((http, tls.into()));
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
