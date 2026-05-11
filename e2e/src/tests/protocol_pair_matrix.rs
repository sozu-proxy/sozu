//! Priority-1 e2e backfill across the four-cell `(frontend × backend)`
//! protocol-pair matrix.
//!
//! Cells are the same four exercised by `tests::try_tls_cardinality_cell`:
//!
//! | Cell    | Frontend         | Backend       |
//! | ------- | ---------------- | ------------- |
//! | h1_h1   | h1 + TLS         | h1 cleartext  |
//! | h1_h2   | h1 + TLS         | h2c cleartext |
//! | h2_h1   | h2 + TLS (ALPN)  | h1 cleartext  |
//! | h2_h2   | h2 + TLS (ALPN)  | h2c cleartext |
//!
//! Each feature is implemented as a single `try_<feature>_cell` function
//! that takes `(frontend_h2: bool, backend_h2: bool)` and returns
//! [`State`]. The [`protocol_pair_matrix!`] macro emits the four
//! wrapper functions plus their `#[test]` harnesses.
//!
//! Features covered here:
//!
//! - **Basic auth** (`required_auth = true` + `authorized_hashes`) —
//!   missing header → 401, correct creds → 200 from the backend.
//! - **301 redirect** (`RedirectPolicy::Permanent`) — backend never
//!   contacted; the response carries `:status 301` (H2) / `HTTP/1.1 301`
//!   (H1) with the resolved `Location` header.
//! - **Custom answer template** (`Cluster.answers."503"`) — operator's
//!   503 template renders verbatim when the backend is unreachable.
//! - **X-Real-IP elide + send** (`with_elide_x_real_ip` +
//!   `with_send_x_real_ip` on the HTTPS listener) — a client-supplied
//!   `X-Real-IP` is stripped and a proxy-generated one carrying the
//!   peer IP reaches the backend.
//!
//! Per-IP `429` and `evict_on_queue_full` are listener-policy
//! features whose existing single-cell coverage in
//! `cluster_ip_limit_tests.rs` and `eviction_tests.rs` already
//! exercises the policy logic; matrix coverage for them needs
//! connection-pinning helpers (sustained TLS + HTTP/2 connection
//! holds while a second connection races) that the current test
//! fixtures do not provide. Tracked in COVERAGE.md as a follow-up.

use std::time::Duration;

use hyper::{Request as HyperRequest, Uri};
use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, ListenerType, RedirectPolicy,
        RedirectScheme, RequestHttpFrontend, SocketAddress, request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        h2_backend::H2Backend,
        https_client::{HttpsClient, build_h2_client, build_https_client},
    },
    sozu::worker::Worker,
    tests::{
        State,
        tests::{create_local_address, create_unbound_local_address},
    },
};

// ── Frontend identification ─────────────────────────────────────────────
//
// `localhost` is the SAN baked into the test certificates at
// `lib/assets/local-certificate.pem` and `lib/assets/local-key.pem`. Any
// other authority would fail the rustls handshake.
const TEST_HOSTNAME: &str = "localhost";
const TLS_CERT_PEM: &str = include_str!("../../../lib/assets/local-certificate.pem");
const TLS_CERT_KEY: &str = include_str!("../../../lib/assets/local-key.pem");

/// Wallclock budget for a Hyper TLS request through the cell helper.
/// Wide enough to absorb a contended CI runner; the cell halts well
/// before the budget on success.
const REQUEST_TIMEOUT_SECS: u64 = 10;

/// Wallclock budget for the H2 mock backend to record the first
/// request after sōzu accepts. Mirrors the value used in
/// `redirect_rewrite_auth_tests`.
const H2_BACKEND_RECORD_BUDGET_MS: u64 = 2500;

/// Poll interval used while waiting for the H2 backend to register a
/// request. Small enough to react quickly under load, large enough to
/// avoid a busy spin.
const H2_BACKEND_RECORD_POLL_MS: u64 = 25;

