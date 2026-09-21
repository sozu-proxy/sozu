//! HTTP/2 SNI ↔ `:authority` binding adversarial e2e tests.
//!
//! These tests exercise the cross-tenant TLS trust boundary introduced by
//! commit `5c37f542` ("SNI captured at handshake, enforced per stream") and
//! the companion opt-out knob `strict_sni_binding` (commit `ac66dfc0`).
//!
//! An attacker holding a valid certificate for tenant A should not be able to
//! reach tenant B's backend simply by changing `:authority` on an H2 stream
//! opened with SNI=A. When the router detects the mismatch it increments
//! `http.sni_authority_mismatch` (see `lib/src/protocol/mux/router.rs:338`)
//! and returns `421 Misdirected Request` per RFC 9110 §15.5.20.
//!
//! Recipes covered (see `tasks/e2e-recipe.md` §2 group 8B):
//! - happy path: SNI=foo, `:authority`=foo → 200 OK
//! - mismatch: SNI=foo, `:authority`=bar → 421 + metric bumped + foreign
//!   backend untouched
//! - wildcard not expanded: SNI=foo, `:authority`=`*.example.com` → 421
//! - plaintext listener: no SNI, cross-`Host` accepted (no check to run)
//! - opt-out: listener with `strict_sni_binding=false` forwards the mismatch

use std::{io::Write, net::SocketAddr, thread, time::Duration};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, ListenerType, QueryMetricsOptions,
        RequestHttpFrontend, ResponseStatus, SocketAddress, filtered_metrics, request::RequestType,
        response_content::ContentType,
    },
};

use super::h2_utils::{
    H2_FRAME_HEADERS, H2Frame, collect_response_frames, decode_status, h2_handshake,
    headers_status_matches, log_frames, raw_h2_connection_with_sni, teardown,
};
use crate::{
    http_utils::{http_ok_response, http_request},
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend, client::Client,
        sync_backend::Backend as SyncBackend,
    },
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or, tests::create_local_address},
};

// ============================================================================
// Helpers
// ============================================================================

/// Build a minimal well-formed HEADERS block for a GET request using
/// literal-without-indexing for the `:authority` value so we can set any
/// hostname without perturbing the HPACK static table.
fn build_request_headers(authority: &[u8]) -> Vec<u8> {
    let mut block = vec![
        0x82, // :method GET (static index 2)
        0x84, // :path / (static index 4)
        0x87, // :scheme https (static index 7)
    ];
    // :authority — literal-without-indexing on top of static table index 1
    // (RFC 7541 §6.2.2: pattern `0000` in the high nibble + 4-bit name
    // index, so `0x01` for index 1 = `:authority`). Encoding stays
    // side-effect-free: the value never enters the HPACK dynamic table,
    // so cross-test interference via shared coder state is impossible.
    // The value-length byte that follows uses a 7-bit prefix (no Huffman
    // bit set), good for any value up to 127 bytes.
    block.push(0x01);
    block.push(authority.len() as u8);
    block.extend_from_slice(authority);
    block
}

/// Whether any response on the connection carried a 2xx status, decoded
/// from its `:status` field (see [`decode_status`]).
fn headers_ok_response(frames: &[(u8, u8, u32, Vec<u8>)]) -> bool {
    frames.iter().any(|(ft, _fl, _sid, payload)| {
        *ft == H2_FRAME_HEADERS && decode_status(payload).is_some_and(|s| (200..300).contains(&s))
    })
}

/// Read a single counter metric from the worker. Returns 0 if the counter
/// has never been emitted (the metric registry only materialises entries on
/// first `incr!`).
fn read_counter(worker: &mut Worker, name: &str) -> i64 {
    worker.send_proxy_request_type(RequestType::QueryMetrics(QueryMetricsOptions {
        list: false,
        cluster_ids: vec![],
        backend_ids: vec![],
        metric_names: vec![name.to_owned()],
        no_clusters: true,
        workers: false,
    }));
    let response = worker
        .read_proxy_response()
        .expect("worker should respond to metrics query");
    assert_eq!(response.status, ResponseStatus::Ok as i32, "{response:?}");

    let Some(content) = response.content.and_then(|c| c.content_type) else {
        return 0;
    };
    let ContentType::WorkerMetrics(metrics) = content else {
        return 0;
    };
    match metrics.proxy.get(name).and_then(|m| m.inner.as_ref()) {
        Some(filtered_metrics::Inner::Count(v)) => *v,
        _ => 0,
    }
}

