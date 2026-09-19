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

use std::{net::SocketAddr, time::Duration};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, ListenerType, PathRule, RequestHttpFrontend, ResponseStatus,
        request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend, client::Client,
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
