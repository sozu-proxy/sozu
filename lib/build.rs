use std::env;

fn main() {
    match &env::var("DEP_OPENSSL_VERSION") {
        Ok(v) if v == "101" || v == "102" => {
            println!("cargo:rustc-cfg=ossl{}", v);
            println!("cargo:rustc-cfg=ossl10x");
        }
        Ok(v) if v == "110" || v == "111" => {
            println!("cargo:rustc-cfg=ossl{}", v);
            println!("cargo:rustc-cfg=ossl11x");
        }
        _ => {
            println!("cargo:rustc-cfg=ossl111");
            println!("cargo:rustc-cfg=ossl11x");
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