// ── protocol_pair_matrix! macro ─────────────────────────────────────────
//
// Emits a `pub mod <name> { ... }` carrying four `pub fn try_h{1,2}_h{1,2}()`
// wrappers and four matching `#[test] fn test_h{1,2}_h{1,2}()` harnesses
// for a feature cell function of the shape
// `fn try_<name>_cell(frontend_h2: bool, backend_h2: bool) -> State`.
//
// Usage:
//
// ```ignore
// fn try_basic_auth_cell(frontend_h2: bool, backend_h2: bool) -> State { ... }
// protocol_pair_matrix!(basic_auth, try_basic_auth_cell, "basic auth");
// ```
//
// The matrix is then reachable as `protocol_pair_matrix::basic_auth::test_h1_h1`,
// `..::test_h1_h2`, `..::test_h2_h1`, `..::test_h2_h2`. `cargo test
// basic_auth` filters all four cells.

/// Emit the four `(frontend_h{1,2}, backend_h{1,2})` `try_*` wrappers
/// and the four matching `#[test]` cells. Identifiers are kept stable
/// by nesting the per-matrix functions inside a child module named
/// after `$name` — `macro_rules!` cannot concatenate identifiers
/// natively, so the module hop replaces what `paste!{ [<try_ $name …>] }`
/// used to do. The test harness still discovers each `#[test]` via the
/// `protocol_pair_matrix::$name::test_h{1,2}_h{1,2}` path; `cargo test
/// $name` filters them as before. No external code calls the generated
/// function names directly (verified by grep over `e2e/`), so the
/// nesting is invisible outside this file.
macro_rules! protocol_pair_matrix {
    ($name:ident, $cell:ident, $description:literal) => {
        pub mod $name {
            pub fn try_h1_h1() -> $crate::tests::State {
                super::$cell(false, false)
            }
            pub fn try_h1_h2() -> $crate::tests::State {
                super::$cell(false, true)
            }
            pub fn try_h2_h1() -> $crate::tests::State {
                super::$cell(true, false)
            }
            pub fn try_h2_h2() -> $crate::tests::State {
                super::$cell(true, true)
            }

            #[test]
            fn test_h1_h1() {
                assert_eq!(
                    $crate::tests::repeat_until_error_or(
                        2,
                        concat!($description, " h1↔h1"),
                        try_h1_h1,
                    ),
                    $crate::tests::State::Success,
                );
            }
            #[test]
            fn test_h1_h2() {
                assert_eq!(
                    $crate::tests::repeat_until_error_or(
                        2,
                        concat!($description, " h1↔h2c"),
                        try_h1_h2,
                    ),
                    $crate::tests::State::Success,
                );
            }
            #[test]
            fn test_h2_h1() {
                assert_eq!(
                    $crate::tests::repeat_until_error_or(
                        2,
                        concat!($description, " h2↔h1"),
                        try_h2_h1,
                    ),
                    $crate::tests::State::Success,
                );
            }
            #[test]
            fn test_h2_h2() {
                assert_eq!(
                    $crate::tests::repeat_until_error_or(
                        2,
                        concat!($description, " h2↔h2c"),
                        try_h2_h2,
                    ),
                    $crate::tests::State::Success,
                );
            }
        }
    };
}

// ── Setup helpers ───────────────────────────────────────────────────────

/// Listener-level customisation each cell wants applied before the
/// worker starts. Keeps `bring_up_https_listener` generic across
/// features.
#[derive(Clone, Copy, Default)]
struct ListenerOpts {
    elide_x_real_ip: bool,
    send_x_real_ip: bool,
}

