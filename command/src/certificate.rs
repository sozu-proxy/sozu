use std::{fmt, str::FromStr};

use hex::{FromHex, FromHexError};
use serde::de::{self, Visitor};
use sha2::{Digest, Sha256};
use x509_parser::{
    certificate::X509Certificate,
    extensions::{GeneralName, ParsedExtension},
    oid_registry::{OID_X509_COMMON_NAME, OID_X509_EXT_SUBJECT_ALT_NAME},
    parse_x509_certificate,
    pem::{parse_x509_pem, Pem},
};

use crate::{
    config::{Config, ConfigError},
    proto::command::{CertificateAndKey, TlsVersion},
};

// -----------------------------------------------------------------------------
// CertificateError

#[derive(thiserror::Error, Debug)]
pub enum CertificateError {
    #[error("Could not parse PEM certificate from bytes: {0}")]
    InvalidCertificate(String),
    #[error("failed to parse tls version '{0}'")]
    InvalidTlsVersion(String),
    #[error("failed to parse fingerprint, {0}")]
    InvalidFingerprint(FromHexError),
    #[error("could not load file on path {path}: {error}")]
    LoadFile { path: String, error: ConfigError },
    #[error("Failed at decoding the hex encoded certificate: {0}")]
    DecodeError(FromHexError),
}

// -----------------------------------------------------------------------------
// parse

/// parse a pem file encoded as binary and convert it into the right structure
/// (a.k.a [`Pem`])
pub fn parse_pem(certificate: &[u8]) -> Result<Pem, CertificateError> {
    let (_, pem) = parse_x509_pem(certificate)
        .map_err(|err| CertificateError::InvalidCertificate(err.to_string()))?;

    Ok(pem)
}

pub fn parse_x509(pem_bytes: &[u8]) -> Result<X509Certificate, CertificateError> {
    parse_x509_certificate(pem_bytes)
        .map_err(|nom_e| CertificateError::InvalidCertificate(nom_e.to_string()))
        .map(|t| t.1)
}

// -----------------------------------------------------------------------------
// get_cn_and_san_attributes

/// Retrieve from the pem (as bytes) the common name (a.k.a `CN`) and the
/// subject alternate names (a.k.a `SAN`)
pub fn get_cn_and_san_attributes(x509: &X509Certificate) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in x509.subject().iter_by_oid(&OID_X509_COMMON_NAME) {
        names.push(
            name.as_str()
                .map(String::from)
                .unwrap_or_else(|_| String::from_utf8_lossy(name.as_slice()).to_string()),
        );
    }

    for extension in x509.extensions() {
        if extension.oid == OID_X509_EXT_SUBJECT_ALT_NAME {
            if let ParsedExtension::SubjectAlternativeName(san) = extension.parsed_extension() {
                for name in &san.general_names {
                    if let GeneralName::DNSName(name) = name {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }
    names.dedup();
    names
}

// -----------------------------------------------------------------------------
// TlsVersion

impl FromStr for TlsVersion {
    type Err = CertificateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "SSL_V2" => Ok(TlsVersion::SslV2),
            "SSL_V3" => Ok(TlsVersion::SslV3),
            "TLSv1" => Ok(TlsVersion::TlsV10),
            "TLS_V11" => Ok(TlsVersion::TlsV11),
            "TLS_V12" => Ok(TlsVersion::TlsV12),
            "TLS_V13" => Ok(TlsVersion::TlsV13),
            _ => Err(CertificateError::InvalidTlsVersion(s.to_string())),
        }
    }
}

// -----------------------------------------------------------------------------
// Fingerprint

//FIXME: make fixed size depending on hash algorithm
/// A TLS certificates, encoded in bytes
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub Vec<u8>);

impl FromStr for Fingerprint {
    type Err = CertificateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        hex::decode(s)
            .map_err(CertificateError::InvalidFingerprint)
            .map(Fingerprint)
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "CertificateFingerprint({})", hex::encode(&self.0))
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "{}", hex::encode(&self.0))
    }
}

