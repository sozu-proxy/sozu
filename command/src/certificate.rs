use openssl::x509::X509;
use openssl::error::ErrorStack;
use openssl::hash::MessageDigest;

pub fn calculate_fingerprint(certificate: &[u8]) -> Result<Vec<u8>, ErrorStack> {
  X509::from_pem(certificate).and_then(|cert| cert.fingerprint(MessageDigest::sha256()))
}