/// Build an HTTPS listener with the multi-SAN cert, two hostname frontends
/// (foo, bar), two async backends and optional `strict_sni_binding`
/// override. Returns worker, backends keyed by name, and the front port.
fn setup_sni_two_tenant_listener(
    name: &str,
    strict: Option<bool>,
) -> (
    Worker,
    AsyncBackend<SimpleAggregator>,
    AsyncBackend<SimpleAggregator>,
    u16,
) {
    let front_port = super::provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    let mut https_listener = ListenerBuilder::new_https(front_address.clone())
        .to_tls(None)
        .expect("build https listener");
    // The listener stores `strict_sni_binding` as `Option<bool>`; leaving it
    // `None` means "default = true" (see `lib/src/https.rs:772`).
    https_listener.strict_sni_binding = strict;
    worker.send_proxy_request_type(RequestType::AddHttpsListener(https_listener));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "foo-cluster",
    )));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "bar-cluster",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("foo.example.com"),
        ..Worker::default_http_frontend("foo-cluster", front_address.clone().into())
    }));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("bar.example.com"),
        ..Worker::default_http_frontend("bar-cluster", front_address.clone().into())
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/multi-sni-cert.pem")),
        key: String::from(include_str!("../../../lib/assets/multi-sni-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let foo_addr = create_local_address();
    let bar_addr = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "foo-cluster",
        "foo-0".to_owned(),
        foo_addr,
        None,
    )));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "bar-cluster",
        "bar-0".to_owned(),
        bar_addr,
        None,
    )));
    let foo_backend = AsyncBackend::spawn_detached_backend(
        "FOO",
        foo_addr,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong-foo".to_owned()),
    );
    let bar_backend = AsyncBackend::spawn_detached_backend(
        "BAR",
        bar_addr,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong-bar".to_owned()),
    );

    worker.read_to_last();
    (worker, foo_backend, bar_backend, front_port)
}

// ============================================================================
// Test 1: SNI matches :authority — happy path
// ============================================================================

/// Baseline: when the H2 client's TLS SNI matches the request's `:authority`,
/// the router forwards the request to the matching cluster's backend and
/// returns a 2xx response.
fn try_h2_sni_strict_match_happy_path() -> State {
    let (mut worker, mut foo, mut bar, front_port) =
        setup_sni_two_tenant_listener("H2-SNI-HAPPY", None);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection_with_sni(front_addr, "foo.example.com");
    h2_handshake(&mut tls);

    let headers = build_request_headers(b"foo.example.com");
    let frame = H2Frame::headers(1, headers, true, true);
    tls.write_all(&frame.encode()).expect("write HEADERS");
    tls.flush().expect("flush HEADERS");

    let frames = collect_response_frames(&mut tls, 500, 4, 500);
    log_frames("H2 SNI happy path", &frames);

    let got_ok = headers_ok_response(&frames);

    drop(tls);
    thread::sleep(Duration::from_millis(200));
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let foo_agg = foo.stop_and_get_aggregator().unwrap_or_default();
    let _ = bar.stop_and_get_aggregator();

    if stopped && got_ok && foo_agg.requests_received == 1 {
        State::Success
    } else {
        println!(
            "happy path FAIL: stopped={stopped} got_ok={got_ok} foo_reqs={}",
            foo_agg.requests_received
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_sni_strict_match_happy_path() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 SNI: strict-match happy path (SNI=foo, :authority=foo \u{2192} 200)",
            try_h2_sni_strict_match_happy_path
        ),
        State::Success
    );
}

// ============================================================================
// Test 2: SNI ↔ :authority — off-SAN authority blocked with 421 and metric bump
// ============================================================================

