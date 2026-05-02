//! End-to-end coverage for the redirect / URL rewrite / per-frontend
//! header / Basic-auth feature stack ported from PR #1206..#1210 to the
//! post-#1209 mux. Each test asserts on what the **backend** or the
//! **client** sees on the wire so a regression in the routing decision,
//! the request-side mutation pass, or the response-side emission pass
//! surfaces directly.
//!
//! Cardinality matrix coverage:
//!   * H1 ↔ H1 — every behaviour, deep wire assertions via [`SyncBackend`]
//!     which exposes the raw HTTP/1.1 request bytes.
//!   * H2 ↔ H1 — frontend-only behaviours (redirect, basic auth, custom
//!     answer templates) and a sample of backend-visible mutations
//!     (rewrite, header inject) — H1 backend still captures via
//!     [`SyncBackend`].
//!   * H1 ↔ H2 / H2 ↔ H2 — backend-side assertions read from
//!     [`H2Backend::recorded_requests`] which captures the rewritten
//!     authority, path, method, and forwarded headers per request.
//!     Smoke coverage plus rewrite-path / response-header tests on this
//!     diagonal so a bug in the H2 emission path (`mux/h2.rs::write_streams`
//!     applying response edits before `kawa.prepare`) surfaces here.

use std::time::Duration;

use sozu_command_lib::proto::command::{
    ActivateListener, Cluster, Header, HeaderPosition, ListenerType, RedirectPolicy,
    RedirectScheme, Request, RequestHttpFrontend, request::RequestType,
};

use crate::{
    http_utils::http_request,
    mock::{client::Client, h2_backend::H2Backend, sync_backend::Backend as SyncBackend},
    sozu::worker::Worker,
    tests::{
        State, repeat_until_error_or,
        tests::{create_local_address, create_unbound_local_address},
    },
};

/// Wallclock budget for the H1 [`SyncBackend`] to register a single
/// connection from sozu after the test sends its request. Calibrated
/// for the listener's 100 ms internal poll plus several retries; large
/// enough to absorb a slow CI runner without bloating happy-path
/// runtime (the loop exits on the first successful accept).
const SYNC_BACKEND_ACCEPT_BUDGET_MS: u64 = 2000;

/// Larger accept budget for tests that drive two sequential client
/// connections through the same backend (e.g. basic-auth tests that
/// retry after the 401). The backend is single-threaded — the second
/// accept needs the budget of two cycles.
const SYNC_BACKEND_TWO_SHOT_ACCEPT_BUDGET_MS: u64 = 4000;

/// Sleep between [`SyncBackend::accept`] retries. Small enough that the
/// 2 s budget gets ~400 attempts; large enough that a busy spin does
/// not burn a CPU core. Does not extend the deadline (see
/// `accept_with_retry`).
const SYNC_BACKEND_RETRY_SLEEP_MS: u64 = 5;

/// Wallclock budget for the [`H2Backend`] mock to record at least one
/// request after the test resolves its frontend response. The hyper
/// service runs on a dedicated tokio runtime; under contended CI we
/// poll for up to this window before reading the recorded snapshot.
const H2_BACKEND_RECORD_BUDGET_MS: u64 = 2500;

/// Poll interval used by [`wait_for_h2_requests`] while waiting for
/// the [`H2Backend`] to register a request. Same trade-off as the
/// SyncBackend retry sleep — small enough to react quickly under load,
/// large enough to avoid a busy spin.
const H2_BACKEND_RECORD_POLL_MS: u64 = 25;

/// Helper to spawn a fresh sozu worker with a single HTTP listener at
/// `front_address` and return the worker. The cluster + frontend +
/// backends are added by each test on top of this baseline so the
/// per-test policy fields can be set declaratively without needing a
/// new constructor parameter for every behaviour.
///
/// The listener fd is pre-reserved via the port registry and passed to
/// the worker through the SCM socket — this matches `setup_test`'s
/// pattern and is required to avoid the "Address already in use" race
/// where sozu's bind would lose to the test's own port reservation.
fn spawn_worker_with_http_listener(name: &str, front_address: std::net::SocketAddr) -> Worker {
    use sozu_command_lib::config::ListenerBuilder;
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

/// Add a backend to `cluster_id` at `back_address` with `backend_id`.
fn add_backend(
    worker: &mut Worker,
    cluster_id: &str,
    backend_id: &str,
    back_address: std::net::SocketAddr,
) {
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            cluster_id,
            backend_id,
            back_address,
            None,
        ))
        .into(),
    );
}

/// Retry [`SyncBackend::accept`] for up to `total_ms` (the underlying
/// listener has a 100 ms read timeout, so a single call can return
/// before sozu has opened its outbound connection on a slow CI runner).
///
/// Sleeps [`SYNC_BACKEND_RETRY_SLEEP_MS`] between attempts so a busy
/// spin doesn't burn a CPU core. Deadline check sits AFTER the accept
/// attempt so the boundary attempt always runs — a `while now < deadline`
/// check at the top would skip the last `accept()` call when the loop
/// sleeps straight onto the deadline.
fn accept_with_retry(backend: &mut SyncBackend, client_id: usize, total_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(total_ms);
    loop {
        if backend.accept(client_id) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(SYNC_BACKEND_RETRY_SLEEP_MS));
    }
}

// ═════════════════════════════════════════════════════════════════════
// Redirect — RedirectPolicy::Permanent emits 301 without contacting backend
// ═════════════════════════════════════════════════════════════════════

/// `RedirectPolicy::Permanent` plus `RedirectScheme::UseHttps` makes the
/// frontend emit a 301 with the resolved HTTPS Location and never opens
/// a TCP connection to any backend. H1 frontend cardinality.
pub fn try_redirect_permanent_h1_skips_backend() -> State {
    let front_address = create_local_address();
    let unused_back = create_unbound_local_address();
    let mut worker = spawn_worker_with_http_listener("REDIR-H1", front_address);

    worker.send_proxy_request(
        RequestType::AddCluster(Cluster {
            https_redirect_port: Some(8443),
            ..Worker::default_cluster("redir_cluster")
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            redirect: Some(RedirectPolicy::Permanent as i32),
            redirect_scheme: Some(RedirectScheme::UseHttps as i32),
            ..Worker::default_http_frontend("redir_cluster", front_address)
        })
        .into(),
    );
    add_backend(&mut worker, "redir_cluster", "redir_back_0", unused_back);
    worker.read_to_last();

    let mut client = Client::new(
        "redir_client",
        front_address,
        http_request("GET", "/path", "ping", "localhost"),
    );
    client.connect();
    client.send();
    let response = client
        .receive()
        .expect("frontend must emit a 301 default-answer");
    worker.soft_stop();
    worker.wait_for_server_stop();

    let ok = response.contains("301") && response.to_ascii_lowercase().contains("location: https");
    if ok {
        State::Success
    } else {
        eprintln!("expected 301 with https Location, got:\n{response}");
        State::Fail
    }
}

