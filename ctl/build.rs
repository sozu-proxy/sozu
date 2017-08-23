use std::env;

// Configuration defaults.

fn main(){
    // Export defaults as compile time environment variables.
    // These variables are to be set by package managers in their build script.
    // `export SOZU_CONFIG=<default configuration file> && cargo build`.
    if let Ok(config) = env::var("SOZU_CONFIG") {
        println!("cargo:rustc-env={}={}", "SOZU_CONFIG", config);
    }
}