//! A unified certificate resolver for rustls.
//!
//! Persists certificates in the Rustls
//! [`CertifiedKey` format](https://docs.rs/rustls/latest/rustls/sign/struct.CertifiedKey.html),
//! exposes them to the HTTPS listener for TLS handshakes.
use std::{
    borrow::ToOwned,
    collections::{HashMap, HashSet},
    convert::From,
    io::BufReader,
    sync::{Arc, Mutex},
};

use once_cell::sync::Lazy;
use rustls::{
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
    Certificate, PrivateKey,
};
use sha2::{Digest, Sha256};

use sozu_command::{
    certificate::{
        get_cn_and_san_attributes, parse_pem, parse_x509, CertificateError, Fingerprint,
    },
    proto::command::{AddCertificate, CertificateAndKey, ReplaceCertificate},
};

use crate::router::trie::{Key, KeyValue, TrieNode};

// -----------------------------------------------------------------------------
// Default ParsedCertificateAndKey

static DEFAULT_CERTIFICATE: Lazy<Option<Arc<CertifiedKey>>> = Lazy::new(|| {
    let certificate_and_key = CertificateAndKey {
        certificate: include_str!("../assets/certificate.pem").to_string(),
        certificate_chain: vec![include_str!("../assets/certificate_chain.pem").to_string()],
        key: include_str!("../assets/key.pem").to_string(),
        versions: vec![],
        names: vec![],
    };

    CertificateResolver::parse(&certificate_and_key)
        .ok()
        .map(|c| c.certified_key)
});

// -----------------------------------------------------------------------------
// CertificateResolver trait

pub trait ResolveCertificate {
    type Error;

    /// return the certificate in both a Rustls-usable form, and the pem format
    fn get_certificate(&self, fingerprint: &Fingerprint) -> Option<StoredCertificate>;

    /// persist a certificate, after ensuring validity, and checking if it can replace another certificate
    fn add_certificate(&mut self, opts: &AddCertificate) -> Result<Fingerprint, Self::Error>;

    /// Delete a certificate from the resolver. May fail if there is no alternative for
    // a domain name
    fn remove_certificate(&mut self, opts: &Fingerprint) -> Result<(), Self::Error>;

    /// Short-hand for `add_certificate` and then `remove_certificate`.
    /// It is possible that the certificate will not be replaced, if the
    /// new certificate does not match `add_certificate` rules.
    fn replace_certificate(
        &mut self,
        opts: &ReplaceCertificate,
    ) -> Result<Fingerprint, Self::Error> {
        let fingerprint = self.add_certificate(&AddCertificate {
            address: opts.address.to_owned(),
            certificate: opts.new_certificate.to_owned(),
            expired_at: opts.new_expired_at.to_owned(),
        })?;

        match hex::decode(&opts.old_fingerprint) {
            Ok(old_fingerprint) => self.remove_certificate(&Fingerprint(old_fingerprint))?,
            Err(err) => {
                error!("failed to parse fingerprint, {}", err);
            }
        }

        Ok(fingerprint)
    }
}

// -----------------------------------------------------------------------------
// CertificateOverride struct

#[derive(Clone, Debug)]
pub struct CertificateOverride {
    pub names: Option<HashSet<String>>,
    pub expiration: Option<i64>,
}

impl From<&AddCertificate> for CertificateOverride {
    fn from(opts: &AddCertificate) -> Self {
        let mut names = None;
        if !opts.certificate.names.is_empty() {
            names = Some(opts.certificate.names.iter().cloned().collect())
        }

        Self {
            names,
            expiration: opts.expired_at.to_owned(),
        }
    }
}

/// A wrapper around the Rustls
/// [`CertifiedKey` type](https://docs.rs/rustls/latest/rustls/sign/struct.CertifiedKey.html),
/// stored and returned by the certificate resolver.
#[derive(Clone)]
pub struct StoredCertificate {
    certified_key: Arc<CertifiedKey>,
}

