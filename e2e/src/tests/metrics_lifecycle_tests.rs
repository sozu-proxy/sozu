//! End-to-end coverage for the metrics lifecycle IPC bridge added in
//! PR #1252 (`RemoveCluster` / `RemoveBackend` →
//! `Aggregator::remove_cluster` / `remove_backend`) and the follow-up
//! tombstone (`AddCluster` → `Aggregator::add_cluster`).
//!
//! Unit tests in `lib/src/metrics/local_drain.rs` and
//! `network_drain.rs` cover the data-structure methods directly. These
//! e2e tests exercise the IPC wiring: master scatters `RemoveCluster`,
//! worker dispatches into `Server::notify_proxys`, the metrics removal
//! fires from there, and a subsequent `QueryMetrics` returns NoMetrics
//! for the removed cluster. The integration point that previously
//! silently regressed (10-min idle GC lingering after a `RemoveCluster`)
//! lives here.

use std::{
    io::{ErrorKind, Read, Write},
    net::SocketAddr,
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::FileConfig,
    proto::command::{
        ActivateListener, ListenerType, MetricDetail, QueryMetricsOptions, RemoveBackend, Request,
        RequestHttpFrontend, ResponseStatus, ServerConfig, SetMetricDetail, WorkerMetrics,
        filtered_metrics, request::RequestType, response_content::ContentType,
    },
    scm_socket::Listeners,
    state::ConfigState,
};

use crate::{
    http_utils::{http_ok_response, http_request},
    mock::{client::Client, sync_backend::Backend as SyncBackend},
    port_registry::attach_reserved_http_listener,
    sozu::worker::Worker,
    tests::{State, h2_utils::*, repeat_until_error_or},
};

use super::tests::create_local_address;

// ── Helpers ────────────────────────────────────────────────────────────────

/// Default config — nothing exotic, just enough to spin up an HTTP
/// listener and a routed cluster.
fn default_config() -> ServerConfig {
    Worker::into_config(FileConfig::default())
}

/// Boot a worker, attach an HTTP listener at `front_address`, register a
/// single cluster + frontend + backend, and return the worker handle.
fn setup_worker_with_cluster(
    name: &str,
    cluster_id: &str,
    backend_id: &str,
    front_address: SocketAddr,
    back_address: SocketAddr,
) -> Worker {
    let config = default_config();
    let mut listeners = Listeners::default();
    attach_reserved_http_listener(&mut listeners, front_address);
    let state = ConfigState::new();

    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpListener(
            sozu_command_lib::config::ListenerBuilder::new_http(front_address.into())
                .to_http(None)
                .unwrap(),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            interface: None,
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: false,
        })),
    });
    worker.send_proxy_request(RequestType::AddCluster(Worker::default_cluster(cluster_id)).into());
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            ..Worker::default_http_frontend(cluster_id, front_address)
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            cluster_id,
            backend_id,
            back_address,
            None,
        ))
        .into(),
    );
    worker.read_to_last();
    worker
}

/// Same legal control-plane shape as `setup_worker_with_cluster`, except the
/// frontend and backend refer to a cluster id that never receives AddCluster.
fn setup_worker_without_cluster(
    name: &str,
    cluster_id: &str,
    backend_id: &str,
    front_address: SocketAddr,
    back_address: SocketAddr,
) -> Worker {
    let config = default_config();
    let mut listeners = Listeners::default();
    attach_reserved_http_listener(&mut listeners, front_address);
    let state = ConfigState::new();
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpListener(
            sozu_command_lib::config::ListenerBuilder::new_http(front_address.into())
                .to_http(None)
                .unwrap(),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            interface: None,
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: false,
        })),
    });
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            ..Worker::default_http_frontend(cluster_id, front_address)
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            cluster_id,
            backend_id,
            back_address,
            None,
        ))
        .into(),
    );
    worker.read_to_last();
    worker
}

