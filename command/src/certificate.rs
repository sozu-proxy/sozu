use std::{error, fmt, str::FromStr};

use anyhow::{self, Context};
use hex::FromHex;
use pem::parse;
use serde::de::{self, Visitor};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CertificateAndKey {
    pub certificate: String,
    pub certificate_chain: Vec<String>,
    pub key: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<TlsVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TlsVersion {
    SSLv2,
    SSLv3,
    #[serde(rename = "TLSv1")]
    TLSv1_0,
    #[serde(rename = "TLSv1.1")]
    TLSv1_1,
    #[serde(rename = "TLSv1.2")]
    TLSv1_2,
    #[serde(rename = "TLSv1.3")]
    TLSv1_3,
}

#[derive(Debug)]
pub struct ParseErrorTlsVersion;

impl fmt::Display for ParseErrorTlsVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Cannot find the TLS version")
    }
}

impl error::Error for ParseErrorTlsVersion {
    fn description(&self) -> &str {
        "Cannot find the TLS version"
    }

    fn cause(&self) -> Option<&dyn error::Error> {
        None
    }
}

impl FromStr for TlsVersion {
    type Err = ParseErrorTlsVersion;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "SSLv2" => Ok(TlsVersion::SSLv2),
            "SSLv3" => Ok(TlsVersion::SSLv3),
            "TLSv1" => Ok(TlsVersion::TLSv1_0),
            "TLSv1.1" => Ok(TlsVersion::TLSv1_1),
            "TLSv1.2" => Ok(TlsVersion::TLSv1_2),
            "TLSv1.3" => Ok(TlsVersion::TLSv1_3),
            _ => Err(ParseErrorTlsVersion {}),
        }
    }
}

//FIXME: make fixed size depending on hash algorithm
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CertificateFingerprint(pub Vec<u8>);

impl fmt::Debug for CertificateFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "CertificateFingerprint({})", hex::encode(&self.0))
    }
}

impl fmt::Display for CertificateFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "{}", hex::encode(&self.0))
    }
}

impl serde::Serialize for CertificateFingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&hex::encode(&self.0))
    }
}

struct CertificateFingerprintVisitor;

impl<'de> Visitor<'de> for CertificateFingerprintVisitor {
    type Value = CertificateFingerprint;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("the certificate fingerprint must be in hexadecimal format")
    }

    fn visit_str<E>(self, value: &str) -> Result<CertificateFingerprint, E>
    where
        E: de::Error,
    {
        FromHex::from_hex(value)
            .map_err(|e| E::custom(format!("could not deserialize hex: {e:?}")))
            .map(CertificateFingerprint)
    }
}

impl<'de> serde::Deserialize<'de> for CertificateFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<CertificateFingerprint, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        deserializer.deserialize_str(CertificateFingerprintVisitor {})
    }
}

pub fn calculate_fingerprint(certificate: &[u8]) -> anyhow::Result<Vec<u8>> {
    let parsed_certificate = parse(certificate).with_context(|| "Can not parse certificate")?;
    let fingerprint = Sha256::digest(parsed_certificate.contents)
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
