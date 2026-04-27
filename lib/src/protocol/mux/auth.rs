//! HTTP Basic authentication helpers shared by the H1 and H2 mux paths.
//!
//! The runtime stores credentials as `username:hex(sha256(password))`
//! entries on each cluster's `authorized_hashes`. On every request that
//! traverses a frontend with `required_auth = true`, the mux extracts the
//! `Authorization: Basic <token>` header from the front kawa, decodes the
//! base64 token, splits on the first `:` into `<user>:<password>`, hashes
//! the password with SHA-256, and rebuilds the canonical
//! `<user>:<hex(sha256)>` form. Comparison against the cluster's hash list
//! uses [`subtle::ConstantTimeEq`] over a full pass, never short-circuiting,
//! so the time spent validating a credential does not leak which slot
//! matched (or whether any did at all).
//!
//! The extractor is intentionally permissive (`Option`-returning) — any
//! malformed input is reported as "no credential", and the caller emits
//! the standard 401 response. We never panic on hostile input.

use base64::{Engine, engine::general_purpose::STANDARD};
use kawa::{Block, Pair, Store};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::GenericHttpStream;
use crate::protocol::http::parser::{Method, compare_no_case};

/// Maximum length of a base64-decoded `Authorization: Basic` payload we
/// accept. RFC 7617 does not impose a limit, but credentials longer than
/// 4 KiB are pathological for HTTP Basic. Capping the decode result keeps
/// hostile peers from forcing the worker to allocate large transient
/// buffers per failed auth attempt.
const MAX_DECODED_CREDENTIAL_BYTES: usize = 4096;

/// Lowercase hex encoding of `bytes`. Matches the format every entry in
/// `Cluster.authorized_hashes` is expected to use, so the encoded value
/// can be compared bit-for-bit.
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // `{:02x}` is the canonical lowercase 2-wide hex representation.
        out.push(char::from_digit((byte >> 4) as u32, 16).expect("hi nibble"));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).expect("lo nibble"));
    }
    out
}

/// Find the first `Authorization` header value in the front kawa.
///
/// Returns the raw bytes of the header value. Header names are matched
/// case-insensitively per RFC 9110 §5.1. Returns `None` when no header is
/// present, when the header has been elided (`Store::Empty`), or when the
/// value's underlying bytes can't be resolved against the kawa buffer.
pub fn extract_authorization_header(kawa: &GenericHttpStream) -> Option<Vec<u8>> {
    let buf = kawa.storage.buffer();
    for block in &kawa.blocks {
        if let Block::Header(Pair { key, val }) = block {
            if matches!(key, Store::Empty) {
                continue;
            }
            let key_bytes = key.data(buf);
            if compare_no_case(key_bytes, b"authorization") {
                return Some(val.data(buf).to_vec());
            }
        }
    }
    None
}

/// Decode a `Basic <token>` value into the canonical
/// `username:hex(sha256(password))` shape that
/// [`check_authorized_hashes`] compares against. Returns `None` for any
/// malformed input — wrong scheme, non-base64 token, missing `:`,
/// non-UTF-8 username, or oversized payload.
pub fn canonicalize_basic_credentials(value: &[u8]) -> Option<String> {
    // Trim leading whitespace so e.g. `Authorization: Basic abc==` (one
    // space, the canonical form) and `Authorization:  Basic abc==` (two
    // spaces, still RFC-compliant) both parse.
    let mut rest = value;
    while let Some((&first, tail)) = rest.split_first() {
        if first == b' ' || first == b'\t' {
            rest = tail;
        } else {
            break;
        }
    }

    // RFC 7617 § 2 mandates ASCII-case-insensitive `Basic` scheme prefix.
    let scheme_len = b"Basic".len();
    if rest.len() < scheme_len || !compare_no_case(&rest[..scheme_len], b"basic") {
        return None;
    }
    rest = &rest[scheme_len..];

    // Exactly one space separates the scheme from the token in practice;
    // trim any whitespace we encounter to be tolerant of single-tab or
    // double-space inputs.
    while let Some((&first, tail)) = rest.split_first() {
        if first == b' ' || first == b'\t' {
            rest = tail;
        } else {
            break;
        }
    }
    if rest.is_empty() {
        return None;
    }

    let decoded = STANDARD.decode(rest).ok()?;
    if decoded.len() > MAX_DECODED_CREDENTIAL_BYTES {
        return None;
    }

    let colon = decoded.iter().position(|&b| b == b':')?;
    let username = std::str::from_utf8(&decoded[..colon]).ok()?;
    let password = &decoded[colon + 1..];

    let mut hasher = Sha256::new();
    hasher.update(password);
    let digest = hasher.finalize();

    Some(format!("{}:{}", username, to_hex(&digest)))
}