/// CWE-346 / CWE-444: SNI=foo but `:authority=evil.org`. `evil.org` is not in
/// the served cert's SAN set (`foo|bar|baz.example.com`, `localhost`), so the
/// router must reject the stream with 421 Misdirected Request (RFC 9110
/// §15.5.20), bump `http.sni_authority_mismatch`, and MUST NOT reach any
/// backend. This guards the post-coalescing semantics: legitimate
/// browser-driven coalescing across SANs is accepted (test 6), but an
/// authority outside the cert's coverage is still rejected.
fn try_h2_sni_authority_mismatch_blocked() -> State {
    let (mut worker, mut foo, mut bar, front_port) =
        setup_sni_two_tenant_listener("H2-SNI-MISMATCH", None);

    let before = read_counter(&mut worker, "http.sni_authority_mismatch");

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection_with_sni(front_addr, "foo.example.com");
    h2_handshake(&mut tls);

    let headers = build_request_headers(b"evil.org");
    let frame = H2Frame::headers(1, headers, true, true);
    tls.write_all(&frame.encode()).expect("write HEADERS");
    tls.flush().expect("flush HEADERS");

    let frames = collect_response_frames(&mut tls, 500, 4, 500);
    log_frames("H2 SNI off-SAN", &frames);

    let got_421 = headers_status_matches(&frames, b"421");

    // Flush the metrics pipe — the counter is bumped synchronously inside
    // `route_from_request` but `read_counter` needs a round-trip to observe
    // it; repeat once to avoid a race with the query.
    let after = read_counter(&mut worker, "http.sni_authority_mismatch");
    let delta = after - before;

    drop(tls);
    thread::sleep(Duration::from_millis(200));
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let foo_agg = foo.stop_and_get_aggregator().unwrap_or_default();
    let bar_agg = bar.stop_and_get_aggregator().unwrap_or_default();

    if stopped
        && got_421
        && delta >= 1
        && foo_agg.requests_received == 0
        && bar_agg.requests_received == 0
    {
        State::Success
    } else {
        println!(
            "off-SAN FAIL: stopped={stopped} got_421={got_421} delta={delta} \
             foo_reqs={} bar_reqs={}",
            foo_agg.requests_received, bar_agg.requests_received
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_sni_authority_mismatch_blocked() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 SNI: off-SAN authority blocked (SNI=foo, :authority=evil.org \u{2192} 421 + metric)",
            try_h2_sni_authority_mismatch_blocked
        ),
        State::Success
    );
}

// ============================================================================
// Test 3: Wildcard :authority is NOT expanded
// ============================================================================

/// RFC 9110 §7.2: `:authority` is a concrete host, never a pattern. Ensure
/// the SNI check does not accidentally treat `*.example.com` as "matches any
/// foo.example.com SNI" — wildcard-looking authority values must be
/// rejected exactly like any other non-matching hostname.
fn try_h2_sni_wildcard_authority_not_expanded() -> State {
    let (mut worker, mut foo, mut bar, front_port) =
        setup_sni_two_tenant_listener("H2-SNI-WILDCARD", None);

    let before = read_counter(&mut worker, "http.sni_authority_mismatch");

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection_with_sni(front_addr, "foo.example.com");
    h2_handshake(&mut tls);

    let headers = build_request_headers(b"*.example.com");
    let frame = H2Frame::headers(1, headers, true, true);
    tls.write_all(&frame.encode()).expect("write HEADERS");
    tls.flush().expect("flush HEADERS");

    let frames = collect_response_frames(&mut tls, 500, 4, 500);
    log_frames("H2 SNI wildcard authority", &frames);

    // Either a 421 is synthesised (mismatch branch) or the frontend lookup
    // fails and sozu returns 404 — both prove the wildcard is NOT expanded
    // into a trust-bypass for foo.example.com.
    let got_421 = headers_status_matches(&frames, b"421");
    let got_404 = headers_status_matches(&frames, b"404");
    let rejected = got_421 || got_404;

    let after = read_counter(&mut worker, "http.sni_authority_mismatch");
    let mismatch_bumped = (after - before) >= 1;

    drop(tls);
    thread::sleep(Duration::from_millis(200));
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let foo_agg = foo.stop_and_get_aggregator().unwrap_or_default();
    let bar_agg = bar.stop_and_get_aggregator().unwrap_or_default();

    let no_backend_touched = foo_agg.requests_received == 0 && bar_agg.requests_received == 0;
    if stopped && rejected && no_backend_touched {
        // When 421 is the rejection, the mismatch counter must be bumped;
        // when 404 is returned, the mismatch branch did not run (the
        // wildcard simply didn't match any frontend) — both outcomes are
        // acceptable as long as the wildcard was not expanded.
        if got_421 && !mismatch_bumped {
            println!("wildcard FAIL: 421 without metric bump");
            return State::Fail;
        }
        State::Success
    } else {
        println!(
            "wildcard FAIL: stopped={stopped} got_421={got_421} got_404={got_404} \
             foo_reqs={} bar_reqs={}",
            foo_agg.requests_received, bar_agg.requests_received
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_sni_wildcard_authority_not_expanded() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 SNI: wildcard :authority not expanded into a trust bypass",
            try_h2_sni_wildcard_authority_not_expanded
        ),
        State::Success
    );
}

