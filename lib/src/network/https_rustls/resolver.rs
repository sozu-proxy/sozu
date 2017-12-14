use std::sync::{Arc,Mutex};
use std::collections::HashMap;
use std::io::BufReader;
use rustls::{ResolvesServerCert, SignatureScheme, PrivateKey};
use rustls::sign::{CertifiedKey, RSASigningKey};
use rustls::internal::pemfile;
use webpki::DNSNameRef;

use sozu_command::messages::{CertificateAndKey, CertFingerprint};
use network::trie::TrieNode;

pub struct CertificateResolver {
  domains:      TrieNode<CertFingerprint>,
  certificates: HashMap<CertFingerprint, CertifiedKey>,
}

impl CertificateResolver {
  pub fn new() -> CertificateResolver {
    CertificateResolver {
      domains:      TrieNode::root(),
      certificates: HashMap::new(),
    }
  }

  pub fn add_certificate(&self, certificate_and_key: CertificateAndKey) -> bool {

    let mut chain = Vec::new();

    let mut cert_reader = BufReader::new(certificate_and_key.certificate.as_bytes());
    let parsed_certs = pemfile::certs(&mut cert_reader);
    //info!("parsed: {:?}", parsed_cert);
    if let Ok(certs) = parsed_certs {
      for cert in certs {
        chain.push(cert);
      }
    } else {
      return false;
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
    let parsed_key = pemfile::pkcs8_private_keys(&mut key_reader);

    if let Ok(keys) = parsed_key {
      if !keys.is_empty() {
        if let Ok(signing_key) = RSASigningKey::new(&keys[0]) {
          let certified = CertifiedKey::new(chain, Arc::new(Box::new(signing_key)));

          // add to certificate list here

          return true;
        }
      }
    }

    false
  }

  pub fn remove_certificate(&self, fingerprint: &CertFingerprint) {

  }

  pub fn add_front(&self, fingerprint: &CertFingerprint) -> bool {
    // increase refcount here
    // mark certificate as initialized

    //return false if we did not find a certificate
    false
  }

  pub fn remove_front(&self, fingerprint: &CertFingerprint) {
    // decrease refcount here
  }
}

pub struct CertificateResolverWrapper(Mutex<CertificateResolver>);

impl CertificateResolverWrapper {
  pub fn new() -> CertificateResolverWrapper {
    CertificateResolverWrapper(Mutex::new(CertificateResolver::new()))
  }

  pub fn add_certificate(&self, certificate_and_key: CertificateAndKey) -> bool {
    if let Ok(ref mut resolver) = self.0.try_lock() {
      resolver.add_certificate(certificate_and_key)
    } else {
      false
    }
  }

  pub fn remove_certificate(&self, fingerprint: &CertFingerprint) {
    if let Ok(ref mut resolver) = self.0.try_lock() {
      resolver.remove_certificate(fingerprint)
    }

  }

  pub fn add_front(&self, fingerprint: &CertFingerprint) -> bool {
    if let Ok(ref mut resolver) = self.0.try_lock() {
      resolver.add_front(fingerprint)
    } else {
      false
    }
  }

  pub fn remove_front(&self, fingerprint: &CertFingerprint) {
    if let Ok(ref mut resolver) = self.0.try_lock() {
      resolver.remove_front(fingerprint)
    }
  }
}

impl ResolvesServerCert for CertificateResolverWrapper {
  fn resolve(
        &self,
        server_name: Option<DNSNameRef>,
        sigschemes: &[SignatureScheme]
    ) -> Option<CertifiedKey> {

    info!("trying to resolve name: {:?} for signature scheme: {:?}", server_name, sigschemes);
    if let Ok(ref mut resolver) = self.0.try_lock() {

    }


    None
  }
}

pub struct TlsData {
  certificate: Vec<u8>,
  refcount:    usize,
  initialized: bool,
}
