//! End-to-end coverage for malformed frontend hostnames reaching the
//! worker's route table.
//!
//! `TrieNode::insert` (`lib/src/router/pattern_trie.rs`) used to
//! `assert_ne!(insert_result, InsertResult::Failed)`, so a hostname the
//! trie grammar rejects — anything ending in `/` with no openable regex
//! segment (`example.com/`), a regex segment that is not `.`-anchored
//! (`abc/[0-9]+/.example.com`), a segment that is not a valid regex
//! (`/[/.example.com`), or an empty label (`.example.com`) — panicked the
//! worker instead of being refused. The master fans an `AddHttpFrontend`
//! out to every worker, so one such frontend killed all of them at once,
//! and killed them again on every restart state replay.
//!
//! A second class panicked one layer earlier: the unconditional
//! `DomainRule` parse walked `convert_regex_domain_rule` out of bounds on
//! a hostname whose last segment is followed by a bare trailing `.`
//! (`/a/.`), before the trie was ever reached.
//!
//! The contract these tests pin: the worker answers
//! `ResponseStatus::Failure`, stays alive, and its route table is
//! undisturbed — a subsequent well-formed frontend still installs on the
//! same listener.
//!
//! Scope note: the harness runs the worker as an in-process thread
//! (`Worker::start_new_worker_owned`), so a panicking worker fails this
//! test through the harness's panic on the dropped command channel — fast
//! and reliable, but the production failure mode (process death, main
//! fan-out, restart loop, state replay) is out of e2e reach by
//! construction.

use std::{io::Write, net::SocketAddr, time::Duration};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, ListenerType, Request, RequestHttpFrontend, ResponseStatus,
        request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend, client::Client,
    },
    sozu::worker::Worker,
    tests::{
        State,
        h2_utils::{
            H2_FRAME_DATA, H2Frame, build_chrome146_get_headers, collect_response_frames,
            h2_handshake, raw_h2_connection, setup_h2_test,
        },
        repeat_until_error_or,
        tests::create_local_address,
    },
};

/// Every hostname here is refused by `TrieNode::insert_recursive`, one
/// per rejection reason in the trie grammar.
const MALFORMED_HOSTNAMES: &[&str] = &[
    // Trailing `/` with no second slash to open a regex segment.
    "example.com/",
    "www.example.com/",
    "foo/",
    // A regex segment must be `.`-anchored on its left.
    "abc/[0-9]+/.example.com",
    // ... and must compile as a regex.
    "/[/.example.com",
    // Empty label (leading dot).
    ".example.com",
    // Bare separators.
    "/",
    "///",
    // Trailing `.` right after a segment: rejected by the `DomainRule`
    // parse (`convert_regex_domain_rule` used to index out of bounds on
    // these, a release panic upstream of the trie).
    "/a/.",
    "a/b/.",
    "x./y/.",
];

/// Spawn a worker with a single plain-HTTP listener. Mirrors the helper
/// in `hsts_tests.rs` — the cheapest setup that exposes the
/// `AddHttpFrontend` IPC entry point.
fn spawn_worker_with_http_listener(name: &str, front_address: std::net::SocketAddr) -> Worker {
    let (config, mut listeners, state) = Worker::empty_config();
    crate::port_registry::attach_reserved_http_listener(&mut listeners, front_address);
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpListener(
            ListenerBuilder::new_http(front_address.into())
                .to_http(None)
                .expect("default HTTP listener must build"),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: true,
        })),
    });
    worker.read_to_last();
    worker
}

