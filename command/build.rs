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
            "#[derive(Hash, Eq, Ord, PartialOrd)]",
        )
        .out_dir("src/proto")
        .compile_protos(&["command.proto"], &["src"])
        .expect("Could not compile protobuf types in command.proto");
}