impl StoredCertificate {
    /// bytes of the pem formatted certificate, first of the chain
    fn pem_bytes(&self) -> &[u8] {
        &self.certified_key.cert[0].0
    }
}

// -----------------------------------------------------------------------------
// CertificateResolverError enum

#[derive(thiserror::Error, Debug)]
pub enum CertificateResolverError {
    #[error("failed to get common name and subject alternate names from pem, {0}")]
    InvalidCommonNameAndSubjectAlternateNames(CertificateError),
    #[error("invalid private key: {0}")]
    InvalidPrivateKey(String),
    #[error("certificate is still in use")]
    IsStillInUse,
    #[error("empty key")]
    EmptyKeys,
    #[error("certificate error: {0}")]
    CertificateError(CertificateError),
}

impl From<CertificateError> for CertificateResolverError {
    fn from(value: CertificateError) -> Self {
        Self::CertificateError(value)
    }
}

// -----------------------------------------------------------------------------
// CertificateResolver struct

/// Parses and stores TLS certificates, makes them available to Rustls for TLS handshakes
#[derive(Default)]
pub struct CertificateResolver {
    pub domains: TrieNode<Fingerprint>,
    /// a map of fingerprint -> stored_certificate
    certificates: HashMap<Fingerprint, StoredCertificate>,
    name_fingerprint_idx: HashMap<String, HashSet<Fingerprint>>,
    overrides: HashMap<Fingerprint, CertificateOverride>,
}

impl ResolveCertificate for CertificateResolver {
    type Error = CertificateResolverError;

    fn get_certificate(&self, fingerprint: &Fingerprint) -> Option<StoredCertificate> {
        self.certificates.get(fingerprint).map(ToOwned::to_owned)
    }

    fn add_certificate(&mut self, opts: &AddCertificate) -> Result<Fingerprint, Self::Error> {
        // Check if we could parse the certificate, chain and private key, if not just throw an
        // error.
        let stored_certificate = Self::parse(&opts.certificate)?;
        let fingerprint = fingerprint(stored_certificate.pem_bytes());
        if !opts.certificate.names.is_empty() || opts.expired_at.is_some() {
            self.overrides
                .insert(fingerprint.to_owned(), CertificateOverride::from(opts));
        } else {
            self.overrides.remove(&fingerprint);
        }

        let (should_insert, certificates_to_remove) =
            self.should_insert(&fingerprint, &stored_certificate)?;
        if !should_insert {
            // if we do not need to insert the fingerprint just return the fingerprint
            return Ok(fingerprint);
        }

        let new_names = match self.get_names_override(&fingerprint) {
            Some(names) => names,
            None => self.certificate_names(stored_certificate.pem_bytes())?,
        };

        self.certificates
            .insert(fingerprint.to_owned(), stored_certificate);
        for name in new_names {
            self.domains
                .insert(name.to_owned().into_bytes(), fingerprint.to_owned());

            self.name_fingerprint_idx
                .entry(name)
                .or_insert_with(HashSet::new)
                .insert(fingerprint.to_owned());
        }

        for (fingerprint, names) in &certificates_to_remove {
            for name in names {
                if let Some(fingerprints) = self.name_fingerprint_idx.get_mut(name) {
                    fingerprints.remove(fingerprint);
                }
            }

            self.certificates.remove(fingerprint);
        }

        Ok(fingerprint.to_owned())
    }

    fn remove_certificate(&mut self, fingerprint: &Fingerprint) -> Result<(), Self::Error> {
        if let Some(certified_key_and_pem) = self.get_certificate(fingerprint) {
            let names = match self.get_names_override(fingerprint) {
                Some(names) => names,
                None => self.certificate_names(certified_key_and_pem.pem_bytes())?,
            };

            if self.is_required_for_domain(&names, fingerprint) {
                return Err(CertificateResolverError::IsStillInUse);
            }

            for name in &names {
                if let Some(fingerprints) = self.name_fingerprint_idx.get_mut(name) {
                    fingerprints.remove(fingerprint);

                    if fingerprints.is_empty() {
                        self.domains.domain_remove(&name.to_owned().into_bytes());
                    }
                }
            }

            self.certificates.remove(fingerprint);
        }

        Ok(())
    }
}