// ============================================================================
// Test 4: Plaintext HTTP/1.1 listener — no SNI to check
// ============================================================================

/// Plaintext listeners never see an SNI, so the strict-binding check is a
/// no-op there. A classic cross-`Host` request must be routed normally, with
/// the mismatch metric untouched. This documents the scope of the fix (TLS
/// only) and guards against an over-eager enforcement that would break
/// plaintext virtual hosting.
fn try_h2_sni_plaintext_no_check() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_http_config(front_address);
    let mut worker = Worker::start_new_worker_owned("H2-SNI-PLAINTEXT", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpListener(
        ListenerBuilder::new_http(front_address.into())
            .to_http(None)
            .expect("build http listener"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "bar-cluster",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(RequestHttpFrontend {
        hostname: String::from("bar.example.com"),
        ..Worker::default_http_frontend("bar-cluster", front_address)
    }));

    let bar_addr = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "bar-cluster",
        "bar-0".to_owned(),
        bar_addr,
        None,
    )));
    let mut bar_backend = SyncBackend::new(
        "BAR".to_owned(),
        bar_addr,
        http_ok_response("pong-bar".to_owned()),
    );
    bar_backend.connect();

    worker.read_to_last();

    let before = read_counter(&mut worker, "http.sni_authority_mismatch");

    // Plaintext HTTP/1.1 client, `Host: bar.example.com` — no SNI in play,
    // the listener must forward to bar-cluster.
    let mut client = Client::new(
        "h2-sni-plaintext",
        front_address,
        http_request("GET", "/", "", "bar.example.com"),
    );
    client.connect();
    client.send();
    bar_backend.accept(0);
    let _ = bar_backend.receive(0);
    bar_backend.send(0);
    let response = client.receive().unwrap_or_default();

    let after = read_counter(&mut worker, "http.sni_authority_mismatch");
    let metric_stable = after == before;

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let got_200 = response.contains("200 OK") && response.contains("pong-bar");

    if stopped && got_200 && metric_stable {
        State::Success
    } else {
        println!(
            "plaintext FAIL: stopped={stopped} got_200={got_200} metric_before={before} \
             metric_after={after} response={response:?}"
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_sni_plaintext_no_check() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 SNI: plaintext listener bypasses the check (no SNI to enforce)",
            try_h2_sni_plaintext_no_check
        ),
        State::Success
    );
}

// ============================================================================
// Test 5: strict_sni_binding=false — mismatch is allowed
// ============================================================================

