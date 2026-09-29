//! Forward-compat regression for `SaveState` / `LoadState` JSON files.
//!
//! `proxy-manager` (and other tools pinned to `sozu-command-lib = "1.1.1"`)
//! writes a state file as `\n\0`-separated JSON `WorkerRequest` records using
//! the 1.1.1 schema, then asks Sōzu to `LoadState` it. Post-1.1.1 schema
//! changes added `repeated` / `map` fields to messages those tools still
//! emit (`Cluster.answers`, `Cluster.authorized_hashes`,
//! `RequestHttpFrontend.headers`, plus listener-level `answers` /
//! `alpn_protocols`). Without `#[serde(default)]` on each new repeated/map
//! field, `serde_json::from_slice::<WorkerRequest>` rejects the older
//! payload with `missing field 'answers'`; `parse_several_requests`'s
//! `many0(complete(...))` then leaves the unparsed bytes as the remainder,
//! and `bin/src/command/requests.rs::load_state` reports
//! `"Error consuming load state message"`.
//!
//! These tests pin the contract:
//!   1. A 1.1.1-shaped payload deserializes cleanly and the new repeated/map
//!      fields default to empty.
//!   2. A record missing a *required scalar* (`cluster_id`) still fails so
//!      the strict path stays strict — guards against future drift toward
//!      a struct-level `#[serde(default)]`, which would silently insert
//!      bogus `""`-keyed clusters.

use sozu_command_lib::{
    parser::parse_several_requests,
    proto::command::{HttpListenerConfig, SocketAddress, WorkerRequest, request::RequestType},
    state::ConfigState,
};

/// `127.0.0.1` as the `IpAddress.Inner.V4` `fixed32` (`u32::from(Ipv4Addr)`
/// per `command/src/request.rs::From<SocketAddr>`).
const LOCALHOST_V4: u32 = 0x7F00_0001;

/// Builds the JSON exactly as `sozu-command-lib = "1.1.1"` would emit it:
/// only the field set the 1.1.1 schema knew about. None of the post-1.1.1
/// `Cluster.answers`, `Cluster.authorized_hashes`,
/// `RequestHttpFrontend.headers`, or the listener-level additions appear.
fn v1_1_1_state_file_payload() -> Vec<u8> {
    let add_cluster = r#"{"id":"PROXY-MANAGER-1","content":{"request_type":{"ADD_CLUSTER":{"cluster_id":"app-1","sticky_session":false,"https_redirect":false,"proxy_protocol":null,"load_balancing":0,"answer_503":null,"load_metric":0}}}}"#
        .to_owned();

    let add_http_frontend = format!(
        r#"{{"id":"PROXY-MANAGER-2","content":{{"request_type":{{"ADD_HTTP_FRONTEND":{{"cluster_id":"app-1","address":{{"ip":{{"inner":{{"V4":{LOCALHOST_V4}}}}},"port":80}},"hostname":"example.com","path":{{"kind":0,"value":"/"}},"method":null,"position":2,"tags":{{}}}}}}}}}}"#,
    );

    let add_backend = format!(
        r#"{{"id":"PROXY-MANAGER-3","content":{{"request_type":{{"ADD_BACKEND":{{"cluster_id":"app-1","backend_id":"app-1-backend-0","address":{{"ip":{{"inner":{{"V4":{LOCALHOST_V4}}}}},"port":8080}},"sticky_id":null,"load_balancing_parameters":null,"backup":null}}}}}}}}"#,
    );

    let mut payload = Vec::new();
    for record in [&add_cluster, &add_http_frontend, &add_backend] {
        payload.extend_from_slice(record.as_bytes());
        payload.extend_from_slice(b"\n\0");
    }
    payload
}