/// hashes bytes of the pem-formatted certificate for storage in the hashmap
fn fingerprint(bytes: &[u8]) -> Fingerprint {
    Fingerprint(Sha256::digest(bytes).iter().cloned().collect())
}

impl CertificateResolver {
    /// return all fingerprints that are available, provided at least one name is given
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

    /// return the hashset of subjects that the certificate is able to handle, by
    /// parsing the pem file and scrapping the information
    fn certificate_names(&self, pem: &[u8]) -> Result<HashSet<String>, CertificateResolverError> {
        let fingerprint = fingerprint(pem);
        if let Some(certificate_override) = self.overrides.get(&fingerprint) {
            if let Some(names) = &certificate_override.names {
                return Ok(names.to_owned());
            }
        }

        get_cn_and_san_attributes(pem)
            .map_err(CertificateResolverError::InvalidCommonNameAndSubjectAlternateNames)
    }

    /// Parse a raw certificate into the Rustls format.
    /// Parses RSA and ECDSA certificates.
    fn parse(
        certificate_and_key: &CertificateAndKey,
    ) -> Result<StoredCertificate, CertificateResolverError> {
        let certificate_pem =
            sozu_command::certificate::parse_pem(certificate_and_key.certificate.as_bytes())?;

        let mut chain = vec![Certificate(certificate_pem.contents)];
        for cert in &certificate_and_key.certificate_chain {
            let chain_link = parse_pem(cert.as_bytes())?.contents;

            chain.push(Certificate(chain_link));
        }

        let mut key_reader = BufReader::new(certificate_and_key.key.as_bytes());

        let item = match rustls_pemfile::read_one(&mut key_reader)
            .map_err(|_| CertificateResolverError::EmptyKeys)?
        {
            Some(item) => item,
            None => return Err(CertificateResolverError::EmptyKeys),
        };

        let private_key = match item {
            rustls_pemfile::Item::RSAKey(rsa_key) => PrivateKey(rsa_key),
            rustls_pemfile::Item::PKCS8Key(pkcs8_key) => PrivateKey(pkcs8_key),
            rustls_pemfile::Item::ECKey(ec_key) => PrivateKey(ec_key),
            _ => return Err(CertificateResolverError::EmptyKeys),
        };
        match rustls::sign::any_supported_type(&private_key) {
            Ok(signing_key) => {
                let stored_certificate = StoredCertificate {
                    certified_key: Arc::new(CertifiedKey::new(chain, signing_key)),
                };
                Ok(stored_certificate)
            }
            Err(sign_error) => Err(CertificateResolverError::InvalidPrivateKey(
                sign_error.to_string(),
            )),
        }
    }
}

impl CertificateResolver {
    fn is_required_for_domain(&self, names: &HashSet<String>, fingerprint: &Fingerprint) -> bool {
        for name in names {
            if let Some(fingerprints) = self.name_fingerprint_idx.get(name) {
                if 1 == fingerprints.len() && fingerprints.get(fingerprint).is_some() {
                    return true;
                }
            }
        }

        false
    }

