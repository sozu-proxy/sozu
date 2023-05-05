pub fn main() {
    prost_build::Config::new()
        .btree_map(["."])
        .message_attribute(".", "#[derive(Hash, Eq, Ord, PartialOrd)]")
        .type_attribute(".", "#[derive(::serde::Serialize, ::serde::Deserialize)]")
        .enum_attribute(".", "#[serde(rename_all = \"SCREAMING_SNAKE_CASE\")]")
        .enum_attribute("Order", "#[derive(Hash, Eq, Ord, PartialOrd)]")
        .enum_attribute("request_type", "#[derive(Hash, Eq, Ord, PartialOrd)]")
        .enum_attribute("inner", "#[derive(Hash, Eq, Ord, PartialOrd)]")
        .enum_attribute("content_type", "#[derive(Hash, Eq, Ord, PartialOrd)]")
        .out_dir("src/proto")
        .compile_protos(&["command.proto"], &["src"])
        .expect("Could not compile protobuf types in command.proto");
}