/// A malformed hostname must come back as `ResponseStatus::Failure` and
/// leave the worker healthy enough to accept the next request. A panicking
/// worker fails this test through the harness: the worker thread's command
/// channel drops on unwind and `read_proxy_response` panics on the EOF.
pub fn try_malformed_hostnames_rejected() -> State {
    let front_address = create_local_address();
    let mut worker = spawn_worker_with_http_listener("ROUTER-HOSTNAME", front_address);

    worker.send_proxy_request(
        RequestType::AddCluster(Worker::default_cluster("malformed_hostname_cluster")).into(),
    );
    worker.read_to_last();

    for hostname in MALFORMED_HOSTNAMES {
        let mut frontend =
            Worker::default_http_frontend("malformed_hostname_cluster", front_address);
        frontend.hostname = (*hostname).to_owned();
        worker.send_proxy_request_type(RequestType::AddHttpFrontend(frontend));

        let Some(response) = worker.read_proxy_response() else {
            eprintln!("worker did not answer AddHttpFrontend for hostname {hostname:?}");
            return State::Fail;
        };
        if response.status != ResponseStatus::Failure as i32 {
            eprintln!(
                "hostname {hostname:?} must be refused, got status={} message={:?}",
                response.status, response.message
            );
            return State::Fail;
        }
    }

    // An oversized hostname (the router bounds hostnames to
    // `MAX_HOSTNAME_LENGTH` = 4096 bytes before parsing, against unbounded
    // trie recursion and pathological regex compilation) is refused
    // through the same path.
    let mut frontend = Worker::default_http_frontend("malformed_hostname_cluster", front_address);
    frontend.hostname = "a".repeat(4097);
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(frontend));
    let Some(response) = worker.read_proxy_response() else {
        eprintln!("worker did not answer the oversized AddHttpFrontend");
        return State::Fail;
    };
    if response.status != ResponseStatus::Failure as i32 {
        eprintln!(
            "an oversized hostname must be refused, got status={} message={:?}",
            response.status, response.message
        );
        return State::Fail;
    }

    // The worker survived every rejection: a well-formed frontend still
    // installs on the very same listener and route table.
    let frontend = Worker::default_http_frontend("malformed_hostname_cluster", front_address);
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(frontend));
    let Some(response) = worker.read_proxy_response() else {
        eprintln!("worker did not answer the well-formed AddHttpFrontend");
        return State::Fail;
    };
    if response.status != ResponseStatus::Ok as i32 {
        eprintln!(
            "a well-formed frontend must still install after the rejections, \
             got status={} message={:?}",
            response.status, response.message
        );
        return State::Fail;
    }

    worker.soft_stop();
    if !worker.wait_for_server_stop() {
        eprintln!("worker did not stop cleanly after the rejections");
        return State::Fail;
    }

    State::Success
}

#[test]
fn test_malformed_hostnames_rejected() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "malformed frontend hostnames are refused without killing the worker",
            try_malformed_hostnames_rejected
        ),
        State::Success
    );
}

// =========================================================================
// Test 2 + 3: a case-varying Host / `:authority` reaches its frontend
// =========================================================================

/// The frontend both case tests below install. Spelled lowercase, which
/// is what an operator writes and what `idna::domain_to_ascii` stores
/// either way.
const CASE_CLUSTER: &str = "host_case_cluster";
const CASE_HOSTNAME: &str = "case.example.com";

/// One HTTP/1.1 request on a fresh connection carrying `host_header`
/// verbatim. `Connection: close` ends the answer on EOF, so the read loop
/// stops on the response rather than on the deadline — including for the
/// builtin 404, which closes.
fn request_with_host(
    front_address: std::net::SocketAddr,
    host_header: &str,
    label: &str,
) -> Option<String> {
    let mut client = Client::new(
        format!("HOST-CASE-{label}"),
        front_address,
        format!("GET / HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n"),
    );
    client.connect();
    client.send();
    let response = client.receive_until_eof(Duration::from_secs(3));
    println!("HOST-CASE-{label}: {response:?}");
    response
}

/// RFC 9110 §4.2.3: the host is case-insensitive. `Router::add_tree_rule`
/// normalises a configured hostname through `idna::domain_to_ascii`
/// (ASCII-lowercasing), while `Router::lookup` walked the trie with the
/// client's raw bytes — so `Host: CASE.EXAMPLE.COM` reached no frontend
/// and no configuration fixed it, because declaring the frontend in
/// uppercase got lowercased on the way in too.
///
/// The unit tests in `lib/src/router/mod.rs` pin `Router::lookup` itself.
/// This one closes the gap the issue called out explicitly — the defect
/// had only ever been traced through source and exercised through
/// `hostname_and_port` plus `Router::lookup`, never against a live proxy
/// over a real socket. It goes through the whole live H1 stack:
/// `mux::Connection::new_h1_server` (`lib/src/http.rs`) → kawa parse →
/// `HttpContext::on_request_headers` → `mux::router::route_from_request`
/// → `HttpProxyListener::frontend_from_request` → `hostname_and_port` →
/// `Router::lookup`.
///
/// To SEE THIS RED: in `Router::lookup` (`lib/src/router/mod.rs`),
/// replace `let normalized_hostname = normalize_hostname(hostname);`
/// with `let normalized_hostname = Cow::Borrowed(hostname);`.
/// `upper` and `mixed` turn false while `lower` and `unrelated` stay
/// true — the asymmetry, over a real socket.
fn try_a_case_varying_host_header_routes_to_its_frontend() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();

    let (config, mut listeners, state) = Worker::empty_config();
    crate::port_registry::attach_reserved_http_listener(&mut listeners, front_address);
    let mut worker = Worker::start_new_worker_owned("HOST-CASE", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpListener(
        ListenerBuilder::new_http(front_address.into())
            .to_http(None)
            .expect("the http listener config must build"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        CASE_CLUSTER,
    )));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        CASE_CLUSTER,
        format!("{CASE_CLUSTER}-0"),
        back_address,
        None,
    )));
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(RequestHttpFrontend {
        hostname: CASE_HOSTNAME.to_owned(),
        ..Worker::default_http_frontend(CASE_CLUSTER, front_address)
    }));
    worker.read_to_last();

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_0",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong"),
    );

    let routed = |host_header: &str, label: &str| {
        request_with_host(front_address, host_header, label)
            .as_deref()
            .map(|response| response.starts_with("HTTP/1.1 200"))
            .unwrap_or(false)
    };

    let lower = routed(CASE_HOSTNAME, "lower");
    let upper = routed("CASE.EXAMPLE.COM", "upper");
    let mixed = routed("CaSe.ExAmPlE.cOm", "mixed");
    let upper_with_port = routed("CASE.EXAMPLE.COM:80", "upper-port");
    // Normalising the case must not make unrelated hosts match: an
    // unconfigured hostname still falls through to the builtin 404. The
    // answer's `route` field must also echo the spelling the CLIENT sent,
    // not the normalised key — `RouterError::RouteNotFound` carries the
    // original hostname on purpose, because that is the diagnostic an
    // operator needs to see when a route misses.
    let unrelated_answer = request_with_host(front_address, "OTHER.EXAMPLE.COM", "unrelated");
    let unrelated = unrelated_answer
        .as_deref()
        .map(|response| response.starts_with("HTTP/1.1 404"))
        .unwrap_or(false);
    let diagnostic_keeps_client_bytes = unrelated_answer
        .as_deref()
        .map(|response| response.contains("GET OTHER.EXAMPLE.COM/"))
        .unwrap_or(false);

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    backend.stop_and_get_aggregator();

    println!(
        "HOST-CASE: lower={lower} upper={upper} mixed={mixed} \
         upper_with_port={upper_with_port} unrelated_still_404={unrelated} \
         diagnostic_keeps_client_bytes={diagnostic_keeps_client_bytes} stopped={stopped}"
    );
    if lower
        && upper
        && mixed
        && upper_with_port
        && unrelated
        && diagnostic_keeps_client_bytes
        && stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_a_case_varying_host_header_routes_to_its_frontend() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "router: an uppercase or mixed-case Host header reaches its lowercase frontend",
            try_a_case_varying_host_header_routes_to_its_frontend,
        ),
        State::Success,
    );
}