#[test]
fn test_redirect_permanent_h1_skips_backend() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "permanent redirect",
            try_redirect_permanent_h1_skips_backend
        ),
        State::Success
    );
}

/// Reusable client labels for the H1 redirect helper. Avoids the
/// `format!(...).leak()` pattern that grew per-call `&'static str`
/// allocations every time the helper retried via
/// `repeat_until_error_or` (M5 from the PR #1238 review).
const REDIR_302_CLIENT: &str = "redir_302_client";
const REDIR_308_CLIENT: &str = "redir_308_client";

/// Helper: drive a single H1 redirect request through `policy` and assert
/// the response carries the expected status line, a `Location: https://...`
/// header pointing at the test's frontend authority, and that the entire
/// response was read from the TCP stream (loop-read until eof per
/// CLAUDE.md). Used by the 302 / 308 tests below (closes #1009).
fn try_redirect_emits_status(
    log_tag: &str,
    client_label: &'static str,
    policy: RedirectPolicy,
    expected_status: u16,
) -> State {
    let front_address = create_local_address();
    let unused_back = create_unbound_local_address();
    let cluster_id = format!("redir_{log_tag}_cluster");
    let back_id = format!("redir_{log_tag}_back_0");
    let mut worker = spawn_worker_with_http_listener(log_tag, front_address);

    worker.send_proxy_request(
        RequestType::AddCluster(Cluster {
            https_redirect_port: Some(8443),
            ..Worker::default_cluster(&cluster_id)
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            redirect: Some(policy as i32),
            redirect_scheme: Some(RedirectScheme::UseHttps as i32),
            ..Worker::default_http_frontend(&cluster_id, front_address)
        })
        .into(),
    );
    add_backend(&mut worker, &cluster_id, &back_id, unused_back);
    worker.read_to_last();

    let mut client = Client::new(
        client_label,
        front_address,
        http_request("GET", "/path", "ping", "localhost"),
    );
    client.connect();
    client.send();
    let response = client
        .receive_until_eof(Duration::from_millis(SYNC_BACKEND_ACCEPT_BUDGET_MS))
        .expect("frontend must emit a redirect default-answer");
    worker.soft_stop();
    worker.wait_for_server_stop();

    // Status-line prefix check (M3) — `response.contains("302")`
    // would false-positive on a body or `Sozu-Id` header carrying the
    // digits.
    let expected_status_line = format!("HTTP/1.1 {expected_status} ");
    let status_ok = response.starts_with(&expected_status_line);
    let lowered = response.to_ascii_lowercase();
    // Location host must match the request's `Host:` value
    // (`http_request(..., "localhost")` above) — the redirect engine
    // preserves the request authority. Asserts against an empty host
    // (`https://:443/...`) regression.
    let location_ok = lowered.contains("location: https://localhost");
    if status_ok && location_ok {
        State::Success
    } else {
        eprintln!(
            "expected status line {expected_status_line:?} + Location: https://localhost, got:\n{response}"
        );
        State::Fail
    }
}

/// `RedirectPolicy::Found` emits 302 with the resolved HTTPS `Location`.
/// Closes #1009 (302 leg).
pub fn try_redirect_found_h1_emits_302() -> State {
    try_redirect_emits_status("REDIR-H1-302", REDIR_302_CLIENT, RedirectPolicy::Found, 302)
}

#[test]
fn test_redirect_found_h1_emits_302() {
    assert_eq!(
        repeat_until_error_or(2, "302 redirect", try_redirect_found_h1_emits_302),
        State::Success
    );
}

/// `RedirectPolicy::PermanentRedirect` emits 308 with the resolved HTTPS
/// `Location`. Closes #1009 (308 leg). 308 differs from 301 in that the
/// HTTP method is preserved on follow (no GET-rewrite on POST).
pub fn try_redirect_permanent_redirect_h1_emits_308() -> State {
    try_redirect_emits_status(
        "REDIR-H1-308",
        REDIR_308_CLIENT,
        RedirectPolicy::PermanentRedirect,
        308,
    )
}

#[test]
fn test_redirect_permanent_redirect_h1_emits_308() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "308 redirect",
            try_redirect_permanent_redirect_h1_emits_308
        ),
        State::Success
    );
}

/// `RedirectPolicy::Unauthorized` returns a 401 even when the cluster
/// would otherwise route normally. H1 frontend.
pub fn try_redirect_unauthorized_h1_emits_401() -> State {
    let front_address = create_local_address();
    let unused_back = create_unbound_local_address();
    let mut worker = spawn_worker_with_http_listener("UNAUTH-H1", front_address);

    worker.send_proxy_request(
        RequestType::AddCluster(Cluster {
            www_authenticate: Some("Basic realm=\"unauth\"".to_owned()),
            ..Worker::default_cluster("unauth_cluster")
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            redirect: Some(RedirectPolicy::Unauthorized as i32),
            ..Worker::default_http_frontend("unauth_cluster", front_address)
        })
        .into(),
    );
    add_backend(&mut worker, "unauth_cluster", "unauth_back_0", unused_back);
    worker.read_to_last();

    let mut client = Client::new(
        "unauth_client",
        front_address,
        http_request("GET", "/", "ping", "localhost"),
    );
    client.connect();
    client.send();
    let response = client.receive().expect("frontend must emit a 401");
    worker.soft_stop();
    worker.wait_for_server_stop();

    if response.contains("401") {
        State::Success
    } else {
        eprintln!("expected 401, got:\n{response}");
        State::Fail
    }
}

#[test]
fn test_redirect_unauthorized_h1_emits_401() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "unauthorized redirect",
            try_redirect_unauthorized_h1_emits_401
        ),
        State::Success
    );
}

// ═════════════════════════════════════════════════════════════════════
// Basic auth — required_auth + authorized_hashes wire path
// ═════════════════════════════════════════════════════════════════════

/// SHA-256 of the literal byte string `"secret"`, lowercase hex,
/// matching `printf 'secret' | sha256sum`. Pinned here so we can
/// generate `authorized_hashes` entries without re-deriving on every
/// run.
const SECRET_SHA256_HEX: &str = "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b";

fn make_basic_auth_cluster() -> Cluster {
    Cluster {
        authorized_hashes: vec![format!("admin:{SECRET_SHA256_HEX}")],
        www_authenticate: Some("Basic realm=\"sozu\"".to_owned()),
        ..Worker::default_cluster("auth_cluster")
    }
}

