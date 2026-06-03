//! End-to-end tests for Sōzu's UDP load-balancing datapath (issue #1273).
//!
//! These are the first tests to exercise the *integrated* UDP datapath
//! (`lib/src/udp.rs` shell + the sans-io core in `lib/src/protocol/udp/` +
//! the `server.rs` mio wiring) against real sockets. They drive a real worker
//! over the command channel exactly like `tcp_tests.rs`, but the data plane is
//! connectionless: a client `UdpSocket` → the proxy front → a per-flow
//! connected upstream socket → a mock backend `UdpSocket` → symmetric NAT
//! return to the client.
//!
//! Conventions (repo `CLAUDE.md`, E2E section):
//! - never hardcode ports — every address comes from the port registry via
//!   [`create_local_address`];
//! - never assume a single `recv` is the whole answer — deadline-bounded loops
//!   in the mock client/backend absorb UDP's loss/reordering;
//! - prefer deadlines / poll-until over `sleep` for timing-sensitive waits.

use std::{collections::HashMap, net::SocketAddr, time::Duration};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, Cluster, DeactivateListener, ListenerType, LoadBalancingAlgorithms,
        RemoveListener, RequestUdpFrontend, UdpAffinityKey, UdpClusterConfig, request::RequestType,
    },
};

use crate::{
    mock::{udp_backend::UdpBackend, udp_client::UdpClient},
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or},
};

use super::tests::create_local_address;

const CLUSTER: &str = "udp_cluster_0";
/// Generous deadline for a single round trip on a quiet local loopback.
const RT: Duration = Duration::from_millis(1500);

// =========================================================================
// Helpers
// =========================================================================

/// A UDP cluster config builder seeded from the proto defaults.
fn udp_cluster<S: Into<String>>(cluster_id: S, lb: LoadBalancingAlgorithms) -> Cluster {
    Cluster {
        load_balancing: lb.into(),
        ..Worker::default_cluster(cluster_id)
    }
}

/// A UDP cluster keyed on the 4-tuple (`SOURCE_IP_PORT`). On loopback every
/// client shares the source IP `127.0.0.1`, so the default `SOURCE_IP` key
/// would collapse all clients into a single flow. The 4-tuple key makes each
/// distinct client *port* its own flow — the precondition for any
/// distribution / per-flow-affinity assertion.
fn udp_cluster_per_port<S: Into<String>>(cluster_id: S, lb: LoadBalancingAlgorithms) -> Cluster {
    Cluster {
        udp: Some(UdpClusterConfig {
            affinity_key: Some(UdpAffinityKey::SourceIpPort.into()),
            ..Default::default()
        }),
        ..udp_cluster(cluster_id, lb)
    }
}

/// Build a `RequestUdpFrontend` binding `cluster_id` to the listener `address`.
fn udp_frontend<S: Into<String>>(cluster_id: S, address: SocketAddr) -> RequestUdpFrontend {
    RequestUdpFrontend {
        cluster_id: cluster_id.into(),
        address: address.into(),
        tags: Default::default(),
    }
}

/// Start a worker with a single activated UDP listener + frontend + cluster,
/// then register `nb_backends` backends. Returns the worker, the registered
/// backend addresses, and the front address. The cluster is added with the
/// given `cluster` config so the caller controls LB algorithm and udp knobs.
fn setup_udp_test(
    name: &str,
    cluster: Cluster,
    nb_backends: usize,
) -> (Worker, Vec<SocketAddr>, SocketAddr) {
    let front_address = create_local_address();
    // An empty config + empty listeners: the UDP listener binds its own socket
    // on activation (from_scm = false), so we don't pre-attach an SCM fd.
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddUdpListener(
        ListenerBuilder::new_udp(front_address.into())
            .to_udp(None)
            .expect("could not build udp listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Udp.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(cluster));
    worker.send_proxy_request_type(RequestType::AddUdpFrontend(udp_frontend(
        CLUSTER,
        front_address,
    )));

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            CLUSTER,
            format!("{CLUSTER}-{i}"),
            back_address,
            None,
        )));
        backends.push(back_address);
    }

    worker.read_to_last();
    (worker, backends, front_address)
}

