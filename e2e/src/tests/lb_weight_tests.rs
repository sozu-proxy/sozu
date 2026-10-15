//! End-to-end test for `weight` under `ROUND_ROBIN` (sozu-proxy/sozu#676).
//!
//! A `ROUND_ROBIN` cluster whose backends carry unequal weights selects by
//! smooth weighted round robin: over every `Σw` selections each backend is
//! picked exactly `w` times, interleaved. The sequence is deterministic for
//! one worker, so the test asserts it exactly, never a statistical spread.
//! Each request opens its own frontend connection, so each one reaches
//! backend selection.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddBackend, Cluster, ListenerType, LoadBalancingAlgorithms,
        LoadBalancingParams, SocketAddress, request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        https_client::{build_https_client_from, resolve_request},
    },
    sozu::worker::Worker,
    tests::{State, provide_port, repeat_until_error_or, tests::create_unbound_local_address},
};

const CLUSTER: &str = "cluster_0";
const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Weights `3` and `1`: smooth weighted round robin picks `0 0 1 0`, then
/// again.
///
/// TO SEE THIS RED: drop the `smooth_weighted` branch of
/// `RoundRobin::next_available_backend` (`lib/src/load_balancing.rs`); the
/// cursor then alternates `0 1 0 1`.
fn try_round_robin_honors_weight() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_http_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned("LB-WEIGHT", config, listeners, state);
    worker.send_proxy_request_type(RequestType::AddHttpListener(
        ListenerBuilder::new_http(front_address.clone())
            .to_http(None)
            .expect("test http listener config must build"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.clone(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        load_balancing: LoadBalancingAlgorithms::RoundRobin as i32,
        ..Worker::default_cluster(CLUSTER)
    }));
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(Worker::default_http_frontend(
        CLUSTER,
        front_address.into(),
    )));

    let mut backends = Vec::new();
    for (index, weight) in [3, 1].into_iter().enumerate() {
        let address: SocketAddr = create_unbound_local_address();
        worker.send_proxy_request_type(RequestType::AddBackend(AddBackend {
            load_balancing_parameters: Some(LoadBalancingParams { weight }),
            ..Worker::default_backend(CLUSTER, format!("{CLUSTER}-{index}"), address, None)
        }));
        backends.push(AsyncBackend::spawn_detached_backend(
            format!("BACKEND_{index}"),
            address,
            SimpleAggregator::default(),
            AsyncBackend::http_handler(format!("pong{index}")),
        ));
    }
    worker.read_to_last();

    let uri: hyper::Uri = format!("http://localhost:{front_port}/weight")
        .parse()
        .expect("the test uri must parse");
    let reached: Vec<String> = (0..8)
        .map(|_| {
            resolve_request(&build_https_client_from(CLIENT), uri.clone())
                .map(|(_, body)| body)
                .unwrap_or_default()
        })
        .collect();
    let expected: Vec<String> = [0, 0, 1, 0, 0, 0, 1, 0]
        .iter()
        .map(|index| format!("pong{index}"))
        .collect();
    let weighted = reached == expected;
    if !weighted {
        println!("expected {expected:?}, reached {reached:?}");
    }

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    for mut backend in backends {
        backend.stop_and_get_aggregator();
    }
    if weighted && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_round_robin_honors_weight() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "ROUND_ROBIN: weights 3 and 1 select 0 0 1 0",
            try_round_robin_honors_weight,
        ),
        State::Success
    );
}