/// Three branches: missing header → 401 with WWW-Authenticate, wrong
/// credential → 401, correct credential → 200 from the backend.
pub fn try_basic_auth_h1_h1() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_http_listener("AUTH-H1H1", front_address);
    worker.send_proxy_request(RequestType::AddCluster(make_basic_auth_cluster()).into());
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            required_auth: Some(true),
            ..Worker::default_http_frontend("auth_cluster", front_address)
        })
        .into(),
    );
    add_backend(&mut worker, "auth_cluster", "auth_back_0", back_address);
    let mut backend = SyncBackend::new(
        "auth_back_0",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();
    worker.read_to_last();

    // 1. No header → 401 + WWW-Authenticate
    {
        let mut client = Client::new(
            "auth_no_header",
            front_address,
            http_request("GET", "/secured", "", "localhost"),
        );
        client.connect();
        client.send();
        let resp = client
            .receive()
            .expect("frontend must emit 401 for missing header");
        if !resp.contains("401")
            || !resp
                .to_ascii_lowercase()
                .contains("www-authenticate: basic realm=")
        {
            eprintln!(
                "missing-header arm failed; expected 401 with WWW-Authenticate, got:\n{resp}"
            );
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    // 2. Wrong creds → 401
    {
        let token =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"admin:wrong");
        let req = format!(
            "GET /secured HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {token}\r\nContent-Length: 0\r\n\r\n"
        );
        let mut client = Client::new("auth_wrong", front_address, req);
        client.connect();
        client.send();
        let resp = client
            .receive()
            .expect("frontend must emit 401 for wrong creds");
        if !resp.contains("401") {
            eprintln!("wrong-creds arm failed; expected 401, got:\n{resp}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    // 3. Correct creds → backend forwards
    {
        let token =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"admin:secret");
        let req = format!(
            "GET /secured HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {token}\r\nContent-Length: 0\r\n\r\n"
        );
        let mut client = Client::new("auth_ok", front_address, req);
        client.connect();
        client.send();
        accept_with_retry(&mut backend, 0, SYNC_BACKEND_ACCEPT_BUDGET_MS);
        let _ = backend.receive(0);
        backend.send(0);
        let resp = client.receive().expect("client must receive 200");
        if !resp.contains("200") {
            eprintln!("correct-creds arm failed; expected 200, got:\n{resp}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_basic_auth_h1_h1() {
    assert_eq!(
        repeat_until_error_or(2, "basic auth H1↔H1", try_basic_auth_h1_h1),
        State::Success
    );
}

// ═════════════════════════════════════════════════════════════════════
// URL rewrite — backend sees rewritten Host + path + X-Forwarded-Host
// ═════════════════════════════════════════════════════════════════════

/// Backend must observe the rewritten `Host:` and `X-Forwarded-Host:`
/// the routing layer injects. H1 frontend, H1 backend.
pub fn try_rewrite_host_h1_h1() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_http_listener("REWRITE-H1H1", front_address);
    worker
        .send_proxy_request(RequestType::AddCluster(Worker::default_cluster("rwh_cluster")).into());
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            rewrite_host: Some("backend.local".to_owned()),
            ..Worker::default_http_frontend("rwh_cluster", front_address)
        })
        .into(),
    );
    add_backend(&mut worker, "rwh_cluster", "rwh_back_0", back_address);
    let mut backend = SyncBackend::new(
        "rwh_back",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();
    worker.read_to_last();

    // Client must match the frontend's `hostname` ("localhost" via
    // `default_http_frontend`) so the routing decision finds the
    // frontend; `rewrite_host` then mutates the on-wire authority to
    // "backend.local" before the backend forward.
    let mut client = Client::new(
        "rwh_client",
        front_address,
        http_request("GET", "/", "ping", "localhost"),
    );
    client.connect();
    client.send();
    accept_with_retry(&mut backend, 0, SYNC_BACKEND_ACCEPT_BUDGET_MS);
    let received = backend.receive(0).expect("backend must receive request");
    backend.send(0);
    let _ = client.receive();
    worker.soft_stop();
    worker.wait_for_server_stop();

    let received_lc = received.to_ascii_lowercase();
    let host_ok = received_lc.contains("host: backend.local");
    // The original `:authority` (post-`hostname_and_port` strip of the
    // listener port) is captured into X-Forwarded-Host. We sent
    // `Host: localhost` — sozu's H1 path may also append the listener
    // port, so we accept either bare `localhost` or `localhost:<port>`
    // in the X-Forwarded-Host check.
    let xfh_ok = received_lc.contains("x-forwarded-host: localhost");
    if host_ok && xfh_ok {
        State::Success
    } else {
        eprintln!(
            "expected rewritten Host + X-Forwarded-Host, got:\n{received}\n(host={host_ok}, xfh={xfh_ok})"
        );
        State::Fail
    }
}

#[test]
fn test_rewrite_host_h1_h1() {
    assert_eq!(
        repeat_until_error_or(2, "rewrite host H1↔H1", try_rewrite_host_h1_h1),
        State::Success
    );
}

/// `rewrite_path` mutates the request-line URI on the wire to the
/// backend. H1 frontend, H1 backend.
pub fn try_rewrite_path_h1_h1() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_http_listener("REWRITE-PATH-H1H1", front_address);
    worker
        .send_proxy_request(RequestType::AddCluster(Worker::default_cluster("rwp_cluster")).into());
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            rewrite_path: Some("/v2/api".to_owned()),
            ..Worker::default_http_frontend("rwp_cluster", front_address)
        })
        .into(),
    );
    add_backend(&mut worker, "rwp_cluster", "rwp_back_0", back_address);
    let mut backend = SyncBackend::new(
        "rwp_back",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();
    worker.read_to_last();

    let mut client = Client::new(
        "rwp_client",
        front_address,
        http_request("GET", "/api/old", "ping", "localhost"),
    );
    client.connect();
    client.send();
    accept_with_retry(&mut backend, 0, SYNC_BACKEND_ACCEPT_BUDGET_MS);
    let received = backend.receive(0).expect("backend must receive request");
    backend.send(0);
    let _ = client.receive();
    worker.soft_stop();
    worker.wait_for_server_stop();

    let path_ok = received.starts_with("GET /v2/api ") || received.contains("GET /v2/api ");
    if path_ok {
        State::Success
    } else {
        eprintln!("expected rewritten path /v2/api, got:\n{received}");
        State::Fail
    }
}

#[test]
fn test_rewrite_path_h1_h1() {
    assert_eq!(
        repeat_until_error_or(2, "rewrite path H1↔H1", try_rewrite_path_h1_h1),
        State::Success
    );
}

