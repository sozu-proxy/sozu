//! Crypto provider abstraction for rustls.
//!
//! This module re-exports the crypto provider selected at compile time via feature flags:
//! - `crypto-ring`: uses the `ring` crypto provider (default in `sozu` binary)
//! - `crypto-aws-lc-rs`: uses the `aws-lc-rs` crypto provider (FIPS-capable)
//! - `crypto-openssl`: uses the `rustls-openssl` crypto provider (OpenSSL-backed)
//!
//! Exactly one provider must be active at a time.

#[cfg(not(any(
    feature = "crypto-ring",
    feature = "crypto-aws-lc-rs",
    feature = "crypto-openssl"
)))]
compile_error!(
    "No crypto provider selected. Enable one of: `crypto-ring`, `crypto-aws-lc-rs`, or `crypto-openssl`."
);

#[cfg(any(
    all(feature = "crypto-ring", feature = "crypto-aws-lc-rs"),
    all(feature = "crypto-ring", feature = "crypto-openssl"),
    all(feature = "crypto-aws-lc-rs", feature = "crypto-openssl"),
))]
compile_error!(
    "Multiple crypto providers selected. Enable exactly one of: `crypto-ring`, `crypto-aws-lc-rs`, or `crypto-openssl`."
);

#[cfg(feature = "crypto-ring")]
pub use rustls::crypto::ring::{
    cipher_suite::{
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
    },
    default_provider,
    sign::any_supported_type,
};

#[cfg(feature = "crypto-aws-lc-rs")]
pub use rustls::crypto::aws_lc_rs::{
    cipher_suite::{
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
    },
    default_provider,
    sign::any_supported_type,
};

#[cfg(feature = "crypto-openssl")]
pub use rustls_openssl::{
    cipher_suite::{
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
    },
    default_provider,
};

/// Load a private key into a signing key.
///
/// For `ring` and `aws-lc-rs`, this delegates to `sign::any_supported_type`.
/// For `rustls-openssl`, this delegates to `KeyProvider::load_private_key`.
#[cfg(feature = "crypto-openssl")]
pub fn any_supported_type(
    der: &rustls::pki_types::PrivateKeyDer<'_>,
) -> Result<std::sync::Arc<dyn rustls::sign::SigningKey>, rustls::Error> {
    use rustls::crypto::KeyProvider;
    rustls_openssl::KeyProvider.load_private_key(der.clone_key())
}
