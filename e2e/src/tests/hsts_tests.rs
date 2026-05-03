//! End-to-end coverage for the typed HSTS (RFC 6797) policy.
//!
//! Wire-level positive coverage (HTTPS frontend emits the
//! `Strict-Transport-Security` header on backend-served responses,
//! redirect-default answers, and 5xx default answers) is intentionally
//! deferred — those tests need a full TLS stack (cert generation,
//! `https_client::build_https_client`, `H2Backend`) and the per-test
//! setup is heavier than the §7.2 reject path covered here. The
//! unit tests in `lib/src/router/mod.rs::tests::render_hsts_*` and
//! `lib/src/protocol/mux/shared.rs::tests` exercise the rendering and
//! `apply_response_header_edits` SetIfAbsent paths exhaustively.
//!
//! What this module covers:
//! - **§7.2 reject at the worker IPC entry point**: `AddHttpFrontend`
//!   carrying `hsts.enabled = true` is rejected with `ResponseStatus::Failure`
//!   (the `ProxyError::HstsOnPlainHttp` arm). Defense in depth on top of the
//!   TOML config-load reject.
//! - **`hsts.enabled = false` on plain HTTP is accepted**: the explicit
//!   disable signal must NOT trip the §7.2 reject — it's the operator's
//!   way of suppressing an inherited listener default.

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, HstsConfig, ListenerType, Request, ResponseStatus, request::RequestType,
    },
};

use crate::{
    sozu::worker::Worker,
    tests::{
        State, repeat_until_error_or,
        tests::{create_local_address, create_unbound_local_address},
    },
};

/// Spawn a worker with a single plain-HTTP listener. Mirrors the helper
/// in `redirect_rewrite_auth_tests.rs` so the §7.2 reject path can be
/// exercised on the cheapest possible setup.
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

// ═════════════════════════════════════════════════════════════════════
// §7.2 reject — AddHttpFrontend with hsts.enabled = true is refused
// ═════════════════════════════════════════════════════════════════════

/// RFC 6797 §7.2 forbids `Strict-Transport-Security` on plaintext HTTP.
/// The worker's `add_http_frontend` chokepoint enforces this even when
/// the misconfiguration bypasses the TOML config-load reject (e.g. a
/// programmatic IPC sender or `sozu frontend http add` with HSTS
/// flags). The response status must be `Failure` and the worker must
/// stay healthy afterwards.
pub fn try_hsts_on_http_frontend_rejected() -> State {
    let front_address = create_local_address();
    let _unused_back = create_unbound_local_address();
    let mut worker = spawn_worker_with_http_listener("HSTS-7.2-REJECT", front_address);

    worker.send_proxy_request(
        RequestType::AddCluster(Worker::default_cluster("hsts_reject_cluster")).into(),
    );
    worker.read_to_last();

    let mut frontend = Worker::default_http_frontend("hsts_reject_cluster", front_address);
    frontend.hsts = Some(HstsConfig {
        enabled: Some(true),
        max_age: Some(31_536_000),
        include_subdomains: Some(true),
        preload: Some(false),
    });
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(frontend));
    let resp = worker
        .read_proxy_response()
        .expect("worker must respond to AddHttpFrontend");
    let rejected = resp.status == ResponseStatus::Failure as i32;
    let mentions_hsts = resp.message.to_ascii_uppercase().contains("HSTS")
        || resp.message.to_ascii_lowercase().contains("plaintext")
        || resp.message.to_ascii_lowercase().contains("plain http");

    worker.soft_stop();
    worker.wait_for_server_stop();

    if rejected && mentions_hsts {
        State::Success
    } else {
        eprintln!(
            "expected ResponseStatus::Failure with HSTS-related message, \
             got status={} message={:?}",
            resp.status, resp.message
        );
        State::Fail
    }
}

#[test]
fn test_hsts_on_http_frontend_rejected() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HSTS on HTTP frontend rejected (RFC 6797 §7.2)",
            try_hsts_on_http_frontend_rejected
        ),
        State::Success
    );
}

// ═════════════════════════════════════════════════════════════════════
// Explicit disable signal on HTTP — accepted, no §7.2 reject
// ═════════════════════════════════════════════════════════════════════

/// `hsts.enabled = Some(false)` is the explicit-disable signal: it must
/// NOT trip the §7.2 reject (the worker's predicate keys on
/// `enabled == Some(true)`). Operators use this shape on plain-HTTP
/// frontends to surface "we know HSTS is off here on purpose" in the
/// configuration without semantic ambiguity.
pub fn try_hsts_explicit_disable_on_http_accepted() -> State {
    let front_address = create_local_address();
    let _unused_back = create_unbound_local_address();
    let mut worker = spawn_worker_with_http_listener("HSTS-7.2-DISABLE", front_address);

    worker.send_proxy_request(
        RequestType::AddCluster(Worker::default_cluster("hsts_disable_cluster")).into(),
    );
    worker.read_to_last();

    let mut frontend = Worker::default_http_frontend("hsts_disable_cluster", front_address);
    frontend.hsts = Some(HstsConfig {
        enabled: Some(false),
        max_age: None,
        include_subdomains: None,
        preload: None,
    });
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(frontend));
    let resp = worker
        .read_proxy_response()
        .expect("worker must respond to AddHttpFrontend");
    let accepted = resp.status == ResponseStatus::Ok as i32;

    worker.soft_stop();
    worker.wait_for_server_stop();

    if accepted {
        State::Success
    } else {
        eprintln!(
            "expected ResponseStatus::Ok for hsts.enabled = false on HTTP \
             frontend, got status={} message={:?}",
            resp.status, resp.message
        );
        State::Fail
    }
}

#[test]
fn test_hsts_explicit_disable_on_http_accepted() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HSTS enabled = false on HTTP frontend accepted",
            try_hsts_explicit_disable_on_http_accepted
        ),
        State::Success
    );
}