/// Operator opt-out: when the listener is configured with
/// `strict_sni_binding=false` (commit `ac66dfc0`), a cross-tenant mismatch
/// must be forwarded normally, with the mismatch counter untouched.
fn try_h2_sni_authority_mismatch_allowed_when_strict_binding_disabled() -> State {
    let (mut worker, mut foo, mut bar, front_port) =
        setup_sni_two_tenant_listener("H2-SNI-STRICT-OFF", Some(false));

    let before = read_counter(&mut worker, "http.sni_authority_mismatch");

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection_with_sni(front_addr, "foo.example.com");
    h2_handshake(&mut tls);

    let headers = build_request_headers(b"bar.example.com");
    let frame = H2Frame::headers(1, headers, true, true);
    tls.write_all(&frame.encode()).expect("write HEADERS");
    tls.flush().expect("flush HEADERS");

    let frames = collect_response_frames(&mut tls, 500, 4, 500);
    log_frames("H2 SNI strict-off", &frames);

    let got_ok = headers_ok_response(&frames);
    let got_421 = headers_status_matches(&frames, b"421");

    let after = read_counter(&mut worker, "http.sni_authority_mismatch");
    let metric_stable = after == before;

    // `teardown(...)` takes the empty backend vec deliberately: this test
    // needs the per-backend `requests_received` aggregators below for its
    // strict-off acceptance check, and `stop_and_get_aggregator` consumes
    // the handle. We stop the backends manually after the worker
    // soft-stops so each aggregator's counter is final by the time we
    // read it.
    let infra_ok = teardown(tls, front_port, worker, vec![]);
    let foo_agg = foo.stop_and_get_aggregator().unwrap_or_default();
    let bar_agg = bar.stop_and_get_aggregator().unwrap_or_default();

    if infra_ok
        && got_ok
        && !got_421
        && metric_stable
        && foo_agg.requests_received == 0
        && bar_agg.requests_received == 1
    {
        State::Success
    } else {
        println!(
            "strict-off FAIL: infra_ok={infra_ok} got_ok={got_ok} got_421={got_421} \
             metric_stable={metric_stable} foo_reqs={} bar_reqs={}",
            foo_agg.requests_received, bar_agg.requests_received
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_sni_authority_mismatch_allowed_when_strict_binding_disabled() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 SNI: opt-out via strict_sni_binding=false forwards the mismatch",
            try_h2_sni_authority_mismatch_allowed_when_strict_binding_disabled
        ),
        State::Success
    );
}

// ============================================================================
// Test 6: H2 connection coalescing across cert SANs (RFC 7540 §9.1.1)
// ============================================================================

/// Browsers (Firefox / Chrome) reuse a single H2 connection for any origin
/// covered by the SAN set of the certificate served at handshake (RFC 7540
/// §9.1.1 / RFC 9113 §9.1.1, with RFC 6125 §6.4.3 wildcard handling). Sōzu
/// must accept this: SNI=foo.example.com but `:authority=bar.example.com`
/// where both names are SANs of the served cert → 200 from the `bar`
/// backend, and the `h2.coalescing.accepted` counter must bump.
///
/// This is the regression test for the Clever Cloud console 421 storm
/// reported on 2026-05-18 (Slack threads C4RT92HLK / p1779119756906459 and
/// C6V040003 / p1776691535478149): legitimate browser coalescing on
/// `*.cleverapps.io` was being rejected as 421.
fn try_h2_coalescing_multi_san_accepted() -> State {
    let (mut worker, mut foo, mut bar, front_port) =
        setup_sni_two_tenant_listener("H2-COALESCING-ACCEPTED", None);

    let coalesce_before = read_counter(&mut worker, "h2.coalescing.accepted");
    let mismatch_before = read_counter(&mut worker, "http.sni_authority_mismatch");

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection_with_sni(front_addr, "foo.example.com");
    h2_handshake(&mut tls);

    // Same TLS session, but request the `bar` tenant — this is what a
    // browser does when it coalesces a second origin onto a wildcard /
    // multi-SAN cert.
    let headers = build_request_headers(b"bar.example.com");
    let frame = H2Frame::headers(1, headers, true, true);
    tls.write_all(&frame.encode()).expect("write HEADERS");
    tls.flush().expect("flush HEADERS");

    let frames = collect_response_frames(&mut tls, 500, 4, 500);
    log_frames("H2 SAN coalescing accepted", &frames);

    let got_ok = headers_ok_response(&frames);

    let coalesce_after = read_counter(&mut worker, "h2.coalescing.accepted");
    let mismatch_after = read_counter(&mut worker, "http.sni_authority_mismatch");
    let coalesce_delta = coalesce_after - coalesce_before;
    let mismatch_delta = mismatch_after - mismatch_before;

    drop(tls);
    thread::sleep(Duration::from_millis(200));
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let foo_agg = foo.stop_and_get_aggregator().unwrap_or_default();
    let bar_agg = bar.stop_and_get_aggregator().unwrap_or_default();

    // `bar` backend MUST receive the request (coalescing accepted); `foo`
    // backend MUST NOT (the request was routed by `:authority`, not SNI).
    // `h2.coalescing.accepted` MUST bump; `http.sni_authority_mismatch`
    // MUST NOT bump (it is reserved for real misses).
    if stopped
        && got_ok
        && coalesce_delta >= 1
        && mismatch_delta == 0
        && bar_agg.requests_received == 1
        && foo_agg.requests_received == 0
    {
        State::Success
    } else {
        println!(
            "coalescing FAIL: stopped={stopped} got_ok={got_ok} \
             coalesce_delta={coalesce_delta} mismatch_delta={mismatch_delta} \
             foo_reqs={} bar_reqs={}",
            foo_agg.requests_received, bar_agg.requests_received
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_coalescing_multi_san_accepted() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 coalescing: SNI=foo, :authority=bar covered by cert SANs \u{2192} 200 + h2.coalescing.accepted",
            try_h2_coalescing_multi_san_accepted
        ),
        State::Success
    );
}

