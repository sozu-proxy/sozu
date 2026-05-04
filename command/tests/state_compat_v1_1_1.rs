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
    proto::command::{WorkerRequest, request::RequestType},
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
