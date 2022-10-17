use std::env;

fn main() {
    if cfg!(feature = "use-openssl") {
        match &env::var("DEP_OPENSSL_VERSION") {
            Ok(v) if v == "101" || v == "102" => {
                println!("cargo:rustc-cfg=ossl{}", v);
                println!("cargo:rustc-cfg=ossl10x");
            }
            Ok(v) if v == "110" || v == "111" => {
                println!("cargo:rustc-cfg=ossl{}", v);
                println!("cargo:rustc-cfg=ossl11x");
            }
            Ok(v) if v == "300" => {
                println!("cargo:rustc-cfg=ossl300");
                println!("cargo:rustc-cfg=ossl30x");
            }
            // Default to OpenSSL v3.0.x
            _ => {
                println!("cargo:rustc-cfg=ossl300");
                println!("cargo:rustc-cfg=ossl30x");
            }
        }

        if env::var("DEP_OPENSSL_LIBRESSL").is_ok() {
            println!("cargo:rustc-cfg=libressl");
        }

        if let Ok(vars) = env::var("DEP_OPENSSL_CONF") {
            for var in vars.split(',') {
                println!("cargo:rustc-cfg=osslconf=\"{}\"", var);
            }
        }
    }
}
