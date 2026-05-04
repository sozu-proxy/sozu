//! End-to-end coverage for the typed HSTS (RFC 6797) policy.
//!
//! Plain-HTTP §7.2 reject paths:
//! - **§7.2 reject at the worker IPC entry point**: `AddHttpFrontend`
//!   carrying `hsts.enabled = true` is rejected with `ResponseStatus::Failure`
//!   (the `ProxyError::HstsOnPlainHttp` arm). Defense in depth on top of the
//!   TOML config-load reject.
//! - **`hsts.enabled = false` on plain HTTP is also rejected**: there is no
//!   listener-default HSTS to inherit on an HTTP listener (the field doesn't
//!   exist on `HttpListenerConfig`), so the explicit-disable signal has
//!   nothing to suppress on this surface.
//!
//! Wire-level HTTPS positive paths (`Strict-Transport-Security` reaches
//! the client on every response code per RFC 6797 §8.1):
//! - **HSTS on a backend-served 200 response** through an HTTPS frontend.
//! - **HSTS on a 301 redirect default answer** rendered when the frontend
//!   carries `redirect = permanent` + `redirect_scheme = use-https`.
//! - **HSTS on a 401 unauthorized default answer** rendered when the
//!   frontend carries `redirect = unauthorized`.
//! - **HSTS on a 503 default answer** rendered when the configured backend
//!   is unreachable. Confirms the snapshot-copy hoist in `mux/router.rs`
//!   and the `set_default_answer_with_retry_after` chokepoint both
//!   thread the per-frontend HSTS edit through to the wire.
//!
//! All HTTPS tests use the `local-certificate.pem` shipped under
//! `lib/assets/`, the `build_h2_or_h1_client` Hyper client, and the
//! `resolve_request_with_headers` helper so the assertion sees both
//! the status line and the `Strict-Transport-Security` header.

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, HstsConfig, ListenerType,
        RedirectPolicy, RedirectScheme, Request, RequestHttpFrontend, ResponseStatus,
        SocketAddress, request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        https_client::{build_h2_or_h1_client, resolve_request_with_headers},
    },
    sozu::worker::Worker,
    tests::{
        State, provide_port, repeat_until_error_or,
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
        force_replace_backend: None,
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
// Explicit disable signal on HTTP — also rejected
// ═════════════════════════════════════════════════════════════════════

/// `hsts.enabled = Some(false)` (the explicit-disable signal) on a
/// plain-HTTP frontend is also rejected by `add_http_frontend`. There
/// is no listener-default HSTS to inherit on an HTTP listener (the
/// field doesn't exist on `HttpListenerConfig`), so the
/// explicit-disable shape has nothing to suppress on this surface —
/// carrying any `hsts` block here is treated as a misconfiguration.
pub fn try_hsts_explicit_disable_on_http_rejected() -> State {
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
        force_replace_backend: None,
    });
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(frontend));
    let resp = worker
        .read_proxy_response()
        .expect("worker must respond to AddHttpFrontend");
    let rejected = resp.status == ResponseStatus::Failure as i32;

    worker.soft_stop();
    worker.wait_for_server_stop();

    if rejected {
        State::Success
    } else {
        eprintln!(
            "expected ResponseStatus::Failure for hsts.enabled = false on HTTP \
             frontend, got status={} message={:?}",
            resp.status, resp.message
        );
        State::Fail
    }
}

#[test]
fn test_hsts_explicit_disable_on_http_rejected() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HSTS enabled = false on HTTP frontend rejected",
            try_hsts_explicit_disable_on_http_rejected
        ),
        State::Success
    );
}

// ═════════════════════════════════════════════════════════════════════
// Wire-level HTTPS positives — HSTS on every response code (RFC §8.1)
// ═════════════════════════════════════════════════════════════════════