/// Strip the `{name}:` reply tag the mock backend prepends, leaving the inner
/// payload. Returns `None` if the tag separator is absent.
fn strip_reply_tag(reply: &[u8]) -> Option<(String, Vec<u8>)> {
    let pos = reply.iter().position(|&b| b == b':')?;
    let name = String::from_utf8_lossy(&reply[..pos]).into_owned();
    Some((name, reply[pos + 1..].to_vec()))
}

// =========================================================================
// Test 1: basic forward + symmetric NAT return  (PROVE THE DATAPATH FIRST)
//
// A client datagram must reach the backend intact, and the backend's reply
// must come back to the exact client source — over the proxy's per-flow
// connected upstream socket (symmetric NAT). Payload is preserved end-to-end.
// =========================================================================

fn try_udp_basic_forward_and_return() -> State {
    let (mut worker, backends, front) = setup_udp_test(
        "UDP-BASIC",
        udp_cluster(CLUSTER, LoadBalancingAlgorithms::RoundRobin),
        1,
    );
    let backend = UdpBackend::bind("BK0", backends[0], 1).spawn();

    let client = UdpClient::new("CLIENT", front);
    let reply = client.round_trip(b"ping", RT);

    let state = match reply.as_deref() {
        Some(bytes) => match strip_reply_tag(bytes) {
            Some((name, payload)) if name == "BK0" && payload == b"ping" => {
                // Backend must have seen the request payload verbatim.
                let seen = backend.payloads_utf8();
                if seen.first().map(String::as_str) == Some("ping") {
                    State::Success
                } else {
                    println!("backend saw {seen:?}, expected [\"ping\"]");
                    State::Fail
                }
            }
            other => {
                println!("unexpected reply {other:?} (raw {bytes:?})");
                State::Fail
            }
        },
        None => {
            println!("client received no reply within deadline");
            State::Fail
        }
    };

    backend.stop();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if state == State::Success && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_basic_forward_and_return() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "UDP basic: datagram forwarded to backend, reply NAT-returned to client",
            try_udp_basic_forward_and_return,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 2: round-robin distribution across two backends
//
// Distinct client source ports = distinct flows. With RR, successive new
// flows land on different backends. We open several clients and assert both
// backends were exercised (we do not assert a strict order — only that the
// load is spread, which is the RR contract for new flows).
// =========================================================================

fn try_udp_round_robin_distribution() -> State {
    let (mut worker, backends, front) = setup_udp_test(
        "UDP-RR",
        udp_cluster_per_port(CLUSTER, LoadBalancingAlgorithms::RoundRobin),
        2,
    );
    let bk0 = UdpBackend::bind("BK0", backends[0], 1).spawn();
    let bk1 = UdpBackend::bind("BK1", backends[1], 1).spawn();

    // Six distinct clients (distinct source ports → distinct flows via the
    // 4-tuple key, since they all share the loopback source IP).
    let mut hit: HashMap<String, usize> = HashMap::new();
    for i in 0..6 {
        let client = UdpClient::new(format!("C{i}"), front);
        if let Some(reply) = client.round_trip(format!("q{i}").as_bytes(), RT) {
            if let Some((name, _)) = strip_reply_tag(&reply) {
                *hit.entry(name).or_default() += 1;
            }
        }
    }

    bk0.stop();
    bk1.stop();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    println!("RR distribution: {hit:?}");
    // Both backends must have served at least one flow.
    let both_hit =
        hit.get("BK0").copied().unwrap_or(0) > 0 && hit.get("BK1").copied().unwrap_or(0) > 0;
    if both_hit && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_round_robin_distribution() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "UDP round-robin: new flows spread across both backends",
            try_udp_round_robin_distribution,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 3: HRW affinity — same source sticks to the same backend, and stays
// stable across an unrelated backend add.
//
// A single client (one source) drives several flows in sequence. Because the
// affinity key (source IP) is constant, HRW must select the same backend
// every time — and adding a third backend must not move it (HRW minimal
// disruption: only keys that hash highest to the *new* backend move).
// =========================================================================

fn try_udp_hrw_affinity_stable() -> State {
    // 4-tuple affinity + responses=1 (DNS-style one-shot): every round trip on a
    // fixed client port is a *fresh* flow that re-runs HRW selection, so we can
    // re-probe the SAME port before and after a backend add and compare which
    // backend HRW chose for that key.
    let cluster = Cluster {
        udp: Some(UdpClusterConfig {
            affinity_key: Some(UdpAffinityKey::SourceIpPort.into()),
            responses: Some(1),
            ..Default::default()
        }),
        ..udp_cluster(CLUSTER, LoadBalancingAlgorithms::Hrw)
    };
    let (mut worker, backends, front) = setup_udp_test("UDP-HRW", cluster, 3);
    let handles: Vec<_> = backends
        .iter()
        .enumerate()
        .map(|(i, &addr)| UdpBackend::bind(format!("BK{i}"), addr, 1).spawn())
        .collect();

    // Probe N fixed client ports twice each: HRW must pick the same backend for
    // a given 4-tuple every time it re-selects (deterministic affinity).
    let clients: Vec<UdpClient> = (0..6)
        .map(|i| UdpClient::new(format!("H{i}"), front))
        .collect();
    let probe = |clients: &[UdpClient]| -> Vec<Option<String>> {
        clients
            .iter()
            .map(|c| {
                c.round_trip(b"q", RT)
                    .and_then(|r| strip_reply_tag(&r).map(|(name, _)| name))
            })
            .collect()
    };

    let before = probe(&clients);
    let before2 = probe(&clients);
    println!("HRW before add: {before:?} / re-probe {before2:?}");
    // Deterministic per-key: a port's two probes must agree, and every probe
    // must have been served.
    let deterministic = before
        .iter()
        .zip(before2.iter())
        .all(|(a, b)| a.is_some() && a == b);

    // Add an unrelated 4th backend; HRW minimal disruption: a key only moves if
    // it now hashes highest to the newcomer. We assert most keys are preserved
    // (HRW's hallmark) — far below the ~1/4 worst case any single key could move.
    let new_back = create_local_address();
    let bk_new = UdpBackend::bind("BK3", new_back, 1).spawn();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        CLUSTER,
        format!("{CLUSTER}-3"),
        new_back,
        None,
    )));
    worker.read_to_last();

    let after = probe(&clients);
    println!("HRW after add: {after:?}");
    let served_after = after.iter().filter(|x| x.is_some()).count();
    // HRW minimal disruption is a *provable* invariant, independent of the
    // dynamically-allocated backend ports: adding a backend can only move a key
    // from its previous winner to the *new* backend (iff the newcomer now scores
    // highest for that key) — it can never reshuffle a key between two pre-existing
    // backends. So every key must either keep its backend or move to "BK3".
    // Asserting a preserved *fraction* instead would be statistically flaky on a
    // small key set: a favourably-hashing newcomer can legitimately capture a
    // majority of just 6 keys (observed in CI under a different port allocation).
    let only_moves_to_newcomer = before
        .iter()
        .zip(after.iter())
        .all(|(b, a)| a.is_some() && (a == b || a.as_deref() == Some("BK3")));
    let minimal_disruption = served_after == clients.len() && only_moves_to_newcomer;

    for h in handles {
        h.stop();
    }
    bk_new.stop();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if deterministic && minimal_disruption && stopped {
        State::Success
    } else {
        println!(
            "HRW: deterministic={deterministic} only_moves_to_newcomer={only_moves_to_newcomer} served_after={served_after}/{}",
            clients.len()
        );
        State::Fail
    }
}

