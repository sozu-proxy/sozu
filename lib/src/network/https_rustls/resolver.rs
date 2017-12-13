use std::sync::Mutex;
use std::collections::HashMap;
use rustls::{ResolvesServerCert, SignatureScheme};
use rustls::sign::CertifiedKey;

use sozu_command::messages::{CertificateAndKey, CertFingerprint};

pub struct CertificateResolver {
  map: Mutex<HashMap<String, String>>,
}

impl CertificateResolver {
  pub fn new() -> CertificateResolver {
    CertificateResolver {
      map: Mutex::new(HashMap::new()),
    }
  }

  pub fn add_certificate(&self, certificate_and_key: CertificateAndKey) -> bool {
    if let Ok(ref mut map) = self.map.try_lock() {
      map.insert(String::from("hello"), String::from("world"));
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

impl ResolvesServerCert for CertificateResolver {
  fn resolve(
        &self,
        server_name: Option<&str>,
        sigschemes: &[SignatureScheme]
    ) -> Option<CertifiedKey> {

    if let Ok(ref mut map) = self.map.try_lock() {
      map.insert(String::from("hello"), String::from("world"));
    }

    None
  }
}

pub struct TlsData {
  certificate: Vec<u8>,
  refcount:    usize,
  initialized: bool,
}
