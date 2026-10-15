//! End-to-end coverage for header predicates on HTTP frontends
//! (sozu-proxy/sozu#1253).
//!
//! A canary by header: two clusters share one hostname and path, the canary
//! frontend carrying an `exact` header predicate. Over HTTP/1.1 and HTTP/2
//! (both over TLS, so the H2 stream reaches the same `route_from_request` as
//! the H1 request — `lib/src/protocol/mux/router.rs`), a request carrying the
//! header must reach the canary backend and every other request the stable
//! one. Removing the canary frontend — which must name the same predicate —
//! must leave the stable frontend serving every request.
//!
//! Unit coverage of the selection rules lives beside `select_tree_rule`
//! (`lib/src/router/mod.rs`); this test proves the request's headers really
//! reach the router on both protocols.
//!
//! ## Test list
//! 1. [`test_header_predicate_canary_routes_h1_and_h2`]

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, HeaderMatchKind, HeaderPredicate,
        ListenerType, RequestHttpFrontend, ResponseStatus, SocketAddress, request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        https_client::{
            HttpsClient, build_h2_client, build_https_client, resolve_prepared_request,
        },
    },
    sozu::worker::Worker,
    tests::{State, provide_port, repeat_until_error_or, tests::create_local_address},
};

const STABLE: &str = "stable";
const CANARY: &str = "canary";
const CANARY_HEADER: &str = "X-Canary";
const CANARY_VALUE: &str = "always";

fn canary_frontend(front_address: SocketAddress) -> RequestHttpFrontend {
    RequestHttpFrontend {
        header_predicates: vec![HeaderPredicate {
            name: CANARY_HEADER.to_owned(),
            kind: HeaderMatchKind::Exact as i32,
            value: Some(CANARY_VALUE.to_owned()),
        }],
        ..Worker::default_http_frontend(CANARY, front_address.into())
    }
}

/// The body of the backend that answered one GET, on a fresh client so
/// every request opens its own frontend connection and is routed on its own.
fn reached(
    build_client: fn() -> HttpsClient,
    front_port: u16,
    header: Option<(&str, &str)>,
) -> Option<String> {
    let client = build_client();
    let mut builder = hyper::Request::builder()
        .method("GET")
        .uri(format!("https://localhost:{front_port}/api"));
    if let Some((name, value)) = header {
        builder = builder.header(name, value);
    }
    let request = builder
        .body(String::new())
        .expect("the test request must build");
    match resolve_prepared_request(&client, request) {
        Some((status, body)) if status.is_success() => Some(body),
        other => {
            println!("HEADER-ROUTING: request failed: {other:?}");
            None
        }
    }
}

/// Every protocol and header case of one phase, `expect_canary` naming the
/// cases a header-carrying request must reach the canary in.
fn check_phase(front_port: u16, expect_canary: bool, phase: &str) -> bool {
    let canary_body = if expect_canary { CANARY } else { STABLE };
    let mut ok = true;
    for (protocol, build_client) in [
        ("h1", build_https_client as fn() -> HttpsClient),
        ("h2", build_h2_client as fn() -> HttpsClient),
    ] {
        for (header, expected) in [
            (Some((CANARY_HEADER, CANARY_VALUE)), canary_body),
            (Some(("x-canary", CANARY_VALUE)), canary_body),
            (Some((CANARY_HEADER, "never")), STABLE),
            (None, STABLE),
        ] {
            let body = reached(build_client, front_port, header);
            if body.as_deref() != Some(expected) {
                println!(
                    "HEADER-ROUTING {phase} {protocol} header={header:?}: expected {expected}, reached {body:?}"
                );
                ok = false;
            }
        }
    }
    ok
}

/// To SEE THIS RED: in `lib/src/protocol/mux/router.rs`, pass
/// `&crate::router::NoHeaders` instead of `&FrontRequestHeaders(front)` to
/// `frontend_from_request` in `route_from_request`; every header-carrying
/// request of the first phase then reaches the stable backend.
fn try_header_predicate_canary_routes_h1_and_h2() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned("HEADER-ROUTING", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .expect("test https listener config must build"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address.clone(),
        certificate: CertificateAndKey {
            certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
            key: String::from(include_str!("../../../lib/assets/local-key.pem")),
            certificate_chain: vec![],
            versions: vec![],
            names: vec![],
        },
        expired_at: None,
    }));

    let mut backends = Vec::new();
    for cluster in [STABLE, CANARY] {
        let address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(cluster)));
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            cluster,
            format!("{cluster}-0"),
            address,
            None,
        )));
        backends.push(AsyncBackend::spawn_detached_backend(
            format!("BACKEND_{cluster}"),
            address,
            SimpleAggregator::default(),
            AsyncBackend::http_handler(cluster),
        ));
    }
    // The canary is declared FIRST: precedence must not depend on order.
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(canary_frontend(
        front_address.clone(),
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(
        Worker::default_http_frontend(STABLE, front_address.clone().into()),
    ));
    worker.read_to_last();

    let routed_with_canary = check_phase(front_port, true, "with-canary");

    worker.send_proxy_request_type(RequestType::RemoveHttpsFrontend(canary_frontend(
        front_address,
    )));
    let removed = worker
        .read_proxy_response()
        .map(|response| response.status == ResponseStatus::Ok as i32)
        .unwrap_or(false);
    let routed_without_canary = check_phase(front_port, false, "without-canary");

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    for mut backend in backends {
        backend.stop_and_get_aggregator();
    }

    println!(
        "HEADER-ROUTING: routed_with_canary={routed_with_canary} removed={removed} routed_without_canary={routed_without_canary} stopped={stopped}"
    );
    if routed_with_canary && removed && routed_without_canary && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_header_predicate_canary_routes_h1_and_h2() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "router: a header-predicate canary serves exactly the requests carrying its header, over H1 and H2",
            try_header_predicate_canary_routes_h1_and_h2,
        ),
        State::Success,
    );
}