#[test]
fn deserialise_v1_1_1_state_file_records_through_parse_several() {
    let payload = v1_1_1_state_file_payload();

    let (rest, requests) = parse_several_requests::<WorkerRequest>(&payload)
        .expect("1.1.1-shaped payload must parse against current schema");

    assert!(
        rest.is_empty(),
        "leftover bytes after parsing: {} bytes — would fire 'Error consuming load state message'",
        rest.len(),
    );
    assert_eq!(requests.len(), 3, "expected 3 records parsed");
    assert_eq!(requests[0].id, "PROXY-MANAGER-1");
    assert_eq!(requests[1].id, "PROXY-MANAGER-2");
    assert_eq!(requests[2].id, "PROXY-MANAGER-3");

    // Defaults pinned for the post-1.1.1 repeated/map fields the older payload
    // omits — these are the actual fields the bug surfaced on.
    let cluster = match requests[0].content.request_type.as_ref() {
        Some(RequestType::AddCluster(c)) => c,
        other => panic!("expected ADD_CLUSTER, got {other:?}"),
    };
    assert_eq!(cluster.cluster_id, "app-1");
    assert!(
        cluster.answers.is_empty(),
        "Cluster.answers must default to empty"
    );
    assert!(
        cluster.authorized_hashes.is_empty(),
        "Cluster.authorized_hashes must default to empty",
    );

    let frontend = match requests[1].content.request_type.as_ref() {
        Some(RequestType::AddHttpFrontend(f)) => f,
        other => panic!("expected ADD_HTTP_FRONTEND, got {other:?}"),
    };
    assert_eq!(frontend.hostname, "example.com");
    assert!(
        frontend.headers.is_empty(),
        "RequestHttpFrontend.headers must default to empty",
    );
}

#[test]
fn load_state_buffer_loop_consumes_v1_1_1_payload() {
    // Mirrors the buffer shape of `bin/src/command/requests.rs::load_state`:
    // one chunk fed to `parse_several_requests`, no leftover bytes => no
    // "Error consuming load state message" branch.
    let payload = v1_1_1_state_file_payload();
    let (rest, requests) =
        parse_several_requests::<WorkerRequest>(&payload).expect("payload must parse");
    assert!(
        rest.is_empty(),
        "EOF + non-empty leftover would fail load_state"
    );
    assert_eq!(requests.len(), 3);
}

/// sozu-proxy/sozu#1279 added `sni` (optional) and `alpn` (repeated) to
/// `RequestTcpFrontend`. A pre-#1279 client emits an `ADD_TCP_FRONTEND`
/// record with neither key present at all; `alpn` needs
/// `#[serde(default)]` (see `command/build.rs`) for the same reason
/// `Cluster.answers` does above, or this record would be rejected with
/// "missing field `alpn`".
#[test]
fn legacy_tcp_frontend_without_sni_alpn_deserializes_with_defaults() {
    let record = format!(
        r#"{{"id":"PROXY-MANAGER-LEGACY-TCP","content":{{"request_type":{{"ADD_TCP_FRONTEND":{{"cluster_id":"app-1","address":{{"ip":{{"inner":{{"V4":{LOCALHOST_V4}}}}},"port":9000}},"tags":{{}}}}}}}}}}"#,
    );
    let mut payload = Vec::new();
    payload.extend_from_slice(record.as_bytes());
    payload.extend_from_slice(b"\n\0");

    let (rest, requests) = parse_several_requests::<WorkerRequest>(&payload)
        .expect("legacy (pre-#1279) TCP frontend record must parse against current schema");
    assert!(
        rest.is_empty(),
        "leftover bytes after parsing: {} bytes — would fire 'Error consuming load state message'",
        rest.len(),
    );
    assert_eq!(requests.len(), 1);

    let frontend = match requests[0].content.request_type.as_ref() {
        Some(RequestType::AddTcpFrontend(f)) => f,
        other => panic!("expected ADD_TCP_FRONTEND, got {other:?}"),
    };
    assert_eq!(frontend.cluster_id, "app-1");
    assert_eq!(
        frontend.sni, None,
        "sni must default to None for a legacy record that never carried it"
    );
    assert!(
        frontend.alpn.is_empty(),
        "RequestTcpFrontend.alpn must default to empty",
    );
}

