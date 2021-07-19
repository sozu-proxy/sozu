use std::collections::HashSet;
use std::error::Error;

use pem::parse;
use sha2::{Sha256, Digest};
use x509_parser::parse_x509_certificate;

pub fn calculate_fingerprint(certificate: &[u8]) -> Option<Vec<u8>> {
  parse(certificate).map(|data| {
    Sha256::digest(&data.contents).iter().cloned().collect()
  }).ok()
}

pub fn calculate_fingerprint_from_der(certificate: &[u8]) -> Vec<u8> {
  Sha256::digest(&certificate).iter().cloned().collect()
}


pub fn split_certificate_chain(mut chain: String) -> Vec<String> {
  let mut v = Vec::new();

  let end = "-----END CERTIFICATE-----";
  loop {
    match chain.find(end) {
      Some(sz) => {
        let cert: String = chain.drain(..sz+end.len()).collect();
        v.push(cert.trim().to_string());
      },
      None     => break,
    }
  }
  v
}

pub fn get_certificate_names(certificate: &[u8]) -> Result<HashSet<String>, Box<dyn Error + Send + Sync>> {
  let (_, x509) = parse_x509_certificate(certificate)
      .map_err(|err| format!("failed to parse certificate, {}", err))?;

  let mut names = HashSet::new();
  for name in x509.subject().iter_common_name() {
    names.insert(name.attr_value
      .as_str()
      .map(String::from)
      .map_err(|err| format!("failed to parse certificate common name as string, {}", err))?
    );
  }

  Ok(names)
}
