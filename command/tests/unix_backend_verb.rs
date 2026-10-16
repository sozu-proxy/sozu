//! A UNIX backend travels on its own request verb, and a UDP cluster refuses it.
//!
//! `AddBackend.unix_path` is a field a worker that predates it skips, and that
//! worker still decodes `Request.add_backend`, so it would dial the identity
//! `address` of a backend meant to be a UNIX socket. The verb `add_unix_backend`
//! (`Request` field 72) is one such a worker cannot decode, so the order fails
//! there instead. These tests pin that with a hand-written copy of the previous
//! schema, so no old binary is needed.

use prost::Message;
use sozu_command_lib::{
    proto::command::{
        AddBackend, Cluster, RequestUdpFrontend, SocketAddress, WorkerRequest, request::RequestType,
    },
    state::ConfigState,
};

/// `AddBackend` as the previous schema declared it: no `unix_path`.
#[derive(Clone, PartialEq, prost::Message)]
struct OldAddBackend {
    #[prost(string, required, tag = "1")]
    cluster_id: String,
    #[prost(string, required, tag = "2")]
    backend_id: String,
    #[prost(message, required, tag = "3")]
    address: SocketAddress,
}

/// `Request` as the previous schema declared it, cut to the verbs the tests
/// exercise: `add_backend` is 24 and nothing above 64 is known.
#[derive(Clone, PartialEq, prost::Message)]
struct OldRequest {
    #[prost(oneof = "OldRequestType", tags = "24")]
    request_type: Option<OldRequestType>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
enum OldRequestType {
    #[prost(message, tag = "24")]
    AddBackend(OldAddBackend),
}

/// What a worker of the previous schema acts on when it receives `request`:
/// the `AddBackend` it would register, if it decodes one.
fn old_worker_adds(request: &sozu_command_lib::proto::command::Request) -> Option<OldAddBackend> {
    let decoded = OldRequest::decode(request.encode_to_vec().as_slice())
        .expect("an unknown field must be skipped, not fail the decode");
    decoded
        .request_type
        .map(|OldRequestType::AddBackend(backend)| backend)
}

fn unix_backend(cluster_id: &str) -> AddBackend {
    AddBackend {
        cluster_id: cluster_id.to_owned(),
        backend_id: format!("{cluster_id}-0"),
        address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
        unix_path: Some("/run/app.sock".to_owned()),
        ..Default::default()
    }
}

/// Both verbs that can carry a unix backend into the state: the canonical one,
/// and the historical one a client may still send.
fn unix_backend_orders(backend: AddBackend) -> [sozu_command_lib::proto::command::Request; 2] {
    [
        RequestType::AddBackend(backend.clone()).into(),
        RequestType::AddUnixBackend(backend).into(),
    ]
}

fn state_with_unix_backend() -> ConfigState {
    let mut state = ConfigState::new();
    state
        .dispatch(
            &RequestType::AddCluster(Cluster {
                cluster_id: "app".to_owned(),
                ..Default::default()
            })
            .into(),
        )
        .expect("the cluster must be added");
    state
        .dispatch(&RequestType::add_backend(unix_backend("app")).into())
        .expect("the backend must be added");
    state
}

/// A worker of the previous schema never acts on a UNIX backend, whichever
/// path produced the order: the saved state, a replay, or a diff.
#[test]
fn an_old_worker_never_acts_on_a_unix_backend_order() {
    let state = state_with_unix_backend();

    let replayed: Vec<_> = state
        .produce_initial_state()
        .requests
        .into_iter()
        .map(|WorkerRequest { content, .. }| content)
        .collect();
    let diffed = ConfigState::new().diff(&state);

    let mut checked = 0;
    for request in replayed.iter().chain(diffed.iter()) {
        let is_backend_order = matches!(
            request.request_type,
            Some(RequestType::AddBackend(_)) | Some(RequestType::AddUnixBackend(_))
        );
        if is_backend_order {
            checked += 1;
            assert_eq!(
                old_worker_adds(request),
                None,
                "an old worker must not register a unix backend as an IP one"
            );
        }
    }
    assert_eq!(checked, 2, "one replayed order and one diffed order");
}

/// The contrast the verb exists for: an IP backend stays on `add_backend`,
/// which every worker version registers.
#[test]
fn an_ip_backend_stays_actionable_for_an_old_worker() {
    let mut ip_backend = unix_backend("app");
    ip_backend.unix_path = None;
    let request = RequestType::AddBackend(ip_backend).into();
    assert!(old_worker_adds(&request).is_some());
}

/// A backend belongs to an existing cluster, so each scenario adds it first.
fn add_cluster(state: &mut ConfigState, cluster_id: &str) {
    state
        .dispatch(
            &RequestType::AddCluster(Cluster {
                cluster_id: cluster_id.to_owned(),
                ..Default::default()
            })
            .into(),
        )
        .expect("the cluster must be added");
}

/// A UDP cluster relays datagrams over UDP sockets, so it refuses a UNIX
/// backend, whichever of the cluster, its frontend or the backend came first,
/// and on whichever verb the backend arrives.
#[test]
fn a_udp_cluster_refuses_a_unix_backend_in_every_order() {
    let udp_frontend = RequestUdpFrontend {
        cluster_id: "dns".to_owned(),
        address: SocketAddress::new_v4(127, 0, 0, 1, 5353),
        ..Default::default()
    };

    for order in unix_backend_orders(unix_backend("dns")) {
        // frontend first, then the backend
        let mut state = ConfigState::new();
        add_cluster(&mut state, "dns");
        state
            .dispatch(&RequestType::AddUdpFrontend(udp_frontend.clone()).into())
            .expect("the frontend must be added");
        assert!(
            state.dispatch(&order).is_err(),
            "a unix backend on a UDP cluster must be refused"
        );
        assert_eq!(state.backends.get("dns"), None, "nothing was stored");

        // backend first, then the frontend
        let mut state = ConfigState::new();
        add_cluster(&mut state, "dns");
        state
            .dispatch(&order)
            .expect("a unix backend on a cluster with no UDP frontend yet is admitted");
        assert!(
            state
                .dispatch(&RequestType::AddUdpFrontend(udp_frontend.clone()).into())
                .is_err(),
            "a UDP frontend on a cluster with a unix backend must be refused"
        );
        assert_eq!(state.udp_fronts.get("dns"), None, "nothing was stored");

        // backend first, then the cluster turns UDP
        let mut state = ConfigState::new();
        add_cluster(&mut state, "dns");
        state.dispatch(&order).expect("the backend must be added");
        assert!(
            state
                .dispatch(
                    &RequestType::AddCluster(Cluster {
                        cluster_id: "dns".to_owned(),
                        udp: Some(Default::default()),
                        ..Default::default()
                    })
                    .into()
                )
                .is_err(),
            "a cluster with a unix backend must not turn UDP"
        );
    }
}

/// The verb is picked from the endpoint, and `into_canonical` moves a unix
/// backend sent on the historical verb to the new one (and no IP backend).
#[test]
fn the_verb_follows_the_endpoint() {
    let unix = unix_backend("app");
    let mut ip = unix.clone();
    ip.unix_path = None;

    assert!(matches!(
        RequestType::add_backend(unix.clone()),
        RequestType::AddUnixBackend(_)
    ));
    assert!(matches!(
        RequestType::add_backend(ip.clone()),
        RequestType::AddBackend(_)
    ));
    assert!(matches!(
        RequestType::AddBackend(unix).into_canonical(),
        RequestType::AddUnixBackend(_)
    ));
    assert!(matches!(
        RequestType::AddUnixBackend(ip).into_canonical(),
        RequestType::AddBackend(_)
    ));
}

/// The new verb is field 72 on the wire, and the state it builds is the one
/// the historical verb builds.
#[test]
fn the_unix_backend_verb_is_request_field_72_with_the_same_state() {
    let request: sozu_command_lib::proto::command::Request =
        RequestType::AddUnixBackend(unix_backend("app")).into();
    // key of field 72, wire type 2 (length-delimited): (72 << 3) | 2 = 578
    assert_eq!(request.encode_to_vec()[..2], [0xC2, 0x04]);

    let mut by_new_verb = ConfigState::new();
    let mut by_old_verb = ConfigState::new();
    for state in [&mut by_new_verb, &mut by_old_verb] {
        state
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: "app".to_owned(),
                    ..Default::default()
                })
                .into(),
            )
            .unwrap();
    }
    let [old_verb, new_verb] = unix_backend_orders(unix_backend("app"));
    by_new_verb.dispatch(&new_verb).unwrap();
    by_old_verb.dispatch(&old_verb).unwrap();
    assert_eq!(by_new_verb.backends, by_old_verb.backends);
}

/// An IP backend on a UDP cluster is unchanged.
#[test]
fn a_udp_cluster_keeps_its_ip_backends() {
    let mut state = ConfigState::new();
    add_cluster(&mut state, "dns");
    state
        .dispatch(
            &RequestType::AddUdpFrontend(RequestUdpFrontend {
                cluster_id: "dns".to_owned(),
                address: SocketAddress::new_v4(127, 0, 0, 1, 5353),
                ..Default::default()
            })
            .into(),
        )
        .expect("the frontend must be added");
    let mut ip_backend = unix_backend("dns");
    ip_backend.unix_path = None;
    state
        .dispatch(&RequestType::AddBackend(ip_backend).into())
        .expect("an IP backend on a UDP cluster must be admitted");
}
