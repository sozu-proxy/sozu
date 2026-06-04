use std::{fmt, str::FromStr};

use hex::{FromHex, FromHexError};
use serde::de::{self, Visitor};
use sha2::{Digest, Sha256};
use x509_parser::{
    certificate::X509Certificate,
    extensions::{GeneralName, ParsedExtension},
    oid_registry::{OID_X509_COMMON_NAME, OID_X509_EXT_SUBJECT_ALT_NAME},
    parse_x509_certificate,
    pem::{Pem, parse_x509_pem},
};

use crate::{
    config::{Config, ConfigError},
    proto::command::{CertificateAndKey, TlsVersion},
};

/// Byte length of a SHA-256 digest. Every fingerprint Sōzu *computes*
/// (as opposed to one it *parses* from operator hex, which may be any
/// length) is a SHA-256 hash and therefore exactly this many bytes.
///
/// Intentionally NOT `#[cfg(debug_assertions)]`-gated: `debug_assert!`
/// compiles its arguments in every profile (it gates execution, not
/// compilation), so a cfg-gated const referenced inside one would fail
/// the release build with E0425. Ungated, it is dead code in release and
/// dropped by the optimizer.
#[allow(dead_code)]
const SHA256_FINGERPRINT_LEN: usize = 32;

// -----------------------------------------------------------------------------
// CertificateError

#[derive(thiserror::Error, Debug)]
pub enum CertificateError {
    #[error("Could not parse PEM certificate from bytes: {0}")]
    ParsePEMCertificate(String),
    #[error("Could not parse X509 certificate from bytes: {0}")]
    ParseX509Certificate(String),
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
        .map_err(|err| CertificateError::ParsePEMCertificate(err.to_string()))?;

    Ok(pem)
}

/// parse x509 certificate from PEM bytes
pub fn parse_x509(pem_bytes: &[u8]) -> Result<X509Certificate<'_>, CertificateError> {
    parse_x509_certificate(pem_bytes)
        .map_err(|nom_e| CertificateError::ParseX509Certificate(nom_e.to_string()))
        .map(|t| t.1)
}

// -----------------------------------------------------------------------------
// get_cn_and_san_attributes