/// Bring up a single-worker HTTPS listener at a freshly-allocated
/// address, attach the test certificate, and return the worker plus
/// the realised `(frontend_address, frontend_port)`.
///
/// The cluster, frontend, and backend are added by each cell. The
/// caller picks the backend address kind: `create_local_address()`
/// when a real backend mock will subsequently bind it,
/// `create_unbound_local_address()` when the cell expects sōzu's
/// connect to be refused (redirect / 503 / answer-engine paths).
fn bring_up_https_listener(
    name: &str,
    listener_opts: ListenerOpts,
) -> (Worker, std::net::SocketAddr, u16) {
    let front_address = create_local_address();
    let front_port = front_address.port();

    let (config, listeners, state) = Worker::empty_https_config(front_address);
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    let mut listener_builder = ListenerBuilder::new_https(SocketAddress::from(front_address));
    if listener_opts.elide_x_real_ip {
        listener_builder.with_elide_x_real_ip(true);
    }
    if listener_opts.send_x_real_ip {
        listener_builder.with_send_x_real_ip(true);
    }
    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        listener_builder
            .to_tls(None)
            .expect("default HTTPS listener must build"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: SocketAddress::from(front_address),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: SocketAddress::from(front_address),
        certificate: CertificateAndKey {
            certificate: TLS_CERT_PEM.to_owned(),
            key: TLS_CERT_KEY.to_owned(),
            certificate_chain: vec![],
            versions: vec![],
            names: vec![],
        },
        expired_at: None,
    }));
    worker.read_to_last();

    (worker, front_address, front_port)
}

/// Add a default cluster (with `cluster.http2 = backend_h2`) plus the
/// supplied `frontend` and a backend pointing at `back_address`.
fn install_cluster_and_frontend(
    worker: &mut Worker,
    cluster_id: &str,
    backend_h2: bool,
    cluster_overrides: impl FnOnce(Cluster) -> Cluster,
    frontend: RequestHttpFrontend,
    back_address: std::net::SocketAddr,
) {
    let cluster = cluster_overrides(Cluster {
        http2: Some(backend_h2),
        ..Worker::default_cluster(cluster_id)
    });
    worker.send_proxy_request_type(RequestType::AddCluster(cluster));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(frontend));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        cluster_id,
        format!("{cluster_id}-0"),
        back_address,
        None,
    )));
    worker.read_to_last();
}

/// Owns one of the two backend mock kinds for the duration of a cell.
/// Encapsulates the H1/H2 spawn + teardown + request-counter accessors
/// behind a uniform shape.
enum CellBackend {
    H1(AsyncBackend<SimpleAggregator>),
    H2(H2Backend),
}

impl CellBackend {
    /// Spawn the appropriate mock for `backend_h2` at `addr`.
    fn start(addr: std::net::SocketAddr, backend_h2: bool) -> Self {
        if backend_h2 {
            CellBackend::H2(H2Backend::start("MATRIX-H2", addr, "matrix-h2-pong"))
        } else {
            CellBackend::H1(AsyncBackend::spawn_detached_backend(
                "MATRIX-H1",
                addr,
                SimpleAggregator::default(),
                AsyncBackend::http_handler("matrix-h1-pong"),
            ))
        }
    }

    /// Number of responses delivered to clients during the cell. Used
    /// to detect the auth-gate / redirect / 503-template path: a
    /// backend that never received a request stays at `0`.
    fn responses_sent(self) -> usize {
        match self {
            CellBackend::H1(mut backend) => backend
                .stop_and_get_aggregator()
                .map(|agg| agg.responses_sent)
                .unwrap_or(0),
            CellBackend::H2(mut backend) => {
                let n = backend.get_responses_sent();
                backend.stop();
                n
            }
        }
    }

    /// `H2Backend` exposes the captured request bytes via
    /// `recorded_requests()`. The X-Real-IP cell uses this to read
    /// the headers actually delivered to the backend; for `H1`
    /// returns an empty vector (the existing `AsyncBackend` mock does
    /// not capture request bytes).
    fn captured_request_headers_h2(&self) -> Vec<(String, Vec<u8>)> {
        let CellBackend::H2(backend) = self else {
            return Vec::new();
        };
        let deadline =
            std::time::Instant::now() + Duration::from_millis(H2_BACKEND_RECORD_BUDGET_MS);
        while std::time::Instant::now() < deadline {
            let snapshot = backend.recorded_requests();
            if let Some(req) = snapshot.first() {
                return req
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
            }
            std::thread::sleep(Duration::from_millis(H2_BACKEND_RECORD_POLL_MS));
        }
        Vec::new()
    }
}

