//! End-to-end coverage for the `--path-equals` frontend lifecycle in the
//! worker's route table.
//!
//! `PathRule`'s hand-written `PartialEq` (`lib/src/router/mod.rs`) had arms for
//! `Prefix` and `Regex` only, so `(Equals, Equals)` fell into `_ => false` and
//! two identical `PathRule::Equals` compared unequal. Two live verbs answered
//! the wrong thing because of it:
//!
//! * `add_http_front` could never deduplicate such a frontend, so re-adding it
//!   pushed a second, unreachable copy instead of the `RouterError::AddRoute`
//!   that `Prefix` and `Regex` frontends have always produced;
//! * `remove_tree_rule`'s `retain` matched nothing and returned `true`
//!   unconditionally, so `remove_http_front` answered `Ok` for a frontend it
//!   left in place — and still routing. That last part is what no unit test on
//!   `Router` alone can falsify: it takes a real worker, a real listener and a
//!   real request to see that traffic kept flowing after a successful removal.
//!
//! The worker is deliberately given ONLY the path-equals frontend, with no
//! catch-all prefix route: after the removal the request must fall through to
//! the builtin 404 answer, which is the observable "stopped routing".
//!
//! Setup mirrors `router_hostname_tests::spawn_worker_with_http_listener`.
//!
//! ## Test list
//! 1. [`test_path_equals_frontend_is_deduplicated_and_stops_routing_when_removed`]
//! 2. [`test_path_only_regex_routes_on_the_path_and_carries_the_query_over`]

use std::{net::SocketAddr, time::Duration};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, ListenerType, PathRule, RedirectPolicy, RequestHttpFrontend,
        ResponseStatus, request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend, client::Client,
        sync_backend::Backend as SyncBackend,
    },
    port_registry::attach_reserved_http_listener,
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or},
};

use super::tests::create_local_address;

const CLUSTER: &str = "path_equals_cluster";

/// The single `--path-equals /exact` frontend these tests add, remove and
/// re-add. Built fresh on every call so each request carries an identical —
/// but distinct — value, which is the whole point: equality, not identity, is
/// what the router must key on.
fn path_equals_frontend(front_address: SocketAddr) -> RequestHttpFrontend {
    RequestHttpFrontend {
        path: PathRule::equals(String::from("/exact")),
        ..Worker::default_http_frontend(CLUSTER, front_address)
    }
}

/// One HTTP/1.1 request on a fresh connection. `Connection: close` ends the
/// answer on EOF, so the read loop stops on the response rather than on the
/// deadline — including for the builtin 404, which closes.
fn request_path(front_address: SocketAddr, path: &str, label: &str) -> Option<String> {
    let mut client = Client::new(
        format!("PATH-{label}"),
        front_address,
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
    );
    client.connect();
    client.send();
    let response = client.receive_until_eof(Duration::from_secs(3));
    println!("PATH-{label}: {response:?}");
    response
}

// =========================================================================
// Test 1: a `--path-equals` frontend deduplicates on add and really stops
// routing on remove
// =========================================================================

