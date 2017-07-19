use openssl::x509::X509;
use openssl::error::ErrorStack;
use openssl::hash::MessageDigest;

pub fn calculate_fingerprint(certificate: &[u8]) -> Result<Vec<u8>, ErrorStack> {
  X509::from_pem(certificate).and_then(|cert| cert.fingerprint(MessageDigest::sha256()))
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