/// Retrieve the certificate's authoritative DNS identities for routing.
///
/// Per RFC 6125 §6.4.4: when the SubjectAlternativeName extension contains
/// at least one `dNSName` entry, the SAN entries are the sole authoritative
/// identities and the Common Name is ignored. The CN is only honoured as a
/// fallback when the certificate omits the SAN extension entirely or
/// declares it without a `dNSName` (e.g. SAN with only `iPAddress` /
/// `rfc822Name` / `directoryName` entries — uncommon, but legal).
///
/// Aligns Sōzu's coalescing trust boundary with browser implementations
/// (Firefox / Chrome both stopped honouring CN for hostname verification
/// circa 2017) so a cert with `CN=tenant-b.example` and `SAN=tenant-a.example`
/// cannot smuggle `tenant-b.example` into the routing authority list.
pub fn get_cn_and_san_attributes(x509: &X509Certificate) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut san_dns_seen = false;

    for extension in x509.extensions() {
        if extension.oid == OID_X509_EXT_SUBJECT_ALT_NAME {
            if let ParsedExtension::SubjectAlternativeName(san) = extension.parsed_extension() {
                for name in &san.general_names {
                    if let GeneralName::DNSName(name) = name {
                        san_dns_seen = true;
                        names.push(name.to_string());
                    }
                }
            }
        }
    }

    // POST: a dNSName SAN entry was observed iff at least one name has been
    // collected from the SAN branch. `san_dns_seen` and a non-empty `names`
    // must agree before the CN fallback runs — otherwise the RFC 6125
    // §6.4.4 trust boundary (SAN dNSName is authoritative when present) is
    // broken and the CN could smuggle an extra identity below.
    debug_assert_eq!(
        san_dns_seen,
        !names.is_empty(),
        "SAN dNSName presence must match the collected-names state before CN fallback"
    );

    if !san_dns_seen {
        for name in x509.subject().iter_by_oid(&OID_X509_COMMON_NAME) {
            names.push(
                name.as_str()
                    .map(String::from)
                    .unwrap_or_else(|_| String::from_utf8_lossy(name.as_slice()).to_string()),
            );
        }
    }
    let before_dedup = names.len();
    names.dedup();
    // POST: dedup only removes *consecutive* equal entries, so it can never
    // grow the list; the deduped length is an invariant upper bound.
    debug_assert!(
        names.len() <= before_dedup,
        "dedup must not grow the identity list"
    );
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6125 §6.4.4: when SAN contains at least one dNSName, the CN is
    /// ignored. A cert with `CN=tenant-b.example` and SAN `tenant-a.example`
    /// is authoritative for `tenant-a.example` only.
    #[test]
    fn san_dns_present_excludes_cn() {
        let pem = parse_pem(include_str!("../../lib/assets/cn-ne-san-cert.pem").as_bytes())
            .expect("parse PEM");
        let x509 = parse_x509(&pem.contents).expect("parse x509");
        let names = get_cn_and_san_attributes(&x509);
        assert_eq!(names, vec![String::from("tenant-a.example")]);
    }

    /// Fallback: SAN extension absent (no dNSName entries) ⇒ CN is honoured.
    /// `lib/assets/certificate.pem` (CN=lolcatho.st, no SAN extension) is the
    /// canonical fixture for this branch.
    #[test]
    fn cn_used_when_san_absent() {
        let pem = parse_pem(include_str!("../../lib/assets/certificate.pem").as_bytes())
            .expect("parse PEM");
        let x509 = parse_x509(&pem.contents).expect("parse x509");
        let names = get_cn_and_san_attributes(&x509);
        assert_eq!(names, vec![String::from("lolcatho.st")]);
    }

    /// SAN dNSName present and CN ∈ SAN ⇒ the resulting list is the SAN
    /// dNSName set verbatim (dedup removes the duplicate CN entry from the
    /// pre-fix code path; the post-fix code never inserts the CN at all,
    /// so the same list is observed but via a tighter path).
    #[test]
    fn san_dns_present_cn_is_san_member() {
        let pem = parse_pem(include_str!("../../lib/assets/multi-sni-cert.pem").as_bytes())
            .expect("parse PEM");
        let x509 = parse_x509(&pem.contents).expect("parse x509");
        let names = get_cn_and_san_attributes(&x509);
        assert!(names.contains(&String::from("foo.example.com")));
        assert!(names.contains(&String::from("bar.example.com")));
        assert!(names.contains(&String::from("baz.example.com")));
        assert!(names.contains(&String::from("localhost")));
        assert_eq!(names.len(), 4);
    }
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
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
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

impl Visitor<'_> for FingerprintVisitor {
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
    let fingerprint: Vec<u8> = Sha256::digest(certificate).iter().cloned().collect();
    // POST: a SHA-256 digest is unconditionally 32 bytes. Anything else
    // means the digest collection went wrong and downstream fingerprint
    // comparison / map keying would silently mismatch.
    debug_assert_eq!(
        fingerprint.len(),
        SHA256_FINGERPRINT_LEN,
        "SHA-256 fingerprint must be exactly 32 bytes"
    );
    fingerprint
}

/// Compute fingerprint from a certificate that is encoded in pem format
pub fn calculate_fingerprint(certificate: &[u8]) -> Result<Vec<u8>, CertificateError> {
    let parsed_certificate = parse_pem(certificate)?;
    let fingerprint = calculate_fingerprint_from_der(&parsed_certificate.contents);
    // POST: the result is a SHA-256 digest and recomputing it over the same
    // DER bytes yields the same value — the fingerprint is a pure function of
    // the parsed certificate contents, never of the surrounding PEM framing.
    debug_assert_eq!(
        fingerprint.len(),
        SHA256_FINGERPRINT_LEN,
        "PEM fingerprint must be a 32-byte SHA-256 digest"
    );
    debug_assert_eq!(
        fingerprint,
        calculate_fingerprint_from_der(&parsed_certificate.contents),
        "fingerprint must be a deterministic function of the DER contents"
    );
    Ok(fingerprint)
}