// ═════════════════════════════════════════════════════════════════════
// Per-frontend header injection — request-side
// ═════════════════════════════════════════════════════════════════════

/// Backend must observe the operator-configured request header.
/// H1 frontend, H1 backend.
pub fn try_request_header_inject_h1_h1() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_http_listener("REQ-INJ-H1H1", front_address);
    worker
        .send_proxy_request(RequestType::AddCluster(Worker::default_cluster("ri_cluster")).into());
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            headers: vec![Header {
                position: HeaderPosition::Request as i32,
                key: "X-Sozu-Probe".to_owned(),
                val: "alpha".to_owned(),
            }],
            ..Worker::default_http_frontend("ri_cluster", front_address)
        })
        .into(),
    );
    add_backend(&mut worker, "ri_cluster", "ri_back_0", back_address);
    let mut backend = SyncBackend::new(
        "ri_back",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();
    worker.read_to_last();

    let mut client = Client::new(
        "ri_client",
        front_address,
        http_request("GET", "/", "ping", "localhost"),
    );
    client.connect();
    client.send();
    accept_with_retry(&mut backend, 0, SYNC_BACKEND_ACCEPT_BUDGET_MS);
    let received = backend.receive(0).expect("backend must receive request");
    backend.send(0);
    let _ = client.receive();
    worker.soft_stop();
    worker.wait_for_server_stop();

    if received
        .to_ascii_lowercase()
        .contains("x-sozu-probe: alpha")
    {
        State::Success
    } else {
        eprintln!("expected X-Sozu-Probe: alpha, got:\n{received}");
        State::Fail
    }
}

#[test]
fn test_request_header_inject_h1_h1() {
    assert_eq!(
        repeat_until_error_or(2, "request inject H1↔H1", try_request_header_inject_h1_h1),
        State::Success
    );
}

/// Empty `val` deletes the named header from the request that reaches
/// the backend (HAProxy `del-header` parity).
pub fn try_request_header_delete_h1_h1() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_http_listener("REQ-DEL-H1H1", front_address);
    worker
        .send_proxy_request(RequestType::AddCluster(Worker::default_cluster("rd_cluster")).into());
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            headers: vec![Header {
                position: HeaderPosition::Request as i32,
                key: "X-Drop-Me".to_owned(),
                val: String::new(),
            }],
            ..Worker::default_http_frontend("rd_cluster", front_address)
        })
        .into(),
    );
    add_backend(&mut worker, "rd_cluster", "rd_back_0", back_address);
    let mut backend = SyncBackend::new(
        "rd_back",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();
    worker.read_to_last();

    let req = "GET / HTTP/1.1\r\nHost: localhost\r\nX-Drop-Me: should-not-reach-backend\r\nContent-Length: 4\r\n\r\nping";
    let mut client = Client::new("rd_client", front_address, req.to_owned());
    client.connect();
    client.send();
    accept_with_retry(&mut backend, 0, SYNC_BACKEND_ACCEPT_BUDGET_MS);
    let received = backend.receive(0).expect("backend must receive request");
    backend.send(0);
    let _ = client.receive();
    worker.soft_stop();
    worker.wait_for_server_stop();

    if !received.to_ascii_lowercase().contains("x-drop-me") {
        State::Success
    } else {
        eprintln!("expected X-Drop-Me to be stripped, got:\n{received}");
        State::Fail
    }
}

#[test]
fn test_request_header_delete_h1_h1() {
    assert_eq!(
        repeat_until_error_or(2, "request delete H1↔H1", try_request_header_delete_h1_h1),
        State::Success
    );
}

// ═════════════════════════════════════════════════════════════════════
// Per-frontend header injection — response-side
// ═════════════════════════════════════════════════════════════════════

/// Client must observe the operator-configured response header. The
/// emission boundary in `mux/h1.rs::writable` applies the edits before
/// `kawa.prepare(...)`.
pub fn try_response_header_inject_h1_h1() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_http_listener("RESP-INJ-H1H1", front_address);
    worker.send_proxy_request(
        RequestType::AddCluster(Worker::default_cluster("respi_cluster")).into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            headers: vec![Header {
                position: HeaderPosition::Response as i32,
                key: "X-Sozu-Stamp".to_owned(),
                val: "served-by-sozu".to_owned(),
            }],
            ..Worker::default_http_frontend("respi_cluster", front_address)
        })
        .into(),
    );
    add_backend(&mut worker, "respi_cluster", "respi_back_0", back_address);
    let mut backend = SyncBackend::new(
        "respi_back",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();
    worker.read_to_last();

    let mut client = Client::new(
        "respi_client",
        front_address,
        http_request("GET", "/", "ping", "localhost"),
    );
    client.connect();
    client.send();
    accept_with_retry(&mut backend, 0, SYNC_BACKEND_ACCEPT_BUDGET_MS);
    let _ = backend.receive(0);
    backend.send(0);
    let resp = client.receive().expect("client must receive response");
    worker.soft_stop();
    worker.wait_for_server_stop();

    if resp
        .to_ascii_lowercase()
        .contains("x-sozu-stamp: served-by-sozu")
    {
        State::Success
    } else {
        eprintln!("expected X-Sozu-Stamp on response, got:\n{resp}");
        State::Fail
    }
}

#[test]
fn test_response_header_inject_h1_h1() {
    assert_eq!(
        repeat_until_error_or(2, "response inject H1↔H1", try_response_header_inject_h1_h1),
        State::Success
    );
}

/// Empty `val` strips a backend-supplied response header before it
/// reaches the client.
pub fn try_response_header_delete_h1_h1() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_http_listener("RESP-DEL-H1H1", front_address);
    worker.send_proxy_request(
        RequestType::AddCluster(Worker::default_cluster("respd_cluster")).into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            headers: vec![Header {
                position: HeaderPosition::Response as i32,
                key: "X-Cache-Backend".to_owned(),
                val: String::new(),
            }],
            ..Worker::default_http_frontend("respd_cluster", front_address)
        })
        .into(),
    );
    add_backend(&mut worker, "respd_cluster", "respd_back_0", back_address);
    let backend_response =
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nX-Cache-Backend: hit\r\n\r\npong";
    let mut backend = SyncBackend::new("respd_back", back_address, backend_response);
    backend.connect();
    worker.read_to_last();

    let mut client = Client::new(
        "respd_client",
        front_address,
        http_request("GET", "/", "ping", "localhost"),
    );
    client.connect();
    client.send();
    accept_with_retry(&mut backend, 0, SYNC_BACKEND_ACCEPT_BUDGET_MS);
    let _ = backend.receive(0);
    backend.send(0);
    let resp = client.receive().expect("client must receive response");
    worker.soft_stop();
    worker.wait_for_server_stop();

    if !resp.to_ascii_lowercase().contains("x-cache-backend") {
        State::Success
    } else {
        eprintln!("expected X-Cache-Backend to be stripped, got:\n{resp}");
        State::Fail
    }
}