// ============================================================================
// Test 7: CN is NOT an authoritative coalescing identity (RFC 6125 §6.4.4)
// ============================================================================

/// Defence-in-depth: a certificate whose Common Name does NOT appear in the
/// SubjectAlternativeName dNSName list must not let an attacker coalesce H2
/// streams onto the CN. The cert fixture `cn-ne-san-cert.pem` is shaped
/// CN=tenant-b.example with SAN=tenant-a.example; browsers (Firefox, Chrome)
/// have ignored CN for hostname verification since 2017 and Sōzu must
/// agree, otherwise an operator who configures only the SAN list could be
/// surprised by traffic reaching a backend keyed off the CN. The check
/// matters because [`command::certificate::get_cn_and_san_attributes`]
/// (`command/src/certificate.rs`) returns the SAN dNSName list only when a
/// dNSName entry is present; the routing layer then can never see the CN.
///
/// The test handshakes with SNI=`tenant-a.example` (the legitimate SAN),
/// then attempts an H2 stream with `:authority=tenant-b.example` (the CN
/// the attacker hopes will be honoured). Expected result: 421 Misdirected
/// Request and `http.sni_authority_mismatch` bumped.
fn try_h2_cn_not_coalesced_with_san() -> State {
    let front_port = super::provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker =
        Worker::start_new_worker_owned("H2-CN-NOT-COALESCED", config, listeners, state);

    let mut https_listener = ListenerBuilder::new_https(front_address.clone())
        .to_tls(None)
        .expect("build https listener");
    https_listener.strict_sni_binding = None;
    worker.send_proxy_request_type(RequestType::AddHttpsListener(https_listener));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "tenant-a-cluster",
    )));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "tenant-b-cluster",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("tenant-a.example"),
        ..Worker::default_http_frontend("tenant-a-cluster", front_address.clone().into())
    }));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("tenant-b.example"),
        ..Worker::default_http_frontend("tenant-b-cluster", front_address.clone().into())
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/cn-ne-san-cert.pem")),
        key: String::from(include_str!("../../../lib/assets/cn-ne-san-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let tenant_a_addr = create_local_address();
    let tenant_b_addr = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "tenant-a-cluster",
        "tenant-a-0".to_owned(),
        tenant_a_addr,
        None,
    )));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "tenant-b-cluster",
        "tenant-b-0".to_owned(),
        tenant_b_addr,
        None,
    )));
    let mut tenant_a = AsyncBackend::spawn_detached_backend(
        "TENANT-A",
        tenant_a_addr,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong-a".to_owned()),
    );
    let mut tenant_b = AsyncBackend::spawn_detached_backend(
        "TENANT-B",
        tenant_b_addr,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong-b".to_owned()),
    );

    worker.read_to_last();

    let before = read_counter(&mut worker, "http.sni_authority_mismatch");

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection_with_sni(front_addr, "tenant-a.example");
    h2_handshake(&mut tls);

    let headers = build_request_headers(b"tenant-b.example");
    let frame = H2Frame::headers(1, headers, true, true);
    tls.write_all(&frame.encode()).expect("write HEADERS");
    tls.flush().expect("flush HEADERS");

    let frames = collect_response_frames(&mut tls, 500, 4, 500);
    log_frames("H2 CN attack via :authority", &frames);

    let got_421 = headers_status_matches(&frames, b"421");
    let after = read_counter(&mut worker, "http.sni_authority_mismatch");
    let delta = after - before;

    drop(tls);
    thread::sleep(Duration::from_millis(200));
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let tenant_a_agg = tenant_a.stop_and_get_aggregator().unwrap_or_default();
    let tenant_b_agg = tenant_b.stop_and_get_aggregator().unwrap_or_default();

    if stopped
        && got_421
        && delta >= 1
        && tenant_a_agg.requests_received == 0
        && tenant_b_agg.requests_received == 0
    {
        State::Success
    } else {
        println!(
            "CN-attack FAIL: stopped={stopped} got_421={got_421} delta={delta} \
             tenant_a_reqs={} tenant_b_reqs={}",
            tenant_a_agg.requests_received, tenant_b_agg.requests_received
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_cn_not_coalesced_with_san() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 RFC 6125 \u{a7}6.4.4: cert CN is NOT a coalescing identity when SAN dNSName is present",
            try_h2_cn_not_coalesced_with_san
        ),
        State::Success
    );
}

