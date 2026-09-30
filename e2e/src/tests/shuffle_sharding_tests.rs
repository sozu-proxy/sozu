//! End-to-end tests for shuffle sharding over the HRW ranking
//! (sozu-proxy/sozu#524).
//!
//! A sharded cluster restricts each client to a shard: the top `k` of the
//! rendezvous ranking of its affinity key over the cluster's primary
//! backends. These tests shard a `ROUND_ROBIN` cluster of four backends at
//! `shard_percent = 50` from `shard_min_backends = 4`, so every client has a
//! shard of two, and PREDICT that shard with the library's own
//! `hrw_score`: the backend ports change between runs, so the assertions are
//! about the predicted members, never a statistical spread.
//!
//! The client is 127.0.0.1 without any PROXY header, so its key is
//! `affinity_key_from_ip(127.0.0.1)`. Each request opens its own frontend
//! connection, so each one reaches backend selection.

use std::{
    cell::RefCell,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    rc::Rc,
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, Cluster, ListenerType, LoadBalancingAlgorithms, LoadBalancingParams,
        ShardMode, SocketAddress, request::RequestType,
    },
};
use sozu_lib::{
    backends::Backend,
    load_balancing::{affinity_key_from_ip, hrw_score},
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
const BACKENDS: usize = 4;
const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// The positions of `client`'s shard of two among `backends`, predicted as
/// the worker computes it: the two best HRW scores of its key.
fn predicted_shard(backends: &[SocketAddr], client: IpAddr) -> Vec<usize> {
    let key = affinity_key_from_ip(client);
    let mut ranked: Vec<(f64, usize)> = backends
        .iter()
        .enumerate()
        .map(|(index, address)| {
            let backend = Rc::new(RefCell::new(Backend::new(
                &format!("{CLUSTER}-{index}"),
                *address,
                None,
                // `Worker::default_backend` registers exactly these parameters.
                Some(LoadBalancingParams::default()),
                None,
            )));
            (hrw_score(key, &backend.borrow()), index)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut shard: Vec<usize> = ranked.iter().take(2).map(|&(_, index)| index).collect();
    shard.sort_unstable();
    shard
}

/// A worker with an HTTP listener and a sharded `ROUND_ROBIN` cluster of
/// [`BACKENDS`] backends in `mode`. Only the backends `serving` names get a
/// listener (answering `pong{index}`); the others refuse connections, as a
/// backend that is down does. Returns the worker, the running backends, the
/// client's predicted shard and the frontend port.
fn setup(
    name: &str,
    mode: ShardMode,
    serving: impl Fn(usize, &[usize]) -> bool,
) -> (Worker, Vec<AsyncBackend<SimpleAggregator>>, Vec<usize>, u16) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_http_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);
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
        shard_percent: Some(50),
        shard_min_backends: Some(BACKENDS as u32),
        shard_mode: Some(mode as i32),
        ..Worker::default_cluster(CLUSTER)
    }));
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(Worker::default_http_frontend(
        CLUSTER,
        front_address.into(),
    )));

    // Unbound addresses, so a backend given no listener refuses connections
    // instead of queueing them on a port-registry reservation.
    let addresses: Vec<SocketAddr> = (0..BACKENDS)
        .map(|_| create_unbound_local_address())
        .collect();
    let shard = predicted_shard(&addresses, CLIENT);
    let mut backends = Vec::new();
    for (index, address) in addresses.iter().enumerate() {
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            CLUSTER,
            format!("{CLUSTER}-{index}"),
            *address,
            None,
        )));
        if serving(index, &shard) {
            backends.push(AsyncBackend::spawn_detached_backend(
                format!("BACKEND_{index}"),
                *address,
                SimpleAggregator::default(),
                AsyncBackend::http_handler(format!("pong{index}")),
            ));
        }
    }
    worker.read_to_last();
    (worker, backends, shard, front_port)
}

/// One request from [`CLIENT`] on a connection of its own.
fn request(front_port: u16) -> Option<(hyper::StatusCode, String)> {
    let uri: hyper::Uri = format!("http://localhost:{front_port}/shard")
        .parse()
        .expect("the test uri must parse");
    resolve_request(&build_https_client_from(CLIENT), uri)
}

fn stop(mut worker: Worker, backends: Vec<AsyncBackend<SimpleAggregator>>) -> bool {
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    for mut backend in backends {
        backend.stop_and_get_aggregator();
    }
    stopped
}