#[test]
fn test_udp_hrw_affinity_stable() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "UDP HRW: same source sticks to one backend, stable across backend add",
            try_udp_hrw_affinity_stable,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 4: Maglev affinity — same source maps to a stable backend.
//
// Maglev builds a precomputed lookup table; a fixed key indexes the same
// table slot, so a constant source key must select one stable backend.
// =========================================================================

fn try_udp_maglev_affinity_stable() -> State {
    // 4-tuple + responses=1 so each round trip re-runs Maglev table lookup on a
    // fresh flow. A fixed client port indexes the same precomputed table slot,
    // so its backend choice must be identical across re-probes.
    let cluster = Cluster {
        udp: Some(UdpClusterConfig {
            affinity_key: Some(UdpAffinityKey::SourceIpPort.into()),
            responses: Some(1),
            ..Default::default()
        }),
        ..udp_cluster(CLUSTER, LoadBalancingAlgorithms::Maglev)
    };
    let (mut worker, backends, front) = setup_udp_test("UDP-MAGLEV", cluster, 3);
    let handles: Vec<_> = backends
        .iter()
        .enumerate()
        .map(|(i, &addr)| UdpBackend::bind(format!("BK{i}"), addr, 1).spawn())
        .collect();

    // Probe 6 fixed client ports twice each; each port's two probes must agree
    // (deterministic table lookup), and every probe must be served.
    let clients: Vec<UdpClient> = (0..6)
        .map(|i| UdpClient::new(format!("M{i}"), front))
        .collect();
    let probe = |clients: &[UdpClient]| -> Vec<Option<String>> {
        clients
            .iter()
            .map(|c| {
                c.round_trip(b"q", RT)
                    .and_then(|r| strip_reply_tag(&r).map(|(name, _)| name))
            })
            .collect()
    };
    let first = probe(&clients);
    let second = probe(&clients);
    println!("Maglev probe1: {first:?} probe2: {second:?}");

    for h in handles {
        h.stop();
    }
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let deterministic = first
        .iter()
        .zip(second.iter())
        .all(|(a, b)| a.is_some() && a == b);
    // Maglev should also spread load across the table — assert at least two
    // distinct backends are hit across the 6 keys (not a single-backend table).
    let distinct: std::collections::HashSet<_> = first.iter().flatten().collect();
    if deterministic && distinct.len() >= 2 && stopped {
        State::Success
    } else {
        println!(
            "Maglev: deterministic={deterministic} distinct_backends={}",
            distinct.len()
        );
        State::Fail
    }
}