// ============================================================================
// Regression: the status checks decode `:status`, they do not scan bytes
// ============================================================================

/// Rebuild the `:status 200` response HEADERS block Sōzu emits for a proxied
/// request, parameterised by the `Sozu-Id` value.
///
/// The byte sequence is a verbatim capture taken on 2026-09-20 from
/// `try_h2_sni_authority_mismatch_allowed_when_strict_binding_disabled`:
/// `88` (indexed static 8 = `:status 200`), `0f 0d 01 '8'`
/// (`content-length: 8`), then `40 07 "sozu-id" 1a <26-byte ULID>`. Kawa's
/// HPACK encoder never sets the Huffman bit, so every literal value — the
/// ULID included — appears as plain ASCII in the block.
fn captured_200_headers(sozu_id: &str) -> Vec<u8> {
    let mut payload = vec![0x88, 0x0f, 0x0d, 0x01, b'8', 0x40, 0x07];
    payload.extend_from_slice(b"sozu-id");
    payload.push(sozu_id.len() as u8);
    payload.extend_from_slice(sozu_id.as_bytes());
    payload
}

/// Rebuild the `:status 421` response HEADERS block, likewise captured on
/// 2026-09-20 from `try_h2_sni_authority_mismatch_blocked`: `0e 03 "421"`
/// (literal without indexing, static name index 14 = `:status`, 3-byte
/// unhuffmanned value), `0f 09 08 "no-cache"`, then the same `sozu-id`
/// literal.
fn captured_421_headers(sozu_id: &str) -> Vec<u8> {
    let mut payload = vec![0x0e, 0x03, b'4', b'2', b'1', 0x0f, 0x09, 0x08];
    payload.extend_from_slice(b"no-cache");
    payload.push(0x40);
    payload.push(0x07);
    payload.extend_from_slice(b"sozu-id");
    payload.push(sozu_id.len() as u8);
    payload.extend_from_slice(sozu_id.as_bytes());
    payload
}