/// Drive one HTTP/1.1 request through the worker so the metrics layer
/// records cluster-scoped samples (access log, response time, backend
/// counters). Without this primer the cluster row never materialises.
fn prime_with_one_request(front_address: SocketAddr, back_address: SocketAddr) {
    let mut backend = SyncBackend::new(
        "metrics_lifecycle_backend",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();
    serve_one_request(&mut backend, front_address);
}

/// Route one `Connection: close` request through the worker and return whether
/// the real backend response crossed the full data-plane path.
fn serve_one_request(backend: &mut SyncBackend, front_address: SocketAddr) -> bool {
    let mut client = crate::mock::client::Client::new(
        "metrics_lifecycle_client",
        front_address,
        "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_owned(),
    );
    client.connect();
    if client.send().is_none() {
        return false;
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut served = false;
    while Instant::now() < deadline {
        if backend.accept(0) {
            backend.receive(0);
            backend.send(0);
            served = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    served
        && client
            .receive_response(Duration::from_secs(1))
            .is_some_and(|response| response.ends_with("\r\n\r\npong"))
}

/// Query the worker for the per-cluster metrics row of `cluster_id`.
/// Returns `true` when a row exists, `false` when the cluster is absent
/// or the response carries no per-cluster content. Empty metric_names
/// asks the drain to return whatever it has; the presence/absence of
/// `cluster_id` in the response is the actual signal.
fn cluster_row_present(worker: &mut Worker, cluster_id: &str) -> bool {
    worker.send_proxy_request_type(RequestType::QueryMetrics(QueryMetricsOptions {
        list: false,
        cluster_ids: vec![cluster_id.to_owned()],
        backend_ids: vec![],
        metric_names: vec![],
        no_clusters: false,
        workers: false,
    }));
    let response = match worker.read_proxy_response() {
        Some(r) => r,
        None => return false,
    };
    if response.status != ResponseStatus::Ok as i32 {
        return false;
    }
    let Some(content) = response.content.and_then(|c| c.content_type) else {
        return false;
    };
    let ContentType::WorkerMetrics(metrics) = content else {
        return false;
    };
    metrics
        .clusters
        .get(cluster_id)
        .map(|m| !m.cluster.is_empty() || !m.backends.is_empty())
        .unwrap_or(false)
}

const SESSION_BARRIER_BUDGET: Duration = Duration::from_secs(3);

/// Send one lifecycle request and consume intervening worker events until its
/// own acknowledgement arrives.
fn request_acknowledged(worker: &mut Worker, request: RequestType) -> bool {
    worker.send_proxy_request_type(request);
    let expected_id = worker.command_id.last.clone();
    loop {
        let Some(response) = worker.read_proxy_response() else {
            return false;
        };
        if response.id == expected_id {
            return response.status == ResponseStatus::Ok as i32;
        }
    }
}

/// Observable backend barrier: the proxy has both connected and forwarded the
/// complete request before the lifecycle command is allowed to proceed.
fn wait_for_backend_request(backend: &mut SyncBackend, client_id: usize) -> bool {
    let deadline = Instant::now() + SESSION_BARRIER_BUDGET;
    while Instant::now() < deadline {
        if !backend.clients.contains_key(&client_id) {
            let _ = backend.accept(client_id);
        }
        if backend.clients.contains_key(&client_id) && backend.receive(client_id).is_some() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Observe frontend-session resource termination after the held backend
/// connection is released. EOF or reset is not a metrics-ordering barrier;
/// the test establishes that separately through `QueryMetrics` below.
fn wait_for_client_termination(client: &mut Client) -> bool {
    let Some(stream) = client.stream.as_mut() else {
        return true;
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("client termination barrier must set a read timeout");
    let deadline = Instant::now() + SESSION_BARRIER_BUDGET;
    let mut buf = [0_u8; 1024];
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return true,
        }
    }
    false
}

/// Query metrics and consume unrelated asynchronous worker events until the
/// response carrying this query's command id arrives.
fn query_worker_metrics(
    worker: &mut Worker,
    options: QueryMetricsOptions,
) -> Option<WorkerMetrics> {
    worker.send_proxy_request_type(RequestType::QueryMetrics(options));
    let expected_id = worker.command_id.last.clone();
    loop {
        let response = worker.read_proxy_response()?;
        if response.id != expected_id {
            continue;
        }
        if response.status != ResponseStatus::Ok as i32 {
            return None;
        }
        let ContentType::WorkerMetrics(metrics) = response.content?.content_type? else {
            return None;
        };
        return Some(metrics);
    }
}

fn backend_connection_gauge(
    worker: &mut Worker,
    cluster_id: &str,
    backend_id: &str,
) -> Option<u64> {
    let metrics = query_worker_metrics(
        worker,
        QueryMetricsOptions {
            list: false,
            cluster_ids: vec![cluster_id.to_owned()],
            backend_ids: vec![],
            metric_names: vec!["connections_per_backend".to_owned()],
            no_clusters: false,
            workers: false,
        },
    )?;
    metrics
        .clusters
        .get(cluster_id)?
        .backends
        .iter()
        .find(|backend| backend.backend_id == backend_id)?
        .metrics
        .get("connections_per_backend")?
        .inner
        .as_ref()
        .and_then(|inner| match inner {
            filtered_metrics::Inner::Gauge(value) => Some(*value),
            _ => None,
        })
}

/// Incarnation-independent connection gauge. Its 2 -> 1 transition proves
/// that the old close reached the metrics pipeline without weakening global
/// accounting while the labelled decrement is fenced off.
fn proxy_backend_connection_gauge(worker: &mut Worker) -> Option<u64> {
    let metrics = query_worker_metrics(
        worker,
        QueryMetricsOptions {
            list: false,
            cluster_ids: vec![],
            backend_ids: vec![],
            metric_names: vec!["backend.connections".to_owned()],
            no_clusters: true,
            workers: false,
        },
    )?;
    metrics
        .proxy
        .get("backend.connections")?
        .inner
        .as_ref()
        .and_then(|inner| match inner {
            filtered_metrics::Inner::Gauge(value) => Some(*value),
            _ => None,
        })
}

/// This new-incarnation response is released only after the proxy gauge has
/// observed the old close. Seeing it proves a causally later event traversed
/// the same synchronous LocalDrain before the final labelled-gauge oracle.
fn backend_2xx_count(worker: &mut Worker, cluster_id: &str, backend_id: &str) -> Option<i64> {
    let metrics = query_worker_metrics(
        worker,
        QueryMetricsOptions {
            list: false,
            cluster_ids: vec![cluster_id.to_owned()],
            backend_ids: vec![],
            metric_names: vec!["http.status.2xx".to_owned()],
            no_clusters: false,
            workers: false,
        },
    )?;
    metrics
        .clusters
        .get(cluster_id)?
        .backends
        .iter()
        .find(|backend| backend.backend_id == backend_id)?
        .metrics
        .get("http.status.2xx")?
        .inner
        .as_ref()
        .and_then(|inner| match inner {
            filtered_metrics::Inner::Count(value) => Some(*value),
            _ => None,
        })
}

fn backend_access_log_count(
    worker: &mut Worker,
    cluster_id: &str,
    backend_id: &str,
) -> Option<i64> {
    let metrics = query_worker_metrics(
        worker,
        QueryMetricsOptions {
            list: false,
            cluster_ids: vec![cluster_id.to_owned()],
            backend_ids: vec![],
            metric_names: vec!["access_logs.count".to_owned()],
            no_clusters: false,
            workers: false,
        },
    )?;
    metrics
        .clusters
        .get(cluster_id)?
        .backends
        .iter()
        .find(|backend| backend.backend_id == backend_id)?
        .metrics
        .get("access_logs.count")?
        .inner
        .as_ref()
        .and_then(|inner| match inner {
            filtered_metrics::Inner::Count(value) => Some(*value),
            _ => None,
        })
}

fn proxy_gauge(worker: &mut Worker, metric_name: &str) -> Option<u64> {
    let metrics = query_worker_metrics(
        worker,
        QueryMetricsOptions {
            list: false,
            cluster_ids: vec![],
            backend_ids: vec![],
            metric_names: vec![metric_name.to_owned()],
            no_clusters: true,
            workers: false,
        },
    )?;
    metrics
        .proxy
        .get(metric_name)?
        .inner
        .as_ref()
        .and_then(|inner| match inner {
            filtered_metrics::Inner::Gauge(value) => Some(*value),
            _ => None,
        })
}

fn lease_backend_metric_detail(worker: &mut Worker) -> bool {
    request_acknowledged(
        worker,
        RequestType::SetMetricDetail(SetMetricDetail {
            client_id: "metrics-lifecycle-incarnation-test".to_owned(),
            detail: Some(MetricDetail::DetailBackend as i32),
            ttl_seconds: Some(60),
            clear: Some(false),
            reason: Some("same-identity cluster-incarnation regression".to_owned()),
            peer_pid: None,
            peer_session_ulid: None,
        }),
    )
}

fn wait_for_proxy_backend_connections(worker: &mut Worker, expected: u64) -> Option<u64> {
    let deadline = Instant::now() + SESSION_BARRIER_BUDGET;
    while Instant::now() < deadline {
        let actual = proxy_backend_connection_gauge(worker);
        if actual == Some(expected) {
            return actual;
        }
        thread::yield_now();
    }
    None
}

fn wait_for_backend_2xx(worker: &mut Worker, cluster_id: &str, backend_id: &str) -> Option<i64> {
    let deadline = Instant::now() + SESSION_BARRIER_BUDGET;
    while Instant::now() < deadline {
        let actual = backend_2xx_count(worker, cluster_id, backend_id);
        if actual.is_some_and(|count| count > 0) {
            return actual;
        }
        thread::yield_now();
    }
    None
}

fn wait_for_backend_access_logs(
    worker: &mut Worker,
    cluster_id: &str,
    backend_id: &str,
    expected: i64,
) -> Option<i64> {
    let deadline = Instant::now() + SESSION_BARRIER_BUDGET;
    while Instant::now() < deadline {
        let actual = backend_access_log_count(worker, cluster_id, backend_id);
        if actual.is_some_and(|value| value >= expected) {
            return actual;
        }
        thread::yield_now();
    }
    None
}

fn wait_for_proxy_gauge(worker: &mut Worker, metric_name: &str, expected: u64) -> Option<u64> {
    let deadline = Instant::now() + SESSION_BARRIER_BUDGET;
    while Instant::now() < deadline {
        let actual = proxy_gauge(worker, metric_name);
        if actual == Some(expected) {
            return actual;
        }
        thread::yield_now();
    }
    None
}

fn wait_for_client_payload(client: &mut Client, expected: &str) -> bool {
    let Some(stream) = client.stream.as_mut() else {
        return false;
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("websocket client barrier must set a read timeout");
    let deadline = Instant::now() + SESSION_BARRIER_BUDGET;
    let mut received = Vec::new();
    let mut buf = [0_u8; 1024];
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => return false,
            Ok(read) => {
                received.extend_from_slice(&buf[..read]);
                if received
                    .windows(expected.len())
                    .any(|window| window == expected.as_bytes())
                {
                    return true;
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return false,
        }
    }
    false
}

fn websocket_text_frame(payload: &str, masked: bool) -> Vec<u8> {
    assert!(
        payload.len() <= 125,
        "test websocket payload must stay short"
    );
    let mut frame = Vec::with_capacity(payload.len() + if masked { 6 } else { 2 });
    frame.push(0x81);
    frame.push(payload.len() as u8 | if masked { 0x80 } else { 0 });
    if masked {
        // A fixed zero mask keeps the witness readable while still exercising
        // the client-frame wire shape (MASK bit plus four-byte key).
        frame.extend_from_slice(&[0, 0, 0, 0]);
    }
    frame.extend_from_slice(payload.as_bytes());
    frame
}

fn send_client_websocket_text(client: &mut Client, payload: &str) -> bool {
    let Some(stream) = client.stream.as_mut() else {
        return false;
    };
    stream
        .write_all(&websocket_text_frame(payload, true))
        .and_then(|()| stream.flush())
        .is_ok()
}

fn send_backend_websocket_text(backend: &mut SyncBackend, client_id: usize, payload: &str) -> bool {
    let Some(stream) = backend.clients.get_mut(&client_id) else {
        return false;
    };
    stream
        .write_all(&websocket_text_frame(payload, false))
        .and_then(|()| stream.flush())
        .is_ok()
}

fn wait_for_backend_payload(backend: &mut SyncBackend, client_id: usize, expected: &str) -> bool {
    let Some(stream) = backend.clients.get_mut(&client_id) else {
        return false;
    };
    let deadline = Instant::now() + SESSION_BARRIER_BUDGET;
    let mut received = Vec::new();
    let mut buf = [0_u8; 1024];
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => return false,
            Ok(read) => {
                received.extend_from_slice(&buf[..read]);
                if received
                    .windows(expected.len())
                    .any(|window| window == expected.as_bytes())
                {
                    return true;
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return false,
        }
    }
    false
}

#[derive(Debug, Default)]
struct H2StreamObservation {
    status_200: bool,
    end_stream: bool,
    reset: bool,
    watched_stream_terminated: bool,
}

/// Read complete H2 frames until `stream_id` terminates or the failure budget
/// expires. `watched_stream_id` lets the caller prove the held stream did not
/// terminate while the replacement stream made progress.
fn wait_for_h2_stream_terminal(
    tls: &mut rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>,
    stream_id: u32,
    watched_stream_id: Option<u32>,
) -> H2StreamObservation {
    tls.sock
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("H2 terminal barrier must set a read timeout");
    let deadline = Instant::now() + SESSION_BARRIER_BUDGET;
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 16_384];
    let mut observation = H2StreamObservation::default();

    while Instant::now() < deadline {
        match tls.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => raw.extend_from_slice(&buffer[..size]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(_) => break,
        }

        let frames = parse_h2_frames(&raw);
        observation.status_200 |= stream_status_matches(&frames, stream_id, 200);
        observation.end_stream |= frames.iter().any(|(frame_type, flags, sid, _)| {
            *sid == stream_id
                && matches!(*frame_type, H2_FRAME_DATA | H2_FRAME_HEADERS)
                && (*flags & H2_FLAG_END_STREAM) != 0
        });
        observation.reset |= frames
            .iter()
            .any(|(frame_type, _, sid, _)| *sid == stream_id && *frame_type == H2_FRAME_RST_STREAM);
        if let Some(watched_stream_id) = watched_stream_id {
            observation.watched_stream_terminated |=
                frames.iter().any(|(frame_type, flags, sid, _)| {
                    *sid == watched_stream_id
                        && ((*frame_type == H2_FRAME_RST_STREAM)
                            || (matches!(*frame_type, H2_FRAME_DATA | H2_FRAME_HEADERS)
                                && (*flags & H2_FLAG_END_STREAM) != 0))
                });
        }
        if observation.end_stream || observation.reset {
            break;
        }
    }

    observation
}

fn h2_get_headers() -> Vec<u8> {
    vec![
        0x82, // :method GET
        0x84, // :path /
        0x87, // :scheme https
        0x41, 0x09, // :authority, value length 9
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ]
}

fn stop_worker_within(mut worker: Worker) -> bool {
    worker.hard_stop();
    let deadline = Instant::now() + SESSION_BARRIER_BUDGET;
    while !worker.server_job.is_finished() {
        if Instant::now() >= deadline {
            println!("worker did not stop within {SESSION_BARRIER_BUDGET:?}");
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    worker.wait_for_server_stop()
}

// ══════════════════════════════════════════════════════════════════════
// Test 1: RemoveCluster IPC drops the cluster row from the worker drain
// ══════════════════════════════════════════════════════════════════════

fn try_remove_cluster_drops_metric_row() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let cluster_id = "lifecycle_cluster_remove";
    let backend_id = "lifecycle_back_remove";

    let mut worker = setup_worker_with_cluster(
        "METRICS-LIFECYCLE-REMOVE",
        cluster_id,
        backend_id,
        front_address,
        back_address,
    );

    prime_with_one_request(front_address, back_address);

    // Sanity: cluster row exists after one request.
    if !cluster_row_present(&mut worker, cluster_id) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Undecided;
    }

    // Send RemoveCluster; the metrics removal happens inside the IPC
    // dispatch in `Server::notify_proxys`.
    worker.send_proxy_request_type(RequestType::RemoveCluster(cluster_id.to_owned()));
    worker.read_to_last();

    let absent = !cluster_row_present(&mut worker, cluster_id);

    worker.soft_stop();
    worker.wait_for_server_stop();

    if absent { State::Success } else { State::Fail }
}

#[test]
fn test_remove_cluster_drops_metric_row() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "RemoveCluster IPC drops the cluster row from the worker LocalDrain",
            try_remove_cluster_drops_metric_row,
        ),
        State::Success,
    );
}

// ══════════════════════════════════════════════════════════════════════
// Test 2: RemoveCluster + AddCluster re-arms; fresh emissions visible
// ══════════════════════════════════════════════════════════════════════
//
// Without the `Aggregator::add_cluster` hook wired into the AddCluster
// arm, the per-drain `removed_clusters` tombstone added in this PR
// follow-up would silently keep dropping every emission for the
// resurrected cluster id forever.

fn try_remove_then_add_cluster_re_arms_metrics() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let cluster_id = "lifecycle_cluster_readd";
    let backend_id = "lifecycle_back_readd";

    let mut worker = setup_worker_with_cluster(
        "METRICS-LIFECYCLE-READD",
        cluster_id,
        backend_id,
        front_address,
        back_address,
    );

    prime_with_one_request(front_address, back_address);

    // Remove, then re-add the same cluster id + frontend + backend.
    worker.send_proxy_request_type(RequestType::RemoveCluster(cluster_id.to_owned()));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(cluster_id)));
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(RequestHttpFrontend {
        ..Worker::default_http_frontend(cluster_id, front_address)
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        cluster_id,
        backend_id,
        back_address,
        None,
    )));
    worker.read_to_last();

    // Drive a fresh request — without the AddCluster re-arm of the
    // tombstone, this emission would be dropped on the floor.
    prime_with_one_request(front_address, back_address);

    let present = cluster_row_present(&mut worker, cluster_id);

    worker.soft_stop();
    worker.wait_for_server_stop();

    if present { State::Success } else { State::Fail }
}

