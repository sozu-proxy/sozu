//! # Tls module
//!
//! This module provides traits and structures to handle tls. It provides a unified
//! certificate resolver for rustls and openssl.
use std::{io::BufReader, collections::{HashSet, HashMap}, convert::From, sync::{Arc, Mutex}, borrow::ToOwned};

use crate::sozu_command::proxy::{AddCertificate, RemoveCertificate, ReplaceCertificate, CertFingerprint, CertificateAndKey};

use x509_parser::{pem::{Pem, parse_x509_pem}};
use trie::TrieNode;
use sha2::{Sha256, Digest};
use rustls::{internal::pemfile, ClientHello, sign::{RSASigningKey, CertifiedKey}, ResolvesServerCert};
#[cfg(feature = "use-openssl")]
use openssl::{x509::X509, pkey::PKey};

pub trait CertificateResolver {
    type Error;

    // `get_certificate` return the certificate, its chain and private key
    fn get_certificate(&self, fingerprint: &CertFingerprint) -> Option<CertificateAndKey>;

    // `find_certificates_by_names` return all fingerprints that are available to provide at least
    // one name
    fn find_certificates_by_names(&self, names: &HashSet<String>) -> Result<HashSet<CertFingerprint>, Self::Error>;

    // `add_certificate` to the certificate resolver, ensure that it is valid and check if it can
    // replace another certificates
    fn add_certificate(&mut self, opts: &AddCertificate) -> Result<CertFingerprint, Self::Error>;

    // `remove_certificate` from the certificate resolver, may fail if there is no alternative for
    // a domain name
    fn remove_certificate(&mut self, opts: &RemoveCertificate) -> Result<(), Self::Error>;

    // `replace_certificate` by a new one, this method is a short-hand for `add_certificate` and
    // then `remove_certificate`. It is possible that the certificate will not be replaced, if the
    // new certificate does not match `add_certificate` rules.
    fn replace_certificate(&mut self, opts: &ReplaceCertificate) -> Result<CertFingerprint, Self::Error> {
        self.remove_certificate(&RemoveCertificate {
            front: opts.front.to_owned(),
            fingerprint: opts.old_fingerprint.to_owned(),
        })?;

        let fingerprint = self.add_certificate(&AddCertificate {
            front: opts.front.to_owned(),
            certificate: opts.new_certificate.to_owned(),
            names: opts.new_names.to_owned(),
            expired_at: opts.new_expired_at.to_owned(),
        })?;

        Ok(fingerprint)
    }
}

pub trait CertificateResolverHelper {
    type Error;

    // `certificate_names` return the hashset of subjects that the certificate is able to handle by
    // parsing the pem file and scrapping the information
    fn certificate_names(&self, pem: &Pem) -> Result<HashSet<String>, Self::Error>;

    // `fingerprint` return the computed fingerprint for the given certificate
    fn fingerprint(certificate: &Pem) -> CertFingerprint;

    // `is_valid` ensure that a certificate, its chain and its private key are valid by parsing them
    // and check that the signature algorithm is supported
    fn is_valid(certificate_and_key: &CertificateAndKey) -> bool;
}

#[derive(thiserror::Error, Clone, Debug)]
pub enum GenericCertificateResolverError {
    #[error("failed to parse certificate common name, {0}")]
    CommonNameParseError(String),
    #[error("failed to parse pem certificate, {0}")]
    PemParseError(String),
    #[error("failed to parse der certificate, {0}")]
    DerParseError(String),
    #[error("certificate, chain or private key is invalid")]
    InvalidCertificateChainOrPrivateKeyError,
    #[error("certificate is still in use")]
    IsStillInUseError,
}

#[derive(Clone, Debug)]
pub struct CertificateOverride {
    pub names: Option<HashSet<String>>,
    pub expiration: Option<i64>,
}

impl From<&AddCertificate> for CertificateOverride {
    fn from(opts: &AddCertificate) -> Self {
        Self {
            names: opts.names.to_owned().map(|names| names.iter().cloned().collect()),
            expiration: opts.expired_at.to_owned(),
        }
    }
}

