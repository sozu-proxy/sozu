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

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, ListenerType, Request, ResponseStatus, request::RequestType,
    },
};

use crate::{
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or, tests::create_local_address},
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
