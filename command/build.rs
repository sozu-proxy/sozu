pub fn main() {
    prost_build::Config::new()
        .btree_map(["."])
        .type_attribute(".", "#[derive(::serde::Serialize, ::serde::Deserialize)]")
        .message_attribute(".", "#[derive(Ord, PartialOrd)]")
        .message_attribute("Request", "#[derive(Eq)]")
        .message_attribute("RequestHttpFrontend", "#[derive(Hash, Eq)]")
        .message_attribute("RequestTcpFrontend", "#[derive(Hash, Eq)]")
        .message_attribute("CertificatesByAddress", "#[derive(Hash, Eq)]")
        .message_attribute("ResponseContent", "#[derive(Hash, Eq)]")
        .message_attribute("WorkerInfos", "#[derive(Hash, Eq)]")
        .message_attribute("AggregatedMetrics", "#[derive(Hash, Eq)]")
        .message_attribute("WorkerResponses", "#[derive(Hash, Eq)]")
        .message_attribute("ListedFrontends", "#[derive(Hash, Eq)]")
        .message_attribute("ClusterInformation", "#[derive(Hash, Eq)]")
        .message_attribute("ClusterInformations", "#[derive(Hash, Eq)]")
        .message_attribute("ClusterHashes", "#[derive(Hash, Eq)]")
        .message_attribute("ListenersList", "#[derive(Hash, Eq)]")
        .message_attribute("WorkerMetrics", "#[derive(Hash, Eq)]")
        .message_attribute("ListOfCertificatesByAddress", "#[derive(Hash, Eq)]")
        .message_attribute("CertificatesWithFingerprints", "#[derive(Hash, Eq)]")
        .message_attribute("RequestCounts", "#[derive(Hash, Eq)]")
        .message_attribute("HealthChecksList", "#[derive(Hash, Eq)]")
        .message_attribute("FilteredMetrics", "#[derive(Hash, Eq)]")
        .message_attribute("ClusterMetrics", "#[derive(Hash, Eq)]")
        .message_attribute("BackendMetrics", "#[derive(Hash, Eq)]")
        .message_attribute("FilteredHistogram", "#[derive(Hash, Eq)]")
        .message_attribute("WorkerRequest", "#[derive(Hash, Eq)]")
        .message_attribute("WorkerResponse", "#[derive(Hash, Eq)]")
        .message_attribute("Request", "#[derive(Hash)]")
        .message_attribute("Response", "#[derive(Hash, Eq)]")
        .message_attribute("InitialState", "#[derive(Hash, Eq)]")
        .message_attribute("ProtobufAccessLog", "#[derive(Hash, Eq)]")
        // The messages below carry at least one `map<string, string>` field
        // (`tags`, `answers`) or a nested `Header` value. prost stops
        // auto-deriving `Hash`/`Eq` once a map appears, and downstream code
        // (state-diff hashing, IPC framing, request fan-out) needs both
        // traits, so re-attach them explicitly here.
        .message_attribute("Cluster", "#[derive(Hash, Eq)]")
        .message_attribute("HttpListenerConfig", "#[derive(Hash, Eq)]")
        .message_attribute("HttpsListenerConfig", "#[derive(Hash, Eq)]")
        .message_attribute("UpdateHttpListenerConfig", "#[derive(Hash, Eq)]")
        .message_attribute("UpdateHttpsListenerConfig", "#[derive(Hash, Eq)]")
        // JSON state-file forward compat: `SaveState`/`LoadState` files are
        // JSON-encoded `WorkerRequest` records. Without `#[serde(default)]`,
        // serde rejects records that don't carry every `Vec`/`map` field — so
        // a payload written by an older `sozu-command-lib` (e.g. 1.1.1) fails
        // to deserialize once a new `repeated`/`map` field lands on a message
        // that older clients still emit. The annotations below mirror the
        // protobuf wire-format default (empty repeated, empty map) for the
        // post-1.1.1 additions on every state-file-emittable message.
        // Required scalars stay strict on purpose: `add_cluster` keys the
        // cluster map by `cluster_id` without a non-empty check, so a
        // struct-level blanket would silently insert a bogus `""`-keyed
        // entry. Whenever a new `repeated`/`map` field is added to a message
        // that appears in `ConfigState::generate_requests` (or in any
        // `RequestType` an external client builds and feeds through
        // `LoadState`), append the matching `field_attribute` line here.
        // Regression guard: `command/tests/state_compat_v1_1_1.rs`.
        .field_attribute("Cluster.answers", "#[serde(default)]")
        .field_attribute("Cluster.authorized_hashes", "#[serde(default)]")
        .field_attribute("RequestHttpFrontend.headers", "#[serde(default)]")
        .field_attribute("HttpListenerConfig.answers", "#[serde(default)]")
        .field_attribute("HttpsListenerConfig.alpn_protocols", "#[serde(default)]")
        .field_attribute("HttpsListenerConfig.answers", "#[serde(default)]")
        .field_attribute("TcpListenerConfig.answers", "#[serde(default)]")
        .enum_attribute(".", "#[serde(rename_all = \"SCREAMING_SNAKE_CASE\")]")
        .enum_attribute(
            "ResponseContent.content_type",
            "#[derive(Hash, Eq, Ord, PartialOrd)]",
        )
        .enum_attribute(
            "FilteredMetrics.inner",
            "#[derive(Hash, Eq, Ord, PartialOrd)]",
        )
        .enum_attribute("IpAddress.inner", "#[derive(Ord, PartialOrd)]")
        .enum_attribute("ProtobufEndpoint.inner", "#[derive(Ord, PartialOrd)]")
        .enum_attribute(
            "Request.request_type",
            "#[allow(clippy::large_enum_variant)]",
        )
        .enum_attribute(
            "Request.request_type",
            "#[derive(Hash, Eq, Ord, PartialOrd)]",
        )
        .format(true)
        .out_dir("src/proto")
        .compile_protos(&["command.proto"], &["src"])
        .expect("Could not compile protobuf types in command.proto");
}