#[test]
fn test_remove_then_add_cluster_re_arms_metrics() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "AddCluster after RemoveCluster re-arms the metrics drain (tombstone clear)",
            try_remove_then_add_cluster_re_arms_metrics,
        ),
        State::Success,
    );
}

// ══════════════════════════════════════════════════════════════════════
// Test 3: a real old HTTP session cannot decrement its same-identity replacement
// ══════════════════════════════════════════════════════════════════════

fn try_old_http_session_does_not_decrement_same_identity_replacement() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let cluster_id = "lifecycle_cluster_incarnation";
    let backend_id = "lifecycle_backend_incarnation";

    let mut worker = setup_worker_with_cluster(
        "METRICS-LIFECYCLE-INCARNATION",
        cluster_id,
        backend_id,
        front_address,
        back_address,
    );
    let metric_detail_ack = lease_backend_metric_detail(&mut worker);
    // Both incarnations deliberately use this one listener. Client 0 is the
    // old connection; client 1 is the replacement at the exact same address.
    let mut backend = SyncBackend::new(
        "METRICS-LIFECYCLE-SAME-BACKEND",
        back_address,
        http_ok_response("replacement-incarnation"),
    );
    backend.connect();

    let mut old_client = Client::new(
        "METRICS-LIFECYCLE-OLD-CLIENT",
        front_address,
        http_request("GET", "/old", "", "localhost"),
    );
    old_client.connect();
    old_client.send();
    let old_session_held = wait_for_backend_request(&mut backend, 0);

    let mut remove_ack = false;
    let mut add_cluster_ack = false;
    let mut add_frontend_ack = false;
    let mut add_backend_ack = false;
    let mut new_session_reached_backend = false;
    let mut replacement_gauge_before_old_close = None;
    let mut proxy_gauge_before_old_close = None;
    let mut proxy_gauge_after_old_close = None;
    let mut old_session_terminated = false;
    let mut new_session_progressed = false;
    let mut new_response_metric = None;
    let mut replacement_gauge_after_old_close = None;
    let mut new_client = None;

    if metric_detail_ack && old_session_held {
        remove_ack = request_acknowledged(
            &mut worker,
            RequestType::RemoveCluster(cluster_id.to_owned()),
        );
        add_cluster_ack = request_acknowledged(
            &mut worker,
            RequestType::AddCluster(Worker::default_cluster(cluster_id)),
        );
        add_frontend_ack = request_acknowledged(
            &mut worker,
            RequestType::AddHttpFrontend(RequestHttpFrontend {
                ..Worker::default_http_frontend(cluster_id, front_address)
            }),
        );
        add_backend_ack = request_acknowledged(
            &mut worker,
            RequestType::AddBackend(Worker::default_backend(
                cluster_id,
                backend_id,
                back_address,
                None,
            )),
        );

        if remove_ack && add_cluster_ack && add_frontend_ack && add_backend_ack {
            let mut current = Client::new(
                "METRICS-LIFECYCLE-NEW-CLIENT",
                front_address,
                http_request("GET", "/new", "", "localhost"),
            );
            current.connect();
            current.send();
            new_session_reached_backend = wait_for_backend_request(&mut backend, 1);
            if new_session_reached_backend {
                replacement_gauge_before_old_close =
                    backend_connection_gauge(&mut worker, cluster_id, backend_id);
                proxy_gauge_before_old_close = proxy_backend_connection_gauge(&mut worker);
            }

            // Source order in the old Mux is proxy-wide decrement first, then
            // the labelled decrement. The global 2 -> 1 witness proves this
            // close ran while the replacement connection remained active.
            let _ = backend.close(0);
            old_session_terminated = wait_for_client_termination(&mut old_client);
            if old_session_terminated
                && let Some(before) = proxy_gauge_before_old_close
                && before > 0
            {
                proxy_gauge_after_old_close =
                    wait_for_proxy_backend_connections(&mut worker, before - 1);
            }

            // Emit and consume one replacement metric only after the old-close
            // witness. This is the same LocalDrain pipeline barrier used by the
            // final incarnation-sensitive query.
            let sent = backend.send(1).is_some();
            let response = current.receive_response(SESSION_BARRIER_BUDGET);
            new_session_progressed = sent
                && response
                    .as_deref()
                    .is_some_and(|value| value.ends_with("replacement-incarnation"));
            if new_session_progressed {
                new_response_metric = wait_for_backend_2xx(&mut worker, cluster_id, backend_id);
            }
            if new_response_metric.is_some() {
                replacement_gauge_after_old_close =
                    backend_connection_gauge(&mut worker, cluster_id, backend_id);
            }
            new_client = Some(current);
        }
    }

    old_client.disconnect();
    if let Some(client) = new_client.as_mut() {
        client.disconnect();
    }
    let _ = backend.close(1);
    backend.disconnect();
    let worker_stopped = stop_worker_within(worker);

    println!(
        "incarnation metrics same identity: detail_ack={metric_detail_ack} \
         old_held={old_session_held} remove_ack={remove_ack} add_cluster_ack={add_cluster_ack} \
         add_frontend_ack={add_frontend_ack} add_backend_ack={add_backend_ack} \
         new_reached={new_session_reached_backend} \
         replacement_before={replacement_gauge_before_old_close:?} \
         proxy_before={proxy_gauge_before_old_close:?} proxy_after={proxy_gauge_after_old_close:?} \
         old_terminated={old_session_terminated} new_progressed={new_session_progressed} \
         new_response_metric={new_response_metric:?} \
         replacement_after={replacement_gauge_after_old_close:?} worker_stopped={worker_stopped}"
    );

    if !old_session_held {
        return State::Undecided;
    }
    if metric_detail_ack
        && remove_ack
        && add_cluster_ack
        && add_frontend_ack
        && add_backend_ack
        && new_session_reached_backend
        && replacement_gauge_before_old_close == Some(1)
        && proxy_gauge_before_old_close == Some(2)
        && proxy_gauge_after_old_close == Some(1)
        && old_session_terminated
        && new_session_progressed
        && new_response_metric.is_some_and(|count| count > 0)
        && replacement_gauge_after_old_close == Some(1)
        && worker_stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_old_http_session_metrics_do_not_decrement_same_identity_replacement() {
    assert_eq!(
        try_old_http_session_does_not_decrement_same_identity_replacement(),
        State::Success,
    );
}