/// Canonical HSTS rendered value used by every HTTPS positive test
/// below. Matches the default `max_age = 31_536_000` (1 year, the HSTS
/// preload list minimum) plus `includeSubDomains` set by the operator.
/// Asserting on the exact value keeps the test honest — a regression in
/// `render_hsts` that, e.g., reordered the directives or dropped
/// `includeSubDomains` would surface as a string mismatch instead of a
/// generic header-presence pass.
const EXPECTED_HSTS_VALUE: &str = "max-age=31536000; includeSubDomains";

/// Build a typed HSTS block: `enabled = true`, `max_age = 31_536_000`,
/// `include_subdomains = true`, `preload = false`. Used by the four
/// positive tests below so the assertion can compare against
/// [`EXPECTED_HSTS_VALUE`] verbatim.
fn make_hsts_default() -> HstsConfig {
    HstsConfig {
        enabled: Some(true),
        max_age: Some(31_536_000),
        include_subdomains: Some(true),
        preload: None,
        force_replace_backend: None,
    }
}

/// Spawn an HTTPS worker with a single TLS listener at `front_address`
/// and the local-only `localhost` certificate attached. Returns the
/// worker. The caller layers cluster + frontend + backends on top.
///
/// Mirrors the setup in `tls_tests.rs::try_tls_sni_routing` so the HSTS
/// e2e tests reuse the existing TLS scaffold (cert generation,
/// `provide_port`, `Worker::empty_https_config`).
fn spawn_https_worker(name: &str, front_address: SocketAddress) -> Worker {
    let (config, listeners, state) = Worker::empty_https_config(front_address.into());
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .expect("default HTTPS listener must build"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    worker
}

/// HTTPS frontend with `hsts.enabled = true` returns the
/// `Strict-Transport-Security` header on a backend-served 200 response.
/// The canonical positive case — RFC 6797 §8.1 says HSTS applies to
/// every response code, but the 2xx Forward path is the most common
/// production shape and the one operators primarily care about.
pub fn try_hsts_on_https_backend_200() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();
    let mut worker = spawn_https_worker("HSTS-HTTPS-200", front_address.clone());

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "hsts_https_200",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: "localhost".to_owned(),
        hsts: Some(make_hsts_default()),
        ..Worker::default_http_frontend("hsts_https_200", front_address.clone().into())
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "hsts_https_200",
        "hsts_https_200-0",
        back_address,
        None,
    )));

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_200",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong-hsts"),
    );
    worker.read_to_last();

    let client = build_h2_or_h1_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/")
        .parse()
        .expect("valid uri");
    let resp = resolve_request_with_headers(&client, uri);

    worker.soft_stop();
    let _ = worker.wait_for_server_stop();
    let _ = backend.stop_and_get_aggregator();

    match resp {
        Some((status, headers, body)) => {
            let sts = headers.get("strict-transport-security");
            let ok = status.is_success()
                && body.contains("pong-hsts")
                && sts.is_some_and(|v| v.as_bytes() == EXPECTED_HSTS_VALUE.as_bytes());
            if ok {
                State::Success
            } else {
                eprintln!(
                    "expected 2xx + body 'pong-hsts' + STS = {EXPECTED_HSTS_VALUE:?}, got \
                     status={status}, sts={sts:?}, body={body:?}"
                );
                State::Fail
            }
        }
        None => {
            eprintln!("HTTPS request failed entirely");
            State::Fail
        }
    }
}

#[test]
fn test_hsts_on_https_backend_200() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HSTS on HTTPS backend-served 200",
            try_hsts_on_https_backend_200
        ),
        State::Success
    );
}