    fn should_insert(
        &self,
        fingerprint: &Fingerprint,
        stored_certificate: &StoredCertificate,
    ) -> Result<(bool, HashMap<Fingerprint, HashSet<String>>), CertificateResolverError> {
        let x509 = parse_x509(stored_certificate.pem_bytes())?;

        // We need to know if the new certificate can replace an already existing one.
        let new_names = match self.get_names_override(fingerprint) {
            Some(names) => names,
            None => self.certificate_names(stored_certificate.pem_bytes())?,
        };

        let expiration = self
            .get_expiration_override(fingerprint)
            .unwrap_or_else(|| x509.validity().not_after.timestamp());

        let fingerprints = self.find_certificates_by_names(&new_names)?;
        let mut certificates = HashMap::new();
        for fingerprint in &fingerprints {
            if let Some(certified_key_and_pem) = self.get_certificate(fingerprint) {
                certificates.insert(fingerprint, certified_key_and_pem);
            }
        }

        let mut should_insert = false;
        let mut certificates_to_remove = HashMap::new();
        let mut certificates_names = HashSet::new();
        for (fingerprint, stored_certificate) in certificates {
            let x509 = parse_x509(stored_certificate.pem_bytes())?;

            let certificate_names = match self.get_names_override(fingerprint) {
                Some(names) => names,
                None => self.certificate_names(stored_certificate.pem_bytes())?,
            };

            let certificate_expiration = self
                .get_expiration_override(fingerprint)
                .unwrap_or_else(|| x509.validity().not_after.timestamp());

            let extra_names = certificate_names
                .difference(&new_names)
                .collect::<HashSet<_>>();

            // if the certificate has at least the same name or less and the expiration date
            // is closer than the new one. We could remove it and allow the new insertion.
            if extra_names.is_empty() && certificate_expiration < expiration {
                certificates_to_remove.insert(fingerprint.to_owned(), certificate_names.to_owned());
                should_insert = true;
            }

            // We keep a track of all name of certificates that match our query to
            // check, if the new certificate provide an extra domain which is not
            // already exposed
            for name in certificate_names {
                certificates_names.insert(name);
            }
        }

        // In the case where we do not insert the certificate, because there is
        // no additional value, we have to check whether it provides an extra domain
        // name not registered yet.
        let diff: HashSet<&String> = new_names.difference(&certificates_names).collect();
        if !should_insert && diff.is_empty() {
            // We already have all domain names registered and there is no update
            // for expiration date of certificate. So, skipping the update.
            return Ok((false, certificates_to_remove));
        }

        Ok((true, certificates_to_remove))
    }

    fn get_expiration_override(&self, fingerprint: &Fingerprint) -> Option<i64> {
        self.overrides.get(fingerprint).and_then(|co| co.expiration)
    }

    fn get_names_override(&self, fingerprint: &Fingerprint) -> Option<HashSet<String>> {
        self.overrides
            .get(fingerprint)
            .and_then(|co| co.names.to_owned())
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
pub struct MutexWrappedCertificateResolver(pub Mutex<CertificateResolver>);

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

                let cert = resolver
                    .certificates
                    .get(fingerprint)
                    .map(|cert| cert.certified_key.clone());

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

// -----------------------------------------------------------------------------
// Unit tests

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        error::Error,
        time::{Duration, SystemTime},
    };

    use super::{fingerprint, CertificateResolver, CertificateResolverError, ResolveCertificate};

    use rand::{seq::SliceRandom, thread_rng};
    use sozu_command::{
        certificate::parse_pem,
        proto::command::{AddCertificate, CertificateAndKey},
    };

    #[test]
    fn lifecycle() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = "127.0.0.1:8080".to_string();
        let mut resolver = CertificateResolver::default();
        let certificate_and_key = CertificateAndKey {
            certificate: String::from(include_str!("../assets/certificate.pem")),
            key: String::from(include_str!("../assets/key.pem")),
            ..Default::default()
        };

        let pem = parse_pem(certificate_and_key.certificate.as_bytes())?;

        let fingerprint = resolver.add_certificate(&AddCertificate {
            address,
            certificate: certificate_and_key,
            expired_at: None,
        })?;

        if resolver.get_certificate(&fingerprint).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        if let Err(err) = resolver.remove_certificate(&fingerprint) {
            match err {
                CertificateResolverError::IsStillInUse => {}
                _ => {
                    return Err(format!("the certificate must not been removed, {err}").into());
                }
            }
        }