#[test]
fn real_http_session_without_add_cluster_records_labelled_metrics() {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let cluster_id = "reviewer_undeclared_cluster";
    let backend_id = "reviewer_undeclared_backend";
    let mut worker = setup_worker_without_cluster(
        "METRICS-REVIEWER-UNDECLARED",
        cluster_id,
        backend_id,
        front_address,
        back_address,
    );
    let detail_ack = lease_backend_metric_detail(&mut worker);
    let mut backend = SyncBackend::new(
        "METRICS-REVIEWER-UNDECLARED-BACKEND",
        back_address,
        http_ok_response("undeclared-cluster"),
    );
    backend.connect();
    let mut client = Client::new(
        "METRICS-REVIEWER-UNDECLARED-CLIENT",
        front_address,
        http_request("GET", "/undeclared", "", "localhost"),
    );
    client.connect();
    client.send();

    let reached_backend = wait_for_backend_request(&mut backend, 0);
    let sent = reached_backend && backend.send(0).is_some();
    let response_ok = sent
        && client
            .receive_response(SESSION_BARRIER_BUDGET)
            .as_deref()
            .is_some_and(|response| response.ends_with("undeclared-cluster"));
    let labelled_2xx = if response_ok {
        wait_for_backend_2xx(&mut worker, cluster_id, backend_id)
    } else {
        None
    };

    client.disconnect();
    let _ = backend.close(0);
    backend.disconnect();
    let worker_stopped = stop_worker_within(worker);

    println!(
        "undeclared real caller: detail_ack={detail_ack} reached={reached_backend} sent={sent} \
         response_ok={response_ok} labelled_2xx={labelled_2xx:?} stopped={worker_stopped}"
    );
    assert!(detail_ack);
    assert!(reached_backend);
    assert!(response_ok);
    assert!(labelled_2xx.is_some_and(|count| count > 0));
    assert!(worker_stopped);
}

