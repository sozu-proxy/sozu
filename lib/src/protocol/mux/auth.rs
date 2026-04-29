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
use crate::protocol::http::parser::compare_no_case;

/// Built-in default for the maximum length, in bytes, of a base64-decoded
/// `Authorization: Basic` payload. RFC 7617 does not impose a limit, but
/// credentials longer than 4 KiB are pathological for HTTP Basic.
/// Operators can override via `basic_auth_max_credential_bytes` in the
/// main TOML config; the override is committed once at worker boot via
/// [`set_max_decoded_credential_bytes`].
const DEFAULT_MAX_DECODED_CREDENTIAL_BYTES: usize = 4096;

/// Process-wide override for [`DEFAULT_MAX_DECODED_CREDENTIAL_BYTES`].
/// Set once on each worker at startup from
/// [`sozu_command::proto::command::ServerConfig::basic_auth_max_credential_bytes`]
/// via [`set_max_decoded_credential_bytes`]. Reading uses a relaxed
/// `OnceLock::get` so the auth fast path stays branch-and-load with no
/// atomic hand-off. Set-once semantics are sufficient because the cap is
/// a global hardening knob — it never changes after boot.
static MAX_DECODED_CREDENTIAL_BYTES_OVERRIDE: std::sync::OnceLock<usize> =
    std::sync::OnceLock::new();

/// Install the operator-configured cap. Called from
/// `lib::server::Server::try_new_from_config` exactly once per worker
/// process. Subsequent calls are no-ops (the `OnceLock` rejects the
/// second `set`); the first wins. A `0` value is treated as "use the
/// built-in default" so an operator config that explicitly sets `0` does
/// not disable Basic-auth length-bound protection by accident.
pub fn set_max_decoded_credential_bytes(cap: usize) {
    if cap == 0 {
        return;
    }
    let _ = MAX_DECODED_CREDENTIAL_BYTES_OVERRIDE.set(cap);
}

/// Resolve the active cap: operator override when present, otherwise the
/// built-in [`DEFAULT_MAX_DECODED_CREDENTIAL_BYTES`].
fn max_decoded_credential_bytes() -> usize {
    MAX_DECODED_CREDENTIAL_BYTES_OVERRIDE
        .get()
        .copied()
        .unwrap_or(DEFAULT_MAX_DECODED_CREDENTIAL_BYTES)
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
    // Trim leading SP only. RFC 7235 §2.1 / RFC 9110 §11.4 define the
    // header value's grammar as `auth-scheme [ 1*SP token68 ]`, so HTAB
    // is not permitted between the value start and the scheme. Some
    // clients still emit a leading SP run before the scheme; that's
    // tolerated here because the OWS preceding the field-value is
    // already stripped by the parser.
    let mut rest = value;
    while let Some((&b' ', tail)) = rest.split_first() {
        rest = tail;
    }

    // RFC 7617 § 2 mandates ASCII-case-insensitive `Basic` scheme prefix.
    let scheme_len = b"Basic".len();
    if rest.len() < scheme_len || !compare_no_case(&rest[..scheme_len], b"basic") {
        return None;
    }
    rest = &rest[scheme_len..];

    // RFC 7235 §2.1: scheme and token68 are separated by `1*SP`.
    // Reject HTAB and reject zero spaces (the scheme-token boundary must
    // exist). Multiple leading spaces are tolerated for compatibility
    // with clients that emit `Basic  abc==`.
    let mut saw_space = false;
    while let Some((&b' ', tail)) = rest.split_first() {
        rest = tail;
        saw_space = true;
    }
    if !saw_space || rest.is_empty() {
        return None;
    }

    // ── Pre-decode length cap ──
    //
    // base64 expands 3 bytes → 4 characters. The post-decode length check
    // below would still allocate the full decoded payload before the
    // rejection runs, so a peer can force a per-attempt allocation up to
    // the request-buffer cap on every failed Basic-auth probe.
    // `basic_auth_max_credential_bytes` is meant as a per-request memory
    // bound; cap the *encoded* size first so `STANDARD.decode` never sees
    // a payload bigger than the bound it implies. The `+ 4` slack covers
    // up to two `=` padding characters plus rounding.
    let max_decoded = max_decoded_credential_bytes();
    let max_encoded = max_decoded.saturating_mul(4).saturating_add(2) / 3 + 4;
    if rest.len() > max_encoded {
        return None;
    }

    let decoded = STANDARD.decode(rest).ok()?;
    if decoded.len() > max_decoded {
        return None;
    }

    let colon = decoded.iter().position(|&b| b == b':')?;
    let username = std::str::from_utf8(&decoded[..colon]).ok()?;
    let password = &decoded[colon + 1..];

    let mut hasher = Sha256::new();
    hasher.update(password);
    let digest = hasher.finalize();

    Some(format!("{}:{}", username, hex::encode(digest)))
}

