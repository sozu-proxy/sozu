use pem::parse;
use sha2::{Sha256, Digest};

pub fn calculate_fingerprint(certificate: &[u8]) -> Option<Vec<u8>> {
  parse(certificate).map(|data| {
    Sha256::digest(&data.contents).iter().cloned().collect()
  }).ok()
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