/// Pick the right Hyper TLS client for the requested frontend protocol.
fn build_frontend_client(frontend_h2: bool) -> HttpsClient {
    if frontend_h2 {
        build_h2_client()
    } else {
        build_https_client()
    }
}

/// Build a `https://localhost:<port><path>` URI rooted at the frontend
/// listener.
fn build_uri(port: u16, path: &str) -> Uri {
    format!("https://{TEST_HOSTNAME}:{port}{path}")
        .parse()
        .expect("URI must parse")
}

/// Drive a single Hyper request through the cell client and return the
/// resulting status code (or `None` on transport failure / timeout).
fn send_request(client: &HttpsClient, req: HyperRequest<String>) -> Option<u16> {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        match tokio::time::timeout(
            Duration::from_secs(REQUEST_TIMEOUT_SECS),
            client.request(req),
        )
        .await
        {
            Ok(Ok(resp)) => Some(resp.status().as_u16()),
            Ok(Err(error)) => {
                eprintln!("matrix request failed: {error}");
                None
            }
            Err(_) => {
                eprintln!("matrix request timed out");
                None
            }
        }
    })
}

/// Drive a single Hyper request and return the full
/// `(status, headers, body)` triple.
fn send_request_full(
    client: &HttpsClient,
    req: HyperRequest<String>,
) -> Option<(u16, Vec<(String, Vec<u8>)>, String)> {
    use http_body_util::BodyExt;

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let fut = async {
            let resp = match client.request(req).await {
                Ok(r) => r,
                Err(error) => {
                    eprintln!("matrix request failed: {error}");
                    return None;
                }
            };
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.as_bytes().to_vec()))
                .collect::<Vec<_>>();
            let body_bytes = match resp.into_body().collect().await {
                Ok(c) => c.to_bytes(),
                Err(error) => {
                    eprintln!("matrix body collect failed: {error}");
                    return Some((status, headers, String::new()));
                }
            };
            let body = String::from_utf8(body_bytes.to_vec()).unwrap_or_default();
            Some((status, headers, body))
        };
        match tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), fut).await {
            Ok(result) => result,
            Err(_) => {
                eprintln!("matrix full-request timed out");
                None
            }
        }
    })
}

// ═════════════════════════════════════════════════════════════════════
// Feature 1 — Basic auth
// ═════════════════════════════════════════════════════════════════════
//
// Three arms per cell:
// - missing Authorization header → 401
// - wrong credentials             → 401
// - correct credentials           → 200 from the backend

/// SHA-256 of the literal byte string `"secret"`, lowercase hex,
/// matching `printf 'secret' | sha256sum`. Pinned so the
/// `authorized_hashes` entry is reproducible without rederivation.
const SECRET_SHA256_HEX: &str = "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b";