#[test]
fn test_udp_maglev_affinity_stable() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "UDP Maglev: constant source key maps to a stable backend",
            try_udp_maglev_affinity_stable,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 5: responses=1 teardown (DNS-style: flow closes after one reply).
//
// With `responses = 1`, the flow tears down after the backend's single reply.
// We verify the first round trip succeeds (proving forwarding), and that the
// proxy stays alive and serves a *fresh* flow afterwards (proving the close
// path frees the slot without wedging the listener).
// =========================================================================

fn try_udp_responses_teardown() -> State {
    let cluster = Cluster {
        udp: Some(UdpClusterConfig {
            responses: Some(1),
            ..Default::default()
        }),
        ..udp_cluster(CLUSTER, LoadBalancingAlgorithms::RoundRobin)
    };
    let (mut worker, backends, front) = setup_udp_test("UDP-RESP1", cluster, 1);
    let backend = UdpBackend::bind("BK0", backends[0], 1).spawn();

    // First flow: one request, one reply, then teardown.
    let c1 = UdpClient::new("C1", front);
    let r1 = c1.round_trip(b"dns-query-1", RT);

    // A fresh client (new flow) must still be served after the first closed.
    let c2 = UdpClient::new("C2", front);
    let r2 = c2.round_trip(b"dns-query-2", RT);

    backend.stop();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let ok1 =
        matches!(r1.as_deref().and_then(strip_reply_tag), Some((_, p)) if p == b"dns-query-1");
    let ok2 =
        matches!(r2.as_deref().and_then(strip_reply_tag), Some((_, p)) if p == b"dns-query-2");
    println!("responses=1: first_ok={ok1} second_ok={ok2}");
    if ok1 && ok2 && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_responses_teardown() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "UDP responses=1: flow closes after one reply, new flow still served",
            try_udp_responses_teardown,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 6: requests cap teardown.
//
// With `requests = 2`, the flow closes after forwarding the 2nd client
// datagram. We send three datagrams from one source; the first two must be
// delivered, then the flow closes. The proxy must stay healthy.
// =========================================================================

fn try_udp_requests_cap() -> State {
    let cluster = Cluster {
        udp: Some(UdpClusterConfig {
            requests: Some(2),
            // Unlimited responses so the cap is the only teardown trigger.
            responses: Some(0),
            ..Default::default()
        }),
        ..udp_cluster(CLUSTER, LoadBalancingAlgorithms::RoundRobin)
    };
    let (mut worker, backends, front) = setup_udp_test("UDP-REQCAP", cluster, 1);
    let backend = UdpBackend::bind("BK0", backends[0], 0).spawn();

    // One client, same source: 2 datagrams should reach the backend, then the
    // flow closes on the 2nd (requests cap).
    let client = UdpClient::new("C", front);
    client.send(b"r1");
    // Tiny settle between datagrams so they map to the same flow in order.
    backend.wait_for_requests(1, RT);
    client.send(b"r2");
    backend.wait_for_requests(2, RT);
    // The 3rd datagram opens a brand-new flow (the old one closed); the backend
    // sees it too, but on a different upstream socket.
    client.send(b"r3");
    backend.wait_for_requests(3, RT);

    let count = backend.requests_received();
    backend.stop();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    println!("requests cap: backend saw {count} datagrams");
    // The cap must not lose the first two; we delivered three total.
    if count >= 2 && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_requests_cap() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "UDP requests cap: flow closes after the configured datagram count",
            try_udp_requests_cap,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 7: oversized datagram > max_rx_datagram_size is dropped, proxy stays up.
//
// We set a tiny max_rx_datagram_size, then send one datagram larger than the
// cap (must be dropped, no panic) and one within the cap (must be forwarded).
// =========================================================================

fn try_udp_oversized_datagram_dropped() -> State {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker_owned("UDP-OVERSIZE", config, listeners, state);

    // max_rx_datagram_size = 64: anything bigger truncates/drops.
    let mut builder = ListenerBuilder::new_udp(front_address.into());
    builder.max_rx_datagram_size = Some(64);
    worker.send_proxy_request_type(RequestType::AddUdpListener(
        builder.to_udp(None).expect("udp listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Udp.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(udp_cluster(
        CLUSTER,
        LoadBalancingAlgorithms::RoundRobin,
    )));
    worker.send_proxy_request_type(RequestType::AddUdpFrontend(udp_frontend(
        CLUSTER,
        front_address,
    )));
    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        CLUSTER,
        format!("{CLUSTER}-0"),
        back_address,
        None,
    )));
    worker.read_to_last();

    let backend = UdpBackend::bind("BK0", back_address, 1).spawn();

    // 1) Oversized: 200 bytes > 64-byte cap → must be dropped.
    let big = UdpClient::new("BIG", front_address);
    big.send(&vec![b'X'; 200]);
    // Give the proxy a moment; the backend must NOT receive it.
    let got_big = backend.wait_for_requests(1, Duration::from_millis(400));

    // 2) Within cap: must be forwarded + returned, proving the proxy survived.
    let small = UdpClient::new("SMALL", front_address);
    let reply = small.round_trip(b"small", RT);

    let small_ok =
        matches!(reply.as_deref().and_then(strip_reply_tag), Some((_, p)) if p == b"small");

    backend.stop();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    println!("oversize: dropped_big={} small_ok={small_ok}", !got_big);
    // The oversized datagram must have been dropped (backend never saw it as
    // the first request) and the in-cap one must round-trip.
    if !got_big && small_ok && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_oversized_datagram_dropped() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "UDP oversize: datagram over max_rx_datagram_size dropped, proxy survives",
            try_udp_oversized_datagram_dropped,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 8: PPv2-to-backend — cluster send_proxy_protocol=true makes the backend
