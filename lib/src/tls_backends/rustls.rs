use std::sync::Arc;

use crate::tls_backends::{MutexWrappedCertificateResolver, DEFAULT_CERTIFICATE};

use rustls::{
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};

impl ResolvesServerCert for MutexWrappedCertificateResolver {
    fn resolve(&self, client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        let server_name = client_hello.server_name();
        let sigschemes = client_hello.signature_schemes();

        if server_name.is_none() {
            error!("cannot look up certificate: no SNI from session");
            return None;
        }

        let name: &str = server_name.unwrap();
        trace!(
            "trying to resolve name: {:?} for signature scheme: {:?}",
            name,
            sigschemes
        );
        if let Ok(ref mut resolver) = self.0.try_lock() {
            //resolver.domains.print();
            if let Some((_, fingerprint)) = resolver.domains.domain_lookup(name.as_bytes(), true) {
                trace!(
                    "looking for certificate for {:?} with fingerprint {:?}",
                    name,
                    fingerprint
                );
                return resolver
                    .certificates
                    .get(fingerprint)
                    .and_then(Self::generate_certified_key)
                    .map(Arc::new);
            }
        }

        // error!("could not look up a certificate for server name '{}'", name);
        // This certificate is used for TLS tunneling with another TLS termination endpoint
        // Note that this is unsafe and you should provide a valid certificate
        debug!("Default certificate is used for {}", name);
        incr!("tls.default_cert_used");
        Self::generate_certified_key(&DEFAULT_CERTIFICATE).map(Arc::new)
    }
}