/// Regression coverage for the "out buffer not empty (38 bytes
/// remaining), clearing" leak observed in production after
/// listener-default HSTS started populating `headers_response` on a
/// non-trivial share of frontends. Symptom: tens of
/// `H2BlockConverter finalize: out buffer not empty` ERROR logs per
/// HSTS-carrying response with a multi-frame body. Root cause:
/// `apply_response_header_edits` was called inconditionally before
/// EVERY `kawa.prepare(...)` pass for a stream — when `write_streams`
/// re-entered the same stream for a follow-on DATA chunk (multi-
/// frame body, flow-control yield, RFC 9218 same-urgency round-robin)
/// the helper re-injected the STS edit AFTER the `Block::Flags{end_header}`
/// anchor had already been consumed, leaving an orphan
/// `Block::Header` past the end-of-headers marker. The next prepare
/// cycle encoded that header into `H2BlockConverter.out` with no
/// closing flags block to flush it as a HEADERS frame, and
/// `finalize` tripped the defense-in-depth log on every re-entry.
/// Beyond the spam, the orphan encode also mutated the HPACK
/// encoder's dynamic table without the peer ever receiving the
/// matching wire frame — silent decoder desync.
///
/// The fix at `lib/src/protocol/mux/h2.rs` and
/// `lib/src/protocol/mux/h1.rs` drains
/// `parts.context.headers_response` via `mem::take` so the injection
/// runs exactly once. This test asserts on the wire-level outcome:
/// a multi-frame H2 body returns intact (status, full content) and
/// carries the STS header exactly once. A regression in the take-
/// once contract surfaces as either a corrupted response (HPACK
/// encoder desync → client decode error → empty body) or, after the
/// `finalize` clear, as a duplicate STS header on the wire (depending
/// on whether the H1 or H2 path drives the test).
pub fn try_hsts_multi_frame_body_no_leak_no_duplicate_sts() -> State {
    // 256 KiB body. Default H2 max_frame_size is 16 384 bytes, so
    // the body splits into ~16 DATA frames — well past the
    // multi-prepare-cycle threshold the bug needs to trip on a
    // single response. The body content is repeated so it survives
    // the round-trip even if a frame is dropped (test would still
    // fail on the size mismatch).
    const BODY_SIZE: usize = 256 * 1024;
    const BODY_PATTERN: &str = "pong-hsts-multi-frame-";
    let body = BODY_PATTERN.repeat(BODY_SIZE / BODY_PATTERN.len() + 1);
    let body = body[..BODY_SIZE].to_owned();

    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();
    let mut worker = spawn_https_worker("HSTS-HTTPS-200-MULTI-FRAME", front_address.clone());

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "hsts_https_multi_frame",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: "localhost".to_owned(),
        hsts: Some(make_hsts_default()),
        ..Worker::default_http_frontend("hsts_https_multi_frame", front_address.clone().into())
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "hsts_https_multi_frame",
        "hsts_https_multi_frame-0",
        back_address,
        None,
    )));

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_200_MULTI_FRAME",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler(body.clone()),
    );
    worker.read_to_last();

    let client = build_h2_or_h1_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/")
        .parse()
        .expect("valid uri");
    let resp = resolve_request_with_headers(&client, uri);

    worker.soft_stop();
    let _ = worker.wait_for_server_stop();
    let _ = backend.stop_and_get_aggregator();

    match resp {
        Some((status, headers, response_body)) => {
            // The multi-prepare-cycle bug was masked at the wire by
            // `H2BlockConverter::finalize` clearing the orphan bytes,
            // so the response BODY can still arrive intact even with
            // the bug present (the test catches the bug via the STS
            // header count below). But if HPACK state desync (a
            // distinct, harder-to-mask consequence of the orphan
            // encode) trips, the response decode fails entirely and
            // we land in `None` above OR see a truncated body here.
            // Both surfaces are covered.
            let body_ok = status.is_success()
                && response_body.len() == BODY_SIZE
                && response_body.starts_with(BODY_PATTERN);
            // Hyper's `HeaderMap::get_all` enumerates EVERY entry
            // for the header name, including duplicates. The fix's
            // contract is that STS reaches the wire exactly once
            // (RFC 6797 §6.1 — UAs MAY ignore the response if more
            // than one STS header is present). A regression in the
            // take-once gate surfaces here as `count >= 2`.
            let sts_count = headers.get_all("strict-transport-security").iter().count();
            let sts_value_ok = headers
                .get("strict-transport-security")
                .is_some_and(|v| v.as_bytes() == EXPECTED_HSTS_VALUE.as_bytes());
            if body_ok && sts_count == 1 && sts_value_ok {
                State::Success
            } else {
                eprintln!(
                    "expected status=2xx, body_len={BODY_SIZE}, STS count=1, STS={EXPECTED_HSTS_VALUE:?}; \
                     got status={status}, body_len={}, sts_count={sts_count}, \
                     sts_first={:?}",
                    response_body.len(),
                    headers.get("strict-transport-security"),
                );
                State::Fail
            }
        }
        None => {
            eprintln!(
                "HTTPS multi-frame request failed entirely — possibly HPACK \
                 desync from an orphan `Block::Header` encoded into the \
                 converter's `out` buffer past the end-of-headers anchor."
            );
            State::Fail
        }
    }
}