/// To SEE THIS RED: in `lib/src/router/mod.rs`, delete the
/// `(PathRule::Equals(s1), PathRule::Equals(s2)) => s1 == s2,` arm from
/// `impl PartialEq for PathRule` so the pair falls back into `_ => false`, and
/// run with `RUSTFLAGS="-C debug-assertions=off"`. `re_add_refused` turns
/// false (the duplicate is accepted and a second unreachable copy is pushed)
/// and `stopped_routing` turns false (the removal still answers `Ok`, but
/// `GET /exact` keeps reaching the backend with 200) — which is exactly the
/// production release behaviour the fix removed.
///
/// The flag is load-bearing, and it is what makes this an e2e test rather than
/// a unit test: `add_tree_rule`'s own post-condition
/// ("a freshly inserted tree domain must resolve to its inserted rule",
/// `lib/src/router/mod.rs`) predates the fix and catches the broken `PartialEq`
/// at ADD time in any debug build, so a plain `cargo test` under the mutation
/// fails on a dead worker instead of on the routing assertions below. With the
/// assertions compiled out, the two verbs answer exactly as they did in a
/// pre-fix release binary, and those assertions are what fail.
fn try_path_equals_frontend_is_deduplicated_and_stops_routing_when_removed() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();

    let (config, mut listeners, state) = Worker::empty_config();
    attach_reserved_http_listener(&mut listeners, front_address);
    let mut worker = Worker::start_new_worker_owned("PATH-EQUALS", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpListener(
        ListenerBuilder::new_http(front_address.into())
            .to_http(None)
            .expect("could not build the http listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.into(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(CLUSTER)));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        CLUSTER,
        format!("{CLUSTER}-0"),
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_0",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong"),
    );

    // Add the path-equals frontend: it must install and route.
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(path_equals_frontend(
        front_address,
    )));
    let added = worker
        .read_proxy_response()
        .map(|response| response.status == ResponseStatus::Ok as i32)
        .unwrap_or(false);

    let before = request_path(front_address, "/exact", "before");
    let routed_before = before
        .as_deref()
        .map(|response| response.starts_with("HTTP/1.1 200"))
        .unwrap_or(false);

    // An identical frontend is already stored: the router must refuse it
    // rather than push a second, unremovable copy.
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(path_equals_frontend(
        front_address,
    )));
    let re_add_refused = worker
        .read_proxy_response()
        .map(|response| response.status == ResponseStatus::Failure as i32)
        .unwrap_or(false);

    // Remove it, then prove it STOPPED ROUTING — the pre-fix defect was a
    // removal that reported success while traffic kept flowing.
    worker.send_proxy_request_type(RequestType::RemoveHttpFrontend(path_equals_frontend(
        front_address,
    )));
    let removed = worker
        .read_proxy_response()
        .map(|response| response.status == ResponseStatus::Ok as i32)
        .unwrap_or(false);

    let after = request_path(front_address, "/exact", "after");
    let stopped_routing = after
        .as_deref()
        .map(|response| response.starts_with("HTTP/1.1 404"))
        .unwrap_or(false);

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    backend.stop_and_get_aggregator();

    println!(
        "PATH-EQUALS: added={added} routed_before={routed_before} re_add_refused={re_add_refused} removed={removed} stopped_routing={stopped_routing} stopped={stopped}"
    );
    if added && routed_before && re_add_refused && removed && stopped_routing && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_path_equals_frontend_is_deduplicated_and_stops_routing_when_removed() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "router: a --path-equals frontend is refused on re-add and really stops routing once removed",
            try_path_equals_frontend_is_deduplicated_and_stops_routing_when_removed,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 2: a `REGEX_PATH` frontend routes on the path alone and carries the
// query string over to its rewritten path and redirect `Location`
// =========================================================================

const REGEX_PATH_CLUSTER: &str = "path_only_regex_cluster";

/// How long the sync backend waits for sozu to open the forwarded connection.
const REGEX_PATH_ACCEPT_BUDGET: Duration = Duration::from_secs(3);