#[derive(Debug)]
pub struct GenericCertificateResolver {
    pub domains: TrieNode<CertFingerprint>,
    certificates: HashMap<CertFingerprint, CertificateAndKey>,
    name_fingerprint_idx: HashMap<String, HashSet<CertFingerprint>>,
    overrides: HashMap<CertFingerprint, CertificateOverride>,
}

impl CertificateResolver for GenericCertificateResolver {
    type Error = GenericCertificateResolverError;

    fn get_certificate(&self, fingerprint: &CertFingerprint) -> Option<CertificateAndKey> {
        self.certificates.get(fingerprint)
            .map(ToOwned::to_owned)
    }

    fn find_certificates_by_names(&self, names: &HashSet<String>) -> Result<HashSet<CertFingerprint>, Self::Error> {
        let mut fingerprints = HashSet::new();
        for name in names {
            if let Some(fprints) = self.name_fingerprint_idx.get(name) {
                fprints.iter().for_each(|fingerprint| { fingerprints.insert(fingerprint.to_owned()); });
            }
        }

        Ok(fingerprints)
    }

    fn add_certificate(&mut self, opts: &AddCertificate) -> Result<CertFingerprint, Self::Error> {
        // Check if we could parse the certificate, chain and private key, if not just throw an
        // error.
        if !Self::is_valid(&opts.certificate) {
            return Err(GenericCertificateResolverError::InvalidCertificateChainOrPrivateKeyError);
        }

        let (_, pem) = parse_x509_pem(opts.certificate.certificate.as_bytes())
            .map_err(|err| GenericCertificateResolverError::PemParseError(err.to_string()))?;

        let fingerprint = Self::fingerprint(&pem);
        if opts.names.is_some() || opts.expired_at.is_some() {
            self.overrides.insert(fingerprint.to_owned(), CertificateOverride::from(opts));
        } else {
            self.overrides.remove(&fingerprint);
        }

        // We do not need to update the entry, if the certificate is already registered
        if self.get_certificate(&fingerprint).is_some() {
            return Ok(fingerprint);
        }

        let x509 = pem
            .parse_x509()
            .map_err(|err| GenericCertificateResolverError::DerParseError(err.to_string()))?;

        // We need to know if the new certificate can replace an already existent one.
        let names = self.certificate_names(&pem)?;
        let expiration = self.get_expiration_override(&fingerprint)
            .unwrap_or_else(|| x509.validity().not_after.timestamp());

        let fingerprints = self.find_certificates_by_names(&names)?;
        let certificates = fingerprints.iter().fold(HashMap::new(), |mut acc, fingerprint| {
            if let Some(certificate_and_key) = self.get_certificate(fingerprint) {
                acc.insert(fingerprint, certificate_and_key);
            }

            acc
        });

        let mut should_insert = false;
        let mut certs_to_remove = vec![];
        let mut certs_names = HashSet::new();
        for (fingerprint, certificate_and_key) in certificates {
            let (_, pem) = parse_x509_pem(certificate_and_key.certificate.as_bytes())
                .map_err(|err| GenericCertificateResolverError::PemParseError(err.to_string()))?;

            let certificate = pem
                .parse_x509()
                .map_err(|err| GenericCertificateResolverError::DerParseError(err.to_string()))?;

            // could not failed as this operation has been already done when loading
            // the certificate
            let certificate_names = self.certificate_names(&pem)?;
            let certificate_expiration = self.get_expiration_override(&fingerprint)
                .unwrap_or_else(|| certificate.validity().not_after.timestamp());

            let extra_names = certificate_names
                .difference(&names)
                .collect::<HashSet<_>>();

            // if the certificate has at least the same name or less and the expiration date
            // is closer than the new one. We could remove it and allow the new insertion.
            if extra_names.is_empty() && certificate_expiration < expiration {
                certs_to_remove.push((fingerprint, certificate_names.to_owned()));
                should_insert = true;
            }

            // We keep a track of all name of certificates that match our query to
            // check, if the new certificate provide an extra domain which is not
            // already exposed
            for name in certificate_names {
                certs_names.insert(name);
            }
        }

        // In the case, where we do not insert the certificate, because there is
        // no additional value. We have to check, if it provide an extra domain
        // name not registered yet.
        if !should_insert {
            let diff: HashSet<&String> = names.difference(&certs_names).collect();
            if diff.is_empty() {
                // We already have all domain names registered and there is no update
                // for expiration date of certificate. So, skipping the update.
                return Ok(fingerprint);
            }
        }

        self.certificates.insert(fingerprint.to_owned(), opts.certificate.to_owned());
        for name in names {
            self.domains.insert(name.to_owned().into_bytes(), fingerprint.to_owned());
            self.name_fingerprint_idx
                .entry(name)
                .or_insert_with(|| HashSet::new())
                .insert(fingerprint.to_owned());
        }

        for (fingerprint, names) in &certs_to_remove {
            for name in names {
                if let Some(fingerprints) = self.name_fingerprint_idx.get_mut(name) {
                    fingerprints.remove(*fingerprint);
                }
            }

            self.certificates.remove(*fingerprint);
        }

        Ok(fingerprint.to_owned())
    }