#[test]
fn test_hsts_multi_frame_body_no_leak_no_duplicate_sts() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HSTS multi-frame body — no converter leak, no duplicate STS",
            try_hsts_multi_frame_body_no_leak_no_duplicate_sts
        ),
        State::Success
    );
}

/// HTTPS frontend with `hsts.enabled = true` AND
/// `redirect = permanent` + `redirect_scheme = use-https` returns the
/// `Strict-Transport-Security` header on the proxy-generated 301
/// default answer. Exercises the snapshot-copy hoist in
/// `lib/src/protocol/mux/router.rs` — without the hoist, the
/// `set_default_answer_with_retry_after` path bypasses
/// `apply_response_header_edits` and the 301 ships without HSTS
/// (RFC 6797 §8.1 violation).
pub fn try_hsts_on_https_redirect_301() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let unused_back = create_unbound_local_address();
    let mut worker = spawn_https_worker("HSTS-HTTPS-301", front_address.clone());

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "hsts_https_301",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: "localhost".to_owned(),
        redirect: Some(RedirectPolicy::Permanent as i32),
        redirect_scheme: Some(RedirectScheme::UseHttps as i32),
        hsts: Some(make_hsts_default()),
        ..Worker::default_http_frontend("hsts_https_301", front_address.clone().into())
    }));
    // Backend never contacted but required so cluster has a backend
    // pool — sozu's redirect short-circuits before backend connect.
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "hsts_https_301",
        "hsts_https_301-0",
        unused_back,
        None,
    )));
    worker.read_to_last();

    let client = build_h2_or_h1_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/")
        .parse()
        .expect("valid uri");
    let resp = resolve_request_with_headers(&client, uri);

    worker.soft_stop();
    let _ = worker.wait_for_server_stop();

    match resp {
        Some((status, headers, _body)) => {
            let sts = headers.get("strict-transport-security");
            let ok = status.as_u16() == 301
                && sts.is_some_and(|v| v.as_bytes() == EXPECTED_HSTS_VALUE.as_bytes());
            if ok {
                State::Success
            } else {
                eprintln!(
                    "expected 301 + STS = {EXPECTED_HSTS_VALUE:?}, got status={status}, sts={sts:?}"
                );
                State::Fail
            }
        }
        None => {
            eprintln!("HTTPS redirect request failed entirely");
            State::Fail
        }
    }
}

#[test]
fn test_hsts_on_https_redirect_301() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HSTS on HTTPS 301 default answer",
            try_hsts_on_https_redirect_301
        ),
        State::Success
    );
}

