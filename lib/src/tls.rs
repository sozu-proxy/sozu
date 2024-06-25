//! A unified certificate resolver for rustls.
//!
//! Persists certificates in the Rustls
//! [`CertifiedKey` format](https://docs.rs/rustls/latest/rustls/sign/struct.CertifiedKey.html),
//! exposes them to the HTTPS listener for TLS handshakes.
#[cfg(test)]
use std::collections::HashSet;
use std::{
    collections::HashMap,
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
///
/// the `domains` TrieNode is an addressing system to resolve a certificate
/// for a given domain name.
/// Certificates are stored in a hashmap that may contain unreachable certificates if
/// no domain name points to it.
#[derive(Default, Debug)]
pub struct CertificateResolver {
    /// routing one domain name to one certificate for fast resolving
    pub domains: TrieNode<Fingerprint>,
    /// a storage map: fingerprint -> stored_certificate
    certificates: HashMap<Fingerprint, CertifiedKeyWrapper>,
    /// maps each domain name to several compatible certificates, sorted by expiration date
    /// map of domain_name -> all fingerprints (and expiration) linked to this domain name
    //  the vector of (fingerprint, expiration) is sorted by expiration
    name_fingerprint_idx: HashMap<String, Vec<(Fingerprint, i64)>>,
}

impl CertificateResolver {
    /// return the certificate in the Rustls-usable form
    pub fn get_certificate(&self, fingerprint: &Fingerprint) -> Option<CertifiedKeyWrapper> {
        self.certificates.get(fingerprint).map(ToOwned::to_owned)
    }

    /// persist a certificate, after ensuring validity, and checking if it can replace another certificate.
    /// return the certificate fingerprint regardless of having inserted it or not
    pub fn add_certificate(
        &mut self,
        add: &AddCertificate,
    ) -> Result<Fingerprint, CertificateResolverError> {
        let cert_to_add = CertifiedKeyWrapper::try_from(add)?;

        trace!("Certificate Resolver: adding certificate {:?}", cert_to_add);

        if self.certificates.contains_key(&cert_to_add.fingerprint) {
            return Ok(cert_to_add.fingerprint);
        }

        for new_name in &cert_to_add.names {
            let fingerprints_for_this_name = self
                .name_fingerprint_idx
                .entry(new_name.to_owned())
                .or_default();

            fingerprints_for_this_name
                .push((cert_to_add.fingerprint.clone(), cert_to_add.expiration));

            // sort expiration ascending (longest-lived to the right)
            fingerprints_for_this_name.sort_by_key(|t| t.1);

            let longest_lived_cert = match fingerprints_for_this_name.last() {
                Some(cert) => cert,
                None => {
                    error!("no fingerprint for this name, this should not happen");
                    continue;
                }
            };

            // update the longest lived certificate in the TriNode
            self.domains.remove(&new_name.to_owned().into_bytes());
            self.domains.insert(
                new_name.to_owned().into_bytes(),
                longest_lived_cert.0.to_owned(),
            );
        }

        self.certificates
            .insert(cert_to_add.fingerprint.to_owned(), cert_to_add.clone());

        trace!("{:#?}", self);

        Ok(cert_to_add.fingerprint)
    }

    /// Delete a certificate from the resolver. May fail if there is no alternative for
    // a domain name
    pub fn remove_certificate(
        &mut self,
        fingerprint: &Fingerprint,
    ) -> Result<(), CertificateResolverError> {
        if let Some(certificate_to_remove) = self.get_certificate(fingerprint) {
            for name in certificate_to_remove.names {
                self.domains.domain_remove(&name.clone().into_bytes());

                if let Some(fingerprints_and_exp) = self.name_fingerprint_idx.get_mut(&name) {
                    // remove fingerprints from the index for this name
                    *fingerprints_and_exp = fingerprints_and_exp
                        .drain(..)
                        .filter(|t| &t.0 != fingerprint)
                        .collect();

                    // if present, reinsert the longest lived certificate in the TrieNode
                    if let Some(longest_lived_cert) = fingerprints_and_exp.last() {
                        self.domains
                            .insert(name.into_bytes(), longest_lived_cert.0.to_owned());
                    }
                }
            }

            self.certificates.remove(fingerprint);
        }
        trace!("{:#?}", self);

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
                    fingerprints.insert(fingerprint.to_owned().0);
                });
            }
        }

        Ok(fingerprints)
    }

    /// return the hashset of subjects that the certificate is able to handle.
    /// the certificate must be already persisted for this check
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

    // use rand::{seq::SliceRandom, thread_rng};
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

        // get the names to try and retrieve the certificate AFTER it is supposed to be removed
        let names = resolver.certificate_names(&fingerprint)?;

        if let Err(err) = resolver.remove_certificate(&fingerprint) {
            return Err(format!("the certificate was not removed, {err}").into());
        }

        if resolver.get_certificate(&fingerprint).is_some() {
            return Err("We have retrieved the certificate that should be deleted".into());
        }

        if !resolver.find_certificates_by_names(&names)?.is_empty() {
            return Err(
                "The certificate should be deleted but one of its names is in the index".into(),
            );
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
    fn keep_resolving_with_wildcard() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        // ---------------------------------------------------------------------
        // load the wildcard certificate,  expiring in 3 years
        let wildcard_example_org = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-3.pem")),
            key: String::from(include_str!("../assets/tests/key.pem")),
            ..Default::default()
        };

        let wildcard_example_org_fingerprint = resolver.add_certificate(&AddCertificate {
            address: address.clone(),
            certificate: wildcard_example_org,
            expired_at: Some(
                (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?
                    + Duration::from_secs(1 * 365 * 24 * 3600))
                .as_secs() as i64,
            ),
        })?;

        if resolver
            .get_certificate(&wildcard_example_org_fingerprint)
            .is_none()
        {
            return Err("could not load the 2-year-valid certificate".into());
        }

        // ---------------------------------------------------------------------
        // try loading the ordinary certificate, expiring in 2 years
        // this one has two names: example.org and www.example.org
        let www_example_org = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-2.pem")),
            key: String::from(include_str!("../assets/tests/key.pem")),
            ..Default::default()
        };

        let www_example_org_fingerprint = resolver.add_certificate(&AddCertificate {
            address,
            certificate: www_example_org,
            expired_at: Some(
                (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?
                    + Duration::from_secs(2 * 365 * 24 * 3600))
                .as_secs() as i64,
            ),
            ..Default::default()
        })?;

        let www_example_org = resolver
            .domain_lookup("www.example.org".as_bytes(), true)
            .expect("there should be a www.example.org cert");
        assert_eq!(www_example_org.1, www_example_org_fingerprint);

        let test_example_org = resolver
            .domain_lookup("test.example.org".as_bytes(), true)
            .expect("there should be a test.example.org cert");
        assert_eq!(test_example_org.1, wildcard_example_org_fingerprint);

        let example_org = resolver
            .domain_lookup("example.org".as_bytes(), true)
            .expect("there should be a example.org cert");
        assert_eq!(example_org.1, www_example_org_fingerprint);

        // check that when removing the www.example.org certificate
        // the resolver falls back on the wildcard
        resolver
            .remove_certificate(&www_example_org_fingerprint)
            .expect("should be able to remove the 2-year certificate");

        let should_be_wildcard_fingerprint = resolver
            .domain_lookup("www.example.org".as_bytes(), true)
            .expect("there should be a www.example.org cert");
        assert_eq!(
            should_be_wildcard_fingerprint.1,
            wildcard_example_org_fingerprint
        );

        assert!(resolver
            .domain_lookup("example.org".as_bytes(), true)
            .is_none());

        Ok(())
    }

    #[test]
    fn resolve_the_longer_lived_cert() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        // ---------------------------------------------------------------------
        // load the 2-year valid certificate
        let certificate_and_key_2y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-2y.pem")),
            key: String::from(include_str!("../assets/tests/key-2y.pem")),
            ..Default::default()
        };

        let fingerprint_2y = resolver.add_certificate(&AddCertificate {
            address: address.clone(),
            certificate: certificate_and_key_2y,
            expired_at: None,
        })?;

        if resolver.get_certificate(&fingerprint_2y).is_none() {
            return Err("could not load the 2-year-valid certificate".into());
        }

        // ---------------------------------------------------------------------
        // try loading the 1-year valid certificate
        let certificate_and_key_1y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key: String::from(include_str!("../assets/tests/key-1y.pem")),
            ..Default::default()
        };

        let fingerprint_1y = resolver.add_certificate(&AddCertificate {
            address,
            certificate: certificate_and_key_1y,
            ..Default::default()
        })?;

        let localhost_cert = resolver
            .domain_lookup("localhost".as_bytes(), true)
            .expect("there should be a localhost cert");

        assert_eq!(localhost_cert.1, fingerprint_2y);

        // check that when removing the longer-lived certificate,
        // the resolver falls back on the shorter-lived one

        resolver
            .remove_certificate(&fingerprint_2y)
            .expect("should be able to remove the 2-year certificate");

        let localhost_cert = resolver
            .domain_lookup("localhost".as_bytes(), true)
            .expect("there should be a localhost cert");

        assert_eq!(localhost_cert.1, fingerprint_1y);

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

        let fingerprint_1y_overriden = resolver.add_certificate(&AddCertificate {
            address: address.clone(),
            certificate: certificate_and_key_1y,
            expired_at: Some(
                (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?
                    + Duration::from_secs(3 * 365 * 24 * 3600))
                .as_secs() as i64,
            ),
        })?;

        if resolver
            .get_certificate(&fingerprint_1y_overriden)
            .is_none()
        {
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

        let localhost_cert = resolver
            .domain_lookup("localhost".as_bytes(), true)
            .expect("there should be a localhost cert");

        assert_eq!(localhost_cert.1, fingerprint_1y_overriden);

        // check that when removing the overriden certificate,
        // the resolver falls back on the other one

        resolver
            .remove_certificate(&fingerprint_1y_overriden)
            .expect("should be able to remove the 1-year (3-year-overriden) certificate");

        let localhost_cert = resolver
            .domain_lookup("localhost".as_bytes(), true)
            .expect("there should be a localhost cert");

        assert_eq!(localhost_cert.1, fingerprint_2y);

        Ok(())
    }
}
