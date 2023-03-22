use prost_build;

// fn main() {
//     prost_build::compile_protos(&["src/protocommand.proto"], &["src/"]).unwrap();
// }

pub fn main() {
    let mut config = prost_build::Config::new();

    // config.btree_map(["."]);
    config.message_attribute(".", "#[derive(Hash, Eq)]");
    config.enum_attribute("Order", "#[derive(Hash, Eq)]");
    config.enum_attribute("inner", "#[derive(Hash, Eq)]");
    // config.enum_attribute("worker_order.Inner", "#[derive(Hash, Eq)]");

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .type_attribute(".", "#[derive(::serde::Serialize, ::serde::Deserialize)]")
        .type_attribute(".", "#[serde(rename_all = \"snake_case\")]")
        .out_dir("src/proto")
        .compile_with_config(config, &["command.proto"], &["src"])
        .unwrap();
}