/// Closes #1353. `got_421` must come from the stream's decoded `:status`,
/// never from the digits `4`, `2`, `1` happening to sit next to each other
/// somewhere in the field block.
///
/// Sōzu stamps every response with the `Sozu-Id` correlation header
/// (`HttpContext::on_response_headers`, `lib/src/protocol/kawa_h1/editor.rs`)
/// whose value is the session's
/// 26-character Crockford base-32 ULID — an alphabet that contains `4`, `2`
/// and `1`. A ULID therefore carries the substring `421` roughly once in
/// 1400, and its leading ten characters are a monotonic millisecond
/// timestamp, so when the hit lands in that prefix every response in a
/// window carries it: 1 ms, 32 ms, ~1 s, ~33 s or ~17.5 min depending on
/// which character triple it occupies, and ~9.3 h, ~12.4 d or ~1.1 y for the
/// three slowest. All 24 windows are a priori equally likely; the slow ones
/// are not rarer per draw, they are fixed for a whole era, so a hit there is
/// a property of the epoch rather than of a run. That is
/// the observed CI signature of #1353: `got_ok=true got_421=true
/// metric_stable=true bar_reqs=1` on both iterations, then green on a re-run
/// of the same commit. No 421 was ever emitted — the only production site
/// that can emit one on this path is `lib/src/protocol/mux/mod.rs:1306`,
/// reachable solely through `RetrieveClusterError::SniAuthorityMismatch`,
/// constructed at the single site `lib/src/protocol/mux/router.rs:672`
/// immediately after `incr!(names::http::SNI_AUTHORITY_MISMATCH)`, and that
/// counter did not move.
///
/// To SEE THIS RED, either:
/// - restore the historical body of `headers_status_matches`,
///   `payload.windows(code.len()).any(|w| w == code)` — the `proxied_200`
///   assertion fails; or
/// - restore the historical body of `headers_ok_response`,
///   `payload.contains(&0x88) || payload.contains(&0x89) ||
///   payload.contains(&0x8a)` — the `stray_indexed_200` assertion fails.
///
/// Both mutations have been run and seen red. Note which fixture reddens
/// which: `misdirected_421` alone does **not** falsify the `0x88` scan,
/// because a captured block holds nothing but ASCII and therefore no `0x88`
/// byte. That is why `stray_indexed_200` exists and why it is the one
/// deliberately synthetic fixture here.
#[test]
fn h2_status_checks_decode_the_status_field_not_any_matching_bytes() {
    // A real 200 whose ULID carries `421` in its random half.
    let proxied_200 = vec![(
        H2_FRAME_HEADERS,
        0x04,
        1u32,
        captured_200_headers("01M2Z5AGKT421M9MFQY89EJMJ9"),
    )];
    assert!(
        headers_ok_response(&proxied_200),
        "a `:status 200` block must read as a 2xx response"
    );
    assert!(
        !headers_status_matches(&proxied_200, b"421"),
        "a `Sozu-Id` carrying the digits 421 is not a 421 response"
    );

    // A real 421 must still be detected — the tightened check has to keep
    // failing the strict-binding tests it exists to guard.
    let misdirected_421 = vec![(
        H2_FRAME_HEADERS,
        0x04,
        1u32,
        captured_421_headers("01M2Z5FD3TWATYZGVA29XQ085D"),
    )];
    assert!(
        headers_status_matches(&misdirected_421, b"421"),
        "a `:status 421` block must read as a 421 response"
    );

    // Constructed rather than captured, and the property it pins is
    // wire-reachable. `0x88` in a field block does not imply `:status 200`:
    // RFC 7541 §5.1 encodes a value length ≥ 127 as a 7-bit prefix plus
    // continuation octets, each `(len % 128) + 128`, so a 263-byte header
    // value — an ordinary `Location`, `Set-Cookie` or CSP header — writes
    // `7f 88 01` with no status anywhere near it (1542 distinct lengths
    // below 100 000 produce a `0x88` octet). A raw value byte ≥ `0x80` gets
    // there too: `lib/src/protocol/mux/converter.rs:388-391` rejects only
    // `0x00..=0x08 | 0x0A..=0x1F | 0x7F` and passes the rest verbatim. Since
    // `headers_ok_response` runs on *proxied* responses in the strict-off
    // and coalescing tests, where backend headers are arbitrary, the
    // historical `contains(&0x88)` scan was falsifiable on the wire. A
    // trailing indexed `0x88` is the cheapest block carrying that byte
    // without a 200, and it pins what `decode_status` promises: the first
    // field decides the status, and a later field — even one that looks
    // exactly like a status — cannot change the answer.
    let mut stray = captured_421_headers("01M2Z5FD3TWATYZGVA29XQ085D");
    stray.push(0x88);
    let stray_indexed_200 = vec![(H2_FRAME_HEADERS, 0x04, 1u32, stray)];
    assert!(
        !headers_ok_response(&stray_indexed_200),
        "only the first field decides the status: a trailing indexed `:status 200` \
         does not make a 421 block a 2xx response"
    );
    assert!(
        headers_status_matches(&stray_indexed_200, b"421"),
        "a trailing field must not displace the real `:status 421`"
    );
}
