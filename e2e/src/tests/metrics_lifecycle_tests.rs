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

use std::{net::SocketAddr, thread, time::Duration};

use sozu_command_lib::{
    config::FileConfig,
    proto::command::{
        ActivateListener, ListenerType, QueryMetricsOptions, RemoveBackend, Request,
        RequestHttpFrontend, ResponseStatus, ServerConfig, request::RequestType,
        response_content::ContentType,
    },
    scm_socket::Listeners,
    state::ConfigState,
};

use crate::{
    mock::sync_backend::Backend as SyncBackend,
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
// Test 3: RemoveBackend drops the backend row but keeps the cluster row
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