/// HTTPS frontend with `hsts.enabled = true` AND
/// `redirect = unauthorized` returns the `Strict-Transport-Security`
/// header on the proxy-generated 401 default answer. Same hoist path
/// as the 301 test above; covers the auth-deny early return at
/// `mux/router.rs:762` separately because the 301 short-circuit lives
/// at `:714` and the snapshot must precede both.
pub fn try_hsts_on_https_unauthorized_401() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let unused_back = create_unbound_local_address();
    let mut worker = spawn_https_worker("HSTS-HTTPS-401", front_address.clone());

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "hsts_https_401",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: "localhost".to_owned(),
        redirect: Some(RedirectPolicy::Unauthorized as i32),
        hsts: Some(make_hsts_default()),
        ..Worker::default_http_frontend("hsts_https_401", front_address.clone().into())
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "hsts_https_401",
        "hsts_https_401-0",
        unused_back,
        None,
    )));
    worker.read_to_last();

    let client = build_h2_or_h1_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/")
        .parse()
        .expect("valid uri");
    let resp = resolve_request_with_headers(&client, uri);

    worker.soft_stop();
    let _ = worker.wait_for_server_stop();

    match resp {
        Some((status, headers, _body)) => {
            let sts = headers.get("strict-transport-security");
            let ok = status.as_u16() == 401
                && sts.is_some_and(|v| v.as_bytes() == EXPECTED_HSTS_VALUE.as_bytes());
            if ok {
                State::Success
            } else {
                eprintln!(
                    "expected 401 + STS = {EXPECTED_HSTS_VALUE:?}, got status={status}, sts={sts:?}"
                );
                State::Fail
            }
        }
        None => {
            eprintln!("HTTPS unauthorized request failed entirely");
            State::Fail
        }
    }
}

#[test]
fn test_hsts_on_https_unauthorized_401() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HSTS on HTTPS 401 default answer",
            try_hsts_on_https_unauthorized_401
        ),
        State::Success
    );
}

/// HTTPS frontend with `hsts.enabled = true` and a backend at an
/// unbound address returns the `Strict-Transport-Security` header on
/// the proxy-generated 503 default answer. Exercises the
/// `set_default_answer_with_retry_after` chokepoint at `mux/answers.rs`
/// — the snapshot-copy hoist runs on the Forward path before the
/// backend-connect attempt fails, so the per-stream
/// `headers_response` is already populated when the 503 default
/// answer is rendered.
pub fn try_hsts_on_https_unreachable_503() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let unbound_back = create_unbound_local_address();
    let mut worker = spawn_https_worker("HSTS-HTTPS-503", front_address.clone());

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "hsts_https_503",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: "localhost".to_owned(),
        hsts: Some(make_hsts_default()),
        ..Worker::default_http_frontend("hsts_https_503", front_address.clone().into())
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "hsts_https_503",
        "hsts_https_503-0",
        unbound_back,
        None,
    )));
    worker.read_to_last();

    let client = build_h2_or_h1_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/")
        .parse()
        .expect("valid uri");
    let resp = resolve_request_with_headers(&client, uri);

    worker.soft_stop();
    let _ = worker.wait_for_server_stop();

    match resp {
        Some((status, headers, _body)) => {
            let sts = headers.get("strict-transport-security");
            // The exact 5xx code depends on whether the backend connect
            // refuses (503) or times out (504). Either is a valid
            // proxy-generated 5xx default answer; HSTS must accompany
            // both.
            let is_5xx = status.is_server_error();
            let ok = is_5xx && sts.is_some_and(|v| v.as_bytes() == EXPECTED_HSTS_VALUE.as_bytes());
            if ok {
                State::Success
            } else {
                eprintln!(
                    "expected 5xx + STS = {EXPECTED_HSTS_VALUE:?}, got status={status}, sts={sts:?}"
                );
                State::Fail
            }
        }
        None => {
            eprintln!("HTTPS unreachable-backend request failed entirely");
            State::Fail
        }
    }
}

#[test]
fn test_hsts_on_https_unreachable_503() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HSTS on HTTPS 5xx default answer",
            try_hsts_on_https_unreachable_503
        ),
        State::Success
    );
}