/// Compare `candidate` against every entry in `authorized_hashes` using
/// constant-time equality. Returns `true` if any entry matches.
///
/// Iterates the entire slice on every call regardless of where (or whether)
/// a match is found, so the time spent validating a credential does not
/// vary with the position of the matching entry. This defeats timing
/// side-channel attacks that would otherwise leak the size of the realm
/// or the index of the matching credential.
pub fn check_authorized_hashes(candidate: &str, authorized_hashes: &[String]) -> bool {
    let candidate = candidate.as_bytes();
    let mut matched = subtle::Choice::from(0u8);
    for hash in authorized_hashes {
        // ConstantTimeEq returns Choice(0)/Choice(1) without short-circuit.
        // BitOr-assign over `Choice` likewise does not branch.
        matched |= candidate.ct_eq(hash.as_bytes());
    }
    bool::from(matched)
}

/// Convenience: pull `Authorization` from the kawa, canonicalise, and
/// compare in constant time against the authorized list.
pub fn check_basic(kawa: &GenericHttpStream, authorized_hashes: &[String]) -> bool {
    if authorized_hashes.is_empty() {
        return false;
    }
    let Some(header_value) = extract_authorization_header(kawa) else {
        return false;
    };
    let Some(canonical) = canonicalize_basic_credentials(&header_value) else {
        return false;
    };
    check_authorized_hashes(&canonical, authorized_hashes)
}

/// Suppress the unused-import lint on `Method` when this module is built
/// without test cfg. The type is named in doc-comment intra-doc links and
/// pulled in here purely for that purpose.
#[allow(dead_code)]
const _METHOD_USED: Option<Method> = None;

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 of the literal byte string `"secret"`, lowercase hex.
    /// Computed once and pinned here so the unit tests don't have to
    /// re-derive it on every run.
    const SECRET_SHA256_HEX: &str =
        "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b";

    #[test]
    fn to_hex_matches_format_macro() {
        let bytes = [0x00, 0x10, 0xff, 0xa5];
        assert_eq!(to_hex(&bytes), "0010ffa5");
    }

    #[test]
    fn canonicalize_round_trips_admin_secret() {
        // base64("admin:secret") = "YWRtaW46c2VjcmV0"
        let canonical = canonicalize_basic_credentials(b"Basic YWRtaW46c2VjcmV0")
            .expect("well-formed credential should canonicalize");
        assert_eq!(canonical, format!("admin:{SECRET_SHA256_HEX}"));
    }

    #[test]
    fn canonicalize_is_case_insensitive_on_scheme() {
        let canonical = canonicalize_basic_credentials(b"basic YWRtaW46c2VjcmV0")
            .expect("lowercase scheme should still canonicalize");
        assert_eq!(canonical, format!("admin:{SECRET_SHA256_HEX}"));
    }

    #[test]
    fn canonicalize_rejects_non_basic_scheme() {
        assert!(canonicalize_basic_credentials(b"Bearer token").is_none());
    }

    #[test]
    fn canonicalize_rejects_garbage_base64() {
        assert!(canonicalize_basic_credentials(b"Basic !!not-base64!!").is_none());
    }

    #[test]
    fn canonicalize_rejects_missing_colon() {
        // base64("admin") = "YWRtaW4="
        assert!(canonicalize_basic_credentials(b"Basic YWRtaW4=").is_none());
    }

    #[test]
    fn canonicalize_rejects_oversized_payload() {
        // 5 KB base64 payload — well above MAX_DECODED_CREDENTIAL_BYTES.
        let payload = "a".repeat(MAX_DECODED_CREDENTIAL_BYTES * 2);
        let token = STANDARD.encode(format!("{payload}:pwd"));
        let header = format!("Basic {token}");
        assert!(canonicalize_basic_credentials(header.as_bytes()).is_none());
    }

    #[test]
    fn check_authorized_hashes_full_pass_match() {
        let valid = format!("admin:{SECRET_SHA256_HEX}");
        let other = "user:0000000000000000000000000000000000000000000000000000000000000000";
        let list = [other.to_owned(), valid.clone()];
        assert!(check_authorized_hashes(&valid, &list));
    }

    #[test]
    fn check_authorized_hashes_rejects_wrong_password() {
        let wrong = "admin:0000000000000000000000000000000000000000000000000000000000000000";
        let list = [format!("admin:{SECRET_SHA256_HEX}")];
        assert!(!check_authorized_hashes(wrong, &list));
    }

    #[test]
    fn check_authorized_hashes_rejects_when_list_empty() {
        let candidate = format!("admin:{SECRET_SHA256_HEX}");
        assert!(!check_authorized_hashes(&candidate, &[]));
    }
}
