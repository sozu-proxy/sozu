use std::sync::Mutex;
use std::collections::HashMap;
use rustls::{ResolvesServerCert, SignatureScheme};
use rustls::sign::CertifiedKey;
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