/// A healthy shard serves every request, and nothing outside it does.
///
/// TO SEE THIS RED: return `false` from `BackendList::compute_shard`
/// (`lib/src/backends.rs`); round-robin then visits all four backends.
fn try_a_client_stays_in_its_shard() -> State {
    let (worker, backends, shard, front_port) = setup("SHARD-IN", ShardMode::Fallback, |_, _| true);
    let reached: Vec<String> = (0..8)
        .map(|_| {
            request(front_port)
                .map(|(_, body)| body)
                .unwrap_or_default()
        })
        .collect();
    let members: Vec<String> = shard.iter().map(|index| format!("pong{index}")).collect();
    let inside = reached.iter().all(|body| members.contains(body));
    if !inside {
        println!("shard {members:?}, reached {reached:?}");
    }
    let stopped = stop(worker, backends);
    if inside && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_a_client_stays_in_its_shard() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "shuffle sharding: requests stay in the client's shard",
            try_a_client_stays_in_its_shard,
        ),
        State::Success
    );
}

/// `FALLBACK`: with both members of the client's shard down, the request
/// spills over to a backend outside the shard and succeeds. The two failed
/// connects consume two of the request's three connection attempts; the
/// third leaves the exhausted shard.
///
/// TO SEE THIS RED: make `FALLBACK` refuse like `STRICT` in
/// `BackendList::select_with_key`; the request is then answered 503.
fn try_an_exhausted_shard_spills_over_in_fallback() -> State {
    let (worker, backends, shard, front_port) =
        setup("SHARD-FALLBACK", ShardMode::Fallback, |index, shard| {
            !shard.contains(&index)
        });
    let answer = request(front_port);
    let spilled = matches!(&answer, Some((status, body))
        if status.as_u16() == 200
            && body.strip_prefix("pong")
                .and_then(|index| index.parse::<usize>().ok())
                .is_some_and(|index| !shard.contains(&index)));
    if !spilled {
        println!("shard {shard:?} down, fallback answered {answer:?}");
    }
    let stopped = stop(worker, backends);
    if spilled && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_an_exhausted_shard_spills_over_in_fallback() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "shuffle sharding FALLBACK: an exhausted shard spills over",
            try_an_exhausted_shard_spills_over_in_fallback,
        ),
        State::Success
    );
}

/// `STRICT`: with both members of the client's shard down, the request is
/// answered 503 although the two other backends are up.
///
/// TO SEE THIS RED: make `STRICT` spill over like `FALLBACK` in
/// `BackendList::select_with_key`; the request then succeeds outside the
/// shard.
fn try_an_exhausted_shard_answers_503_in_strict() -> State {
    let (worker, backends, shard, front_port) =
        setup("SHARD-STRICT", ShardMode::Strict, |index, shard| {
            !shard.contains(&index)
        });
    let answer = request(front_port);
    let refused = matches!(&answer, Some((status, _)) if status.as_u16() == 503);
    if !refused {
        println!("shard {shard:?} down, strict answered {answer:?}");
    }
    let stopped = stop(worker, backends);
    if refused && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_an_exhausted_shard_answers_503_in_strict() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "shuffle sharding STRICT: an exhausted shard answers 503",
            try_an_exhausted_shard_answers_503_in_strict,
        ),
        State::Success
    );
}

/// Re-sending `AddCluster` without `shard_percent` turns sharding off, from
/// the next selection: the same client whose `STRICT` shard is down is
/// refused while the cluster shards, and served by a backend outside its
/// former shard once the cluster is re-added unsharded.
///
/// TO SEE THIS RED: in `Server::add_cluster` (`lib/src/server.rs`), call
/// `set_shuffle_sharding_for_cluster` only when the cluster carries
/// `shard_percent`; the re-added cluster then keeps its old shard and the
/// second request is refused too.
fn try_re_adding_a_cluster_without_shard_percent_turns_sharding_off() -> State {
    let (mut worker, backends, shard, front_port) =
        setup("SHARD-OFF", ShardMode::Strict, |index, shard| {
            !shard.contains(&index)
        });
    let sharded = request(front_port);
    // Re-added under `HRW`, so the worker still derives the client's key:
    // only the absence of `shard_percent` can turn the shard off, not a
    // missing key.
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        load_balancing: LoadBalancingAlgorithms::Hrw as i32,
        ..Worker::default_cluster(CLUSTER)
    }));
    worker.read_to_last();
    let unsharded = request(front_port);
    let refused_then_served = matches!(&sharded, Some((status, _)) if status.as_u16() == 503)
        && matches!(&unsharded, Some((status, body))
            if status.as_u16() == 200
                && body.strip_prefix("pong")
                    .and_then(|index| index.parse::<usize>().ok())
                    .is_some_and(|index| !shard.contains(&index)));
    if !refused_then_served {
        println!("shard {shard:?} down: sharded {sharded:?}, then unsharded {unsharded:?}");
    }
    let stopped = stop(worker, backends);
    if refused_then_served && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_re_adding_a_cluster_without_shard_percent_turns_sharding_off() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "shuffle sharding: AddCluster without shard_percent turns it off",
            try_re_adding_a_cluster_without_shard_percent_turns_sharding_off,
        ),
        State::Success
    );
}