#[test]
fn test_response_header_delete_h1_h1() {
    assert_eq!(
        repeat_until_error_or(2, "response delete H1↔H1", try_response_header_delete_h1_h1),
        State::Success
    );
}

// ═════════════════════════════════════════════════════════════════════
// Custom answer template — per-cluster 503 override
// ═════════════════════════════════════════════════════════════════════

/// When the backend is unreachable, sozu emits the cluster's custom 503
/// template instead of the listener default. H1 frontend.
pub fn try_per_cluster_503_template() -> State {
    let front_address = create_local_address();
    let unused_back = create_unbound_local_address();
    let mut worker = spawn_worker_with_http_listener("CUSTOM-503-H1", front_address);

    let mut answers = std::collections::BTreeMap::new();
    // Templates must not declare a literal `Content-Length` — the
    // engine injects it from the rendered body size at fill time
    // (see `lib/src/protocol/kawa_h1/answers.rs::Template::new`).
    // Including a literal `Content-Length` makes kawa parse the
    // template-body as a real HTTP body and the engine rejects it
    // with `TemplateError::InvalidSizeInfo`.
    answers.insert(
        "503".to_owned(),
        "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\nsozu-503-tag".to_owned(),
    );
    worker.send_proxy_request(
        RequestType::AddCluster(Cluster {
            answers,
            ..Worker::default_cluster("c503_cluster")
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(Worker::default_http_frontend("c503_cluster", front_address))
            .into(),
    );
    // Backend reserved but never started → connect fails → 503
    add_backend(&mut worker, "c503_cluster", "c503_back_0", unused_back);
    worker.read_to_last();

    let mut client = Client::new(
        "c503_client",
        front_address,
        http_request("GET", "/", "ping", "localhost"),
    );
    client.connect();
    client.send();
    let resp = client.receive().expect("frontend must emit 503");
    worker.soft_stop();
    worker.wait_for_server_stop();

    if resp.contains("503") && resp.contains("sozu-503-tag") {
        State::Success
    } else {
        eprintln!("expected per-cluster 503 with sozu-503-tag, got:\n{resp}");
        State::Fail
    }
}

#[test]
fn test_per_cluster_503_template() {
    assert_eq!(
        repeat_until_error_or(2, "per-cluster 503 template", try_per_cluster_503_template),
        State::Success
    );
}

// ═════════════════════════════════════════════════════════════════════
// Cardinality matrix — H2 frontend variants
// ═════════════════════════════════════════════════════════════════════

/// Spawn a worker with a single HTTPS listener bound at `front_address`,
/// pre-reserved via the port registry. Uses the test certificate
/// fixtures from `lib/assets/local-{certificate,key}.pem` so the H2
/// client (advertising `h2` ALPN over TLS) can complete the handshake.
fn spawn_worker_with_https_listener(name: &str, front_address: std::net::SocketAddr) -> Worker {
    use sozu_command_lib::{
        config::ListenerBuilder,
        proto::command::{AddCertificate, CertificateAndKey, SocketAddress},
    };

    let (config, mut listeners, state) = Worker::empty_config();
    crate::port_registry::attach_reserved_https_listener(&mut listeners, front_address);
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpsListener(
            ListenerBuilder::new_https(front_address.into())
                .to_tls(None)
                .expect("default HTTPS listener must build"),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            address: front_address.into(),
            proxy: ListenerType::Https.into(),
            from_scm: true,
        })),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddCertificate(AddCertificate {
            address: SocketAddress::from(front_address),
            certificate: CertificateAndKey {
                certificate: include_str!("../../../lib/assets/local-certificate.pem").to_owned(),
                key: include_str!("../../../lib/assets/local-key.pem").to_owned(),
                certificate_chain: vec![],
                versions: vec![],
                names: vec![],
            },
            expired_at: None,
        })),
    });
    worker.read_to_last();
    worker
}

/// `RedirectPolicy::Permanent` on an H2 frontend — the H2 client
/// negotiates `h2` over TLS, sends a HEADERS frame, and observes a
/// :status 301 with a `location` response header without the backend
/// ever being contacted.
pub fn try_redirect_permanent_h2_skips_backend() -> State {
    use crate::mock::https_client::{build_h2_client, resolve_request};

    let front_address = create_local_address();
    let unused_back = create_unbound_local_address();
    let mut worker = spawn_worker_with_https_listener("REDIR-H2", front_address);

    worker.send_proxy_request(
        RequestType::AddCluster(Cluster {
            https_redirect_port: Some(8443),
            ..Worker::default_cluster("redir_h2_cluster")
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpsFrontend(RequestHttpFrontend {
            redirect: Some(RedirectPolicy::Permanent as i32),
            redirect_scheme: Some(RedirectScheme::UseHttps as i32),
            ..Worker::default_http_frontend("redir_h2_cluster", front_address)
        })
        .into(),
    );
    add_backend(
        &mut worker,
        "redir_h2_cluster",
        "redir_h2_back_0",
        unused_back,
    );
    worker.read_to_last();

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{}/", front_address.port())
        .parse()
        .expect("H2 redirect URI must parse");
    let outcome = resolve_request(&client, uri);
    worker.soft_stop();
    worker.wait_for_server_stop();

    match outcome {
        Some((status, _body)) if status.as_u16() == 301 => State::Success,
        Some((status, body)) => {
            eprintln!("expected H2 :status 301, got {status} body={body:?}");
            State::Fail
        }
        None => {
            eprintln!("H2 client could not resolve the redirect request");
            State::Fail
        }
    }
}

#[test]
fn test_redirect_permanent_h2_skips_backend() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "permanent redirect H2 frontend",
            try_redirect_permanent_h2_skips_backend,
        ),
        State::Success
    );
}