impl serde::Serialize for Fingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&hex::encode(&self.0))
    }
}

struct FingerprintVisitor;

impl<'de> Visitor<'de> for FingerprintVisitor {
    type Value = Fingerprint;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("the certificate fingerprint must be in hexadecimal format")
    }

    fn visit_str<E>(self, value: &str) -> Result<Fingerprint, E>
    where
        E: de::Error,
    {
        FromHex::from_hex(value)
            .map_err(|e| E::custom(format!("could not deserialize hex: {e:?}")))
            .map(Fingerprint)
    }
}

impl<'de> serde::Deserialize<'de> for Fingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Fingerprint, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        deserializer.deserialize_str(FingerprintVisitor {})
    }
}

/// Compute fingerprint from decoded pem as binary value
pub fn calculate_fingerprint_from_der(certificate: &[u8]) -> Vec<u8> {
    Sha256::digest(certificate).iter().cloned().collect()
}

/// Compute fingerprint from a certificate that is encoded in pem format
pub fn calculate_fingerprint(certificate: &[u8]) -> Result<Vec<u8>, CertificateError> {
    let parsed_certificate = parse_pem(certificate)
        .map_err(|parse_error| CertificateError::InvalidCertificate(parse_error.to_string()))?;

    Ok(calculate_fingerprint_from_der(&parsed_certificate.contents))
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

pub fn get_fingerprint_from_certificate_path(
    certificate_path: &str,
) -> Result<Fingerprint, CertificateError> {
    let bytes =
        Config::load_file_bytes(certificate_path).map_err(|e| CertificateError::LoadFile {
            path: certificate_path.to_string(),
            error: e,
        })?;

    let parsed_bytes = calculate_fingerprint(&bytes)?;

    Ok(Fingerprint(parsed_bytes))
}

pub fn decode_fingerprint(fingerprint: &str) -> Result<Fingerprint, CertificateError> {
    let bytes = hex::decode(fingerprint).map_err(CertificateError::DecodeError)?;
    Ok(Fingerprint(bytes))
}

pub fn load_full_certificate(
    certificate_path: &str,
    certificate_chain_path: &str,
    key_path: &str,
    versions: Vec<TlsVersion>,
    names: Vec<String>,
) -> Result<CertificateAndKey, CertificateError> {
    let certificate =
        Config::load_file(certificate_path).map_err(|e| CertificateError::LoadFile {
            path: certificate_path.to_string(),
            error: e,
        })?;

    let certificate_chain = Config::load_file(certificate_chain_path)
        .map(split_certificate_chain)
        .map_err(|e| CertificateError::LoadFile {
            path: certificate_chain_path.to_string(),
            error: e,
        })?;

    let key = Config::load_file(key_path).map_err(|e| CertificateError::LoadFile {
        path: key_path.to_string(),
        error: e,
    })?;

    let versions = versions.iter().map(|v| *v as i32).collect();

    Ok(CertificateAndKey {
        certificate,
        certificate_chain,
        key,
        versions,
        names,
    })
}

impl CertificateAndKey {
    pub fn fingerprint(&self) -> Result<Fingerprint, CertificateError> {
        let pem = parse_pem(self.certificate.as_bytes())?;
        let fingerprint = Fingerprint(Sha256::digest(pem.contents).iter().cloned().collect());
        Ok(fingerprint)
    }

    pub fn get_overriding_names(&self) -> Result<Vec<String>, CertificateError> {
        if self.names.is_empty() {
            let pem = parse_pem(self.certificate.as_bytes())?;
            let x509 = parse_x509(&pem.contents)?;

            let overriding_names = get_cn_and_san_attributes(&x509);

            Ok(overriding_names.into_iter().collect())
        } else {
            Ok(self.names.to_owned())
        }
    }

    pub fn apply_overriding_names(&mut self) -> Result<(), CertificateError> {
        self.names = self.get_overriding_names()?;
        Ok(())
    }
}