        let names = resolver.certificate_names(&pem.contents)?;
        if resolver.find_certificates_by_names(&names)?.is_empty()
            || resolver.get_certificate(&fingerprint).is_none()
        {
            return Err(
                "failed to retrieve certificate that we had the command to delete, but mandatory"
                    .into(),
            );
        }

        Ok(())
    }

    #[test]
    fn name_override() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = "127.0.0.1:8080".to_string();
        let mut resolver = CertificateResolver::default();
        let certificate_and_key = CertificateAndKey {
            certificate: String::from(include_str!("../assets/certificate.pem")),
            key: String::from(include_str!("../assets/key.pem")),
            names: vec!["localhost".into(), "lolcatho.st".into()],
            ..Default::default()
        };

        let pem = parse_pem(certificate_and_key.certificate.as_bytes())?;

        let fingerprint = resolver.add_certificate(&AddCertificate {
            address: address,
            certificate: certificate_and_key,
            expired_at: None,
        })?;

        if resolver.get_certificate(&fingerprint).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        if let Err(err) = resolver.remove_certificate(&fingerprint) {
            match err {
                CertificateResolverError::IsStillInUse => {}
                _ => {
                    return Err(format!("the certificate must not been removed, {err}").into());
                }
            }
        }

        let names = resolver.certificate_names(&pem.contents)?;
        if resolver.find_certificates_by_names(&names)?.is_empty()
            || resolver.get_certificate(&fingerprint).is_none()
        {
            return Err(
                "failed to retrieve certificate that we had the command to delete, but mandatory"
                    .into(),
            );
        }

        let mut lolcat = HashSet::new();
        lolcat.insert(String::from("lolcatho.st"));
        if resolver.find_certificates_by_names(&lolcat)?.is_empty()
            || resolver.get_certificate(&fingerprint).is_none()
        {
            return Err(
                "failed to retrieve certificate that we had the command to delete, but mandatory"
                    .into(),
            );
        }

        Ok(())
    }

    #[test]
    fn replacement() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = "127.0.0.1:8080".to_string();
        let mut resolver = CertificateResolver::default();

        // ---------------------------------------------------------------------
        // load first certificate
        let certificate_and_key_1y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key: String::from(include_str!("../assets/tests/key-1y.pem")),
            ..Default::default()
        };
        let pem = parse_pem(certificate_and_key_1y.certificate.as_bytes())?;

        let names_1y = resolver.certificate_names(&pem.contents)?;
        let fingerprint_1y = resolver.add_certificate(&AddCertificate {
            address: address.clone(),
            certificate: certificate_and_key_1y,
            expired_at: None,
        })?;

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
        let address = "127.0.0.1:8080".to_string();
        let mut resolver = CertificateResolver::default();

        // ---------------------------------------------------------------------
        // load first certificate
        let certificate_and_key_1y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key: String::from(include_str!("../assets/tests/key-1y.pem")),
            ..Default::default()
        };

        let pem = parse_pem(certificate_and_key_1y.certificate.as_bytes())?;

        let names_1y = resolver.certificate_names(&pem.contents)?;
        let fingerprint_1y = resolver.add_certificate(&AddCertificate {
            address: address.clone(),
            certificate: certificate_and_key_1y,
            expired_at: Some(
                (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?
                    + Duration::from_secs(3 * 365 * 24 * 3600))
                .as_secs() as i64,
            ),
        })?;

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
        for certificate in &certificates {
            let pem = parse_pem(certificate.certificate.as_bytes())?;

            fingerprints.push(fingerprint(&pem.contents));
        }

        // randomize entries
        certificates.shuffle(&mut thread_rng());

        // ---------------------------------------------------------------------
        // load certificates in resolver
        let address = "127.0.0.1:8080".to_string();
        let mut resolver = CertificateResolver::default();

        for certificate in &certificates {
            resolver.add_certificate(&AddCertificate {
                address: address.clone(),
                certificate: certificate.to_owned(),
                expired_at: None,
            })?;
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
