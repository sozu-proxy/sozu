//! A unified certificate resolver for rustls.
//!
//! Persists certificates in the Rustls
//! [`CertifiedKey` format](https://docs.rs/rustls/latest/rustls/sign/struct.CertifiedKey.html),
//! exposes them to the HTTPS listener for TLS handshakes.
use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::BufReader,
    str::FromStr,
    sync::{Arc, Mutex},
};

use once_cell::sync::Lazy;
use rustls::{
    crypto::ring::sign::any_supported_type,
    pki_types::{CertificateDer, PrivateKeyDer},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use sha2::{Digest, Sha256};
use sozu_command::{
    certificate::{
        get_cn_and_san_attributes, parse_pem, parse_x509, CertificateError, Fingerprint,
    },
    proto::command::{AddCertificate, CertificateAndKey, ReplaceCertificate, SocketAddress},
};

use crate::router::trie::{Key, KeyValue, TrieNode};

// -----------------------------------------------------------------------------
// Default ParsedCertificateAndKey

static DEFAULT_CERTIFICATE: Lazy<Option<Arc<CertifiedKey>>> = Lazy::new(|| {
    let add = AddCertificate {
        certificate: CertificateAndKey {
            certificate: include_str!("../assets/certificate.pem").to_string(),
            certificate_chain: vec![include_str!("../assets/certificate_chain.pem").to_string()],
            key: include_str!("../assets/key.pem").to_string(),
            versions: vec![],
            names: vec![],
        },
        address: SocketAddress::new_v4(0, 0, 0, 0, 8080), // not used anyway
        expired_at: None,
    };
    CertifiedKeyWrapper::try_from(&add).ok().map(|c| c.inner)
});

#[derive(thiserror::Error, Debug)]
pub enum CertificateResolverError {
    #[error("failed to get common name and subject alternate names from pem, {0}")]
    InvalidCommonNameAndSubjectAlternateNames(CertificateError),
    #[error("invalid private key: {0}")]
    InvalidPrivateKey(String),
    #[error("empty key")]
    EmptyKeys,
    #[error("error parsing x509 cert from bytes: {0}")]
    ParseX509(CertificateError),
    #[error("error parsing pem formated certificate from bytes: {0}")]
    ParsePem(CertificateError),
    #[error("error parsing overriding names in new certificate: {0}")]
    ParseOverridingNames(CertificateError),
}

/// A wrapper around the Rustls
/// [`CertifiedKey` type](https://docs.rs/rustls/latest/rustls/sign/struct.CertifiedKey.html),
/// stored and returned by the certificate resolver.
#[derive(Clone, Debug)]
pub struct CertifiedKeyWrapper {
    inner: Arc<CertifiedKey>,
    /// domain names, override what can be found in the cert
    names: Vec<String>,
    expiration: i64,
    fingerprint: Fingerprint,
}

/// Convert an AddCertificate request into the Rustls format.
/// Support RSA and ECDSA certificates.
impl TryFrom<&AddCertificate> for CertifiedKeyWrapper {
    type Error = CertificateResolverError;

    fn try_from(add: &AddCertificate) -> Result<Self, Self::Error> {
        let cert = add.certificate.clone();

        let pem =
            parse_pem(cert.certificate.as_bytes()).map_err(CertificateResolverError::ParsePem)?;

        let x509 = parse_x509(&pem.contents).map_err(CertificateResolverError::ParseX509)?;

        let overriding_names = if add.certificate.names.is_empty() {
            get_cn_and_san_attributes(&x509)
        } else {
            add.certificate.names.clone()
        };

        let expiration = add
            .expired_at
            .unwrap_or(x509.validity().not_after.timestamp());

        let fingerprint = Fingerprint(Sha256::digest(&pem.contents).iter().cloned().collect());

        let mut chain = vec![CertificateDer::from(pem.contents)];
        for cert in &cert.certificate_chain {
            let chain_link = parse_pem(cert.as_bytes())
                .map_err(CertificateResolverError::ParsePem)?
                .contents;

            chain.push(CertificateDer::from(chain_link));
        }

        let mut key_reader = BufReader::new(cert.key.as_bytes());

        let item = match rustls_pemfile::read_one(&mut key_reader)
            .map_err(|_| CertificateResolverError::EmptyKeys)?
        {
            Some(item) => item,
            None => return Err(CertificateResolverError::EmptyKeys),
        };

        let private_key = match item {
            rustls_pemfile::Item::Pkcs1Key(rsa_key) => PrivateKeyDer::from(rsa_key),
            rustls_pemfile::Item::Pkcs8Key(pkcs8_key) => PrivateKeyDer::from(pkcs8_key),
            rustls_pemfile::Item::Sec1Key(ec_key) => PrivateKeyDer::from(ec_key),
            _ => return Err(CertificateResolverError::EmptyKeys),
        };

        match any_supported_type(&private_key) {
            Ok(signing_key) => {
                let stored_certificate = CertifiedKeyWrapper {
                    inner: Arc::new(CertifiedKey::new(chain, signing_key)),
                    names: overriding_names,
                    expiration,
                    fingerprint,
                };
                Ok(stored_certificate)
            }
            Err(sign_error) => Err(CertificateResolverError::InvalidPrivateKey(
                sign_error.to_string(),
            )),
        }
    }
}

/// Parses and stores TLS certificates, makes them available to Rustls for TLS handshakes
#[derive(Default, Debug)]
pub struct CertificateResolver {
    /// all fingerprints of all
    pub domains: TrieNode<Fingerprint>,
    /// a map of fingerprint -> stored_certificate
    certificates: HashMap<Fingerprint, CertifiedKeyWrapper>,
    /// map of domain_name -> all fingerprints linked to this domain name
    name_fingerprint_idx: HashMap<String, HashSet<Fingerprint>>,
}

impl CertificateResolver {
    /// return the certificate in the Rustls-usable form
    pub fn get_certificate(&self, fingerprint: &Fingerprint) -> Option<CertifiedKeyWrapper> {
        self.certificates.get(fingerprint).map(ToOwned::to_owned)
    }

    /// persist a certificate, after ensuring validity, and checking if it can replace another certificate
    pub fn add_certificate(
        &mut self,
        add: &AddCertificate,
    ) -> Result<Fingerprint, CertificateResolverError> {
        let cert_to_add = CertifiedKeyWrapper::try_from(add)?;

        let (should_insert, outdated_certs) = self.should_insert(&cert_to_add)?;

        if !should_insert {
            // if we do not need to insert the fingerprint just return the fingerprint
            return Ok(cert_to_add.fingerprint);
        }

        for new_name in &cert_to_add.names {
            self.domains.remove(&new_name.to_owned().into_bytes());

            self.domains.insert(
                new_name.to_owned().into_bytes(),
                cert_to_add.fingerprint.to_owned(),
            );

            self.name_fingerprint_idx
                .entry(new_name.to_owned())
                .or_default()
                .insert(cert_to_add.fingerprint.to_owned());
        }

        for name in &cert_to_add.names {
            if let Some(fingerprints) = self.name_fingerprint_idx.get_mut(name) {
                for outdated in &outdated_certs {
                    fingerprints.remove(outdated);
                }
            }
        }

        for outdated in &outdated_certs {
            self.certificates.remove(outdated);
        }

        self.certificates
            .insert(cert_to_add.fingerprint.to_owned(), cert_to_add.clone());

        Ok(cert_to_add.fingerprint.to_owned())
    }

    /// Delete a certificate from the resolver. May fail if there is no alternative for
    // a domain name
    pub fn remove_certificate(
        &mut self,
        fingerprint: &Fingerprint,
    ) -> Result<(), CertificateResolverError> {
        if let Some(certificate_to_remove) = self.get_certificate(fingerprint) {
            for name in certificate_to_remove.names {
                if let Some(fingerprints) = self.name_fingerprint_idx.get_mut(&name) {
                    fingerprints.remove(fingerprint);

                    self.domains.domain_remove(&name.clone().into_bytes());

                    if let Some(fingerprint) = fingerprints.iter().next() {
                        self.domains
                            .insert(name.into_bytes(), fingerprint.to_owned());
                    }
                }
            }

            self.certificates.remove(fingerprint);
        }

        Ok(())
    }

    /// Short-hand for `add_certificate` and then `remove_certificate`.
    /// It is possible that the certificate will not be replaced, if the
    /// new certificate does not match `add_certificate` rules.
    pub fn replace_certificate(
        &mut self,
        replace: &ReplaceCertificate,
    ) -> Result<Fingerprint, CertificateResolverError> {
        match Fingerprint::from_str(&replace.old_fingerprint) {
            Ok(old_fingerprint) => self.remove_certificate(&old_fingerprint)?,
            Err(err) => {
                error!("failed to parse fingerprint, {}", err);
            }
        }

        self.add_certificate(&AddCertificate {
            address: replace.address.to_owned(),
            certificate: replace.new_certificate.to_owned(),
            expired_at: replace.new_expired_at.to_owned(),
        })
    }

    /// return all fingerprints that are available for these domain names,
    /// provided at least one name is given
    #[cfg(test)]
    fn find_certificates_by_names(
        &self,
        names: &HashSet<String>,
    ) -> Result<HashSet<Fingerprint>, CertificateResolverError> {
        let mut fingerprints = HashSet::new();
        for name in names {
            if let Some(fprints) = self.name_fingerprint_idx.get(name) {
                fprints.iter().for_each(|fingerprint| {
                    fingerprints.insert(fingerprint.to_owned());
                });
            }
        }

        Ok(fingerprints)
    }

    /// return the hashset of subjects that the certificate is able to handle
    #[cfg(test)]
    fn certificate_names(
        &self,
        fingerprint: &Fingerprint,
    ) -> Result<HashSet<String>, CertificateResolverError> {
        if let Some(cert) = self.certificates.get(fingerprint) {
            return Ok(cert.names.iter().cloned().collect());
        }
        Ok(HashSet::new())
    }

    /// check the certificate expiration and related certificates,
    /// return a list of outdated certificates that should be removed
    fn should_insert(
        &self,
        candidate_cert: &CertifiedKeyWrapper,
    ) -> Result<(bool, Vec<Fingerprint>), CertificateResolverError> {
        let mut should_insert = false;

        let mut related_certificates = HashSet::new();

        for name in &candidate_cert.names {
            match self.name_fingerprint_idx.get(name) {
                None => should_insert = true,
                Some(fingerprints) if fingerprints.is_empty() => should_insert = true,
                Some(fingerprints) => related_certificates.extend(fingerprints),
            }
        }

        let mut outdated_certificates = Vec::new();

        for fingerprint in related_certificates {
            let related_certificate = match self.certificates.get(fingerprint) {
                Some(cert) => cert,
                None => {
                    error!("certificates and fingerprint hashmaps are desynchronized");
                    continue;
                }
            };

            if related_certificate.expiration > candidate_cert.expiration {
                continue;
            }

            for name in &related_certificate.names {
                if !candidate_cert.names.contains(name) {
                    continue;
                }
            }

            should_insert = true;

            outdated_certificates.push(fingerprint.clone());
        }

        Ok((should_insert, outdated_certificates))
    }

    pub fn domain_lookup(
        &self,
        domain: &[u8],
        accept_wildcard: bool,
    ) -> Option<&KeyValue<Key, Fingerprint>> {
        self.domains.domain_lookup(domain, accept_wildcard)
    }
}

// -----------------------------------------------------------------------------
// MutexWrappedCertificateResolver struct

#[derive(Default)]
pub struct MutexCertificateResolver(pub Mutex<CertificateResolver>);

impl ResolvesServerCert for MutexCertificateResolver {
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

                let cert = resolver
                    .certificates
                    .get(fingerprint)
                    .map(|cert| cert.inner.clone());

                trace!("Found for fingerprint {}: {}", fingerprint, cert.is_some());
                return cert;
            }
        }

        // error!("could not look up a certificate for server name '{}'", name);
        // This certificate is used for TLS tunneling with another TLS termination endpoint
        // Note that this is unsafe and you should provide a valid certificate
        debug!("Default certificate is used for {}", name);
        incr!("tls.default_cert_used");
        DEFAULT_CERTIFICATE.clone()
    }
}

