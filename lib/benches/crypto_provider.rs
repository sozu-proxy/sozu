use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use rustls::{ServerConfig, crypto::CryptoProvider};
use sozu_command_lib::proto::command::{AddCertificate, CertificateAndKey, SocketAddress};
use sozu_lib::{
    crypto::{default_provider, any_supported_type},
    tls::{CertifiedKeyWrapper, CertificateResolver, MutexCertificateResolver},
};

const CERT_PEM: &str = include_str!("../assets/certificate.pem");
const CHAIN_PEM: &str = include_str!("../assets/certificate_chain.pem");
const KEY_PEM: &str = include_str!("../assets/key.pem");

fn bench_default_provider(c: &mut Criterion) {
    c.bench_function("default_provider", |b| {
        b.iter(|| default_provider());
    });
}

fn bench_certificate_loading(c: &mut Criterion) {
    let add = AddCertificate {
        certificate: CertificateAndKey {
            certificate: CERT_PEM.to_string(),
            certificate_chain: vec![CHAIN_PEM.to_string()],
            key: KEY_PEM.to_string(),
            ..Default::default()
        },
        address: SocketAddress::new_v4(127, 0, 0, 1, 8080),
        expired_at: None,
    };

    c.bench_function("certificate_load", |b| {
        b.iter(|| CertifiedKeyWrapper::try_from(&add).unwrap());
    });
}

fn bench_private_key_signing(c: &mut Criterion) {
    use rustls::pki_types::PrivateKeyDer;
    use std::io::BufReader;

    let mut reader = BufReader::new(KEY_PEM.as_bytes());
    let item = rustls_pemfile::read_one(&mut reader).unwrap().unwrap();
    let private_key = match item {
        rustls_pemfile::Item::Pkcs1Key(k) => PrivateKeyDer::from(k),
        rustls_pemfile::Item::Pkcs8Key(k) => PrivateKeyDer::from(k),
        rustls_pemfile::Item::Sec1Key(k) => PrivateKeyDer::from(k),
        _ => panic!("unexpected key type"),
    };

    c.bench_function("private_key_load", |b| {
        b.iter(|| any_supported_type(&private_key).unwrap());
    });
}

fn bench_server_config_build(c: &mut Criterion) {
    let resolver = Arc::new(MutexCertificateResolver::default());

    c.bench_function("server_config_build", |b| {
        let resolver = resolver.clone();
        b.iter(|| {
            let provider = default_provider();
            ServerConfig::builder_with_provider(Arc::new(provider))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_cert_resolver(resolver.clone())
        });
    });
}

fn bench_certificate_resolver_add(c: &mut Criterion) {
    let add = AddCertificate {
        certificate: CertificateAndKey {
            certificate: CERT_PEM.to_string(),
            certificate_chain: vec![CHAIN_PEM.to_string()],
            key: KEY_PEM.to_string(),
            ..Default::default()
        },
        address: SocketAddress::new_v4(127, 0, 0, 1, 8080),
        expired_at: None,
    };

    c.bench_function("resolver_add_remove", |b| {
        b.iter(|| {
            let mut resolver = CertificateResolver::default();
            let fp = resolver.add_certificate(&add).unwrap();
            resolver.remove_certificate(&fp).unwrap();
        });
    });
}

criterion_group!(
    benches,
    bench_default_provider,
    bench_certificate_loading,
    bench_private_key_signing,
    bench_server_config_build,
    bench_certificate_resolver_add,
);
criterion_main!(benches);