// Additional H1 coverage: recreate the cluster ID with a distinct backend
// identity and address, then prove late teardown cannot recreate the retired row.

fn try_old_http_session_does_not_contaminate_readded_cluster() -> State {
    let front_address = create_local_address();
    let old_back_address = create_local_address();
    let new_back_address = create_local_address();
    let cluster_id = "lifecycle_cluster_incarnation";
    let old_backend_id = "lifecycle_backend_old";
    let new_backend_id = "lifecycle_backend_new";

    let mut worker = setup_worker_with_cluster(
        "METRICS-LIFECYCLE-INCARNATION",
        cluster_id,
        old_backend_id,
        front_address,
        old_back_address,
    );
    let metric_detail_ack = lease_backend_metric_detail(&mut worker);
    let mut old_backend = SyncBackend::new(
        "METRICS-LIFECYCLE-OLD",
        old_back_address,
        http_ok_response("old-incarnation"),
    );
    let mut new_backend = SyncBackend::new(
        "METRICS-LIFECYCLE-NEW",
        new_back_address,
        http_ok_response("new-incarnation"),
    );
    old_backend.connect();
    new_backend.connect();

    // The old response is held by construction: the backend receives the
    // request but sends nothing until after the same-id replacement exists.
    let mut old_client = Client::new(
        "METRICS-LIFECYCLE-OLD-CLIENT",
        front_address,
        http_request("GET", "/old", "", "localhost"),
    );
    old_client.connect();
    old_client.send();
    let old_session_held = wait_for_backend_request(&mut old_backend, 0);

    let mut remove_ack = false;
    let mut add_cluster_ack = false;
    let mut add_frontend_ack = false;
    let mut add_backend_ack = false;
    let mut new_session_reached_backend = false;
    let mut new_gauge = None;
    let mut proxy_gauge_before_old_close = None;
    let mut proxy_gauge_after_old_close = None;
    let mut new_session_progressed = false;
    let mut new_response_metric = None;
    let mut old_session_terminated = false;
    let mut old_gauge = None;
    let mut new_client = None;

    if metric_detail_ack && old_session_held {
        remove_ack = request_acknowledged(
            &mut worker,
            RequestType::RemoveCluster(cluster_id.to_owned()),
        );
        add_cluster_ack = request_acknowledged(
            &mut worker,
            RequestType::AddCluster(Worker::default_cluster(cluster_id)),
        );
        add_frontend_ack = request_acknowledged(
            &mut worker,
            RequestType::AddHttpFrontend(RequestHttpFrontend {
                ..Worker::default_http_frontend(cluster_id, front_address)
            }),
        );
        add_backend_ack = request_acknowledged(
            &mut worker,
            RequestType::AddBackend(Worker::default_backend(
                cluster_id,
                new_backend_id,
                new_back_address,
                None,
            )),
        );

        if remove_ack && add_cluster_ack && add_frontend_ack && add_backend_ack {
            let mut current = Client::new(
                "METRICS-LIFECYCLE-NEW-CLIENT",
                front_address,
                http_request("GET", "/new", "", "localhost"),
            );
            current.connect();
            current.send();
            new_session_reached_backend = wait_for_backend_request(&mut new_backend, 0);
            if new_session_reached_backend {
                // Positive witness: the new incarnation has its own live
                // backend connection before the old one is released.
                new_gauge = backend_connection_gauge(&mut worker, cluster_id, new_backend_id);
                proxy_gauge_before_old_close = proxy_backend_connection_gauge(&mut worker);
            }

            // This is the real late-emission trigger. Closing the backend held
            // by the pre-remove Mux reaches its backend-close bookkeeping and
            // emits GaugeAdd(-1) with the old cluster/backend labels.
            let _ = old_backend.close(0);
            old_session_terminated = wait_for_client_termination(&mut old_client);
            if old_session_terminated
                && let Some(before) = proxy_gauge_before_old_close
                && before > 0
            {
                proxy_gauge_after_old_close =
                    wait_for_proxy_backend_connections(&mut worker, before - 1);
            }

            // Phase two supplies a metric whose source is causally later than
            // the old connection's labelled decrement: the new backend stays
            // held until the proxy-gauge terminal witness above has returned.
            let sent = new_backend.send(0).is_some();
            let response = current.receive_response(SESSION_BARRIER_BUDGET);
            new_session_progressed = sent
                && response
                    .as_deref()
                    .is_some_and(|value| value.ends_with("new-incarnation"));
            if new_session_progressed {
                new_response_metric = wait_for_backend_2xx(&mut worker, cluster_id, new_backend_id);
            }
            if new_response_metric.is_some() {
                // Only evaluate incarnation isolation after the later 2xx
                // metric has traversed the same synchronous LocalDrain.
                old_gauge = backend_connection_gauge(&mut worker, cluster_id, old_backend_id);
            }
            new_client = Some(current);
        }
    }

    old_client.disconnect();
    if let Some(client) = new_client.as_mut() {
        client.disconnect();
    }
    let _ = new_backend.close(0);
    old_backend.disconnect();
    new_backend.disconnect();
    let worker_stopped = stop_worker_within(worker);

    println!(
        "incarnation metrics: detail_ack={metric_detail_ack} old_held={old_session_held} \
         remove_ack={remove_ack} \
         add_cluster_ack={add_cluster_ack} add_frontend_ack={add_frontend_ack} \
         add_backend_ack={add_backend_ack} new_reached={new_session_reached_backend} \
         new_gauge={new_gauge:?} proxy_before={proxy_gauge_before_old_close:?} \
         proxy_after={proxy_gauge_after_old_close:?} new_progressed={new_session_progressed} \
         new_response_metric={new_response_metric:?} \
         old_terminated={old_session_terminated} old_gauge={old_gauge:?} \
         worker_stopped={worker_stopped}"
    );

    if !old_session_held {
        return State::Undecided;
    }
    if metric_detail_ack
        && remove_ack
        && add_cluster_ack
        && add_frontend_ack
        && add_backend_ack
        && new_session_reached_backend
        && new_gauge == Some(1)
        && proxy_gauge_before_old_close == Some(2)
        && proxy_gauge_after_old_close == Some(1)
        && new_session_progressed
        && new_response_metric.is_some_and(|count| count > 0)
        && old_session_terminated
        && old_gauge.is_none()
        && worker_stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_old_http_session_metrics_do_not_contaminate_readded_cluster() {
    assert_eq!(
        try_old_http_session_does_not_contaminate_readded_cluster(),
        State::Success,
    );
}

/// Hold stream 1 on a real TLS/ALPN H2 connection, replace its cluster with
/// the same cluster/backend/address, then complete stream 3 on that same H2
/// connection before releasing stream 1. The replacement backend gauge must
/// stay at one when the old stream terminates; the proxy gauge proves the old
/// close reached the metrics pipeline and the stream-3 2xx is its ordered
/// same-pipeline witness.
fn try_old_h2_stream_does_not_decrement_same_identity_replacement() -> State {
    const CLUSTER_ID: &str = "cluster_0";
    const BACKEND_ID: &str = "cluster_0-0";
    const OLD_STREAM_ID: u32 = 1;
    const NEW_STREAM_ID: u32 = 3;

    let back_address = create_local_address();
    let (mut worker, front_port, front_address) =
        setup_h2_listener_only("METRICS-LIFECYCLE-H2-INCARNATION");
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        CLUSTER_ID,
        BACKEND_ID,
        back_address,
        None,
    )));
    worker.read_to_last();
    let metric_detail_ack = lease_backend_metric_detail(&mut worker);

    let mut backend = SyncBackend::new(
        "METRICS-LIFECYCLE-H2-SAME-BACKEND",
        back_address,
        http_ok_response("replacement-incarnation"),
    );
    backend.connect();

    let front_socket: SocketAddr = format!("127.0.0.1:{front_port}")
        .parse()
        .expect("reserved H2 listener address must parse");
    let mut tls = raw_h2_connection(front_socket);
    h2_handshake(&mut tls);
    tls.write_all(&H2Frame::headers(OLD_STREAM_ID, h2_get_headers(), true, true).encode())
        .expect("old H2 request must be written");
    tls.flush().expect("old H2 request must be flushed");
    let old_stream_held = wait_for_backend_request(&mut backend, 0);

    let mut remove_ack = false;
    let mut add_cluster_ack = false;
    let mut add_frontend_ack = false;
    let mut add_backend_ack = false;
    let mut new_stream_reached_backend = false;
    let mut replacement_before_old_close = None;
    let mut proxy_before_old_close = None;
    let mut new_stream_observation = H2StreamObservation::default();
    let mut proxy_after_old_close = None;
    let mut replacement_after_old_close = None;
    let mut old_stream_observation = H2StreamObservation::default();
    let mut new_response_metric = None;
    let mut proxy_after_all_close = None;
    let mut replacement_after_all_close = None;

    if metric_detail_ack && old_stream_held {
        remove_ack = request_acknowledged(
            &mut worker,
            RequestType::RemoveCluster(CLUSTER_ID.to_owned()),
        );
        add_cluster_ack = request_acknowledged(
            &mut worker,
            RequestType::AddCluster(Worker::default_cluster(CLUSTER_ID)),
        );
        add_frontend_ack = request_acknowledged(
            &mut worker,
            RequestType::AddHttpsFrontend(RequestHttpFrontend {
                hostname: "localhost".to_owned(),
                ..Worker::default_http_frontend(CLUSTER_ID, front_address.into())
            }),
        );
        add_backend_ack = request_acknowledged(
            &mut worker,
            RequestType::AddBackend(Worker::default_backend(
                CLUSTER_ID,
                BACKEND_ID,
                back_address,
                None,
            )),
        );

        if remove_ack && add_cluster_ack && add_frontend_ack && add_backend_ack {
            tls.write_all(&H2Frame::headers(NEW_STREAM_ID, h2_get_headers(), true, true).encode())
                .expect("replacement H2 request must be written");
            tls.flush().expect("replacement H2 request must be flushed");
            new_stream_reached_backend = wait_for_backend_request(&mut backend, 1);
            if new_stream_reached_backend {
                replacement_before_old_close =
                    backend_connection_gauge(&mut worker, CLUSTER_ID, BACKEND_ID);
                proxy_before_old_close = proxy_backend_connection_gauge(&mut worker);

                let _ = backend.send(1);
                new_stream_observation =
                    wait_for_h2_stream_terminal(&mut tls, NEW_STREAM_ID, Some(OLD_STREAM_ID));
                if new_stream_observation.status_200
                    && new_stream_observation.end_stream
                    && !new_stream_observation.reset
                {
                    new_response_metric = wait_for_backend_2xx(&mut worker, CLUSTER_ID, BACKEND_ID);
                }

                let _ = backend.close(0);
                old_stream_observation = wait_for_h2_stream_terminal(&mut tls, OLD_STREAM_ID, None);
                if let Some(before) = proxy_before_old_close
                    && before > 0
                {
                    proxy_after_old_close =
                        wait_for_proxy_backend_connections(&mut worker, before - 1);
                }
                replacement_after_old_close =
                    backend_connection_gauge(&mut worker, CLUSTER_ID, BACKEND_ID);

                let _ = backend.close(1);
                proxy_after_all_close = wait_for_proxy_backend_connections(&mut worker, 0);
                replacement_after_all_close =
                    backend_connection_gauge(&mut worker, CLUSTER_ID, BACKEND_ID);
            }
        }
    }

    drop(tls);
    backend.disconnect();
    let worker_stopped = stop_worker_within(worker);

    println!(
        "H2 incarnation metrics same identity: detail_ack={metric_detail_ack} \
         old_held={old_stream_held} remove_ack={remove_ack} add_cluster_ack={add_cluster_ack} \
         add_frontend_ack={add_frontend_ack} add_backend_ack={add_backend_ack} \
         new_reached={new_stream_reached_backend} replacement_before={replacement_before_old_close:?} \
         proxy_before={proxy_before_old_close:?} new_stream={new_stream_observation:?} \
         new_response_metric={new_response_metric:?} old_stream={old_stream_observation:?} \
         proxy_after_old={proxy_after_old_close:?} replacement_after_old={replacement_after_old_close:?} \
         proxy_after_all={proxy_after_all_close:?} replacement_after_all={replacement_after_all_close:?} \
         worker_stopped={worker_stopped}"
    );

    if !old_stream_held {
        return State::Undecided;
    }
    if metric_detail_ack
        && remove_ack
        && add_cluster_ack
        && add_frontend_ack
        && add_backend_ack
        && new_stream_reached_backend
        && replacement_before_old_close == Some(1)
        && proxy_before_old_close == Some(2)
        && new_stream_observation.status_200
        && new_stream_observation.end_stream
        && !new_stream_observation.reset
        && !new_stream_observation.watched_stream_terminated
        && new_response_metric.is_some_and(|count| count > 0)
        && (old_stream_observation.end_stream || old_stream_observation.reset)
        && proxy_after_old_close == Some(1)
        && replacement_after_old_close == Some(1)
        && proxy_after_all_close == Some(0)
        && replacement_after_all_close == Some(0)
        && worker_stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_old_h2_stream_metrics_do_not_decrement_same_identity_replacement() {
    assert_eq!(
        try_old_h2_stream_does_not_decrement_same_identity_replacement(),
        State::Success,
    );
}