    fn remove_certificate(&mut self, opts: &RemoveCertificate) -> Result<(), Self::Error> {
        if let Some(certificate_and_key) = self.get_certificate(&opts.fingerprint) {
            let (_, pem) = parse_x509_pem(certificate_and_key.certificate.as_bytes())
                .map_err(|err| GenericCertificateResolverError::PemParseError(err.to_string()))?;

            let fingerprint = Self::fingerprint(&pem);
            let names = self.certificate_names(&pem)?;
            if self.is_required_for_domain(&names, &fingerprint) {
                return Err(GenericCertificateResolverError::IsStillInUseError);
            }

            for name in &names {
                if let Some(fingerprints) = self.name_fingerprint_idx.get_mut(name) {
                    fingerprints.remove(&opts.fingerprint);

                    if fingerprints.is_empty() {
                        self.domains.domain_remove(&name.to_owned().into_bytes());
                    }
                }
            }

            self.certificates.remove(&opts.fingerprint);
        }

        Ok(())
    }
}

impl CertificateResolverHelper for GenericCertificateResolver {
    type Error = GenericCertificateResolverError;

    fn certificate_names(&self, pem: &Pem) -> Result<HashSet<String>, Self::Error> {
        let fingerprint = Self::fingerprint(pem);
        if let Some(certificate_override) = self.overrides.get(&fingerprint) {
            if let Some(names) = &certificate_override.names {
                return Ok(names.to_owned());
            }
        }

        let x509 = pem
            .parse_x509()
            .map_err(|err| GenericCertificateResolverError::DerParseError(err.to_string()))?;

        let mut names: HashSet<String> = HashSet::new();
        for name in x509.subject().iter_common_name() {
            names.insert(name.attr_value
                .as_str()
                .map(String::from)
                .map_err(|err| GenericCertificateResolverError::CommonNameParseError(err.to_string()))?
            );
        }

        Ok(names)
    }

    fn fingerprint(pem: &Pem) -> CertFingerprint {
        CertFingerprint(Sha256::digest(&pem.contents).iter().cloned().collect())
    }

    #[cfg(not(feature = "use-openssl"))]
    fn is_valid(certificate_and_key: &CertificateAndKey) -> bool {
        let mut cert_reader = BufReader::new(certificate_and_key.certificate.as_bytes());
        if pemfile::certs(&mut cert_reader).is_err() {
            return false;
        }

        for cert in certificate_and_key.certificate_chain.iter() {
            let mut chain_cert_reader = BufReader::new(cert.as_bytes());
            if pemfile::certs(&mut chain_cert_reader).is_err() {
                return false;
            }
        }

        // try to parse key as rsa private key
        let mut key_reader = BufReader::new(certificate_and_key.key.as_bytes());
        let parsed_keys = pemfile::rsa_private_keys(&mut key_reader);
        if let Ok(keys) = parsed_keys {
            if !keys.is_empty() {
                if RSASigningKey::new(&keys[0]).is_ok() {
                    return true;
                }
            }
        }

        // try to parse key as pkcs8-encoded private
        let mut key_reader = BufReader::new(certificate_and_key.key.as_bytes());
        let parsed_keys = pemfile::pkcs8_private_keys(&mut key_reader);
        if let Ok(keys) = parsed_keys {
            if !keys.is_empty() {
                // try to read as rsa private key
                if RSASigningKey::new(&keys[0]).is_ok() {
                    return true;
                }

                // try to read as ecdsa private key
                if rustls::sign::any_ecdsa_type(&keys[0]).is_ok() {
                    return true;
                }
            }
        }

        false
    }