fn try_basic_auth_cell(frontend_h2: bool, backend_h2: bool) -> State {
    use base64::{Engine, engine::general_purpose::STANDARD};

    let cluster_id = "matrix_auth_cluster";
    let (mut worker, front_address, front_port) =
        bring_up_https_listener("MATRIX-AUTH", ListenerOpts::default());
    let back_address = create_local_address();
    install_cluster_and_frontend(
        &mut worker,
        cluster_id,
        backend_h2,
        |c| Cluster {
            authorized_hashes: vec![format!("admin:{SECRET_SHA256_HEX}")],
            www_authenticate: Some("Basic realm=\"sozu\"".to_owned()),
            ..c
        },
        RequestHttpFrontend {
            required_auth: Some(true),
            ..Worker::default_http_frontend(cluster_id, front_address)
        },
        back_address,
    );

    let backend = CellBackend::start(back_address, backend_h2);
    let client = build_frontend_client(frontend_h2);
    let uri = build_uri(front_port, "/secured");

    // Arm 1: no Authorization header → 401
    let req_no_auth = HyperRequest::builder()
        .method("GET")
        .uri(uri.clone())
        .body(String::new())
        .expect("no-auth request builds");
    let arm1 = send_request(&client, req_no_auth);

    // Arm 2: wrong credentials → 401
    let token_wrong = STANDARD.encode(b"admin:wrong");
    let req_wrong = HyperRequest::builder()
        .method("GET")
        .uri(uri.clone())
        .header("authorization", format!("Basic {token_wrong}"))
        .body(String::new())
        .expect("wrong-auth request builds");
    let arm2 = send_request(&client, req_wrong);

    // Arm 3: correct credentials → 200 from the backend.
    let token_ok = STANDARD.encode(b"admin:secret");
    let req_ok = HyperRequest::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Basic {token_ok}"))
        .body(String::new())
        .expect("authed request builds");
    let arm3 = send_request(&client, req_ok);

    worker.soft_stop();
    let stop_ok = worker.wait_for_server_stop();
    let backend_hits = backend.responses_sent();

    if arm1 != Some(401) {
        eprintln!("basic-auth arm1: expected 401 (no Authorization), got {arm1:?}");
        return State::Fail;
    }
    if arm2 != Some(401) {
        eprintln!("basic-auth arm2: expected 401 (wrong creds), got {arm2:?}");
        return State::Fail;
    }
    if arm3 != Some(200) {
        eprintln!("basic-auth arm3: expected 200 (correct creds), got {arm3:?}");
        return State::Fail;
    }
    if backend_hits == 0 {
        eprintln!("basic-auth: backend received no successful request");
        return State::Fail;
    }
    if !stop_ok {
        return State::Fail;
    }
    State::Success
}

protocol_pair_matrix!(basic_auth, try_basic_auth_cell, "basic auth");

// ═════════════════════════════════════════════════════════════════════
// Feature 2 — 301 redirect (RedirectPolicy::Permanent)
// ═════════════════════════════════════════════════════════════════════
//
// The redirect path skips the backend entirely. The h1_h2 / h2_h2
// cells are nominally degenerate (no backend connect happens), but we
// still run them to confirm the redirect remains independent of
// `cluster.http2`.

/// Common redirect-cell shape: spawn an HTTPS worker with `policy` on the
/// frontend, send a single `GET`, assert the response carries
/// `expected_status` plus an `https://` `Location` header. The backend is
/// intentionally unbound — a redirect MUST short-circuit before the
/// upstream connect, so a registry-bound port would mask a regression.
///
/// Used by the Permanent (301), Found (302), and PermanentRedirect (308)
/// matrix cells (closes #1009 with full 4-cell coverage per status).
fn try_redirect_cell_inner(
    log_tag: &str,
    cluster_id: &str,
    frontend_h2: bool,
    backend_h2: bool,
    policy: RedirectPolicy,
    expected_status: u16,
) -> State {
    let (mut worker, front_address, front_port) =
        bring_up_https_listener(log_tag, ListenerOpts::default());
    let back_address = create_unbound_local_address();
    install_cluster_and_frontend(
        &mut worker,
        cluster_id,
        backend_h2,
        |c| Cluster {
            https_redirect_port: Some(8443),
            ..c
        },
        RequestHttpFrontend {
            redirect: Some(policy as i32),
            redirect_scheme: Some(RedirectScheme::UseHttps as i32),
            ..Worker::default_http_frontend(cluster_id, front_address)
        },
        back_address,
    );

    let client = build_frontend_client(frontend_h2);
    let uri = build_uri(front_port, "/path");
    let req = HyperRequest::builder()
        .method("GET")
        .uri(uri)
        .body(String::new())
        .expect("redirect request builds");
    let outcome = send_request_full(&client, req);

    worker.soft_stop();
    let stop_ok = worker.wait_for_server_stop();

    let Some((status, headers, _)) = outcome else {
        eprintln!("{log_tag}: client did not resolve a response");
        return State::Fail;
    };
    if status != expected_status {
        eprintln!("{log_tag}: expected {expected_status}, got {status}");
        return State::Fail;
    }
    let location = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("location"))
        .map(|(_, v)| String::from_utf8_lossy(v).to_string());
    let location = match location {
        Some(loc) => loc,
        None => {
            eprintln!("{log_tag}: response missing Location header");
            return State::Fail;
        }
    };
    let lowered = location.to_ascii_lowercase();
    if !lowered.starts_with("https://") {
        eprintln!("{log_tag}: expected https Location, got {location}");
        return State::Fail;
    }
    // Catch the regressions a `starts_with("https://")` check would
    // miss: empty host (`https://:443/...`) or cross-tenant bleed
    // from a state-replay ordering bug. The frontend binds via
    // `bring_up_https_listener` whose hostname is `localhost`
    // (HOST_NAME is the SAN baked into the test certs); the
    // redirect URL preserves the request's `Host:` verbatim.
    if !lowered.contains("localhost") {
        eprintln!("{log_tag}: expected Location host localhost, got {location}");
        return State::Fail;
    }
    if !stop_ok {
        return State::Fail;
    }
    State::Success
}