/// Exercise Sōzu's real HTTP/1 status-101 promotion into `Pipe`, exchange
/// structurally valid framed text in both directions, and keep the replacement
/// pipe live while the old incarnation closes. The fixture response omits
/// `Sec-WebSocket-Accept`, matching the existing mux fixtures, so this covers
/// promotion and pipe accounting rather than a complete RFC 6455 handshake.
#[test]
fn old_websocket_metrics_do_not_contaminate_same_identity_replacement() {
    const UPGRADE_RESPONSE: &str =
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
    const UPGRADE_REQUEST: &str = "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";

    let front_address = create_local_address();
    let back_address = create_local_address();
    let cluster_id = "lifecycle_websocket_incarnation";
    let backend_id = "lifecycle_websocket_backend";
    let mut worker = setup_worker_with_cluster(
        "METRICS-LIFECYCLE-WEBSOCKET-INCARNATION",
        cluster_id,
        backend_id,
        front_address,
        back_address,
    );
    let metric_detail_ack = lease_backend_metric_detail(&mut worker);
    let mut backend = SyncBackend::new(
        "METRICS-LIFECYCLE-WEBSOCKET-BACKEND",
        back_address,
        UPGRADE_RESPONSE,
    );
    backend.connect();

    let mut old_client = Client::new(
        "METRICS-LIFECYCLE-OLD-WEBSOCKET",
        front_address,
        UPGRADE_REQUEST,
    );
    old_client.connect();
    old_client.send();
    let old_handshake_reached = wait_for_backend_request(&mut backend, 0);
    let old_upgraded = old_handshake_reached
        && backend.send(0).is_some()
        && wait_for_client_payload(&mut old_client, "101 Switching Protocols");
    let old_client_to_backend = old_upgraded
        && send_client_websocket_text(&mut old_client, "old-websocket-client-witness")
        && wait_for_backend_payload(&mut backend, 0, "old-websocket-client-witness");
    let old_pipe_held = old_client_to_backend
        && send_backend_websocket_text(&mut backend, 0, "old-websocket-backend-witness")
        && wait_for_client_payload(&mut old_client, "old-websocket-backend-witness");

    let mut remove_ack = false;
    let mut add_cluster_ack = false;
    let mut add_frontend_ack = false;
    let mut add_backend_ack = false;
    let mut new_pipe_exchanged = false;
    let mut access_logs_before_old_close = None;
    let mut access_logs_after_old_close = None;
    let mut access_logs_after_new_close = None;
    let mut ws_before_old_close = None;
    let mut ws_after_old_close = None;
    let mut ws_after_new_close = None;
    let mut old_terminated = false;
    let mut new_terminated = false;
    let mut new_client = None;

    if metric_detail_ack && old_pipe_held {
        remove_ack = request_acknowledged(
            &mut worker,
            RequestType::RemoveCluster(cluster_id.to_owned()),
        );
        add_cluster_ack = request_acknowledged(
            &mut worker,
            RequestType::AddCluster(Worker::default_cluster(cluster_id)),
        );
        add_frontend_ack = request_acknowledged(
            &mut worker,
            RequestType::AddHttpFrontend(RequestHttpFrontend {
                ..Worker::default_http_frontend(cluster_id, front_address)
            }),
        );
        add_backend_ack = request_acknowledged(
            &mut worker,
            RequestType::AddBackend(Worker::default_backend(
                cluster_id,
                backend_id,
                back_address,
                None,
            )),
        );

        if remove_ack && add_cluster_ack && add_frontend_ack && add_backend_ack {
            backend.set_response(UPGRADE_RESPONSE);
            let mut current = Client::new(
                "METRICS-LIFECYCLE-NEW-WEBSOCKET",
                front_address,
                UPGRADE_REQUEST,
            );
            current.connect();
            current.send();
            let new_handshake_reached = wait_for_backend_request(&mut backend, 1);
            let new_upgraded = new_handshake_reached
                && backend.send(1).is_some()
                && wait_for_client_payload(&mut current, "101 Switching Protocols");
            let new_client_to_backend = new_upgraded
                && send_client_websocket_text(&mut current, "new-websocket-client-witness")
                && wait_for_backend_payload(&mut backend, 1, "new-websocket-client-witness");
            new_pipe_exchanged = new_client_to_backend
                && send_backend_websocket_text(&mut backend, 1, "new-websocket-backend-witness")
                && wait_for_client_payload(&mut current, "new-websocket-backend-witness");

            if new_pipe_exchanged {
                access_logs_before_old_close =
                    wait_for_backend_access_logs(&mut worker, cluster_id, backend_id, 1);
                ws_before_old_close = wait_for_proxy_gauge(&mut worker, "protocol.ws", 2);
            }

            let _ = backend.close(0);
            old_terminated = wait_for_client_termination(&mut old_client);
            if old_terminated {
                ws_after_old_close = wait_for_proxy_gauge(&mut worker, "protocol.ws", 1);
                access_logs_after_old_close =
                    wait_for_backend_access_logs(&mut worker, cluster_id, backend_id, 1);
            }

            let _ = backend.close(1);
            new_terminated = wait_for_client_termination(&mut current);
            if new_terminated {
                ws_after_new_close = wait_for_proxy_gauge(&mut worker, "protocol.ws", 0);
                access_logs_after_new_close =
                    wait_for_backend_access_logs(&mut worker, cluster_id, backend_id, 2);
            }
            new_client = Some(current);
        }
    }

    old_client.disconnect();
    if let Some(client) = new_client.as_mut() {
        client.disconnect();
    }
    backend.disconnect();
    let worker_stopped = stop_worker_within(worker);

    println!(
        "websocket metrics same identity: detail_ack={metric_detail_ack} \
         old_handshake={old_handshake_reached} old_pipe_held={old_pipe_held} \
         remove_ack={remove_ack} add_cluster_ack={add_cluster_ack} \
         add_frontend_ack={add_frontend_ack} add_backend_ack={add_backend_ack} \
         new_exchanged={new_pipe_exchanged} access_before={access_logs_before_old_close:?} \
         ws_before={ws_before_old_close:?} old_terminated={old_terminated} \
         access_after_old={access_logs_after_old_close:?} ws_after_old={ws_after_old_close:?} \
         new_terminated={new_terminated} access_after_new={access_logs_after_new_close:?} \
         ws_after_new={ws_after_new_close:?} worker_stopped={worker_stopped}"
    );

    assert!(metric_detail_ack);
    assert!(old_pipe_held);
    assert!(remove_ack && add_cluster_ack && add_frontend_ack && add_backend_ack);
    assert!(new_pipe_exchanged);
    assert_eq!(access_logs_before_old_close, Some(1));
    assert_eq!(ws_before_old_close, Some(2));
    assert!(old_terminated);
    assert_eq!(access_logs_after_old_close, Some(1));
    assert_eq!(ws_after_old_close, Some(1));
    assert!(new_terminated);
    assert_eq!(access_logs_after_new_close, Some(2));
    assert_eq!(ws_after_new_close, Some(0));
    assert!(worker_stopped);
}

