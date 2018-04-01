use std::env;

fn main(){
    // Export defaults as compile time environment variables.
    // These variables are to be set by package managers in their build script.
    // `export SOZU_CONFIG=<default configuration file> && cargo build`.
    let variables = vec!("SOZU_CONFIG", "SOZU_PID_FILE_PATH");
    for variable in variables {
        if let Ok(val) = env::var(variable) {
            println!("cargo:rustc-env={}={}", variable, val);
        }
    }
}
