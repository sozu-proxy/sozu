use std::env;

fn main() {
    match env::var("DEP_OPENSSL_VERSION") {
        Ok(ref v) if v == "101" => {
            println!("cargo:rustc-cfg=ossl101");
            println!("cargo:rustc-cfg=ossl10x");
        }
        Ok(ref v) if v == "102" => {
            println!("cargo:rustc-cfg=ossl102");
            println!("cargo:rustc-cfg=ossl10x");
        }
        Ok(ref v) if v == "110" => {
            println!("cargo:rustc-cfg=ossl110");
        }
        _ => {
            println!("cargo:rustc-cfg=ossl110");
        } //panic!("Unable to detect OpenSSL version"),
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