fn try_redirect_permanent_cell(frontend_h2: bool, backend_h2: bool) -> State {
    try_redirect_cell_inner(
        "MATRIX-REDIR-301",
        "matrix_redir_301_cluster",
        frontend_h2,
        backend_h2,
        RedirectPolicy::Permanent,
        301,
    )
}

protocol_pair_matrix!(
    redirect_permanent,
    try_redirect_permanent_cell,
    "redirect permanent"
);

fn try_redirect_found_cell(frontend_h2: bool, backend_h2: bool) -> State {
    try_redirect_cell_inner(
        "MATRIX-REDIR-302",
        "matrix_redir_302_cluster",
        frontend_h2,
        backend_h2,
        RedirectPolicy::Found,
        302,
    )
}

protocol_pair_matrix!(redirect_found, try_redirect_found_cell, "redirect 302");

fn try_redirect_permanent_redirect_cell(frontend_h2: bool, backend_h2: bool) -> State {
    try_redirect_cell_inner(
        "MATRIX-REDIR-308",
        "matrix_redir_308_cluster",
        frontend_h2,
        backend_h2,
        RedirectPolicy::PermanentRedirect,
        308,
    )
}

protocol_pair_matrix!(
    redirect_permanent_redirect,
    try_redirect_permanent_redirect_cell,
    "redirect 308"
);

// ═════════════════════════════════════════════════════════════════════
// Feature 3 — Custom 503 answer template
// ═════════════════════════════════════════════════════════════════════
//
// The cluster's `answers."503"` template renders verbatim when the
// backend is unreachable. The backend mock is never started so the
// connect attempt fails and the answer engine fires.

fn try_custom_503_template_cell(frontend_h2: bool, backend_h2: bool) -> State {
    let cluster_id = "matrix_503_cluster";
    let (mut worker, front_address, front_port) =
        bring_up_https_listener("MATRIX-503", ListenerOpts::default());
    // Backend is intentionally unbound so sōzu's connect attempt
    // gets refused; the answer engine then renders the 503 template.
    let back_address = create_unbound_local_address();
    install_cluster_and_frontend(
        &mut worker,
        cluster_id,
        backend_h2,
        |c| {
            let mut answers = std::collections::BTreeMap::new();
            // The template engine rejects literal `Content-Length`
            // and injects the real value at fill time; see
            // `lib/src/protocol/kawa_h1/answers.rs`. The X-Sozu-Stamp
            // header is the load-bearing signal that the operator
            // template ran (the listener default does not emit it).
            answers.insert(
                "503".to_owned(),
                "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nX-Sozu-Stamp: matrix-503-tag\r\n\r\nmatrix-503-body".to_owned(),
            );
            Cluster { answers, ..c }
        },
        Worker::default_http_frontend(cluster_id, front_address),
        back_address,
    );

    let client = build_frontend_client(frontend_h2);
    let uri = build_uri(front_port, "/");
    let req = HyperRequest::builder()
        .method("GET")
        .uri(uri)
        .body(String::new())
        .expect("503 request builds");
    let outcome = send_request_full(&client, req);

    worker.soft_stop();
    let stop_ok = worker.wait_for_server_stop();

    let Some((status, headers, body)) = outcome else {
        eprintln!("503 cell: client did not resolve a response");
        return State::Fail;
    };
    if status != 503 {
        eprintln!("503 cell: expected 503, got {status}");
        return State::Fail;
    }
    let stamp = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("x-sozu-stamp") && v == b"matrix-503-tag");
    if !stamp {
        eprintln!(
            "503 cell: missing X-Sozu-Stamp from operator template; headers={headers:?} body={body:?}"
        );
        return State::Fail;
    }
    if !body.contains("matrix-503-body") {
        eprintln!("503 cell: body did not carry operator marker; body={body:?}");
        return State::Fail;
    }
    if !stop_ok {
        return State::Fail;
    }
    State::Success
}