    #[cfg(feature = "use-openssl")]
    fn is_valid(certificate_and_key: &CertificateAndKey) -> bool {
        let mut certificate_reader = certificate_and_key.certificate.as_bytes();
        let mut pkey_reader = certificate_and_key.key.as_bytes();

        for certificate in &certificate_and_key.certificate_chain {
            if X509::from_pem(certificate.as_bytes()).is_err() {
                return false;
            }
        }

        if X509::from_pem(&mut certificate_reader).is_ok() && PKey::private_key_from_pem(&mut pkey_reader).is_ok() {
            return true;
        }

        false
    }
}

impl Default for GenericCertificateResolver {
    fn default() -> Self {
        Self {
            domains: TrieNode::root(),
            certificates: Default::default(),
            name_fingerprint_idx: Default::default(),
            overrides: Default::default()
        }
    }
}

impl GenericCertificateResolver {
    pub fn new() -> Self { Self::default() }

    fn is_required_for_domain(&self, names: &HashSet<String>, fingerprint: &CertFingerprint) -> bool {
        for name in names {
            if let Some(fingerprints) = self.name_fingerprint_idx.get(name) {
                if 1 == fingerprints.len() && fingerprints.get(fingerprint).is_some() {
                    return true;
                }
            }
        }

        false
    }

    fn get_expiration_override(&self, fingerprint: &CertFingerprint) -> Option<i64> {
        self.overrides.get(fingerprint)
            .map(|co| co.expiration)
            .flatten()
    }
}

#[derive(Debug)]
pub struct MutexWrappedCertificateResolver(pub Mutex<GenericCertificateResolver>);

impl ResolvesServerCert for MutexWrappedCertificateResolver {
    fn resolve(&self, client_hello: ClientHello) -> Option<CertifiedKey> {
        let server_name = client_hello.server_name();
        let sigschemes = client_hello.sigschemes();

        if server_name.is_none() {
            error!("cannot look up certificate: no SNI from session");
            return None;
        }

        let name: &str = server_name.unwrap().into();
        trace!("trying to resolve name: {:?} for signature scheme: {:?}", name, sigschemes);
        if let Ok(ref mut resolver) = self.0.try_lock() {
            //resolver.domains.print();
            if let Some((_, fingerprint)) = resolver.domains.domain_lookup(name.as_bytes(), true) {
                trace!("looking for certificate for {:?} with fingerprint {:?}", name, fingerprint);
                return resolver.certificates.get(fingerprint)
                    .map(Self::generate_certified_key)
                    .flatten();
            }
        }

        error!("could not look up a certificate for server name '{}'", name);
        None
    }
}

impl Default for MutexWrappedCertificateResolver {
    fn default() -> Self {
        Self(Mutex::new(GenericCertificateResolver::default()))
    }
}

impl MutexWrappedCertificateResolver {
    pub fn new() -> Self { Self::default() }