/// sozu-proxy/sozu#1909 over the wire, through one worker carrying three
/// `REGEX_PATH` frontends:
///
/// * `^/api/v2$` rewritten to `/v2` — `GET /api/v2?x=1` routes although the
///   pattern spells no query (a `REGEX` would not), and the backend receives
///   `GET /v2?x=1`: the query is appended to the rewritten path;
/// * `/only/.+` — `GET /only/?x=1` gets the builtin 404, because the path
///   `/only/` has nothing after the slash (a `REGEX` would match `?x=1`);
/// * `/redir/(.+)` as a permanent redirect rewritten to `/new/$PATH[1]` —
///   `GET /redir/a?y=2` answers 301 with `Location: http://localhost/new/a?y=2`:
///   the capture stops at `?` and the redirect carries the query over too.
///
/// To SEE THIS RED: in `PathRule::matches` (`lib/src/router/mod.rs`) make the
/// `RegexPath` arm test the whole request-target instead of its `split_query`
/// path half: `GET /only/?x=1` is routed instead of answering 404, and
/// `GET /api/v2?x=1` answers 404 instead of reaching the backend. The
/// redirect leg stays green under that mutation (`(.+)` then captures the
/// query itself); `RouteResult::from_frontend`'s query append is held by the
/// router unit test
/// `a_path_only_regex_rewrite_captures_the_path_and_carries_the_query_over`.
fn try_path_only_regex_routes_on_the_path_and_carries_the_query_over() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();

    let (config, mut listeners, state) = Worker::empty_config();
    attach_reserved_http_listener(&mut listeners, front_address);
    let mut worker = Worker::start_new_worker_owned("PATH-REGEX-PATH", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpListener(
        ListenerBuilder::new_http(front_address.into())
            .to_http(None)
            .expect("could not build the http listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.into(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        REGEX_PATH_CLUSTER,
    )));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        REGEX_PATH_CLUSTER,
        format!("{REGEX_PATH_CLUSTER}-0"),
        back_address,
        None,
    )));
    for frontend in [
        RequestHttpFrontend {
            path: PathRule::regex_path("^/api/v2$"),
            rewrite_path: Some("/v2".to_owned()),
            ..Worker::default_http_frontend(REGEX_PATH_CLUSTER, front_address)
        },
        RequestHttpFrontend {
            path: PathRule::regex_path("/only/.+"),
            ..Worker::default_http_frontend(REGEX_PATH_CLUSTER, front_address)
        },
        RequestHttpFrontend {
            path: PathRule::regex_path("/redir/(.+)"),
            rewrite_path: Some("/new/$PATH[1]".to_owned()),
            redirect: Some(RedirectPolicy::Permanent as i32),
            ..Worker::default_http_frontend(REGEX_PATH_CLUSTER, front_address)
        },
    ] {
        worker.send_proxy_request_type(RequestType::AddHttpFrontend(frontend));
    }
    worker.read_to_last();

    let mut backend = SyncBackend::new(
        "PATH-REGEX-PATH-BACKEND",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();

    // The query must not take part in the match: `/only/` is the whole path.
    let unrouted = request_path(front_address, "/only/?x=1", "regex-path-404")
        .is_some_and(|response| response.starts_with("HTTP/1.1 404"));

    // The redirect `Location` follows the rewrite, query included.
    let redirected =
        request_path(front_address, "/redir/a?y=2", "regex-path-301").is_some_and(|response| {
            response.starts_with("HTTP/1.1 301")
                && response
                    .to_ascii_lowercase()
                    .contains("location: http://localhost/new/a?y=2\r\n")
        });

    // Routed although `^/api/v2$` spells no query, rewritten with it appended.
    let mut client = Client::new(
        "PATH-regex-path-forward",
        front_address,
        "GET /api/v2?x=1 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client.connect();
    client.send();
    let deadline = std::time::Instant::now() + REGEX_PATH_ACCEPT_BUDGET;
    let mut accepted = backend.accept(0);
    while !accepted && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
        accepted = backend.accept(0);
    }
    let received = if accepted { backend.receive(0) } else { None };
    if accepted {
        backend.send(0);
    }
    let answered = client
        .receive_until_eof(Duration::from_secs(3))
        .is_some_and(|response| response.starts_with("HTTP/1.1 200"));
    println!("PATH-REGEX-PATH: backend received {received:?}");
    let forwarded_with_query = received
        .as_deref()
        .is_some_and(|request| request.starts_with("GET /v2?x=1 HTTP/1.1\r\n"));

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    println!(
        "PATH-REGEX-PATH: unrouted={unrouted} redirected={redirected} forwarded_with_query={forwarded_with_query} answered={answered} stopped={stopped}"
    );
    if unrouted && redirected && forwarded_with_query && answered && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_path_only_regex_routes_on_the_path_and_carries_the_query_over() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "router: a REGEX_PATH frontend matches the path alone and carries the query over",
            try_path_only_regex_routes_on_the_path_and_carries_the_query_over,
        ),
        State::Success,
    );
}