protocol_pair_matrix!(
    custom_503_template,
    try_custom_503_template_cell,
    "custom 503 template"
);

// ═════════════════════════════════════════════════════════════════════
// Feature 4 — X-Real-IP elide + send (initial HEADERS)
// ═════════════════════════════════════════════════════════════════════
//
// Listener flips both flags. The test sends a request carrying a
// spoofed `X-Real-IP: 1.2.3.4` and asserts the H2 backend records the
// proxy substitute (`X-Real-IP: 127.0.0.1`) and not the spoof. For H1
// backend cells we degrade the assertion: `AsyncBackend` does not
// expose request bytes, so the H1 cell only confirms that the
// frontend forwards (status 200) — the listener flag pair is
// validated end-to-end by the H2 backend cells in this matrix.
// Trailer-frame elision (the H2-specific regression scaffolded in
// `tests::test_x_real_ip_elide_h2_trailer`) remains a separate
// targeted test; this matrix covers initial-HEADERS elision.

fn try_x_real_ip_elide_send_cell(frontend_h2: bool, backend_h2: bool) -> State {
    let cluster_id = "matrix_xrip_cluster";
    let (mut worker, front_address, front_port) = bring_up_https_listener(
        "MATRIX-XRIP",
        ListenerOpts {
            elide_x_real_ip: true,
            send_x_real_ip: true,
        },
    );
    let back_address = create_local_address();
    install_cluster_and_frontend(
        &mut worker,
        cluster_id,
        backend_h2,
        |c| c,
        Worker::default_http_frontend(cluster_id, front_address),
        back_address,
    );

    let backend = CellBackend::start(back_address, backend_h2);
    let client = build_frontend_client(frontend_h2);
    let uri = build_uri(front_port, "/");
    let req = HyperRequest::builder()
        .method("GET")
        .uri(uri)
        .header("x-real-ip", "1.2.3.4")
        .body(String::new())
        .expect("X-Real-IP request builds");
    let status = send_request(&client, req);

    let captured = if backend_h2 {
        backend.captured_request_headers_h2()
    } else {
        Vec::new()
    };
    let backend_hits = backend.responses_sent();
    worker.soft_stop();
    let stop_ok = worker.wait_for_server_stop();

    if status != Some(200) {
        eprintln!("X-Real-IP cell: expected 200 from backend, got {status:?}");
        return State::Fail;
    }
    if backend_hits == 0 {
        eprintln!("X-Real-IP cell: backend received no request");
        return State::Fail;
    }

    if backend_h2 {
        let spoof_present = captured
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-real-ip") && v == b"1.2.3.4");
        if spoof_present {
            eprintln!(
                "X-Real-IP cell: client-supplied 1.2.3.4 reached backend; headers={captured:?}"
            );
            return State::Fail;
        }
        let proxy_present = captured.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("x-real-ip")
                && std::str::from_utf8(v)
                    .map(|s| s == "127.0.0.1")
                    .unwrap_or(false)
        });
        if !proxy_present {
            eprintln!(
                "X-Real-IP cell: proxy-supplied 127.0.0.1 not present on backend; headers={captured:?}"
            );
            return State::Fail;
        }
    }
    if !stop_ok {
        return State::Fail;
    }
    State::Success
}

protocol_pair_matrix!(
    x_real_ip_elide_send,
    try_x_real_ip_elide_send_cell,
    "x-real-ip elide+send"
);