    fn generate_certified_key(certificate_and_key: &CertificateAndKey) -> Option<CertifiedKey> {
        let mut chain = Vec::new();

        let mut cert_reader = BufReader::new(certificate_and_key.certificate.as_bytes());
        let parsed_certs = pemfile::certs(&mut cert_reader);

        if let Ok(certs) = parsed_certs {
            for cert in certs {
                chain.push(cert);
            }
        } else {
            return None;
        }

        for ref cert in certificate_and_key.certificate_chain.iter() {
            let mut chain_cert_reader = BufReader::new(cert.as_bytes());
            if let Ok(parsed_chain_certs) = pemfile::certs(&mut chain_cert_reader) {
                for cert in parsed_chain_certs {
                    chain.push(cert);
                }
            }
        }

        let mut key_reader = BufReader::new(certificate_and_key.key.as_bytes());
        let parsed_key = pemfile::rsa_private_keys(&mut key_reader);

        if let Ok(keys) = parsed_key {
            if !keys.is_empty() {
                if let Ok(signing_key) = RSASigningKey::new(&keys[0]) {
                    let certified = CertifiedKey::new(chain, Arc::new(Box::new(signing_key)));
                    return Some(certified);
                }
            } else {
                let mut key_reader = BufReader::new(certificate_and_key.key.as_bytes());
                let parsed_key = pemfile::pkcs8_private_keys(&mut key_reader);
                if let Ok(keys) = parsed_key {
                    if !keys.is_empty() {
                        if let Ok(signing_key) = RSASigningKey::new(&keys[0]) {
                            let certified = CertifiedKey::new(chain, Arc::new(Box::new(signing_key)));
                            return Some(certified);
                        } else {
                            if let Ok(k) = rustls::sign::any_ecdsa_type(&keys[0]) {
                                let certified = CertifiedKey::new(chain, Arc::new(k));
                                return Some(certified);
                            } else {
                                error!("could not decode signing key (tried RSA and ECDSA)");
                            }
                        }
                    }
                }
            }
        } else {
            error!("could not parse private key: {:?}", parsed_key);
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, time::{SystemTime, Duration}, collections::HashSet};

    use super::{GenericCertificateResolver, GenericCertificateResolverError, CertificateResolver, CertificateResolverHelper};

    use crate::sozu_command::proxy::{AddCertificate, RemoveCertificate, CertificateAndKey};

    use x509_parser::pem::parse_x509_pem;

    #[test]
    fn lifecycle() -> Result<(), Box<dyn Error + Send + Sync>> {
        let front = "127.0.0.1:8080".parse()?;
        let mut resolver = GenericCertificateResolver::new();
        let certificate_and_key = CertificateAndKey {
            certificate:       String::from(include_str!("../assets/certificate.pem")),
            key:               String::from( include_str!("../assets/key.pem")),
            certificate_chain: vec![],
            versions:          vec![],
        };

        let (_, pem) = parse_x509_pem(certificate_and_key.certificate.as_bytes())
            .map_err(|err| GenericCertificateResolverError::PemParseError(err.to_string()))?;

        let fingerprint = resolver.add_certificate(&AddCertificate {
            front,
            certificate: certificate_and_key,
            names: None,
            expired_at: None
        })?;

        if resolver.get_certificate(&fingerprint).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        if let Err(err) = resolver.remove_certificate(&RemoveCertificate { front, fingerprint: fingerprint.to_owned() }) {
            match err {
                GenericCertificateResolverError::IsStillInUseError => { },
                _ => {
                    return Err(format!("the certificate must not been removed, {}", err.to_string()).into());
                }
            }
        }

        let names = resolver.certificate_names(&pem)?;
        if resolver.find_certificates_by_names(&names)?.is_empty() || resolver.get_certificate(&fingerprint).is_none() {
            return Err("failed to retrieve certificate that we had the command to delete, but mandatory".into());
        }

        Ok(())
    }

    #[test]
    fn name_override() -> Result<(), Box<dyn Error + Send + Sync>> {
        let front = "127.0.0.1:8080".parse()?;
        let mut resolver = GenericCertificateResolver::new();
        let certificate_and_key = CertificateAndKey {
            certificate:       String::from(include_str!("../assets/certificate.pem")),
            key:               String::from( include_str!("../assets/key.pem")),
            certificate_chain: vec![],
            versions:          vec![],
        };

        let (_, pem) = parse_x509_pem(certificate_and_key.certificate.as_bytes())
            .map_err(|err| GenericCertificateResolverError::PemParseError(err.to_string()))?;

        let fingerprint = resolver.add_certificate(&AddCertificate {
            front,
            certificate: certificate_and_key,
            names: Some(vec!["localhost".into(), "lolcatho.st".into()]),
            expired_at: None
        })?;

        if resolver.get_certificate(&fingerprint).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        if let Err(err) = resolver.remove_certificate(&RemoveCertificate { front, fingerprint: fingerprint.to_owned() }) {
            match err {
                GenericCertificateResolverError::IsStillInUseError => { },
                _ => {
                    return Err(format!("the certificate must not been removed, {}", err.to_string()).into());
                }
            }
        }

        let names = resolver.certificate_names(&pem)?;
        if resolver.find_certificates_by_names(&names)?.is_empty() || resolver.get_certificate(&fingerprint).is_none() {
            return Err("failed to retrieve certificate that we had the command to delete, but mandatory".into());
        }

        let mut lolcat = HashSet::new();
        lolcat.insert(String::from("lolcatho.st"));
        if resolver.find_certificates_by_names(&lolcat)?.is_empty() || resolver.get_certificate(&fingerprint).is_none() {
            return Err("failed to retrieve certificate that we had the command to delete, but mandatory".into());
        }

        Ok(())
    }

    #[test]
    fn replacement() -> Result<(), Box<dyn Error + Send + Sync>> {
        let front = "127.0.0.1:8080".parse()?;
        let mut resolver = GenericCertificateResolver::new();

        // ---------------------------------------------------------------------
        // load first certificate
        let certificate_and_key_1y = CertificateAndKey {
            certificate:       String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key:               String::from( include_str!("../assets/tests/key-1y.pem")),
            certificate_chain: vec![],
            versions:          vec![],
        };

        let (_, pem) = parse_x509_pem(certificate_and_key_1y.certificate.as_bytes())
            .map_err(|err| GenericCertificateResolverError::PemParseError(err.to_string()))?;

        let names_1y = resolver.certificate_names(&pem)?;
        let fingerprint_1y = resolver.add_certificate(&AddCertificate {
            front,
            certificate: certificate_and_key_1y,
            names: None,
            expired_at: None
        })?;

        if resolver.get_certificate(&fingerprint_1y).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        // ---------------------------------------------------------------------
        // load second certificate
        let certificate_and_key_2y = CertificateAndKey {
            certificate:       String::from(include_str!("../assets/tests/certificate-2y.pem")),
            key:               String::from( include_str!("../assets/tests/key-2y.pem")),
            certificate_chain: vec![],
            versions:          vec![],
        };

        let fingerprint_2y = resolver.add_certificate(&AddCertificate {
            front,
            certificate: certificate_and_key_2y,
            names: None,
            expired_at: None
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
        let front = "127.0.0.1:8080".parse()?;
        let mut resolver = GenericCertificateResolver::new();

        // ---------------------------------------------------------------------
        // load first certificate
        let certificate_and_key_1y = CertificateAndKey {
            certificate:       String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key:               String::from( include_str!("../assets/tests/key-1y.pem")),
            certificate_chain: vec![],
            versions:          vec![],
        };

        let (_, pem) = parse_x509_pem(certificate_and_key_1y.certificate.as_bytes())
            .map_err(|err| GenericCertificateResolverError::PemParseError(err.to_string()))?;

        let names_1y = resolver.certificate_names(&pem)?;
        let fingerprint_1y = resolver.add_certificate(&AddCertificate {
            front,
            certificate: certificate_and_key_1y,
            names: None,
            expired_at: Some((SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)? + Duration::from_secs(3*365*24*3600)).as_secs() as i64)
        })?;

        if resolver.get_certificate(&fingerprint_1y).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        // ---------------------------------------------------------------------
        // load second certificate
        let certificate_and_key_2y = CertificateAndKey {
            certificate:       String::from(include_str!("../assets/tests/certificate-2y.pem")),
            key:               String::from( include_str!("../assets/tests/key-2y.pem")),
            certificate_chain: vec![],
            versions:          vec![],
        };

        let fingerprint_2y = resolver.add_certificate(&AddCertificate {
            front,
            certificate: certificate_and_key_2y,
            names: None,
            expired_at: None
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
}