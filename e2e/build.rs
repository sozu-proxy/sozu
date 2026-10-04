fn main() {
    println!("cargo:rerun-if-changed=proto/lifecycle_grpc.proto");

    #[cfg(feature = "grpc-e2e")]
    tonic_prost_build::configure()
        .compile_protos(&["proto/lifecycle_grpc.proto"], &["proto"])
        .expect("could not generate the gRPC lifecycle test service");
}