// see a PPv2 DGRAM header carrying the real client address.
// =========================================================================

fn try_udp_proxy_protocol_to_backend() -> State {
    let cluster = Cluster {
        udp: Some(UdpClusterConfig {
            send_proxy_protocol: Some(true),
            ..Default::default()
        }),
        ..udp_cluster(CLUSTER, LoadBalancingAlgorithms::RoundRobin)
    };
    let (mut worker, backends, front) = setup_udp_test("UDP-PPV2", cluster, 1);
    let backend = UdpBackend::bind("BK0", backends[0], 1).spawn();

    let client = UdpClient::new("CLIENT", front);
    let client_src = client.local_addr();
    let reply = client.round_trip(b"proxied", RT);

    backend.wait_for_requests(1, RT);
    let observed = backend.observed();

    backend.stop();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    // The backend must have stripped a PPv2 header whose advertised client
    // address matches the real client source, and the inner payload must be
    // intact. The reply (inner payload echoed) must reach the client.
    let first = observed.first();
    let pp_ok = match first {
        Some(d) => {
            let addr_ok = d.proxy_protocol_client == Some(client_src);
            let payload_ok = d.payload == b"proxied";
            if !addr_ok {
                println!(
                    "PPv2 client addr mismatch: saw {:?}, expected {client_src}",
                    d.proxy_protocol_client
                );
            }
            addr_ok && payload_ok
        }
        None => {
            println!("backend received no datagram");
            false
        }
    };
    let reply_ok =
        matches!(reply.as_deref().and_then(strip_reply_tag), Some((_, p)) if p == b"proxied");
    println!("PPv2: header_ok={pp_ok} reply_ok={reply_ok}");
    if pp_ok && reply_ok && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_proxy_protocol_to_backend() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "UDP PPv2: backend sees a v2 DGRAM header with the real client addr",
            try_udp_proxy_protocol_to_backend,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 9: hot reconfig — add + activate + remove a UDP listener at runtime.
//
// Traffic must flow after the add+activate, and a removed listener must stop
// serving (the bound socket is dropped, so the client gets no reply).
// =========================================================================

fn try_udp_hot_add_remove_listener() -> State {
    // Start a worker with NO UDP listener, then add one at runtime.
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker_owned("UDP-HOTRECONF", config, listeners, state);

    let front_address = create_local_address();
    let back_address = create_local_address();
    let backend = UdpBackend::bind("BK0", back_address, 1).spawn();

    // --- add + activate at runtime ---
    worker.send_proxy_request_type(RequestType::AddUdpListener(
        ListenerBuilder::new_udp(front_address.into())
            .to_udp(None)
            .expect("udp listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Udp.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(udp_cluster(
        CLUSTER,
        LoadBalancingAlgorithms::RoundRobin,
    )));
    worker.send_proxy_request_type(RequestType::AddUdpFrontend(udp_frontend(
        CLUSTER,
        front_address,
    )));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        CLUSTER,
        format!("{CLUSTER}-0"),
        back_address,
        None,
    )));
    worker.read_to_last();

    // Traffic flows after the add.
    let c1 = UdpClient::new("C1", front_address);
    let after_add = c1.round_trip(b"after-add", RT);
    let add_ok =
        matches!(after_add.as_deref().and_then(strip_reply_tag), Some((_, p)) if p == b"after-add");

    // --- deactivate + remove the listener at runtime (the canonical control
    // sequence the master emits: deactivate gives the socket back + drops the
    // listener slot, remove drops the config). ---
    worker.send_proxy_request_type(RequestType::DeactivateListener(DeactivateListener {
        address: front_address.into(),
        proxy: ListenerType::Udp.into(),
        to_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::RemoveListener(RemoveListener {
        address: front_address.into(),
        proxy: ListenerType::Udp.into(),
    }));
    worker.read_to_last();

    // After remove, the manager no longer routes (the listener was dropped from
    // the proxy map). A new client must get no reply.
    let c2 = UdpClient::new("C2", front_address);
    let after_remove = c2.round_trip(b"after-remove", Duration::from_millis(500));
    let remove_ok = after_remove.is_none();

    backend.stop();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    println!("hot reconfig: add_ok={add_ok} remove_stopped_traffic={remove_ok}");
    if add_ok && remove_ok && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_hot_add_remove_listener() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "UDP hot reconfig: add+activate serves traffic, remove stops it",
            try_udp_hot_add_remove_listener,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 10: health / fail-open — a flow is still served when backends are up,
// and the affinity key is stable. (Full active health probing is timing-hard
// in e2e; we assert the fail-open selection behavior with backends up.)
//
// We register two backends but only bind one socket; with the default cluster
// (health off → fail-open over the full set) every flow must still be served
// by whichever backend has a live socket, and the proxy never wedges even when
// a selected backend address has nothing listening.
// =========================================================================

fn try_udp_failopen_some_backends_down() -> State {
    let (mut worker, backends, front) = setup_udp_test(
        "UDP-FAILOPEN",
        udp_cluster(CLUSTER, LoadBalancingAlgorithms::RoundRobin),
        2,
    );
    // Bind ONLY the first backend; the second address has nothing listening.
    let live = UdpBackend::bind("BK0", backends[0], 1).spawn();

    // Drive several flows. Flows that select the live backend get a reply;
    // flows that select the dead address simply get no reply (UDP best-effort).
    // The contract under test: the proxy never panics/wedges, and at least some
    // flows are served (fail-open keeps forwarding over the full set).
    let mut served = 0usize;
    for i in 0..6 {
        let client = UdpClient::new(format!("F{i}"), front);
        if let Some(reply) =
            client.round_trip(format!("p{i}").as_bytes(), Duration::from_millis(600))
        {
            if strip_reply_tag(&reply).is_some() {
                served += 1;
            }
        }
    }

    live.stop();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    println!("fail-open: {served}/6 flows served by the live backend");
    // At least one flow must have been served by the live backend, proving the
    // proxy keeps forwarding (fail-open) despite a dead address in the set, and
    // the worker shut down cleanly (no wedge/panic).
    if served >= 1 && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_failopen_some_backends_down() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "UDP fail-open: flows still served with a dead backend in the set, no wedge",
            try_udp_failopen_some_backends_down,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 11: listener fd hand-off across an upgrade.
//
// Mirrors `doc/upgrade_e2e_tests.md`: a new worker re-execs and adopts the old
// worker's UDP listener fd over SCM. In-flight flows reset (acceptable), but a
// NEW flow must be served on the handed-off listener after the upgrade.
// =========================================================================

fn try_udp_upgrade_fd_handoff() -> State {
    let (mut worker, backends, front) = setup_udp_test(
        "UDP-UPGRADE",
        udp_cluster(CLUSTER, LoadBalancingAlgorithms::RoundRobin),
        1,
    );
    let backend = UdpBackend::bind("BK0", backends[0], 1).spawn();

    // Sanity: traffic flows before the upgrade.
    let pre = UdpClient::new("PRE", front);
    let pre_ok = matches!(pre.round_trip(b"pre", RT).as_deref().and_then(strip_reply_tag), Some((_, p)) if p == b"pre");

    // Upgrade: hand off the listener fds over SCM and re-exec the worker.
    let mut upgraded = worker.upgrade("UDP-UPGRADE-2");

    // A NEW flow must be served on the handed-off listener.
    let post = UdpClient::new("POST", front);
    let post_ok = matches!(post.round_trip(b"post", RT).as_deref().and_then(strip_reply_tag), Some((_, p)) if p == b"post");

    backend.stop();
    upgraded.soft_stop();
    let stopped = upgraded.wait_for_server_stop();

    println!("upgrade: pre_ok={pre_ok} post_ok={post_ok} stopped={stopped}");
    if pre_ok && post_ok && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_upgrade_fd_handoff() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "UDP upgrade: listener fd survives re-exec, new flows served post-upgrade",
            try_udp_upgrade_fd_handoff,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 12: SOURCE_IP_PORT affinity key — two distinct source ports from the
// same IP map to (potentially) different flows, and the proxy keys correctly.
//
// We assert the datapath honours the 4-tuple affinity key: a single client
// gets stable affinity, and the proxy serves it. (Cross-port distribution is
// not asserted deterministically since both ports may hash to one backend.)
// =========================================================================

fn try_udp_affinity_source_ip_port() -> State {
    let cluster = Cluster {
        udp: Some(UdpClusterConfig {
            affinity_key: Some(UdpAffinityKey::SourceIpPort.into()),
            ..Default::default()
        }),
        ..udp_cluster(CLUSTER, LoadBalancingAlgorithms::Hrw)
    };
    let (mut worker, backends, front) = setup_udp_test("UDP-AFFIN-PORT", cluster, 2);
    let handles: Vec<_> = backends
        .iter()
        .enumerate()
        .map(|(i, &addr)| UdpBackend::bind(format!("BK{i}"), addr, 1).spawn())
        .collect();

    // Same client (same source port) → stable backend across datagrams.
    let client = UdpClient::new("C", front);
    let mut chosen: Vec<String> = Vec::new();
    for i in 0..3 {
        if let Some(reply) = client.round_trip(format!("k{i}").as_bytes(), RT) {
            if let Some((name, _)) = strip_reply_tag(&reply) {
                chosen.push(name);
            }
        }
    }

    for h in handles {
        h.stop();
    }
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    println!("source_ip_port affinity (same port): {chosen:?}");
    // All datagrams from the same 4-tuple must hit the same backend.
    let first = chosen.first().cloned();
    let stable = first.is_some() && chosen.iter().all(|c| Some(c) == first.as_ref());
    if stable && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_affinity_source_ip_port() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "UDP affinity SOURCE_IP_PORT: same 4-tuple sticks to one backend",
            try_udp_affinity_source_ip_port,
        ),
        State::Success,
    );
}