#[test]
fn missing_required_scalar_still_fails() {
    // Strictness contract: a record without the required `cluster_id` scalar
    // must still be rejected. Without this guard, a future move to a
    // struct-level `#[serde(default)]` would silently default `cluster_id`
    // to "", and `ConfigState::add_cluster` would insert a `""`-keyed
    // cluster (no non-empty validation at the dispatch site).
    let bad = br#"{"id":"BAD","content":{"request_type":{"ADD_CLUSTER":{"sticky_session":false,"https_redirect":false,"load_balancing":0}}}}"# as &[u8];
    let mut payload = Vec::new();
    payload.extend_from_slice(bad);
    payload.extend_from_slice(b"\n\0");

    let (rest, requests) = parse_several_requests::<WorkerRequest>(&payload)
        .expect("nom returns Ok with leftover bytes when serde rejects a record");
    assert!(
        requests.is_empty(),
        "no record may be parsed when a required scalar is missing",
    );
    assert!(
        !rest.is_empty(),
        "the unparsed bytes must remain — load_state would surface them as the error",
    );
}

/// Listeners gained an optional `interface` (sozu-proxy/sozu#719), which is
/// part of their identity. A pre-#719 writer emits listener records with no
/// `interface` key at all; they must still load, as listeners without an
/// interface keyed by their bare address — exactly as before.
#[test]
fn legacy_listener_records_without_interface_load_as_bare_address_listeners() {
    let add_tcp_listener = format!(
        r#"{{"id":"LEGACY-LISTENER-1","content":{{"request_type":{{"ADD_TCP_LISTENER":{{"address":{{"ip":{{"inner":{{"V4":{LOCALHOST_V4}}}}},"port":9000}},"public_address":null,"expect_proxy":false,"front_timeout":60,"back_timeout":30,"connect_timeout":3,"active":false}}}}}}}}"#,
    );
    let activate_listener = format!(
        r#"{{"id":"LEGACY-LISTENER-2","content":{{"request_type":{{"ACTIVATE_LISTENER":{{"address":{{"ip":{{"inner":{{"V4":{LOCALHOST_V4}}}}},"port":9000}},"proxy":2,"from_scm":false}}}}}}}}"#,
    );
    let mut payload = Vec::new();
    for record in [&add_tcp_listener, &activate_listener] {
        payload.extend_from_slice(record.as_bytes());
        payload.extend_from_slice(b"\n\0");
    }

    let (rest, requests) = parse_several_requests::<WorkerRequest>(&payload)
        .expect("pre-#719 listener records must parse against the current schema");
    assert!(rest.is_empty(), "{} leftover bytes", rest.len());
    assert_eq!(requests.len(), 2);

    let mut state = ConfigState::new();
    for request in &requests {
        state
            .dispatch(&request.content)
            .expect("a pre-#719 listener record must dispatch");
    }
    let listeners = state.list_listeners();
    let listener = listeners
        .tcp_listeners
        .get("127.0.0.1:9000")
        .expect("the listener keeps its bare-address key");
    assert_eq!(listener.interface, None);
    assert!(listener.active, "the legacy ACTIVATE_LISTENER found it");
}

/// `ConfigState` crosses a main-process upgrade serialized as JSON
/// (`UpgradeData` in `bin/src/command/upgrade.rs`). The state an older main
/// writes keys its listener maps by bare address and carries no `interface`
/// field; the new main must rebuild the identical state from it.
#[test]
fn legacy_config_state_json_without_interface_deserializes() {
    let mut state = ConfigState::new();
    let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
    state
        .dispatch(
            &RequestType::AddHttpListener(HttpListenerConfig {
                address,
                sticky_name: "SOZUBALANCEID".to_owned(),
                front_timeout: 60,
                back_timeout: 30,
                connect_timeout: 3,
                request_timeout: 10,
                ..Default::default()
            })
            .into(),
        )
        .expect("add the listener");

    let mut json = serde_json::to_value(&state).expect("serialize the state");
    let listeners = json["http_listeners"]
        .as_object_mut()
        .expect("http_listeners is a JSON object");
    assert_eq!(
        listeners.keys().collect::<Vec<_>>(),
        vec!["127.0.0.1:8080"],
        "a listener without interface keeps the bare-address key an older main reads"
    );
    // What an older main writes: no `interface` field at all.
    for listener in listeners.values_mut() {
        listener
            .as_object_mut()
            .expect("a listener is a JSON object")
            .remove("interface")
            .expect("the current schema writes the interface field");
    }

    let legacy: ConfigState =
        serde_json::from_value(json).expect("a pre-#719 ConfigState must deserialize");
    assert!(
        legacy == state,
        "the legacy state must rebuild the same state"
    );
}