// ══════════════════════════════════════════════════════════════════════
// Test 4: RemoveBackend drops the backend row but keeps the cluster row
// ══════════════════════════════════════════════════════════════════════

fn try_remove_backend_keeps_cluster_row_when_others_remain() -> State {
    let front_address = create_local_address();
    let back_address_a = create_local_address();
    let back_address_b = create_local_address();
    let cluster_id = "lifecycle_cluster_two_backends";
    let backend_a = "lifecycle_back_a";
    let backend_b = "lifecycle_back_b";

    let mut worker = setup_worker_with_cluster(
        "METRICS-LIFECYCLE-BACKEND",
        cluster_id,
        backend_a,
        front_address,
        back_address_a,
    );
    // Second backend on the same cluster.
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        cluster_id,
        backend_b,
        back_address_b,
        None,
    )));
    worker.read_to_last();

    prime_with_one_request(front_address, back_address_a);
    // Run a second request to give backend_b a chance at the load-balancer.
    // Either of the two backends may have served, so we just need one of
    // them to have recorded samples.
    prime_with_one_request(front_address, back_address_b);

    // Remove backend A only.
    worker.send_proxy_request_type(RequestType::RemoveBackend(RemoveBackend {
        cluster_id: cluster_id.to_owned(),
        backend_id: backend_a.to_owned(),
        address: back_address_a.into(),
    }));
    worker.read_to_last();

    let present = cluster_row_present(&mut worker, cluster_id);

    worker.soft_stop();
    worker.wait_for_server_stop();

    // Cluster row must remain because the cluster still exists (only
    // one backend was removed).
    if present { State::Success } else { State::Fail }
}