pub fn split_certificate_chain(mut chain: String) -> Vec<String> {
    let mut v = Vec::new();

    let end = "-----END CERTIFICATE-----";
    // PRE: the loop consumes exactly one END marker per iteration, so the
    // final chain length must equal the number of markers present on entry.
    // The leaf certificate is, by PEM convention, the first block — it lands
    // at index 0 because we drain from the front. (`matches().count()` is
    // overlap-free; the END marker cannot overlap itself.)
    let expected_certs = chain.matches(end).count();
    loop {
        if let Some(sz) = chain.find(end) {
            let cert: String = chain.drain(..sz + end.len()).collect();
            // INV: every emitted block carries exactly the END marker that
            // terminated it — the drain range includes `end.len()` bytes past
            // the match, so a non-terminated trailing block is impossible.
            debug_assert!(
                cert.contains(end),
                "each split block must contain its END CERTIFICATE marker"
            );
            v.push(cert.trim().to_string());
            continue;
        }

        break;
    }

    // POST: one block per END marker, leaf at index 0.
    debug_assert_eq!(
        v.len(),
        expected_certs,
        "split must yield exactly one certificate per END marker"
    );
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

    // POST: a computed fingerprint is always a 32-byte SHA-256 digest. (This
    // is distinct from a *parsed* fingerprint — see `decode_fingerprint` /
    // `Fingerprint::from_str` — which may be any operator-supplied length.)
    debug_assert_eq!(
        parsed_bytes.len(),
        SHA256_FINGERPRINT_LEN,
        "fingerprint loaded from a certificate path must be 32 bytes"
    );
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

    let versions_len = versions.len();
    let names_len = names.len();
    let versions: Vec<i32> = versions.iter().map(|v| *v as i32).collect();

    // POST: the i32-encoded TLS-version list is a 1:1 map of the input — no
    // version is dropped or duplicated by the `as i32` projection.
    debug_assert_eq!(
        versions.len(),
        versions_len,
        "version encoding must preserve the input cardinality"
    );

    let built = CertificateAndKey {
        certificate,
        certificate_chain,
        key,
        versions,
        names,
    };

    // POST: the routing-name list is carried through verbatim — the builder
    // does not synthesize or drop names (overriding names are derived later
    // via `apply_overriding_names`, never here).
    debug_assert_eq!(
        built.names.len(),
        names_len,
        "names must be carried through the builder unchanged"
    );
    Ok(built)
}

impl CertificateAndKey {
    pub fn fingerprint(&self) -> Result<Fingerprint, CertificateError> {
        let pem = parse_pem(self.certificate.as_bytes())?;
        let fingerprint = Fingerprint(Sha256::digest(&pem.contents).iter().cloned().collect());
        // POST: the certificate fingerprint is a 32-byte SHA-256 digest of the
        // DER contents and agrees with the free-function recompute over the
        // same bytes — the two fingerprint paths must never diverge or a cert
        // would key into two different map slots.
        debug_assert_eq!(
            fingerprint.0.len(),
            SHA256_FINGERPRINT_LEN,
            "CertificateAndKey fingerprint must be 32 bytes"
        );
        debug_assert_eq!(
            fingerprint.0,
            calculate_fingerprint_from_der(&pem.contents),
            "method and free-function fingerprints must agree on the same DER"
        );
        Ok(fingerprint)
    }

    pub fn get_overriding_names(&self) -> Result<Vec<String>, CertificateError> {
        if self.names.is_empty() {
            let pem = parse_pem(self.certificate.as_bytes())?;
            let x509 = parse_x509(&pem.contents)?;

            let overriding_names = get_cn_and_san_attributes(&x509);

            Ok(overriding_names.into_iter().collect())
        } else {
            let names = self.names.to_owned();
            // POST: when explicit names are set, they are returned verbatim —
            // the cert is NOT consulted, so the operator's intent is the sole
            // authority. (`to_owned` preserves both length and order.)
            debug_assert_eq!(
                names, self.names,
                "explicit names must be returned unchanged when present"
            );
            Ok(names)
        }
    }

    pub fn apply_overriding_names(&mut self) -> Result<(), CertificateError> {
        let resolved = self.get_overriding_names()?;
        self.names = resolved.clone();
        // POST: after applying, the stored names are exactly the resolved set
        // and re-resolving is idempotent — a second `apply_overriding_names`
        // would now hit the "explicit names present" branch and be a no-op.
        debug_assert_eq!(
            self.names, resolved,
            "applied names must equal the resolved set"
        );
        Ok(())
    }
}