/// The H2 leg of the same contract. H1 and H2 are two different parsers
/// but one route path: `pkawa`'s HPACK decode and kawa's H1 parse both
/// drive `HttpContext::on_request_headers`, and both reach
/// `mux::router::route_from_request` → `frontend_from_request` →
/// `Router::lookup`. Covering only H1 would leave the H2 half asserted by
/// reading the code.
///
/// `setup_h2_test` installs an HTTPS frontend for `localhost` and serves
/// the repository's `local-certificate.pem`; the request below sends
/// `:authority: LOCALHOST` over SNI `localhost`. The TLS↔authority
/// binding (`authority_matched_cert_name`) is already ASCII
/// case-insensitive, so the request reaches routing and the routing
/// answer is what this test reads.
///
/// The backend body `pong0` is the routing witness: Sōzu's builtin 404
/// answer also arrives as a DATA frame, so the frame type alone proves
/// nothing.
///
/// To SEE THIS RED: same mutation as
/// `try_a_case_varying_host_header_routes_to_its_frontend`. `upper` turns
/// false while `lower` stays true.
fn try_a_case_varying_h2_authority_routes_to_its_frontend() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_test("H2-HOST-CASE", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}")
        .parse()
        .expect("the h2 front address must parse");

    let reaches_backend = |authority: &str| {
        let mut tls = raw_h2_connection(front_addr);
        h2_handshake(&mut tls);
        let block = build_chrome146_get_headers(authority, "/", None);
        tls.write_all(&H2Frame::headers(1, block, true, true).encode())
            .expect("the HEADERS frame must be written");
        tls.flush().expect("the HEADERS frame must flush");

        let frames = collect_response_frames(&mut tls, 200, 6, 300);
        let body: Vec<u8> = frames
            .iter()
            .filter(|(frame_type, _, _, _)| *frame_type == H2_FRAME_DATA)
            .flat_map(|(_, _, _, payload)| payload.iter().copied())
            .collect();
        let reached = String::from_utf8_lossy(&body).contains("pong0");
        println!("H2-HOST-CASE {authority:?}: reached_backend={reached}");
        reached
    };

    let lower = reaches_backend("localhost");
    let upper = reaches_backend("LOCALHOST");
    let mixed = reaches_backend("LoCaLhOsT");

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    for backend in &mut backends {
        backend.stop_and_get_aggregator();
    }

    println!("H2-HOST-CASE: lower={lower} upper={upper} mixed={mixed} stopped={stopped}");
    if lower && upper && mixed && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_a_case_varying_h2_authority_routes_to_its_frontend() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "router: an uppercase or mixed-case H2 :authority reaches its lowercase frontend",
            try_a_case_varying_h2_authority_routes_to_its_frontend,
        ),
        State::Success,
    );
}