impl fmt::Debug for MutexCertificateResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MutexWrappedCertificateResolver")
    }
}

// -----------------------------------------------------------------------------
// Unit tests

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        error::Error,
        time::{Duration, SystemTime},
    };

    use super::CertificateResolver;

    use rand::{seq::SliceRandom, thread_rng};
    use sozu_command::proto::command::{AddCertificate, CertificateAndKey, SocketAddress};

    #[test]
    fn lifecycle() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();
        let certificate_and_key = CertificateAndKey {
            certificate: String::from(include_str!("../assets/certificate.pem")),
            key: String::from(include_str!("../assets/key.pem")),
            ..Default::default()
        };

        let fingerprint = resolver
            .add_certificate(&AddCertificate {
                address,
                certificate: certificate_and_key,
                expired_at: None,
            })
            .expect("could not add certificate");

        if resolver.get_certificate(&fingerprint).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        if let Err(err) = resolver.remove_certificate(&fingerprint) {
            return Err(format!("the certificate must not been removed, {err}").into());
        }

        let names = resolver.certificate_names(&fingerprint)?;
        if !resolver.find_certificates_by_names(&names)?.is_empty()
            && resolver.get_certificate(&fingerprint).is_some()
        {
            return Err("We have retrieve the certificate that should be deleted".into());
        }

        Ok(())
    }

    #[test]
    fn name_override() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();
        let certificate_and_key = CertificateAndKey {
            certificate: String::from(include_str!("../assets/certificate.pem")),
            key: String::from(include_str!("../assets/key.pem")),
            names: vec!["localhost".into(), "lolcatho.st".into()],
            ..Default::default()
        };

        let fingerprint = resolver.add_certificate(&AddCertificate {
            address,
            certificate: certificate_and_key,
            expired_at: None,
        })?;

        if resolver.get_certificate(&fingerprint).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        let mut lolcat = HashSet::new();
        lolcat.insert(String::from("lolcatho.st"));
        if resolver.find_certificates_by_names(&lolcat)?.is_empty()
            || resolver.get_certificate(&fingerprint).is_none()
        {
            return Err("failed to retrieve certificate with custom names".into());
        }

        if let Err(err) = resolver.remove_certificate(&fingerprint) {
            return Err(format!("the certificate could not be removed, {err}").into());
        }

        let names = resolver.certificate_names(&fingerprint)?;
        if !resolver.find_certificates_by_names(&names)?.is_empty()
            && resolver.get_certificate(&fingerprint).is_some()
        {
            return Err("We have retrieved the certificate that should be deleted".into());
        }

        Ok(())
    }

    #[test]
    fn properly_replace_outdated_cert() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        let first_certificate = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key: String::from(include_str!("../assets/tests/key.pem")),
            names: vec!["localhost".into()],
            ..Default::default()
        };
        let first = resolver.add_certificate(&AddCertificate {
            address: address.clone(),
            certificate: first_certificate,
            expired_at: None,
        })?;
        if resolver.get_certificate(&first).is_none() {
            return Err("failed to retrieve first certificate".into());
        }
        match resolver.domain_lookup("localhost".as_bytes(), true) {
            Some((_, fingerprint)) if fingerprint == &first => {}
            Some((domain, fingerprint)) => {
                return Err(format!(
                    "failed to lookup first inserted certificate. domain: {:?}, fingerprint: {}",
                    domain, fingerprint
                )
                .into())
            }
            _ => return Err("failed to lookup first inserted certificate".into()),
        }

        let second_certificate = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-2y.pem")),
            key: String::from(include_str!("../assets/tests/key.pem")),
            names: vec!["localhost".into(), "lolcatho.st".into()],
            ..Default::default()
        };
        let second = resolver.add_certificate(&AddCertificate {
            address,
            certificate: second_certificate,
            expired_at: None,
        })?;

        if resolver.get_certificate(&second).is_none() {
            return Err("failed to retrieve second certificate".into());
        }

        match resolver.domain_lookup("localhost".as_bytes(), true) {
            Some((_, fingerprint)) if fingerprint == &second => {}
            Some((domain, fingerprint)) => {
                return Err(format!(
                    "failed to lookup second inserted certificate. domain: {:?}, fingerprint: {}",
                    domain, fingerprint
                )
                .into())
            }
            _ => return Err("the former certificate has not been overriden by the new one".into()),
        }

        Ok(())
    }

    #[test]
    fn replacement() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        // ---------------------------------------------------------------------
        // load first certificate
        let certificate_and_key_1y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key: String::from(include_str!("../assets/tests/key-1y.pem")),
            ..Default::default()
        };

        let fingerprint_1y = resolver.add_certificate(&AddCertificate {
            address: address.clone(),
            certificate: certificate_and_key_1y,
            expired_at: None,
        })?;
        let names_1y = resolver.certificate_names(&fingerprint_1y)?;

        if resolver.get_certificate(&fingerprint_1y).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        // ---------------------------------------------------------------------
        // load second certificate
        let certificate_and_key_2y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-2y.pem")),
            key: String::from(include_str!("../assets/tests/key-2y.pem")),
            ..Default::default()
        };

        let fingerprint_2y = resolver.add_certificate(&AddCertificate {
            address,
            certificate: certificate_and_key_2y,
            expired_at: None,
        })?;

        if resolver.get_certificate(&fingerprint_2y).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        // ---------------------------------------------------------------------
        // Check if the fist certificate has been successfully replaced
        if resolver.get_certificate(&fingerprint_1y).is_some() {
            return Err("certificate must be replaced by the 2y expiration one".into());
        }

        if resolver.get_certificate(&fingerprint_2y).is_none() {
            return Err("certificate must be added instead of the 1y expiration one".into());
        }

        let fingerprints = resolver.find_certificates_by_names(&names_1y)?;
        if fingerprints.get(&fingerprint_1y).is_some() {
            return Err("index must not reference the 1y expiration certificate".into());
        }

        if fingerprints.get(&fingerprint_2y).is_none() {
            return Err("index have to reference the 2y expiration certificate".into());
        }

        Ok(())
    }

    #[test]
    fn expiration_override() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        // ---------------------------------------------------------------------
        // load first certificate
        let certificate_and_key_1y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key: String::from(include_str!("../assets/tests/key-1y.pem")),
            ..Default::default()
        };

        let fingerprint_1y = resolver.add_certificate(&AddCertificate {
            address: address.clone(),
            certificate: certificate_and_key_1y,
            expired_at: Some(
                (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?
                    + Duration::from_secs(3 * 365 * 24 * 3600))
                .as_secs() as i64,
            ),
        })?;
        let names_1y = resolver.certificate_names(&fingerprint_1y)?;

        if resolver.get_certificate(&fingerprint_1y).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        // ---------------------------------------------------------------------
        // load second certificate
        let certificate_and_key_2y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-2y.pem")),
            key: String::from(include_str!("../assets/tests/key-2y.pem")),
            ..Default::default()
        };

        let fingerprint_2y = resolver.add_certificate(&AddCertificate {
            address,
            certificate: certificate_and_key_2y,
            expired_at: None,
        })?;

        if resolver.get_certificate(&fingerprint_2y).is_some() {
            return Err("certificate should not be loaded".into());
        }

        // ---------------------------------------------------------------------
        // Check if the fist certificate has been successfully not replaced
        if resolver.get_certificate(&fingerprint_1y).is_none() {
            return Err("certificate must not be replaced by the 2y expiration one".into());
        }

        if resolver.get_certificate(&fingerprint_2y).is_some() {
            return Err("certificate must not be added instead of the 1y expiration one".into());
        }

        let fingerprints = resolver.find_certificates_by_names(&names_1y)?;
        if fingerprints.get(&fingerprint_1y).is_none() {
            return Err("index must reference the 1y expiration certificate".into());
        }

        if fingerprints.get(&fingerprint_2y).is_some() {
            return Err("index must not reference the 2y expiration certificate".into());
        }

        Ok(())
    }

    #[test]
    fn random() -> Result<(), Box<dyn Error + Send + Sync>> {
        // ---------------------------------------------------------------------
        // load certificates
        let mut certificates = vec![
            CertificateAndKey {
                certificate: include_str!("../assets/tests/certificate-1.pem").to_string(),
                key: include_str!("../assets/tests/key.pem").to_string(),
                ..Default::default()
            },
            CertificateAndKey {
                certificate: include_str!("../assets/tests/certificate-2.pem").to_string(),
                key: include_str!("../assets/tests/key.pem").to_string(),
                ..Default::default()
            },
            CertificateAndKey {
                certificate: include_str!("../assets/tests/certificate-3.pem").to_string(),
                key: include_str!("../assets/tests/key.pem").to_string(),
                ..Default::default()
            },
            CertificateAndKey {
                certificate: include_str!("../assets/tests/certificate-4.pem").to_string(),
                key: include_str!("../assets/tests/key.pem").to_string(),
                ..Default::default()
            },
            CertificateAndKey {
                certificate: include_str!("../assets/tests/certificate-5.pem").to_string(),
                key: include_str!("../assets/tests/key.pem").to_string(),
                ..Default::default()
            },
            CertificateAndKey {
                certificate: include_str!("../assets/tests/certificate-6.pem").to_string(),
                key: include_str!("../assets/tests/key.pem").to_string(),
                ..Default::default()
            },
        ];

        let mut fingerprints = vec![];

        // randomize entries
        certificates.shuffle(&mut thread_rng());

        // ---------------------------------------------------------------------
        // load certificates in resolver
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        for certificate in &certificates {
            fingerprints.push(resolver.add_certificate(&AddCertificate {
                address: address.clone(),
                certificate: certificate.to_owned(),
                expired_at: None,
            })?);
        }

        let mut names = HashSet::new();
        names.insert("example.org".to_string());

        let fprints = resolver.find_certificates_by_names(&names)?;
        if 1 != fprints.len() && !fprints.contains(&fingerprints[1]) {
            return Err("domain 'example.org' resolve to the wrong certificate".into());
        }

        let mut names = HashSet::new();
        names.insert("*.example.org".to_string());

        let fprints = resolver.find_certificates_by_names(&names)?;
        if 1 != fprints.len() && !fprints.contains(&fingerprints[2]) {
            return Err("domain '*.example.org' resolve to the wrong certificate".into());
        }

        let mut names = HashSet::new();
        names.insert("clever-cloud.com".to_string());

        let fprints = resolver.find_certificates_by_names(&names)?;
        if 1 != fprints.len() && !fprints.contains(&fingerprints[4]) {
            return Err("domain 'clever-cloud.com' resolve to the wrong certificate".into());
        }

        let mut names = HashSet::new();
        names.insert("*.clever-cloud.com".to_string());

        let fprints = resolver.find_certificates_by_names(&names)?;
        if 1 != fprints.len() && !fprints.contains(&fingerprints[5]) {
            return Err("domain '*.clever-cloud.com' resolve to the wrong certificate".into());
        }

        Ok(())
    }
}