#[test]
fn test_remove_backend_keeps_cluster_row_when_others_remain() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "RemoveBackend drops per-backend row but keeps cluster row when others remain",
            try_remove_backend_keeps_cluster_row_when_others_remain,
        ),
        State::Success,
    );
}

// ══════════════════════════════════════════════════════════════════════
// Test 5: an abandoned metric-detail lease expires with no later command
// ══════════════════════════════════════════════════════════════════════
//
// Contract (sozu-proxy/sozu#1831): once its owner goes silent, a short
// metric-detail lease expires within its TTL plus the five-second janitor
// cadence without another worker command. The refuting observation is a real
// worker that continues serving data-plane traffic but emits no passive
// `lease_tick_expired` transition before that bound.

fn try_abandoned_lease_expires_without_a_command() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let cluster_id = "lifecycle_cluster_lease";

    let mut worker = setup_worker_with_cluster(
        "METRICS-LIFECYCLE-LEASE",
        cluster_id,
        "lifecycle_back_lease",
        front_address,
        back_address,
    );

    let mut backend = SyncBackend::new(
        "metrics_lifecycle_lease_backend",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();

    let ttl = Duration::from_secs(1);
    worker.send_proxy_request_type(RequestType::SetMetricDetail(SetMetricDetail {
        client_id: "top:1831:abandoned".to_owned(),
        detail: Some(MetricDetail::DetailBackend as i32),
        ttl_seconds: Some(ttl.as_secs() as u32),
        clear: None,
        reason: None,
        peer_pid: None,
        peer_session_ulid: None,
    }));
    let mut lease_applied = false;
    loop {
        let Some(response) = worker.read_proxy_response() else {
            break;
        };
        let terminal = response.id == worker.command_id.last;
        if let Some(ContentType::WorkerMetricDetailStatus(status)) =
            response.content.and_then(|content| content.content_type)
        {
            lease_applied = status.effective == MetricDetail::DetailBackend as i32
                && status.active_lease_count == 1;
        }
        if terminal {
            break;
        }
    }

    // Reading the worker channel is passive. From this point until the
    // observation deadline, only HTTP traffic enters the worker; no command can
    // accidentally invoke `Server::notify` and become the janitor trigger.
    let deadline = Instant::now() + ttl + Duration::from_secs(5);
    let observation_started = Instant::now();
    let mut expired = None;
    let mut traffic_responses = 0usize;
    while expired.is_none() && Instant::now() < deadline {
        if serve_one_request(&mut backend, front_address) {
            traffic_responses += 1;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(response) = worker
            .command_channel
            .read_message_blocking_timeout(Some(remaining.min(Duration::from_millis(500))))
        else {
            continue;
        };
        if let Some(ContentType::Event(event)) = response.content.and_then(|c| c.content_type)
            && let Some(transition) = event.metric_detail
            && transition.transition_kind == "lease_tick_expired"
        {
            expired = Some(transition);
        }
    }

    // Cleanup begins only after the passive observation is complete. SoftStop
    // is intentionally the first later worker command and cannot retroactively
    // satisfy the oracle above.
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    println!(
        "metric lease evidence: lease_applied={lease_applied}, \
         traffic_responses={traffic_responses}, expired={expired:?}, \
         observation_elapsed={:?}, worker_stopped={stopped}",
        observation_started.elapsed(),
    );

    match expired {
        Some(transition)
            if lease_applied
                && traffic_responses > 0
                && stopped
                && transition.previous_effective == MetricDetail::DetailBackend as i32
                && transition.effective == MetricDetail::DetailCluster as i32 =>
        {
            State::Success
        }
        _ => State::Fail,
    }
}

#[test]
fn test_abandoned_lease_expires_without_a_command() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "an abandoned metric-detail lease expires within TTL + five seconds with no later \
             worker command",
            try_abandoned_lease_expires_without_a_command,
        ),
        State::Success,
    );
}