/// `required_auth = true` on an H2 frontend — missing Authorization
/// header → 401, correct credentials → 200 from H1 backend.
pub fn try_basic_auth_h2_h1() -> State {
    use crate::mock::https_client::{build_h2_client, resolve_request};
    use base64::{Engine, engine::general_purpose::STANDARD};

    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_https_listener("AUTH-H2H1", front_address);
    worker.send_proxy_request(RequestType::AddCluster(make_basic_auth_cluster()).into());
    worker.send_proxy_request(
        RequestType::AddHttpsFrontend(RequestHttpFrontend {
            required_auth: Some(true),
            ..Worker::default_http_frontend("auth_cluster", front_address)
        })
        .into(),
    );
    add_backend(&mut worker, "auth_cluster", "auth_back_0", back_address);
    let mut backend = SyncBackend::new(
        "auth_h2h1_back",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();
    worker.read_to_last();

    let uri: hyper::Uri = format!("https://localhost:{}/secured", front_address.port())
        .parse()
        .expect("H2 auth URI must parse");

    // 1. No Authorization header → 401
    let client_no_auth = build_h2_client();
    let unauth = match resolve_request(&client_no_auth, uri.clone()) {
        Some((status, _)) => status.as_u16(),
        None => {
            worker.soft_stop();
            worker.wait_for_server_stop();
            eprintln!("H2 client could not resolve no-header arm");
            return State::Fail;
        }
    };
    if unauth != 401 {
        worker.soft_stop();
        worker.wait_for_server_stop();
        eprintln!("expected H2 401 on missing auth, got {unauth}");
        return State::Fail;
    }

    // 2. Correct creds → backend forwards (status 200). Build the
    //    request manually so we can attach the Authorization header.
    let client_ok = build_h2_client();
    let token = STANDARD.encode(b"admin:secret");
    let req = hyper::Request::builder()
        .method("GET")
        .uri(uri.clone())
        .header("authorization", format!("Basic {token}"))
        .body(String::new())
        .expect("H2 request must build");
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let backend_handle = std::thread::spawn(move || {
        if accept_with_retry(&mut backend, 0, SYNC_BACKEND_TWO_SHOT_ACCEPT_BUDGET_MS) {
            let _ = backend.receive(0);
            backend.send(0);
        }
        backend
    });
    let resp_status = rt.block_on(async {
        match client_ok.request(req).await {
            Ok(resp) => Some(resp.status().as_u16()),
            Err(e) => {
                eprintln!("H2 authed request failed: {e}");
                None
            }
        }
    });
    let mut backend = backend_handle
        .join()
        .expect("backend thread should not panic");
    backend.disconnect();
    worker.soft_stop();
    worker.wait_for_server_stop();

    match resp_status {
        Some(200) => State::Success,
        Some(other) => {
            eprintln!("expected H2 200 on correct creds, got {other}");
            State::Fail
        }
        None => State::Fail,
    }
}

#[test]
fn test_basic_auth_h2_h1() {
    assert_eq!(
        repeat_until_error_or(2, "basic auth H2↔H1", try_basic_auth_h2_h1),
        State::Success
    );
}

/// Per-frontend response-side header injection on an H2 frontend.
/// The H2 client must observe the operator-configured response
/// header — the emission boundary in `mux/h2.rs::write_streams`
/// applies the edits before `kawa.prepare(...)`.
pub fn try_response_header_inject_h2_h1() -> State {
    use crate::mock::https_client::{build_h2_client, resolve_request};

    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_https_listener("RESP-INJ-H2H1", front_address);
    worker.send_proxy_request(
        RequestType::AddCluster(Worker::default_cluster("respi_h2_cluster")).into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpsFrontend(RequestHttpFrontend {
            headers: vec![Header {
                position: HeaderPosition::Response as i32,
                key: "X-Sozu-Stamp".to_owned(),
                val: "served-by-sozu".to_owned(),
            }],
            ..Worker::default_http_frontend("respi_h2_cluster", front_address)
        })
        .into(),
    );
    add_backend(
        &mut worker,
        "respi_h2_cluster",
        "respi_h2_back_0",
        back_address,
    );
    let mut backend = SyncBackend::new(
        "respi_h2_back",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();
    worker.read_to_last();

    let uri: hyper::Uri = format!("https://localhost:{}/", front_address.port())
        .parse()
        .expect("H2 response-inject URI must parse");

    let client = build_h2_client();
    let backend_handle = std::thread::spawn(move || {
        if accept_with_retry(&mut backend, 0, SYNC_BACKEND_TWO_SHOT_ACCEPT_BUDGET_MS) {
            let _ = backend.receive(0);
            backend.send(0);
        }
        backend
    });
    let outcome = resolve_request(&client, uri);
    let mut backend = backend_handle
        .join()
        .expect("backend thread should not panic");
    backend.disconnect();
    worker.soft_stop();
    worker.wait_for_server_stop();

    match outcome {
        Some((status, _body)) if status.as_u16() == 200 => State::Success,
        Some((status, body)) => {
            eprintln!("expected H2 200 with response header, got status={status} body={body:?}");
            State::Fail
        }
        None => {
            eprintln!("H2 client could not resolve the response-inject request");
            State::Fail
        }
    }
}

#[test]
fn test_response_header_inject_h2_h1() {
    assert_eq!(
        repeat_until_error_or(2, "response inject H2↔H1", try_response_header_inject_h2_h1),
        State::Success
    );
}

// ═════════════════════════════════════════════════════════════════════
// Cardinality matrix — H2 backend coverage (cluster.http2 = true)
// ═════════════════════════════════════════════════════════════════════

/// Snapshot the H2 backend until at least `target` requests are
/// recorded or the deadline expires. Polls every 25 ms so a slow CI
/// runner does not return an empty log.
fn wait_for_h2_requests(
    backend: &H2Backend,
    target: usize,
    deadline: std::time::Instant,
) -> Vec<crate::mock::h2_backend::RecordedH2Request> {
    while std::time::Instant::now() < deadline {
        let snapshot = backend.recorded_requests();
        if snapshot.len() >= target {
            return snapshot;
        }
        std::thread::sleep(Duration::from_millis(H2_BACKEND_RECORD_POLL_MS));
    }
    backend.recorded_requests()
}

/// H1 frontend → H2 backend smoke. Asserts on the recorded request
/// shape (method, path, authority) so a future regression in the H2
/// pseudo-header emission surfaces here, not just in the request
/// counter.
pub fn try_h1_to_h2_backend_smoke() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_http_listener("H1-H2B-SMOKE", front_address);
    worker.send_proxy_request(
        RequestType::AddCluster(Cluster {
            http2: Some(true),
            ..Worker::default_cluster("h1h2_cluster")
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(Worker::default_http_frontend("h1h2_cluster", front_address))
            .into(),
    );
    add_backend(&mut worker, "h1h2_cluster", "h1h2_back_0", back_address);
    let backend = H2Backend::start("h1h2_back", back_address, "pong");
    worker.read_to_last();

    let mut client = Client::new(
        "h1h2_client",
        front_address,
        http_request("GET", "/", "ping", "localhost"),
    );
    client.connect();
    client.send();
    let resp = client.receive();

    let deadline = std::time::Instant::now() + Duration::from_millis(H2_BACKEND_RECORD_BUDGET_MS);
    let snapshot = wait_for_h2_requests(&backend, 1, deadline);
    drop(backend);
    worker.soft_stop();
    worker.wait_for_server_stop();

    if resp.is_none() || snapshot.is_empty() {
        eprintln!(
            "H1↔H2 smoke: expected ≥1 backend request and a frontend response, got resp={resp:?} backend={snapshot:?}"
        );
        return State::Fail;
    }
    let req = &snapshot[0];
    if req.method != "GET" {
        eprintln!("H1↔H2 smoke: expected method GET, got {:?}", req.method);
        return State::Fail;
    }
    if req.path != "/" {
        eprintln!("H1↔H2 smoke: expected path '/', got {:?}", req.path);
        return State::Fail;
    }
    if !req.authority.starts_with("localhost") {
        eprintln!(
            "H1↔H2 smoke: expected :authority to start with 'localhost', got {:?}",
            req.authority
        );
        return State::Fail;
    }
    State::Success
}

#[test]
fn test_h1_to_h2_backend_smoke() {
    assert_eq!(
        repeat_until_error_or(1, "H1↔H2-backend smoke", try_h1_to_h2_backend_smoke),
        State::Success
    );
}

/// H1 frontend → H2 backend with a `--rewrite-path` policy. Asserts
/// the H2 backend received the rewritten path AND the rewritten
/// authority is reflected in `:authority` (mux applies both via the
/// kawa StatusLine.path/uri rewrite then the converter emits them as
/// pseudo-headers).
pub fn try_rewrite_path_h1_h2() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_http_listener("REW-H1H2", front_address);
    worker.send_proxy_request(
        RequestType::AddCluster(Cluster {
            http2: Some(true),
            ..Worker::default_cluster("rewp_h1h2_cluster")
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            rewrite_host: Some("backend.example.com".to_owned()),
            rewrite_path: Some("/v2/api".to_owned()),
            ..Worker::default_http_frontend("rewp_h1h2_cluster", front_address)
        })
        .into(),
    );
    add_backend(
        &mut worker,
        "rewp_h1h2_cluster",
        "rewp_h1h2_back_0",
        back_address,
    );
    let backend = H2Backend::start("rewp_h1h2_back", back_address, "pong");
    worker.read_to_last();

    let mut client = Client::new(
        "rewp_h1h2_client",
        front_address,
        http_request("GET", "/legacy/api", "ping", "localhost"),
    );
    client.connect();
    client.send();
    let resp = client.receive();

    let deadline = std::time::Instant::now() + Duration::from_millis(H2_BACKEND_RECORD_BUDGET_MS);
    let snapshot = wait_for_h2_requests(&backend, 1, deadline);
    drop(backend);
    worker.soft_stop();
    worker.wait_for_server_stop();

    if resp.is_none() || snapshot.is_empty() {
        eprintln!("rewrite H1↔H2: missing data, resp={resp:?} backend={snapshot:?}");
        return State::Fail;
    }
    let req = &snapshot[0];
    if req.path != "/v2/api" {
        eprintln!(
            "rewrite H1↔H2: expected :path '/v2/api', got {:?}",
            req.path
        );
        return State::Fail;
    }
    if req.authority != "backend.example.com" {
        eprintln!(
            "rewrite H1↔H2: expected :authority 'backend.example.com', got {:?}",
            req.authority
        );
        return State::Fail;
    }
    let xfh = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-forwarded-host"))
        .map(|(_, v)| v.as_slice());
    if !matches!(xfh, Some(v) if v.starts_with(b"localhost")) {
        eprintln!("rewrite H1↔H2: expected X-Forwarded-Host carrying 'localhost', got {xfh:?}");
        return State::Fail;
    }
    State::Success
}

#[test]
fn test_rewrite_path_h1_h2() {
    assert_eq!(
        repeat_until_error_or(1, "rewrite H1↔H2", try_rewrite_path_h1_h2),
        State::Success
    );
}

/// H2 frontend (TLS h2) → H2 backend (h2c) with a `--header
/// request=X-Tenant=foo` injection. Asserts the H2 backend received
/// the operator-configured request header on the wire.
pub fn try_request_header_inject_h2_h2() -> State {
    use crate::mock::https_client::{build_h2_client, resolve_request};

    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_worker_with_https_listener("REQ-INJ-H2H2", front_address);
    worker.send_proxy_request(
        RequestType::AddCluster(Cluster {
            http2: Some(true),
            ..Worker::default_cluster("reqi_h2h2_cluster")
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpsFrontend(RequestHttpFrontend {
            headers: vec![Header {
                position: HeaderPosition::Request as i32,
                key: "X-Tenant".to_owned(),
                val: "alpha".to_owned(),
            }],
            ..Worker::default_http_frontend("reqi_h2h2_cluster", front_address)
        })
        .into(),
    );
    add_backend(
        &mut worker,
        "reqi_h2h2_cluster",
        "reqi_h2h2_back_0",
        back_address,
    );
    let backend = H2Backend::start("reqi_h2h2_back", back_address, "pong");
    worker.read_to_last();

    let uri: hyper::Uri = format!("https://localhost:{}/", front_address.port())
        .parse()
        .expect("H2H2 inject URI must parse");
    let client = build_h2_client();
    let outcome = resolve_request(&client, uri);

    let deadline = std::time::Instant::now() + Duration::from_millis(H2_BACKEND_RECORD_BUDGET_MS);
    let snapshot = wait_for_h2_requests(&backend, 1, deadline);
    drop(backend);
    worker.soft_stop();
    worker.wait_for_server_stop();

    if !matches!(outcome, Some((s, _)) if s.as_u16() == 200) {
        eprintln!("inject H2↔H2: expected status 200, got {outcome:?}");
        return State::Fail;
    }
    let Some(req) = snapshot.first() else {
        eprintln!("inject H2↔H2: H2 backend recorded no request");
        return State::Fail;
    };
    let injected = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-tenant"))
        .map(|(_, v)| v.as_slice());
    if injected != Some(b"alpha".as_slice()) {
        eprintln!("inject H2↔H2: expected X-Tenant: alpha on backend, got {injected:?}");
        return State::Fail;
    }
    State::Success
}

#[test]
fn test_request_header_inject_h2_h2() {
    assert_eq!(
        repeat_until_error_or(2, "request inject H2↔H2", try_request_header_inject_h2_h2),
        State::Success
    );
}

// ═════════════════════════════════════════════════════════════════════
// Redirect plumbing — `redirect_template` + `rewrite_host` on 301
// ═════════════════════════════════════════════════════════════════════

/// Two interlocking invariants on the `RedirectPolicy::PERMANENT` path,
/// pinned in one fixture because they share the same call site:
///
/// - `redirect_template` is plumbed into the 301 default-answer. When
///   the field was destructured out of `RouteResult` only to be dropped,
///   the operator-supplied template had no observable effect; the
///   response always rendered the listener / cluster default. The test
///   ships a template carrying a load-bearing `X-Custom-Redirect`
///   header that the listener default does NOT emit, so its presence
///   in the response confirms the inline-render path ran.
///
/// - `rewrite_host` feeds the 301 `Location`. When
///   `build_redirect_location` only read `context.authority`, a
///   `rewrite_host = "new.example.com"` frontend redirected the client
///   back to the original `Host:` header. The test sends a request to
///   `old.example.com` (here: `localhost`) and asserts the resulting
///   `Location:` carries `new.example.com`.
pub fn try_redirect_permanent_uses_rewrite_host_and_template() -> State {
    let front_address = create_local_address();
    let unused_back = create_unbound_local_address();
    let mut worker = spawn_worker_with_http_listener("REDIR-TEMPLATE-REWRITE", front_address);

    worker.send_proxy_request(
        RequestType::AddCluster(Cluster {
            https_redirect_port: Some(8443),
            ..Worker::default_cluster("redir_tmpl_cluster")
        })
        .into(),
    );

    // Custom 301 template carrying a load-bearing `X-Custom-Redirect`
    // header. RFC 9110 §5: header-field-name is case-insensitive; the
    // listener-default `http.301.redirection` template does NOT emit
    // this header, so its presence in the response is a clean signal
    // that the inline-render path ran.
    //
    // `%REDIRECT_LOCATION` is the canonical placeholder for the 301
    // Location URL — the same variable schema the listener default
    // uses, so the same `(REDIRECT_LOCATION, ROUTE, REQUEST_ID)`
    // variable feed produced by `Router::connect` flows in cleanly.
    let custom_template = "\
HTTP/1.1 301 Moved Permanently\r\n\
Location: %REDIRECT_LOCATION\r\n\
X-Custom-Redirect: from-template\r\n\
Content-Length: 0\r\n\r\n"
        .to_owned();

    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            redirect: Some(RedirectPolicy::Permanent as i32),
            redirect_scheme: Some(RedirectScheme::UseHttps as i32),
            redirect_template: Some(custom_template),
            // `rewrite_host` is a literal target host (the static
            // grammar; capture-templating uses `%1` etc., not exercised
            // here). The frontend's hostname stays `localhost` so the
            // request still routes to this cluster; the rewrite kicks
            // in for the post-route `Location` URL.
            rewrite_host: Some("new.example.com".to_owned()),
            ..Worker::default_http_frontend("redir_tmpl_cluster", front_address)
        })
        .into(),
    );
    add_backend(
        &mut worker,
        "redir_tmpl_cluster",
        "redir_tmpl_back_0",
        unused_back,
    );
    worker.read_to_last();

    let mut client = Client::new(
        "redir_tmpl_client",
        front_address,
        http_request("GET", "/path", "ping", "localhost"),
    );
    client.connect();
    client.send();
    let response = client
        .receive()
        .expect("frontend must emit a 301 default-answer");
    worker.soft_stop();
    worker.wait_for_server_stop();

    let lower = response.to_ascii_lowercase();

    // Template assertion: the operator-supplied template ran. The
    // listener default never emits `X-Custom-Redirect`, so this header
    // proves the inline render path took over the answer.
    if !lower.contains("x-custom-redirect: from-template") {
        eprintln!("expected X-Custom-Redirect from operator template, got:\n{response}");
        return State::Fail;
    }

    // Host assertion: the 301 Location uses the rewritten host, not the
    // original `Host: localhost` from the request line.
    if !lower.contains("location: https://new.example.com") {
        eprintln!("expected Location to use rewritten host new.example.com, got:\n{response}");
        return State::Fail;
    }

    if !response.contains("301") {
        eprintln!("expected 301 status, got:\n{response}");
        return State::Fail;
    }

    State::Success
}

#[test]
fn test_redirect_permanent_uses_rewrite_host_and_template() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "permanent redirect renders custom redirect_template and Location uses rewrite_host",
            try_redirect_permanent_uses_rewrite_host_and_template,
        ),
        State::Success,
    );
}