/// Maximum byte length of a canonical `username:hex(sha256)` credential
/// we are willing to compare in constant time. The realistic shape uses a
/// short username plus a 65-byte `:hex64` tail, so 256 covers every
/// reasonable operator config while keeping the per-compare stack buffer
/// small.
const AUTH_COMPARE_PAD_LEN: usize = 256;

/// Pad `input` into a fixed-length envelope so [`subtle::ConstantTimeEq`]
/// runs its full byte-loop regardless of whether the candidate and the
/// stored hash differ in length. The trailing 8 bytes encode the input's
/// actual length as little-endian `u64` so two inputs that share a prefix
/// but differ in total length cannot collide post-padding.
fn pad_for_constant_time_compare(input: &[u8]) -> [u8; AUTH_COMPARE_PAD_LEN + 8] {
    let mut buf = [0u8; AUTH_COMPARE_PAD_LEN + 8];
    let n = input.len().min(AUTH_COMPARE_PAD_LEN);
    buf[..n].copy_from_slice(&input[..n]);
    buf[AUTH_COMPARE_PAD_LEN..].copy_from_slice(&(input.len() as u64).to_le_bytes());
    buf
}

/// Compare `candidate` against every entry in `authorized_hashes` using
/// constant-time equality. Returns `true` if any entry matches.
///
/// Both sides are padded to a fixed [`AUTH_COMPARE_PAD_LEN`] envelope plus
/// a length suffix before [`subtle::ConstantTimeEq`] runs, so:
///   * the per-entry compare loop iterates the full padded length even
///     when the candidate and the stored hash differ in length (subtle's
///     slice `ct_eq` short-circuits on length mismatch — the padding here
///     defeats that leak);
///   * the outer loop iterates the entire slice on every call regardless
///     of where (or whether) a match is found, so the time spent
///     validating a credential does not vary with the position of the
///     matching entry.
///
/// This defeats timing side-channel attacks that would otherwise leak
/// the size of the realm, the length of the matching username, or the
/// index of the matching credential.
///
/// ── Length-bound prelude ──
///
/// `pad_for_constant_time_compare` silently truncates inputs longer than
/// [`AUTH_COMPARE_PAD_LEN`]. Two credentials that share their first 256
/// bytes — for example, the same long username with different password
/// digests — would produce identical padded buffers (and identical length
/// suffixes if the inputs share a length), letting a bogus password
/// authenticate as the long-username slot. The pre-loop guard below
/// rejects any input that would be truncated before the compare runs, so
/// the constant-time loop only ever sees values we can encode without
/// loss. Stored entries that exceed the bound are skipped by the same
/// rule — operator config should never produce them, but we refuse to
/// silently truncate one if it slips through.
pub fn check_authorized_hashes(candidate: &str, authorized_hashes: &[String]) -> bool {
    if candidate.len() > AUTH_COMPARE_PAD_LEN {
        return false;
    }
    let candidate_padded = pad_for_constant_time_compare(candidate.as_bytes());
    let mut matched = subtle::Choice::from(0u8);
    for hash in authorized_hashes {
        if hash.len() > AUTH_COMPARE_PAD_LEN {
            continue;
        }
        let entry_padded = pad_for_constant_time_compare(hash.as_bytes());
        // Both buffers are exactly `AUTH_COMPARE_PAD_LEN + 8` bytes, so
        // subtle's slice `ct_eq` runs the full byte-loop instead of
        // bailing on length. `BitOr` over `Choice` likewise does not
        // branch.
        matched |= candidate_padded.as_slice().ct_eq(entry_padded.as_slice());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 of the literal byte string `"secret"`, lowercase hex.
    /// Computed once and pinned here so the unit tests don't have to
    /// re-derive it on every run.
    const SECRET_SHA256_HEX: &str =
        "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b";

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
        // 5 KB+ base64 payload — well above the active cap.
        let payload = "a".repeat(max_decoded_credential_bytes() * 2);
        let token = STANDARD.encode(format!("{payload}:pwd"));
        let header = format!("Basic {token}");
        assert!(canonicalize_basic_credentials(header.as_bytes()).is_none());
    }

    /// The post-decode length check used to let `STANDARD.decode` allocate
    /// the full payload before rejection, so an attacker could force a
    /// per-attempt allocation up to the request-buffer cap on every
    /// failed Basic-auth probe. The pre-decode length cap rejects an
    /// oversized encoded payload before any allocation.
    ///
    /// We don't measure the allocator directly here; instead we pass an
    /// encoded payload that is *much* larger than `max_decoded * 4 / 3`
    /// and assert `None`. Combined with the source code's pre-decode
    /// guard, that's enough to pin the contract.
    #[test]
    fn canonicalize_rejects_oversized_encoded_payload_before_decode() {
        // 16× the maximum encoded budget. Without the pre-decode cap,
        // base64::STANDARD::decode would allocate ~12× max_decoded bytes
        // before the post-decode rejection ran.
        let oversize = "A".repeat(max_decoded_credential_bytes() * 16);
        let header = format!("Basic {oversize}");
        assert!(canonicalize_basic_credentials(header.as_bytes()).is_none());
    }

    /// Calling `set_max_decoded_credential_bytes(0)` is a no-op so an
    /// operator config that explicitly zeroes the field cannot disable
    /// the hardening cap by accident — the default 4096 stays in force.
    #[test]
    fn set_max_decoded_credential_bytes_zero_is_noop() {
        let before = max_decoded_credential_bytes();
        set_max_decoded_credential_bytes(0);
        assert_eq!(max_decoded_credential_bytes(), before);
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

    /// `pad_for_constant_time_compare` truncates inputs to
    /// `AUTH_COMPARE_PAD_LEN = 256` bytes. Two canonical credentials whose
    /// first 256 bytes match — e.g. the same long username with different
    /// password digests at offsets > 256 — would otherwise produce
    /// identical padded buffers and authenticate as each other. The fix
    /// rejects any candidate whose canonical length exceeds the envelope
    /// before the compare loop runs.
    #[test]
    fn check_authorized_hashes_rejects_overlong_candidate() {
        // Username large enough that the `:hex64` suffix sits past byte 256.
        // 250 bytes of `a` + ":" + 64 hex = 315 bytes total.
        let long_user = "a".repeat(250);
        let stored = format!("{long_user}:{SECRET_SHA256_HEX}");
        let attacker =
            format!("{long_user}:0000000000000000000000000000000000000000000000000000000000000000");
        assert!(stored.len() > AUTH_COMPARE_PAD_LEN);
        assert!(attacker.len() > AUTH_COMPARE_PAD_LEN);
        assert_eq!(stored.len(), attacker.len()); // same length suffix
        let list = [stored];
        // Without the length guard the attacker credential would compare
        // equal to `stored` because the differing digest is past byte 256
        // and `pad_for_constant_time_compare` would truncate both inputs
        // to the same prefix. The guard rejects both — no auth bypass.
        assert!(!check_authorized_hashes(&attacker, &list));
    }

    /// Stored entries that are themselves overlong are skipped rather than
    /// silently truncated. An operator config containing such an entry
    /// would not authenticate any candidate against it; that is preferred
    /// over admitting a collision.
    #[test]
    fn check_authorized_hashes_skips_overlong_stored_entry() {
        let valid = format!("admin:{SECRET_SHA256_HEX}");
        let overlong = format!("{}:{SECRET_SHA256_HEX}", "a".repeat(250));
        assert!(overlong.len() > AUTH_COMPARE_PAD_LEN);
        // The valid entry still matches; the overlong stored entry is skipped.
        let list = [overlong, valid.clone()];
        assert!(check_authorized_hashes(&valid, &list));
    }

    #[test]
    fn check_authorized_hashes_rejects_when_list_empty() {
        let candidate = format!("admin:{SECRET_SHA256_HEX}");
        assert!(!check_authorized_hashes(&candidate, &[]));
    }
}
