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

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::NamedGroup;

    #[test]
    fn default_provider_has_tls13_cipher_suites() {
        let provider = default_provider();
        let names: Vec<_> = provider.cipher_suites.iter().map(|cs| cs.suite()).collect();
        assert!(
            names.contains(&rustls::CipherSuite::TLS13_AES_256_GCM_SHA384),
            "provider must support TLS13_AES_256_GCM_SHA384"
        );
        assert!(
            names.contains(&rustls::CipherSuite::TLS13_AES_128_GCM_SHA256),
            "provider must support TLS13_AES_128_GCM_SHA256"
        );
        assert!(
            names.contains(&rustls::CipherSuite::TLS13_CHACHA20_POLY1305_SHA256),
            "provider must support TLS13_CHACHA20_POLY1305_SHA256"
        );
    }

    #[test]
    fn default_provider_has_tls12_cipher_suites() {
        let provider = default_provider();
        let names: Vec<_> = provider.cipher_suites.iter().map(|cs| cs.suite()).collect();
        assert!(
            names.contains(&rustls::CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384),
            "provider must support TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384"
        );
        assert!(
            names.contains(&rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384),
            "provider must support TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384"
        );
    }

    #[test]
    fn default_provider_has_classical_kx_groups() {
        let provider = default_provider();
        let groups: Vec<_> = provider.kx_groups.iter().map(|g| g.name()).collect();
        assert!(
            groups.contains(&NamedGroup::X25519),
            "provider must support X25519 key exchange"
        );
        assert!(
            groups.contains(&NamedGroup::secp256r1),
            "provider must support secp256r1 key exchange"
        );
        assert!(
            groups.contains(&NamedGroup::secp384r1),
            "provider must support secp384r1 key exchange"
        );
    }

    #[cfg(feature = "crypto-aws-lc-rs")]
    #[test]
    fn aws_lc_rs_supports_post_quantum_kx() {
        let provider = default_provider();
        let groups: Vec<_> = provider.kx_groups.iter().map(|g| g.name()).collect();
        assert!(
            groups.contains(&NamedGroup::X25519MLKEM768),
            "aws-lc-rs provider must support X25519MLKEM768 post-quantum key exchange"
        );
    }

    #[cfg(feature = "crypto-aws-lc-rs")]
    #[test]
    fn aws_lc_rs_has_more_kx_groups_than_classical() {
        let provider = default_provider();
        // aws-lc-rs should have more kx groups than just the 3 classical ones
        // (X25519, secp256r1, secp384r1) because it also includes PQ groups
        assert!(
            provider.kx_groups.len() > 3,
            "aws-lc-rs should have more than 3 kx groups (has {}), including post-quantum",
            provider.kx_groups.len()
        );
    }

    #[cfg(feature = "crypto-ring")]
    #[test]
    fn ring_has_no_mlkem() {
        let provider = default_provider();
        let groups: Vec<_> = provider.kx_groups.iter().map(|g| g.name()).collect();
        assert!(
            !groups.contains(&NamedGroup::X25519MLKEM768),
            "ring provider should not advertise X25519MLKEM768"
        );
    }

    #[cfg(feature = "crypto-openssl")]
    #[test]
    fn openssl_supports_post_quantum_kx() {
        let provider = default_provider();
        let groups: Vec<_> = provider.kx_groups.iter().map(|g| g.name()).collect();
        assert!(
            groups.contains(&NamedGroup::X25519MLKEM768),
            "openssl provider should support X25519MLKEM768 post-quantum key exchange"
        );
    }

    #[test]
    fn can_load_rsa_private_key() {
        use rustls::pki_types::PrivateKeyDer;
        use std::io::BufReader;

        let key_pem = include_str!("../assets/key.pem");
        let mut reader = BufReader::new(key_pem.as_bytes());
        let item = rustls_pemfile::read_one(&mut reader)
            .expect("failed to read PEM")
            .expect("no PEM item found");
        let private_key = match item {
            rustls_pemfile::Item::Pkcs1Key(k) => PrivateKeyDer::from(k),
            rustls_pemfile::Item::Pkcs8Key(k) => PrivateKeyDer::from(k),
            rustls_pemfile::Item::Sec1Key(k) => PrivateKeyDer::from(k),
            _ => panic!("unexpected key type"),
        };
        any_supported_type(&private_key).expect("provider must be able to load RSA private key");
    }

    #[test]
    fn can_build_server_config_with_tls13() {
        use std::sync::Arc;

        let provider = default_provider();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("failed to build TLS 1.3 config")
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(crate::tls::MutexCertificateResolver::default()));
        assert!(
            !config.alpn_protocols.contains(&b"h2".to_vec()),
            "default config should not have ALPN set"
        );
    }

    #[test]
    fn can_build_server_config_with_tls12_and_tls13() {
        use std::sync::Arc;

        let provider = default_provider();
        rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
            .expect("failed to build TLS 1.2+1.3 config")
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(crate::tls::MutexCertificateResolver::default()));
    }

    #[cfg(feature = "crypto-aws-lc-rs")]
    #[test]
    fn pq_kx_compatible_with_server_config() {
        use std::sync::Arc;

        let provider = default_provider();
        // Verify the PQ kx group is present
        let has_pq = provider
            .kx_groups
            .iter()
            .any(|g| g.name() == NamedGroup::X25519MLKEM768);
        assert!(has_pq, "X25519MLKEM768 must be in the provider");

        // Build a TLS 1.3 config (PQ kx is only for TLS 1.3)
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3 config with PQ kx should build successfully")
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(crate::tls::MutexCertificateResolver::default()));

        // The config should be valid
        assert!(
            !config.alpn_protocols.contains(&b"h2".to_vec()),
            "default config should not have ALPN set"
        );
    }
}