// ═════════════════════════════════════════════════════════════════════
// Clusterless `RedirectPolicy::Permanent` — 301 reachable without cluster
// ═════════════════════════════════════════════════════════════════════

/// A frontend may declare `redirect = permanent` without a backing
/// cluster — the canonical "this hostname has moved, no backing service
/// remains" shape from the original proposal in #1161.
///
/// Regression: a previous ordering in `Router::route_from_request`
/// short-circuited every `cluster_id == None` route to 401 BEFORE the
/// `RedirectPolicy::Permanent` branch, so a clusterless frontend with
/// `redirect = permanent` returned 401 instead of 301. The fix moves the
/// `Permanent` branch ahead of the clusterless-deny so the documented
/// behaviour is reachable.
pub fn try_clusterless_permanent_redirect_emits_301() -> State {
    let front_address = create_local_address();
    let mut worker = spawn_worker_with_http_listener("CLUSTERLESS-REDIR", front_address);

    // No `AddCluster` request — the frontend below is intentionally
    // detached. The router must still emit 301 because `redirect`
    // overrides the clusterless-deny.
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            cluster_id: None,
            address: front_address.into(),
            hostname: "old.example.com".to_owned(),
            path: sozu_command_lib::proto::command::PathRule::prefix(String::from("/")),
            position: sozu_command_lib::proto::command::RulePosition::Tree.into(),
            redirect: Some(RedirectPolicy::Permanent as i32),
            redirect_scheme: Some(RedirectScheme::UseSame as i32),
            rewrite_host: Some("new.example.com".to_owned()),
            ..Default::default()
        })
        .into(),
    );
    worker.read_to_last();

    let mut client = Client::new(
        "clusterless_redir_client",
        front_address,
        http_request("GET", "/path", "ping", "old.example.com"),
    );
    client.connect();
    client.send();
    let response = client
        .receive()
        .expect("frontend must emit a 301 default-answer for the clusterless permanent redirect");
    worker.soft_stop();
    worker.wait_for_server_stop();

    let lower = response.to_ascii_lowercase();
    if !response.contains("301") {
        eprintln!(
            "expected 301 for clusterless RedirectPolicy::Permanent (regression on the route ordering); got:\n{response}"
        );
        return State::Fail;
    }
    if !lower.contains("location: http://new.example.com") {
        eprintln!(
            "expected Location to use the rewritten host new.example.com on the same scheme; got:\n{response}"
        );
        return State::Fail;
    }
    State::Success
}

#[test]
fn test_clusterless_permanent_redirect_emits_301() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "clusterless permanent redirect emits 301 (not 401)",
            try_clusterless_permanent_redirect_emits_301,
        ),
        State::Success,
    );
}
