use anyhow::{self, Context};
use pem::parse;
use sha2::{Digest, Sha256};

pub fn calculate_fingerprint(certificate: &[u8]) -> anyhow::Result<Vec<u8>> {
    let parsed_certificate = parse(certificate).with_context(|| "Can not parse certificate")?;
    let fingerprint = Sha256::digest(&parsed_certificate.contents)
        .iter()
        .cloned()
        .collect();
    Ok(fingerprint)
}

pub fn calculate_fingerprint_from_der(certificate: &[u8]) -> Vec<u8> {
    Sha256::digest(certificate).iter().cloned().collect()
}

pub fn split_certificate_chain(mut chain: String) -> Vec<String> {
    let mut v = Vec::new();

    let end = "-----END CERTIFICATE-----";
    loop {
        if let Some(sz) = chain.find(end) {
            let cert: String = chain.drain(..sz + end.len()).collect();
            v.push(cert.trim().to_string());
            continue;
        }

        break;
    }

    v
}
