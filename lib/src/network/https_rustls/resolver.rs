use std::sync::{Arc,Mutex};
use std::collections::HashMap;
use std::io::BufReader;
use webpki;
use rustls::{ResolvesServerCert, SignatureScheme, PrivateKey};
use rustls::sign::{CertifiedKey, RSASigningKey};
use rustls::internal::pemfile;

use sozu_command::messages::{CertificateAndKey, CertFingerprint};
use sozu_command::certificate::calculate_fingerprint_from_der;

use network::trie::TrieNode;

struct TlsData {
  pub cert:     CertifiedKey,
  pub refcount: usize,
}

pub struct CertificateResolver {
  domains:      TrieNode<CertFingerprint>,
  certificates: HashMap<CertFingerprint, TlsData>,
}

impl CertificateResolver {
  pub fn new() -> CertificateResolver {
    CertificateResolver {
      domains:      TrieNode::root(),
      certificates: HashMap::new(),
    }
  }

  pub fn add_certificate(&mut self, certificate_and_key: CertificateAndKey) -> Option<CertFingerprint> {
    if let Some(certified_key) = generate_certified_key(certificate_and_key) {
      //FIXME: waiting for https://github.com/briansmith/webpki/pull/65 to merge to get the DNS names
      let mut names = vec!(String::from("lolcatho.st"));
      let fingerprint = calculate_fingerprint_from_der(&certified_key.cert[0].0);
      info!("cert fingerprint: {:?}", fingerprint);
      // create a untrusted::Input
      // let input = untrusted::Input::from(&certs[0].0);
      // create an EndEntityCert
      // let ee = webpki::EndEntityCert::from(input).unwrap()
      // get names
      // let dns_names = ee.list_dns_names()
      // names.extend(dns_names.drain(..).map(|name| name.to_String()));

      let data = TlsData {
        cert:     certified_key,
        refcount: 0,
      };

      let fingerprint = CertFingerprint(fingerprint);
      self.certificates.insert(fingerprint.clone(), data);
      for name in names.drain(..) {
        self.domains.domain_insert(name.into_bytes(), fingerprint.clone());
      }

      Some(fingerprint)
    } else {
      None
    }
  }

  pub fn remove_certificate(&mut self, fingerprint: &CertFingerprint) {
    let must_delete = self.certificates.get(fingerprint).map(|data| data.refcount == 0).unwrap_or(false);

    if let Some(data) = self.certificates.get(fingerprint) {
      let cert = &data.cert.cert[0];
      let names = vec!(String::from("https://lolcatho.st"));

      // create a untrusted::Input
      // let input = untrusted::Input::from(&certs[0].0);
      // create an EndEntityCert
      // let ee = webpki::EndEntityCert::from(input).unwrap()
      // get names
      // let dns_names = ee.list_dns_names()
      // names.extend(dns_names.drain(..).map(|name| name.to_String()));

      for name in names {
        self.domains.domain_remove(&name.into_bytes());
      }
    }

    if must_delete {
      self.certificates.remove(fingerprint);
    }
  }

  pub fn add_front(&mut self, fingerprint: &CertFingerprint) -> bool {
    if self.certificates.contains_key(fingerprint) {
      self.certificates.get_mut(fingerprint).map(|data| data.refcount += 1);
      true
    } else {
      false
    }
  }

  pub fn remove_front(&mut self, fingerprint: &CertFingerprint) {
    self.certificates.get_mut(fingerprint).map(|data| data.refcount -= 1);

    let must_delete = self.certificates.get(fingerprint).map(|data| data.refcount == 0).unwrap_or(false);
    if must_delete {
      self.certificates.remove(fingerprint);
    }
  }
}

pub struct CertificateResolverWrapper(Mutex<CertificateResolver>);

impl CertificateResolverWrapper {
  pub fn new() -> CertificateResolverWrapper {
    CertificateResolverWrapper(Mutex::new(CertificateResolver::new()))
  }

  pub fn add_certificate(&self, certificate_and_key: CertificateAndKey) -> Option<CertFingerprint> {
    if let Ok(ref mut resolver) = self.0.try_lock() {
      resolver.add_certificate(certificate_and_key)
    } else {
      None
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
        server_name: Option<webpki::DNSNameRef>,
        sigschemes: &[SignatureScheme]
    ) -> Option<CertifiedKey> {

    info!("trying to resolve name: {:?} for signature scheme: {:?}", server_name, sigschemes);
    if let Ok(ref mut resolver) = self.0.try_lock() {
      info!("got the resolver");
      if let Some(name) = server_name {
        resolver.domains.print();
        let s: &str = name.into();
        if let Some(kv) = resolver.domains.domain_lookup(s.as_bytes()) {
           info!("looking for certificate for {:?} with fingerprint {:?}", s, kv.1);
           return resolver.certificates.get(&kv.1).as_ref().map(|data| data.cert.clone());
        }
      }
    }


    None
  }
}

pub fn generate_certified_key(certificate_and_key: CertificateAndKey) -> Option<CertifiedKey> {
  let mut chain = Vec::new();

  let mut cert_reader = BufReader::new(certificate_and_key.certificate.as_bytes());
  let parsed_certs = pemfile::certs(&mut cert_reader);

  let fingerprint:Vec<u8> = Vec::new();
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
          }
        }
      }
    }
  } else {
    error!("could not parse private key: {:?}", parsed_key);
  }

  None
}
