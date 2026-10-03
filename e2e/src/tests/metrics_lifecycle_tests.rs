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
    io::{ErrorKind, Read},
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
    tests::{State, repeat_until_error_or},
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

/// Send one `Connection: close` request through the worker and let
/// `backend` (already bound) answer it.
fn serve_one_request(backend: &mut SyncBackend, front_address: SocketAddr) {
    let mut client = crate::mock::client::Client::new(
        "metrics_lifecycle_client",
        front_address,
        "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_owned(),
    );
    client.connect();
    client.send();

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if backend.accept(0) {
            backend.receive(0);
            backend.send(0);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = client.receive();
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
// sozu-proxy/sozu#1831: the lease janitor (`Aggregator::lease_tick`) only
// ran at the top of `Server::notify`, so an expired lease outlived its TTL
// until the next worker command arrived. A `sozu top` that crashes leaves
// exactly that situation: no renewal, no clear, no further request. The
// owner here applies a one-second lease and then goes silent; only data
// plane traffic flows, and the expiry must surface on its own as the
// worker-pushed `lease_tick_expired` event. Reading the channel is passive,
// so the observation cannot be what triggers the cleanup.

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
    worker.read_to_last();

    // The bound the janitor promises: TTL plus its five-second cadence, plus
    // one second for the event loop's poll timeout and a little scheduling
    // slack. Nothing is written on the command channel from here on.
    let deadline = Instant::now() + ttl + Duration::from_secs(8);
    let mut expired = None;
    while expired.is_none() && Instant::now() < deadline {
        serve_one_request(&mut backend, front_address);
        let Ok(response) = worker
            .command_channel
            .read_message_blocking_timeout(Some(Duration::from_millis(500)))
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

    worker.soft_stop();
    worker.wait_for_server_stop();

    match expired {
        Some(transition)
            if transition.previous_effective == MetricDetail::DetailBackend as i32
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
            "an abandoned metric-detail lease expires within TTL + the janitor cadence \
             with no later worker command",
            try_abandoned_lease_expires_without_a_command,
        ),
        State::Success,
    );
}
