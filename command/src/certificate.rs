use pem::parse;
use sha2::{Digest, Sha256};

pub fn calculate_fingerprint(certificate: &[u8]) -> Option<Vec<u8>> {
    parse(certificate)
        .map(|data| Sha256::digest(&data.contents).iter().cloned().collect())
        .ok()
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
                let cert: String = chain.drain(..sz + end.len()).collect();
                v.push(cert.trim().to_string());
            }
            None => break,
        }
    }
    v
}
