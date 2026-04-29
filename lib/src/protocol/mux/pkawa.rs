//! HPACK decode + pseudo-header validation + RFC 9218 priority parsing.
//!
//! Lifts decoded HPACK blocks into the Kawa request/response shape used by
//! the rest of the stack, enforces pseudo-header ordering, the per-stream
//! trailer budget, and emits the rejection metrics surfaced in
//! `doc/configure.md`. Owns the priority extraction path consumed by the H2
//! `Prioriser`.

use std::{borrow::Cow, io::Write, str::from_utf8};

use kawa::{
    Block, BodySize, Flags, Kind, Pair, ParsingPhase, StatusLine, Store, Version,
    h1::ParserCallbacks, repr::Slice,
};
use sozu_command::logging::ansi_palette;

use crate::{
    pool::Checkout,
    protocol::{
        http::parser::compare_no_case,
        mux::{
            GenericHttpStream, StreamId,
            h2::Prioriser,
            parser::{H2Error, PriorityPart},
        },
    },
};

/// Module-level prefix used on every log line emitted from the pkawa H2
/// converter. Pkawa operates on raw HPACK blocks without direct session
/// context, so a single `MUX-PKAWA` label is used, colored bold bright-white
/// (uniform across every protocol) when the logger supports ANSI.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}MUX-PKAWA{reset}\t >>>", open = open, reset = reset)
    }};
}

/// QW7 helper: dispatch a request pseudo-header (`:method`, `:scheme`,
/// `:path`, `:authority`) through `store_pseudo_header`, recording the
/// per-reason rejection metric and flipping `invalid_headers` on
/// failure. The four call sites in `handle_header` differ only by the
/// destination identifier; the macro keeps the dispatch shape uniform
/// so a future fifth pseudo-header inherits both the reject metric and
/// the invalid-flag plumbing automatically.
macro_rules! store_or_reject {
    ($field:expr, $regular:expr, $kawa:expr, $value:expr, $invalid:expr) => {
        match store_pseudo_header(&$field, $regular, $kawa, &$value) {
            Ok(s) => $field = s,
            Err(reason) => {
                metric_reject(reason);
                *$invalid = true;
            }
        }
    };
}

/// Per-trailer-block byte cap (trailer carve-out). HEADERS and trailers
/// run independent budget counters against
/// `SETTINGS_MAX_HEADER_LIST_SIZE`, so without a tighter trailer-only
/// ceiling the effective per-stream cap doubles. Real-world gRPC trailer
/// blocks observed in production are 1–4 KB; 8 KiB leaves comfortable
/// headroom while denying a peer the chance to spend a second
/// `MAX_HEADER_LIST_SIZE` worth of bytes after the request HEADERS were
/// already accepted. True cumulative-per-stream tracking is a follow-up
/// (it requires adding a `Stream::cumulative_header_bytes` field plumbed
/// through every HEADERS / CONTINUATION / trailer entry point).
pub const MAX_TRAILER_BYTES: usize = 8 * 1024;

/// Returns true if the header name contains any uppercase ASCII letter (A-Z).
/// RFC 9113 section 8.2 requires all header field names to be lowercase in HTTP/2.
#[cfg(test)]
fn has_uppercase_ascii(name: &[u8]) -> bool {
    name.iter().any(|b| b.is_ascii_uppercase())
}

/// Returns true if the byte is a valid HTTP token character (tchar per RFC 9110 §5.6.2).
/// ```text
/// tchar = "!" / "#" / "$" / "%" / "&" / "'" / "*" / "+" / "-" / "." /
///         DIGIT / "^" / "_" / "`" / ALPHA / "|" / "~"
/// ```
fn is_tchar(b: u8) -> bool {
    matches!(
        b,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'0'..=b'9'
            | b'A'..=b'Z'
            | b'^'
            | b'_'
            | b'`'
            | b'a'..=b'z'
            | b'|'
            | b'~'
    )
}

/// Returns true if the header name contains any byte that is not a valid
/// lowercase HTTP token character. In HTTP/2, field names must be lowercase
/// tokens (RFC 9113 §8.2, RFC 9110 §5.6.2). Non-token bytes — CTLs (`\r\n`),
/// space, and separators like `:` — flow verbatim through H1 serialization
/// and enable request smuggling (CWE-93, CWE-444).
fn has_invalid_name_byte(name: &[u8]) -> bool {
    name.iter().any(|&b| b.is_ascii_uppercase() || !is_tchar(b))
}

/// Returns true if the header name is a connection-specific header field
/// that MUST NOT appear in HTTP/2 (RFC 9113 section 8.2.2).
pub(super) fn is_connection_specific_header(name: &[u8]) -> bool {
    match name.first() {
        Some(b'c' | b'C') => compare_no_case(name, b"connection"),
        Some(b'p' | b'P') => compare_no_case(name, b"proxy-connection"),
        Some(b't' | b'T') => compare_no_case(name, b"transfer-encoding"),
        Some(b'u' | b'U') => compare_no_case(name, b"upgrade"),
        Some(b'k' | b'K') => compare_no_case(name, b"keep-alive"),
        _ => false,
    }
}

/// Returns true if the TE header has a value other than "trailers".
/// RFC 9113 section 8.2.2: the only acceptable value for TE in HTTP/2 is "trailers".
fn is_invalid_te_value(value: &[u8]) -> bool {
    !compare_no_case(value, b"trailers")
}

/// Strip the ``:port`` suffix (if any) from an ``authority``/``host`` value.
///
/// This does not validate that the remaining segment is a legal host — it is only
/// used for the RFC 9113 §8.3.1 equivalence check between a literal ``host``
/// header and the ``:authority`` pseudo-header when both are present.
fn strip_port(value: &[u8]) -> &[u8] {
    // Do not strip port from IPv6 literals (e.g., [::1]:8080)
    if value.contains(&b'[') {
        // Bracketed IPv6: look for "]:port"
        return match value.iter().rposition(|&b| b == b']') {
            Some(bracket) => {
                if value.get(bracket + 1) == Some(&b':')
                    && bracket + 2 < value.len()
                    && value[bracket + 2..].iter().all(|b| b.is_ascii_digit())
                {
                    &value[..bracket + 1]
                } else {
                    value
                }
            }
            None => value,
        };
    }
    match value.iter().rposition(|&b| b == b':') {
        Some(i) if i + 1 < value.len() && value[i + 1..].iter().all(|b| b.is_ascii_digit()) => {
            &value[..i]
        }
        _ => value,
    }
}

/// Case-insensitively compare a literal ``host`` header value to an
/// ``:authority`` pseudo-header value, respecting port semantics.
///
/// RFC 9113 §8.3.1: when both are present they must identify the same origin.
/// Per RFC 6454 §5, different explicit ports mean different origins.
/// When only one side carries a port (implicit default), compare hostnames only.
fn host_matches_authority(host: &[u8], authority: &[u8]) -> bool {
    // Fast path: exact case-insensitive match (covers identical port or both absent)
    if compare_no_case(host, authority) {
        return true;
    }
    // If both have explicit ports but differ, the origins differ.
    let host_stripped = strip_port(host);
    let auth_stripped = strip_port(authority);
    let host_has_port = host_stripped.len() != host.len();
    let auth_has_port = auth_stripped.len() != authority.len();
    if host_has_port && auth_has_port {
        // Both have explicit ports but the full values differ → different origins.
        return false;
    }
    // One side has a port, the other doesn't (implicit default) → compare hostnames.
    compare_no_case(host_stripped, auth_stripped)
}

/// Like `has_invalid_value_byte` but also rejects HTAB (0x09), which is
/// allowed in regular header values (RFC 9110 §5.5) but not in pseudo-header
/// values that end up in the H1 request-line (RFC 9112 §3).
fn has_invalid_pseudo_value_byte(value: &[u8]) -> bool {
    value.iter().any(|&b| matches!(b, 0x00..=0x1F | 0x7F))
}

/// Returns true if the value contains any byte forbidden in HTTP field values
/// (RFC 9110 §5.5 + RFC 9113 §8.2.1): NUL, CR, LF, DEL, and other C0 controls.
/// HTAB (0x09) and visible ASCII (0x20..=0x7E) plus obs-text (0x80..=0xFF) are allowed.
///
/// Without this check, HPACK-decoded bytes containing `\r\n` flow verbatim through
/// kawa's H1 serializer and reach backends as injected headers — request smuggling
/// (CWE-93, CWE-444) on H2→H1 traffic.
#[cfg(test)]
fn has_invalid_value_byte(value: &[u8]) -> bool {
    value
        .iter()
        .any(|&b| matches!(b, 0x00..=0x08 | 0x0A..=0x1F | 0x7F))
}

/// Bounded enumeration of reasons a single HPACK header may be rejected by the
/// H2 → H1 converter. Each variant maps 1:1 to a metric label suffix exposed
/// as `h2.headers.rejected.<reason>` so SOC dashboards can break down rejection
/// pressure by root cause without unbounded cardinality.
///
/// New variants must update the corresponding row in `doc/configure.md`
/// (HTTP/2 header validation subsection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RejectReason {
    /// Header name contains an invalid byte (uppercase, CTL, separator, space).
    /// CWE-93 — turns into request smuggling once the H1 serializer emits the
    /// raw bytes on the wire.
    InvalidNameByte,
    /// Connection-specific header that MUST NOT appear in HTTP/2
    /// (RFC 9113 §8.2.2): `connection`, `proxy-connection`, `transfer-encoding`,
    /// `upgrade`, `keep-alive`.
    ConnectionSpecificHeader,
    /// `te` header with any value other than `trailers` (RFC 9113 §8.2.2).
    TeNotTrailers,
    /// CR or LF byte in a field value — request-smuggling vector
    /// (CWE-444). NUL is reported separately via `NulInValue`.
    CrlfInValue,
    /// NUL byte (0x00) in a field value. Distinct from CR/LF for triage:
    /// many smuggling toolchains pad with NULs first.
    NulInValue,
    /// Pseudo-header value contains a CTL or DEL byte that would corrupt the
    /// H1 request line (RFC 9112 §3 — pseudo values are stricter than regular
    /// field values: HTAB is also forbidden).
    OversizedPseudoValue,
    /// `:method` value contains a non-token byte (RFC 9110 §9 method = token).
    /// Distinct from name-byte rejection because the byte is in the *value*,
    /// not the name.
    InvalidMethod,
    /// `:scheme` value is not exactly `http` or `https` (RFC 9113 §8.3.1).
    InvalidScheme,
    /// `:path` value contains a `#` (fragment identifier — RFC 9112 §3.2 forbids
    /// fragments in request-targets).
    InvalidPath,
    /// `:status` value is not exactly three ASCII digits (RFC 9113 §8.3.2).
    InvalidStatus,
    /// Two `content-length` headers with disagreeing values, or a
    /// `transfer-encoding`/`content-length` conflict surfaced via the same
    /// validation hook.
    ClTeConflict,
    /// `content-length` value is missing, empty, or contains non-digit bytes —
    /// already disagrees with itself before any second header arrives.
    DuplicateCl,
    /// A pseudo-header arrived twice (`:method` after `:method`, etc. —
    /// RFC 9113 §8.3 requires uniqueness).
    DuplicatePseudo,
    /// A pseudo-header arrived AFTER a regular header (RFC 9113 §8.3 requires
    /// pseudos to precede all regular fields).
    PseudoAfterRegular,
    /// A `:`-prefixed name that is not one of the known request/response
    /// pseudo-headers (RFC 9113 §8.3 — no extension pseudos accepted here).
    UnknownPseudo,
    /// A pseudo-header arrived with a zero-length value (RFC 9113 §8.3.1
    /// forbids empty `:path`/`:method`/`:authority`/`:scheme`/`:status`).
    EmptyPseudo,
}

/// Compile-time mapping from `(prefix, RejectReason)` to a static metric key.
///
/// Same `concat!`-based pattern as `h2_error_metric_key!` in `mux/h2.rs`:
/// the literal is materialised at compile time so the statsd drain can store
/// it as `&'static str` without a per-rejection allocation. Adding a new
/// `RejectReason` variant fails the build inside this match — the metric
/// breakdown stays in lock-step with the bounded enum (and with
/// `doc/configure.md`'s HTTP/2 header validation table).
macro_rules! reject_metric_key {
    ($prefix:literal, $reason:expr) => {
        match $reason {
            RejectReason::InvalidNameByte => concat!($prefix, ".invalid_name_byte"),
            RejectReason::ConnectionSpecificHeader => {
                concat!($prefix, ".connection_specific_header")
            }
            RejectReason::TeNotTrailers => concat!($prefix, ".te_not_trailers"),
            RejectReason::CrlfInValue => concat!($prefix, ".crlf_in_value"),
            RejectReason::NulInValue => concat!($prefix, ".nul_in_value"),
            RejectReason::OversizedPseudoValue => concat!($prefix, ".oversized_pseudo_value"),
            RejectReason::InvalidMethod => concat!($prefix, ".invalid_method"),
            RejectReason::InvalidScheme => concat!($prefix, ".invalid_scheme"),
            RejectReason::InvalidPath => concat!($prefix, ".invalid_path"),
            RejectReason::InvalidStatus => concat!($prefix, ".invalid_status"),
            RejectReason::ClTeConflict => concat!($prefix, ".cl_te_conflict"),
            RejectReason::DuplicateCl => concat!($prefix, ".duplicate_cl"),
            RejectReason::DuplicatePseudo => concat!($prefix, ".duplicate_pseudo"),
            RejectReason::PseudoAfterRegular => concat!($prefix, ".pseudo_after_regular"),
            RejectReason::UnknownPseudo => concat!($prefix, ".unknown_pseudo"),
            RejectReason::EmptyPseudo => concat!($prefix, ".empty_pseudo"),
        }
    };
}

/// Increment the per-reason counter `h2.headers.rejected.<reason>` and the
/// per-rejection-event counter `h2.headers.rejected.total`. The total is the
/// coarse roll-up dashboards alert on; the per-reason counter is the breakdown
/// SOC uses to triage. Emitting both per call (rather than total once per
/// request) keeps the totals in lockstep with the breakdown — easier to
/// reconcile when a single request piles up several rejections.
fn metric_reject(reason: RejectReason) {
    incr!("h2.headers.rejected.total");
    incr!(reject_metric_key!("h2.headers.rejected", reason));
}

/// Classify a header rejected by `is_invalid_h2_header`. Splits the original
/// boolean check into the bounded `RejectReason` enum so the caller can emit
/// the exact per-reason counter. Order matches `is_invalid_h2_header` so the
/// observable validation policy is unchanged.
fn classify_invalid_h2_header(name: &[u8], value: &[u8]) -> Option<RejectReason> {
    if name.is_empty() {
        // An empty name has no further byte to point at — bucket under
        // `invalid_name_byte`; the original check treated it as a name issue.
        return Some(RejectReason::InvalidNameByte);
    }
    if name[0] != b':' && has_invalid_name_byte(name) {
        return Some(RejectReason::InvalidNameByte);
    }
    if is_connection_specific_header(name) {
        return Some(RejectReason::ConnectionSpecificHeader);
    }
    if compare_no_case(name, b"te") && is_invalid_te_value(value) {
        return Some(RejectReason::TeNotTrailers);
    }
    if let Some(reason) = classify_invalid_value_byte(value) {
        return Some(reason);
    }
    None
}

/// Sub-classify which forbidden value byte caused a rejection. Distinguishes
/// NUL from CR/LF/other-CTL because the two have different threat profiles
/// (NUL → C string truncation, CR/LF → smuggling).
fn classify_invalid_value_byte(value: &[u8]) -> Option<RejectReason> {
    let mut saw_crlf = false;
    let mut saw_nul = false;
    for &b in value {
        match b {
            0x00 => saw_nul = true,
            0x0A | 0x0D => saw_crlf = true,
            0x01..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F | 0x7F => {
                return Some(RejectReason::CrlfInValue);
            }
            _ => {}
        }
    }
    if saw_crlf {
        Some(RejectReason::CrlfInValue)
    } else if saw_nul {
        Some(RejectReason::NulInValue)
    } else {
        None
    }
}

/// Thin wrapper kept for tests and any code path that just needs the boolean.
/// New code should call `classify_invalid_h2_header` to surface the exact
/// rejection reason for the per-reason counter.
#[cfg(test)]
fn is_invalid_h2_header(name: &[u8], value: &[u8]) -> bool {
    classify_invalid_h2_header(name, value).is_some()
}

/// Validate and set Content-Length, returning false if it conflicts with a prior value
/// (RFC 9110 §8.6: multiple disagreeing Content-Length values are malformed).
fn set_content_length(body_size: &mut BodySize, length: usize) -> bool {
    match *body_size {
        BodySize::Length(existing) if existing != length => false,
        _ => {
            *body_size = BodySize::Length(length);
            true
        }
    }
}

/// Store a pseudo-header value into kawa storage.
///
/// Returns `Ok(Store::Slice)` on success, or `Err(RejectReason)` describing
/// which validity rule the pseudo violated. Callers should set
/// `invalid_headers = true` and increment the per-reason counter on `Err`.
///
/// Reasons:
/// - `DuplicatePseudo` — `dest` is already populated (RFC 9113 §8.3 uniqueness)
/// - `PseudoAfterRegular` — a regular header arrived first (RFC 9113 §8.3 ordering)
/// - `EmptyPseudo` — zero-length value (RFC 9113 §8.3.1 forbids empty pseudos)
/// - `OversizedPseudoValue` — CTL/DEL byte that would corrupt the H1 request
///   line. Empty values are rejected before storing because
///   `Store::Slice { len: 0 }` is NOT `Store::is_empty()`, so the presence
///   checks after the decode loop would silently accept a zero-length pseudo.
fn store_pseudo_header(
    dest: &Store,
    regular_headers: bool,
    kawa: &mut GenericHttpStream,
    value: &[u8],
) -> Result<Store, RejectReason> {
    if !dest.is_empty() {
        return Err(RejectReason::DuplicatePseudo);
    }
    if regular_headers {
        return Err(RejectReason::PseudoAfterRegular);
    }
    if value.is_empty() {
        return Err(RejectReason::EmptyPseudo);
    }
    if has_invalid_pseudo_value_byte(value) {
        return Err(RejectReason::OversizedPseudoValue);
    }
    let start = kawa.storage.end as u32;
    if kawa.storage.write_all(value).is_err() {
        // Storage exhaustion: report as `oversized_pseudo_value` since a
        // pseudo whose bytes don't fit is functionally an oversized payload
        // for the configured buffer pool. Keeps the counter cardinality
        // bounded without inventing a transport-failure reason.
        return Err(RejectReason::OversizedPseudoValue);
    }
    Ok(Store::Slice(Slice {
        start,
        len: value.len() as u32,
    }))
}

/// Write a regular (non-pseudo) header into kawa storage and push a `Block::Header`.
///
/// Also handles `content-length` validation: if the header is `content-length`, its
/// value is parsed and checked for consistency with any previously seen value.
///
/// Returns `Ok(())` on success, or `Err(RejectReason)` describing which validity
/// rule failed (caller should set `invalid_headers = true` and emit the
/// matching counter):
///
/// - `DuplicateCl` — `content-length` empty / non-digit / parse failure
/// - `ClTeConflict` — second `content-length` disagrees with the first one
/// - `OversizedPseudoValue` — kawa storage write failed (buffer exhausted)
fn write_regular_header(
    kawa: &mut GenericHttpStream,
    key: &[u8],
    value: &[u8],
) -> Result<(), RejectReason> {
    let len_key = key.len() as u32;
    let len_val = value.len() as u32;
    let start = kawa.storage.end as u32;
    if kawa.storage.write_all(value).is_err() {
        return Err(RejectReason::OversizedPseudoValue);
    }
    let val = Store::Slice(Slice {
        start,
        len: len_val,
    });
    if compare_no_case(key, b"content-length") {
        // RFC 9110 §8.6: Content-Length is 1*DIGIT. `usize::from_str` would
        // accept leading whitespace, a leading `+`, or trailing garbage — all
        // of which create a parser-divergence vector against backends. Require
        // every byte to be an ASCII digit before parsing.
        if value.is_empty() || !value.iter().all(|b| b.is_ascii_digit()) {
            return Err(RejectReason::DuplicateCl);
        }
        if let Some(length) = from_utf8(value).ok().and_then(|v| v.parse::<usize>().ok()) {
            if !set_content_length(&mut kawa.body_size, length) {
                return Err(RejectReason::ClTeConflict);
            }
        } else {
            return Err(RejectReason::DuplicateCl);
        }
    }
    if kawa.storage.write_all(key).is_err() {
        return Err(RejectReason::OversizedPseudoValue);
    }
    let key = Store::Slice(Slice {
        start: start + len_val,
        len: len_key,
    });
    kawa.push_block(Block::Header(Pair { key, val }));
    Ok(())
}

/// Trims leading and trailing OWS (SP / HTAB per RFC 9110 §5.6.3) from a byte slice.
fn trim_ows(input: &[u8]) -> &[u8] {
    let start = input
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .unwrap_or(input.len());
    let end = input
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t')
        .map_or(start, |p| p + 1);
    &input[start..end]
}

/// Parse an RFC 9218 `priority` header value into (urgency, incremental).
///
/// The structured field format is: `u=N, i` or `u=N` or just `i`.
/// - `u=N`: urgency, integer 0-7 (default 3 per RFC 9218 section 4)
/// - `i`: incremental flag (default false)
///
/// Values outside the valid range are clamped (urgency to 0-7).
/// Malformed tokens are silently ignored, falling back to defaults.
///
/// Re-used by the RFC 9218 §7.1 PRIORITY_UPDATE frame handler
/// (`h2::ConnectionH2::handle_priority_update_frame`) to decode the
/// priority field value payload — same wire format as the request header.
pub(super) fn parse_rfc9218_priority(value: &[u8]) -> (u8, bool) {
    let mut urgency: u8 = 3; // RFC 9218 §4: default urgency
    let mut incremental = false;

    for token in value.split(|&b| b == b',') {
        let token = trim_ows(token);
        if token.is_empty() {
            continue;
        }
        if token.len() >= 3 && token[0] == b'u' && token[1] == b'=' {
            // Parse `u=N` where N may be a multi-digit value (clamped to 0-7)
            if let Ok(s) = std::str::from_utf8(&token[2..]) {
                if let Ok(n) = s.parse::<u8>() {
                    urgency = n.min(7);
                }
            }
        } else if token == b"i" || token == b"i=?1" {
            incremental = true;
        } else if token == b"i=?0" {
            incremental = false;
        }
    }

    (urgency, incremental)
}

/// Run the HPACK decoder while tracking the cumulative decoded header-list size
/// against `max_decoded_bytes` (RFC 9113 §6.5.2 SETTINGS_MAX_HEADER_LIST_SIZE).
///
/// `per_header` is invoked for every successfully decoded `(name, value)` pair
/// that fits within the budget; it may set its `&mut bool` argument to mark the
/// stream as carrying invalid headers, in which case subsequent pairs are still
/// decoded (to keep HPACK dynamic-table state in sync) but skipped.
///
/// Returns the final value of the `invalid_headers` flag so the caller can
/// combine it with kind-specific validity checks (e.g. mandatory pseudo-headers).
fn decode_headers_with_budget<F>(
    decoder: &mut loona_hpack::Decoder<'static>,
    input: &[u8],
    max_decoded_bytes: usize,
    mut per_header: F,
) -> Result<bool, (H2Error, bool)>
where
    F: FnMut(Cow<[u8]>, Cow<[u8]>, &mut bool),
{
    let mut invalid_headers = false;
    let mut budget_exceeded = false;
    let mut decoded_bytes: usize = 0;
    let decode_status = decoder.decode_with_cb(input, |k, v| {
        if invalid_headers || budget_exceeded {
            return;
        }
        decoded_bytes = decoded_bytes.saturating_add(k.len() + v.len());
        if decoded_bytes > max_decoded_bytes {
            budget_exceeded = true;
            return;
        }
        if let Some(reason) = classify_invalid_h2_header(&k, &v) {
            metric_reject(reason);
            invalid_headers = true;
            return;
        }
        per_header(k, v, &mut invalid_headers);
    });
    if let Err(error) = decode_status {
        error!("{} INVALID FRAGMENT: {:?}", log_module_context!(), error);
        return Err((H2Error::CompressionError, true));
    }
    if budget_exceeded {
        error!(
            "{} HPACK decoded header size {} exceeds MAX_HEADER_LIST_SIZE {}",
            log_module_context!(),
            decoded_bytes,
            max_decoded_bytes
        );
        return Err((H2Error::EnhanceYourCalm, false));
    }
    Ok(invalid_headers)
}

#[allow(clippy::too_many_arguments)]
pub fn handle_header<C>(
    decoder: &mut loona_hpack::Decoder<'static>,
    prioriser: &mut Prioriser,
    stream_id: StreamId,
    kawa: &mut GenericHttpStream,
    input: &[u8],
    end_stream: bool,
    callbacks: &mut C,
    max_header_list_size: u32,
    elide_x_real_ip: bool,
) -> Result<(), (H2Error, bool)>
where
    C: ParserCallbacks<Checkout>,
{
    if !kawa.is_initial() {
        return handle_trailer(
            kawa,
            input,
            end_stream,
            decoder,
            max_header_list_size,
            elide_x_real_ip,
        );
    }
    kawa.push_block(Block::StatusLine);
    let max_decoded_bytes = max_header_list_size as usize;
    kawa.detached.status_line = match kawa.kind {
        Kind::Request => {
            let mut method = Store::Empty;
            let mut authority = Store::Empty;
            let mut path = Store::Empty;
            let mut scheme = Store::Empty;
            let mut regular_headers = false;
            let mut cookies_added = false;
            // RFC 9113 §8.3.1: a literal ``host`` header may appear, but if it
            // does it MUST identify the same origin as ``:authority``. We defer
            // the comparison to after the decode loop (authority may arrive in
            // any order) and drop the literal header so the H1 serializer emits
            // exactly one ``Host:`` line from the authority pseudo-header.
            let mut host_value: Option<Vec<u8>> = None;
            let mut host_conflict = false;
            let invalid_headers = decode_headers_with_budget(
                decoder,
                input,
                max_decoded_bytes,
                |k, v, invalid_headers| {
                    if compare_no_case(&k, b":method") {
                        // RFC 9110 §9: method = token. Validate every byte is
                        // a tchar — spaces, delimiters, and CTLs in the method
                        // reach the H1 request line and can smuggle extra path
                        // segments or headers to backends.
                        if !v.iter().all(|&b| is_tchar(b)) {
                            metric_reject(RejectReason::InvalidMethod);
                            *invalid_headers = true;
                            return;
                        }
                        store_or_reject!(method, regular_headers, kawa, v, invalid_headers);
                    } else if compare_no_case(&k, b":scheme") {
                        // RFC 9113 §8.3.1: the HPACK lowercase rule means a
                        // valid :scheme arrives exactly as "http" or "https".
                        // Anything else is a smuggling / SSRF vector.
                        if v.as_ref() != b"http" && v.as_ref() != b"https" {
                            metric_reject(RejectReason::InvalidScheme);
                            *invalid_headers = true;
                            return;
                        }
                        store_or_reject!(scheme, regular_headers, kawa, v, invalid_headers);
                    } else if compare_no_case(&k, b":path") {
                        // RFC 9112 §3.2: fragment identifiers (`#`) are
                        // prohibited in request-targets.
                        if v.contains(&b'#') {
                            metric_reject(RejectReason::InvalidPath);
                            *invalid_headers = true;
                            return;
                        }
                        store_or_reject!(path, regular_headers, kawa, v, invalid_headers);
                    } else if compare_no_case(&k, b":authority") {
                        store_or_reject!(authority, regular_headers, kawa, v, invalid_headers);
                    } else if k.starts_with(b":") {
                        metric_reject(RejectReason::UnknownPseudo);
                        *invalid_headers = true;
                    } else if compare_no_case(&k, b"cookie") {
                        regular_headers = true;
                        // RFC 9113 §8.2.3: When converting H2 to H1, multiple cookie
                        // header fields MUST be concatenated into a single octet string
                        // using "; " as separator. We store each cookie-pair in the
                        // detached jar (same as the H1 parser) so that:
                        //  1. H1BlockConverter emits a single "Cookie: k1=v1; k2=v2" header
                        //  2. H2BlockConverter re-encodes each as a separate HPACK header
                        //  3. HttpContext::on_request_headers can find sticky session cookies
                        for cookie_pair in v.split(|&b| b == b';') {
                            let trimmed = trim_ows(cookie_pair);
                            if trimmed.is_empty() {
                                continue;
                            }
                            // Split on first '=' to separate cookie name and value
                            let (cookie_key, cookie_val) =
                                match trimmed.iter().position(|&b| b == b'=') {
                                    Some(eq_pos) => (&trimmed[..eq_pos], &trimmed[eq_pos + 1..]),
                                    None => (trimmed, &b""[..]),
                                };
                            // RFC 6265 §4.2.1 + RFC 9110 §5.5: cookie-name and
                            // cookie-value must not contain CTLs. Without this
                            // check, a crafted cookie can smuggle headers
                            // through kawa's H1 serializer.
                            if let Some(reason) = classify_invalid_value_byte(cookie_key)
                                .or_else(|| classify_invalid_value_byte(cookie_val))
                            {
                                metric_reject(reason);
                                *invalid_headers = true;
                                return;
                            }
                            let key_start = kawa.storage.end as u32;
                            if kawa.storage.write_all(cookie_key).is_err() {
                                metric_reject(RejectReason::OversizedPseudoValue);
                                *invalid_headers = true;
                                return;
                            }
                            let key_len = cookie_key.len() as u32;
                            let val_start = kawa.storage.end as u32;
                            if kawa.storage.write_all(cookie_val).is_err() {
                                metric_reject(RejectReason::OversizedPseudoValue);
                                *invalid_headers = true;
                                return;
                            }
                            let val_len = cookie_val.len() as u32;
                            if !cookies_added {
                                kawa.push_block(Block::Cookies);
                                cookies_added = true;
                            }
                            kawa.detached.jar.push_back(Pair {
                                key: Store::Slice(Slice {
                                    start: key_start,
                                    len: key_len,
                                }),
                                val: Store::Slice(Slice {
                                    start: val_start,
                                    len: val_len,
                                }),
                            });
                        }
                    } else if compare_no_case(&k, b"host") {
                        // RFC 9113 §8.3.1: a literal ``host`` header is tolerated
                        // but only if it matches ``:authority``. We buffer the
                        // value here and reconcile after the decode loop — at
                        // which point authority is known regardless of order.
                        regular_headers = true;
                        if let Some(reason) = classify_invalid_value_byte(&v) {
                            metric_reject(reason);
                            *invalid_headers = true;
                            return;
                        }
                        match host_value {
                            Some(ref existing) if existing.as_slice() != v.as_ref() => {
                                // Multiple disagreeing `host` headers are
                                // malformed (RFC 9110 §7.2 / §5.3.1) — reject.
                                host_conflict = true;
                            }
                            Some(_) => {
                                // Duplicate identical `host:` headers are
                                // also malformed per RFC 9110 §7.2 ("a
                                // server MUST respond with a 400
                                // (Bad Request) status code to any HTTP/1.1
                                // request message that lacks a Host header
                                // field and to any request message that
                                // contains more than one Host header field
                                // value or has any Host header field value
                                // that is not a valid URI authority"). The
                                // identical-value short-circuit was lenient
                                // because two equal `host` values produce
                                // the same wire shape after normalisation,
                                // but the spec is strict on COUNT not on
                                // VALUE. Reject so request smuggling that
                                // relies on duplicate headers cannot hide
                                // behind value equality.
                                host_conflict = true;
                            }
                            None => host_value = Some(v.to_vec()),
                        }
                    } else {
                        regular_headers = true;
                        if let Err(reason) = write_regular_header(kawa, &k, &v) {
                            metric_reject(reason);
                            *invalid_headers = true;
                            return;
                        }
                        if compare_no_case(&k, b"priority") {
                            let (urgency, incremental) = parse_rfc9218_priority(&v);
                            prioriser.push_priority(
                                stream_id,
                                PriorityPart::Rfc9218 {
                                    urgency,
                                    incremental,
                                },
                            );
                        }
                    }
                },
            )?;
            // Post-decode :path form validation (deferred because :method may
            // arrive after :path in HPACK — RFC 9113 does not mandate ordering).
            // RFC 9112 §3.2 / RFC 9113 §8.3.1: for http/https URIs we accept
            // origin-form (starts with `/`) and — only when the method is
            // OPTIONS — asterisk-form (`*`).
            if let Some(path_data) = path.data_opt(kawa.storage.buffer()) {
                let is_asterisk = path_data == b"*";
                let starts_with_slash = path_data.first() == Some(&b'/');
                let method_is_options = method
                    .data_opt(kawa.storage.buffer())
                    .is_some_and(|m| m == b"OPTIONS");
                if !(starts_with_slash || (is_asterisk && method_is_options)) {
                    return Err((H2Error::ProtocolError, false));
                }
            }
            // RFC 9113 §8.3.1 requires all four pseudo-headers to be present and non-empty.
            // Note: Store::is_empty() only matches Store::Empty — a pseudo-header stored
            // with an empty value yields Store::Slice { len: 0 } which is_empty() misses.
            // HPACK never produces zero-length pseudo-header values for valid requests, so
            // is_empty() is sufficient here; store_pseudo_header already rejects duplicates.
            // Note: CONNECT requests (RFC 9113 §8.5) only need :method + :authority,
            // but we don't advertise SETTINGS_ENABLE_CONNECT_PROTOCOL so CONNECT is
            // intentionally unsupported for now.
            if invalid_headers
                || method.is_empty()
                || authority.is_empty()
                || path.is_empty()
                || scheme.is_empty()
            {
                error!("{} INVALID HEADERS", log_module_context!());
                return Err((H2Error::ProtocolError, false));
            }
            // RFC 9113 §8.3.1: if a literal ``host`` header appears, it must
            // match ``:authority`` (modulo optional port) and be deduplicated
            // so the H1 serializer emits exactly one ``Host:`` line. Mismatches
            // are request-smuggling vectors and are rejected as PROTOCOL_ERROR.
            if host_conflict {
                error!(
                    "{} H2 host header: multiple disagreeing values",
                    log_module_context!()
                );
                return Err((H2Error::ProtocolError, false));
            }
            if let Some(ref host) = host_value {
                let authority_bytes = authority.data_opt(kawa.storage.buffer()).unwrap_or(&[]);
                if !host_matches_authority(host, authority_bytes) {
                    error!(
                        "{} H2 host header does not match :authority",
                        log_module_context!()
                    );
                    return Err((H2Error::ProtocolError, false));
                }
                // Match — drop it. kawa's H1 serializer emits Host: from
                // :authority on its own.
            }
            StatusLine::Request {
                version: Version::V20,
                method,
                uri: path.clone(),
                authority,
                path,
            }
        }
        Kind::Response => {
            let mut code = 0;
            let mut status = Store::Empty;
            let mut regular_headers = false;
            let invalid_headers = decode_headers_with_budget(
                decoder,
                input,
                max_decoded_bytes,
                |k, v, invalid_headers| {
                    if compare_no_case(&k, b":status") {
                        // RFC 9113 §8.3.2 / RFC 9110 §15: :status is exactly
                        // three ASCII digits. `u16::from_str` would accept
                        // `+200`, ` 200`, `00200` etc., all of which diverge
                        // from the backend's H1 parser.
                        if v.len() != 3 || !v.iter().all(|b| b.is_ascii_digit()) {
                            metric_reject(RejectReason::InvalidStatus);
                            *invalid_headers = true;
                            return;
                        }
                        match store_pseudo_header(&status, regular_headers, kawa, &v) {
                            Ok(s) => {
                                status = s;
                                code = (v[0] - b'0') as u16 * 100
                                    + (v[1] - b'0') as u16 * 10
                                    + (v[2] - b'0') as u16;
                            }
                            Err(reason) => {
                                metric_reject(reason);
                                *invalid_headers = true;
                            }
                        }
                    } else if k.starts_with(b":") {
                        metric_reject(RejectReason::UnknownPseudo);
                        *invalid_headers = true;
                    } else {
                        regular_headers = true;
                        if let Err(reason) = write_regular_header(kawa, &k, &v) {
                            metric_reject(reason);
                            *invalid_headers = true;
                        }
                    }
                },
            )?;
            if invalid_headers || status.is_empty() {
                error!("{} INVALID HEADERS", log_module_context!());
                return Err((H2Error::ProtocolError, false));
            }
            StatusLine::Response {
                version: Version::V20,
                code,
                status,
                reason: Store::Static(b"FromH2"),
            }
        }
    };

    // everything has been parsed
    kawa.storage.head = kawa.storage.end;
    debug!(
        "{} index: {}/{}/{}",
        log_module_context!(),
        kawa.storage.start,
        kawa.storage.head,
        kawa.storage.end
    );

    callbacks.on_headers(kawa);

    if end_stream {
        // RFC 9113 §8.1.1: when END_STREAM is set on HEADERS, no DATA frames
        // follow, so the payload length is 0. A non-zero Content-Length is a
        // stream error (PROTOCOL_ERROR). Body-exempt responses (1xx, 204, 304)
        // are excluded — they may carry Content-Length per RFC 9110 §8.6.
        if let BodySize::Length(n) = kawa.body_size {
            let body_exempt = matches!(kawa.kind, Kind::Response)
                && matches!(
                    kawa.detached.status_line,
                    StatusLine::Response { code, .. } if (100..200).contains(&code) || code == 204 || code == 304
                );
            if n > 0 && !body_exempt {
                error!(
                    "{} END_STREAM with non-zero Content-Length: {} (RFC 9113 §8.1.1)",
                    log_module_context!(),
                    n
                );
                return Err((H2Error::ProtocolError, false));
            }
        }
        if let BodySize::Empty = kawa.body_size {
            // RFC 9110 §8.6: Do not inject Content-Length: 0 for responses where
            // message body is forbidden (1xx, 204, 304). Only inject for requests
            // and other response codes.
            let skip_content_length = matches!(kawa.kind, Kind::Response)
                && matches!(
                    kawa.detached.status_line,
                    StatusLine::Response { code, .. } if (100..200).contains(&code) || code == 204 || code == 304
                );
            if !skip_content_length {
                kawa.body_size = BodySize::Length(0);
                kawa.push_block(Block::Header(Pair {
                    key: Store::Static(b"Content-Length"),
                    val: Store::Static(b"0"),
                }));
            }
        }
    }

    // If body will follow (!end_stream) and no Content-Length was declared,
    // inject Transfer-Encoding: chunked so the H1 backend can frame the body.
    // The H2 converter filters this header (connection-specific per RFC 9113 §8.2.2).
    //
    // Note on H2→H1 trailers (RFC 9110 §6.5): when the H2 peer declares a
    // Content-Length (`body_size == BodySize::Length(_)`) and later sends a
    // trailer HEADERS frame, the H1 backend cannot receive those trailers —
    // HTTP/1.1 only carries trailers with chunked transfer-coding. In that
    // case the trailers are silently dropped by `handle_trailer`'s caller
    // (no Transfer-Encoding can be retro-fitted once the request line and
    // headers have already gone out on the wire). Peers that require
    // trailer delivery should omit Content-Length; the branch below then
    // upgrades the framing to chunked and H2BlockConverter on the back side
    // passes the trailer block through intact.
    if !end_stream && kawa.body_size == BodySize::Empty {
        kawa.body_size = BodySize::Chunked;
        kawa.push_block(Block::Header(Pair {
            key: Store::Static(b"Transfer-Encoding"),
            val: Store::Static(b"chunked"),
        }));
    }

    kawa.push_block(Block::Flags(Flags {
        end_body: end_stream,
        end_chunk: false,
        end_header: true,
        end_stream,
    }));

    if kawa.parsing_phase == ParsingPhase::Terminated {
        return Ok(());
    }

    kawa.parsing_phase = match kawa.body_size {
        BodySize::Chunked => ParsingPhase::Chunks { first: true },
        BodySize::Length(0) => ParsingPhase::Terminated,
        BodySize::Length(_) => ParsingPhase::Body,
        BodySize::Empty => ParsingPhase::Chunks { first: true },
    };
    Ok(())
}

/// Decode an H2 trailer HEADERS frame and append the validated trailer
/// pairs to `kawa.blocks`.
///
/// The shared `HttpContext::on_request_headers` callback that handles
/// elision on **initial** HEADERS frames is **not** invoked here, so any
/// listener-scoped header rewrites must be plumbed in explicitly.
///
/// ── Spoof-vector elision per RFC 9110 §6.5 ──
///
/// RFC 9110 §6.5 forbids trailers from carrying fields that affect
/// "message framing, message routing, or response semantics." sōzu
/// rewrites four client-attribution headers on the initial HEADERS pass
/// (`HttpContext::on_request_headers` in
/// `lib/src/protocol/kawa_h1/editor.rs`):
///   * `X-Real-IP` is replaced by the post-PROXY-v2 peer IP when
///     `send_x_real_ip = true` and stripped client-side when
///     `elide_x_real_ip = true`.
///   * `X-Forwarded-For` has the peer IP appended.
///   * `Forwarded` (RFC 7239) has a `proto=…;for=…;by=…` clause appended.
///   * `X-Request-Id` is preserved verbatim (de-facto correlation
///     header used by Envoy/HAProxy/most LBs).
///
/// A naive H2 client could otherwise smuggle any of those by sending
/// them as a trailer block: an H2 backend that merges trailers into
/// its header view (gRPC-style, or anything that uses
/// `headers::extend(trailers.iter())`) would observe an attacker-
/// controlled value. The four elisions below run **unconditionally** —
/// the spec already prohibits them, the operator gains no observability
/// or routing signal from a trailer-side copy, and forwarding them is
/// strictly a spoof vector.
///
/// `elide_x_real_ip` is kept as an explicit parameter for symmetry
/// with the initial-HEADERS path's listener flag, but the trailer-side
/// elision of `x-real-ip` no longer depends on it — the listener flag
/// only governs whether sōzu *injects* an `X-Real-IP` header on the
/// forward, never whether it filters one off the wire.
pub fn handle_trailer(
    kawa: &mut GenericHttpStream,
    input: &[u8],
    end_stream: bool,
    decoder: &mut loona_hpack::Decoder<'static>,
    max_header_list_size: u32,
    elide_x_real_ip: bool,
) -> Result<(), (H2Error, bool)> {
    // Acknowledge the parameter; dropping it would force a callsite
    // change. The elision below covers x-real-ip unconditionally so
    // the listener flag is no longer a gate for trailer-side defence.
    let _ = elide_x_real_ip;
    if !end_stream {
        return Err((H2Error::ProtocolError, false));
    }
    let mut invalid_trailers = false;
    let mut budget_exceeded = false;
    let mut decoded_bytes: usize = 0;
    // HEADERS and trailers each tracked an independent `decoded_bytes`
    // counter against `max_header_list_size`, so the
    // effective per-stream budget was `2 × MAX_HEADER_LIST_SIZE`
    // (~128 KiB at the default). True per-stream cumulative tracking
    // would require plumbing a shared counter through the `Stream`
    // struct; in the interim, cap trailer-only budget at `MAX_TRAILER_BYTES`
    // — well above legitimate gRPC trailer sizes (1-4 KB observed in the
    // wild, RFC 9110 §6.5 imposes no minimum) and an order of magnitude
    // tighter than the HEADERS cap.
    let max_decoded = (max_header_list_size as usize).min(MAX_TRAILER_BYTES);
    let decode_status = decoder.decode_with_cb(input, |k, v| {
        if invalid_trailers || budget_exceeded {
            return;
        }
        decoded_bytes = decoded_bytes.saturating_add(k.len() + v.len());
        if decoded_bytes > max_decoded {
            budget_exceeded = true;
            return;
        }
        // RFC 9113 §8.1: Trailers MUST NOT contain pseudo-header fields.
        if k.starts_with(b":") {
            metric_reject(RejectReason::UnknownPseudo);
            invalid_trailers = true;
            return;
        }
        // Reuse the same validation policy as the main header path: reject
        // invalid name bytes (CTLs, separators, uppercase), connection-specific
        // fields (RFC 9113 §8.2.2), invalid TE values, and forbidden value
        // bytes (RFC 9110 §5.5). Without this, crafted trailer names can
        // smuggle H1 lines through kawa's serializer.
        if let Some(reason) = classify_invalid_h2_header(&k, &v) {
            metric_reject(reason);
            invalid_trailers = true;
            return;
        }
        // ── Spoof-vector elision per RFC 9110 §6.5 ──
        //
        // RFC 9110 §6.5 forbids trailers from carrying message-routing
        // semantics. The four client-attribution headers below are
        // rewritten by sōzu on the initial-HEADERS pass; admitting them
        // as trailers would let a naive H2 client smuggle a spoofed
        // value to a backend that merges trailers into its header view.
        // Drop them unconditionally — keys are already lower-case here
        // (`classify_invalid_h2_header` rejects any uppercase byte per
        // RFC 9113 §8.2.2), so a byte-equality check is sufficient.
        // `incr!` records the rejection so dashboards observe the
        // attempted smuggle without spamming logs.
        if matches!(
            k.as_ref(),
            b"x-real-ip" | b"x-forwarded-for" | b"forwarded" | b"x-request-id"
        ) {
            incr!("h2.trailer.spoof_vector_elided");
            return;
        }
        let start = kawa.storage.end as u32;
        if kawa.storage.write_all(&k).is_err() || kawa.storage.write_all(&v).is_err() {
            metric_reject(RejectReason::OversizedPseudoValue);
            invalid_trailers = true;
            return;
        }
        let len_key = k.len() as u32;
        let len_val = v.len() as u32;
        let key = Store::Slice(Slice {
            start,
            len: len_key,
        });
        let val = Store::Slice(Slice {
            start: start + len_key,
            len: len_val,
        });
        kawa.push_block(Block::Header(Pair { key, val }));
    });

    if let Err(error) = decode_status {
        error!("{} INVALID FRAGMENT: {:?}", log_module_context!(), error);
        return Err((H2Error::CompressionError, true));
    }
    if budget_exceeded {
        error!(
            "{} HPACK decoded trailer size {} exceeds MAX_HEADER_LIST_SIZE {}",
            log_module_context!(),
            decoded_bytes,
            max_decoded
        );
        return Err((H2Error::EnhanceYourCalm, false));
    }
    if invalid_trailers {
        error!("{} INVALID TRAILERS", log_module_context!());
        return Err((H2Error::ProtocolError, false));
    }

    // Codex G7 / RFC 9110 §6.5: if the request/response was framed with a
    // fixed Content-Length (not chunked), the H1-side serializer cannot emit
    // trailers — HTTP/1.1 only carries trailers with chunked transfer-coding.
    // The trailer block stays in kawa but the downstream writer will never
    // reach it. Emit a visible signal so operators chasing "vanished gRPC
    // trailers" have something to grep, without changing the drop behaviour
    // (changing it would retroactively need to rewrite the framing, which
    // is not possible once headers are on the wire).
    if matches!(kawa.body_size, BodySize::Length(_)) {
        warn!(
            "{} H2 trailers arrived on a Content-Length-framed stream; \
             trailers will be silently dropped by the H1 serializer \
             (RFC 9110 §6.5). Peer should omit Content-Length for trailer \
             delivery to upgrade framing to chunked.",
            log_module_context!()
        );
        incr!("h2.trailers_dropped_content_length");
    }

    kawa.push_block(Block::Flags(Flags {
        end_body: false,
        end_chunk: false,
        end_header: true,
        end_stream: true,
    }));
    kawa.parsing_phase = ParsingPhase::Terminated;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_rfc9218_priority ────────────────────────────────────────────

    #[test]
    fn test_parse_rfc9218_priority_defaults() {
        // Empty value -> RFC 9218 §4 defaults: urgency 3, incremental false
        let (u, i) = parse_rfc9218_priority(b"");
        assert_eq!(u, 3);
        assert!(!i);
    }

    #[test]
    fn test_parse_rfc9218_priority_urgency_only() {
        assert_eq!(parse_rfc9218_priority(b"u=0"), (0, false));
        assert_eq!(parse_rfc9218_priority(b"u=3"), (3, false));
        assert_eq!(parse_rfc9218_priority(b"u=7"), (7, false));
    }

    #[test]
    fn test_parse_rfc9218_priority_urgency_clamped() {
        // Values > 7 are clamped to 7
        assert_eq!(parse_rfc9218_priority(b"u=9"), (7, false));
        assert_eq!(parse_rfc9218_priority(b"u=255"), (7, false));
    }

    #[test]
    fn test_parse_rfc9218_priority_incremental_only() {
        // Just "i" -> default urgency 3, incremental true
        assert_eq!(parse_rfc9218_priority(b"i"), (3, true));
    }

    #[test]
    fn test_parse_rfc9218_priority_incremental_boolean_form() {
        assert_eq!(parse_rfc9218_priority(b"i=?1"), (3, true));
        assert_eq!(parse_rfc9218_priority(b"i=?0"), (3, false));
    }

    #[test]
    fn test_parse_rfc9218_priority_combined() {
        assert_eq!(parse_rfc9218_priority(b"u=3, i"), (3, true));
        assert_eq!(parse_rfc9218_priority(b"u=0, i"), (0, true));
        assert_eq!(parse_rfc9218_priority(b"u=7, i=?1"), (7, true));
        assert_eq!(parse_rfc9218_priority(b"u=5, i=?0"), (5, false));
    }

    #[test]
    fn test_parse_rfc9218_priority_whitespace_tolerance() {
        // OWS around tokens should be trimmed
        assert_eq!(parse_rfc9218_priority(b"u=3,  i"), (3, true));
        assert_eq!(parse_rfc9218_priority(b" u=3 , i "), (3, true));
        assert_eq!(parse_rfc9218_priority(b"\tu=2\t,\ti\t"), (2, true));
    }

    #[test]
    fn test_parse_rfc9218_priority_malformed_ignored() {
        // Unknown tokens are silently ignored
        assert_eq!(parse_rfc9218_priority(b"x=5"), (3, false));
        assert_eq!(parse_rfc9218_priority(b"u=3, x=5"), (3, false));
        // "u=" with non-numeric value: parse fails, urgency stays default
        assert_eq!(parse_rfc9218_priority(b"u=abc"), (3, false));
    }

    #[test]
    fn test_parse_rfc9218_priority_order_independent() {
        // "i" before "u=N" should work
        assert_eq!(parse_rfc9218_priority(b"i, u=1"), (1, true));
    }

    // ── is_connection_specific_header ─────────────────────────────────────

    #[test]
    fn test_is_connection_specific_header_positive() {
        assert!(is_connection_specific_header(b"connection"));
        assert!(is_connection_specific_header(b"Connection"));
        assert!(is_connection_specific_header(b"proxy-connection"));
        assert!(is_connection_specific_header(b"Proxy-Connection"));
        assert!(is_connection_specific_header(b"transfer-encoding"));
        assert!(is_connection_specific_header(b"Transfer-Encoding"));
        assert!(is_connection_specific_header(b"upgrade"));
        assert!(is_connection_specific_header(b"Upgrade"));
        assert!(is_connection_specific_header(b"keep-alive"));
        assert!(is_connection_specific_header(b"Keep-Alive"));
    }

    #[test]
    fn test_is_connection_specific_header_negative() {
        assert!(!is_connection_specific_header(b"content-type"));
        assert!(!is_connection_specific_header(b"accept"));
        assert!(!is_connection_specific_header(b"host"));
        assert!(!is_connection_specific_header(b"te"));
        assert!(!is_connection_specific_header(b"cookie"));
        assert!(!is_connection_specific_header(b""));
    }

    // ── is_invalid_h2_header ─────────────────────────────────────────────

    #[test]
    fn test_is_invalid_h2_header_uppercase_name() {
        assert!(is_invalid_h2_header(b"Content-Type", b"text/html"));
        assert!(is_invalid_h2_header(b"X-Custom", b"value"));
    }

    #[test]
    fn test_is_invalid_h2_header_connection_specific() {
        assert!(is_invalid_h2_header(b"connection", b"close"));
        assert!(is_invalid_h2_header(b"transfer-encoding", b"chunked"));
        assert!(is_invalid_h2_header(b"upgrade", b"h2c"));
        assert!(is_invalid_h2_header(b"keep-alive", b"timeout=5"));
        assert!(is_invalid_h2_header(b"proxy-connection", b"keep-alive"));
    }

    #[test]
    fn test_is_invalid_h2_header_te_trailers_ok() {
        // TE: trailers is the only valid TE value in HTTP/2
        assert!(!is_invalid_h2_header(b"te", b"trailers"));
        assert!(!is_invalid_h2_header(b"te", b"Trailers")); // case insensitive
    }

    #[test]
    fn test_is_invalid_h2_header_te_other_invalid() {
        assert!(is_invalid_h2_header(b"te", b"gzip"));
        assert!(is_invalid_h2_header(b"te", b"deflate"));
        assert!(is_invalid_h2_header(b"te", b"chunked"));
    }

    #[test]
    fn test_is_invalid_h2_header_valid() {
        assert!(!is_invalid_h2_header(b"content-type", b"text/html"));
        assert!(!is_invalid_h2_header(b"accept", b"*/*"));
        assert!(!is_invalid_h2_header(b"x-custom", b"value"));
    }

    // ── has_uppercase_ascii ──────────────────────────────────────────────

    #[test]
    fn test_has_uppercase_ascii() {
        assert!(has_uppercase_ascii(b"Content-Type"));
        assert!(has_uppercase_ascii(b"X"));
        assert!(!has_uppercase_ascii(b"content-type"));
        assert!(!has_uppercase_ascii(b""));
        assert!(!has_uppercase_ascii(b"123-header"));
    }

    // ── trim_ows ─────────────────────────────────────────────────────────

    #[test]
    fn test_trim_ows() {
        assert_eq!(trim_ows(b"  hello  "), b"hello");
        assert_eq!(trim_ows(b"\thello\t"), b"hello");
        assert_eq!(trim_ows(b" \t hello \t "), b"hello");
        assert_eq!(trim_ows(b"hello"), b"hello");
        assert_eq!(trim_ows(b""), b"" as &[u8]);
        assert_eq!(trim_ows(b"   "), b"" as &[u8]);
    }

    // ── store_pseudo_header ──────────────────────────────────────────────

    /// Create a `GenericHttpStream` backed by a pool `Checkout` for testing.
    fn make_generic_kawa(pool: &mut crate::pool::Pool, kind: Kind) -> GenericHttpStream {
        let checkout = pool.checkout().expect("pool checkout should succeed");
        kawa::Kawa::new(kind, kawa::Buffer::new(checkout))
    }

    #[test]
    fn test_store_pseudo_header_rejects_duplicate() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = make_generic_kawa(&mut pool, Kind::Request);

        // First store succeeds
        let dest = Store::Empty;
        let result = store_pseudo_header(&dest, false, &mut kawa, b"GET");
        assert!(result.is_ok());

        // Second store with non-empty dest fails (duplicate pseudo-header)
        let dest = result.unwrap();
        let result = store_pseudo_header(&dest, false, &mut kawa, b"POST");
        assert!(result.is_err());
    }

    #[test]
    fn test_store_pseudo_header_rejects_after_regular_headers() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = make_generic_kawa(&mut pool, Kind::Request);

        let dest = Store::Empty;
        // regular_headers = true means regular headers have already appeared
        let result = store_pseudo_header(&dest, true, &mut kawa, b"GET");
        assert!(result.is_err());
    }

    // ── is_connection_specific_header (additional case sensitivity) ────

    #[test]
    fn test_is_connection_specific_header_mixed_case() {
        // Verify case-insensitive matching for various mixed-case forms
        assert!(is_connection_specific_header(b"CONNECTION"));
        assert!(is_connection_specific_header(b"CoNnEcTiOn"));
        assert!(is_connection_specific_header(b"PROXY-CONNECTION"));
        assert!(is_connection_specific_header(b"Proxy-connection"));
        assert!(is_connection_specific_header(b"TRANSFER-ENCODING"));
        assert!(is_connection_specific_header(b"transfer-Encoding"));
        assert!(is_connection_specific_header(b"UPGRADE"));
        assert!(is_connection_specific_header(b"KEEP-ALIVE"));
        assert!(is_connection_specific_header(b"Keep-alive"));
    }

    #[test]
    fn test_is_connection_specific_header_partial_match() {
        // Substrings or superstrings must not match
        assert!(!is_connection_specific_header(b"connection-extra"));
        assert!(!is_connection_specific_header(b"my-connection"));
        assert!(!is_connection_specific_header(b"upgrade-insecure-requests"));
        assert!(!is_connection_specific_header(b"keep-alive-timeout"));
        assert!(!is_connection_specific_header(b"x-keep-alive"));
    }

    // ── is_invalid_te_value ─────────────────────────────────────────────

    #[test]
    fn test_is_invalid_te_value_trailers_ok() {
        assert!(!is_invalid_te_value(b"trailers"));
        assert!(!is_invalid_te_value(b"Trailers"));
        assert!(!is_invalid_te_value(b"TRAILERS"));
    }

    #[test]
    fn test_is_invalid_te_value_other_rejected() {
        assert!(is_invalid_te_value(b"gzip"));
        assert!(is_invalid_te_value(b"deflate"));
        assert!(is_invalid_te_value(b"chunked"));
        assert!(is_invalid_te_value(b"compress"));
        assert!(is_invalid_te_value(b""));
        assert!(is_invalid_te_value(b"trailers, gzip"));
    }

    // ── is_invalid_h2_header (additional edge cases) ──────────────────

    #[test]
    fn test_is_invalid_h2_header_empty_name() {
        // Empty header name is invalid per RFC 9113 §8.2.1 (field names MUST be non-empty)
        assert!(is_invalid_h2_header(b"", b""));
    }

    #[test]
    fn test_is_invalid_h2_header_single_uppercase() {
        // Even a single uppercase letter makes it invalid
        assert!(is_invalid_h2_header(b"X", b""));
        assert!(is_invalid_h2_header(b"hostA", b"value"));
    }

    // ── has_uppercase_ascii (additional) ───────────────────────────────

    #[test]
    fn test_has_uppercase_ascii_non_ascii_bytes() {
        // Non-ASCII bytes (128+) are not uppercase ASCII
        assert!(!has_uppercase_ascii(&[0x80, 0xFF, 0xC0]));
        assert!(!has_uppercase_ascii(b"\xc3\xa9")); // UTF-8 for 'e' with accent
    }

    #[test]
    fn test_has_uppercase_ascii_mixed_with_numbers() {
        assert!(!has_uppercase_ascii(b"content-type-2"));
        assert!(has_uppercase_ascii(b"content-Type-2"));
    }

    // ── trim_ows (additional) ─────────────────────────────────────────

    #[test]
    fn test_trim_ows_single_char() {
        assert_eq!(trim_ows(b"x"), b"x");
        assert_eq!(trim_ows(b" "), b"" as &[u8]);
        assert_eq!(trim_ows(b"\t"), b"" as &[u8]);
    }

    #[test]
    fn test_trim_ows_preserves_internal_whitespace() {
        assert_eq!(trim_ows(b"  hello world  "), b"hello world");
        assert_eq!(trim_ows(b"\ta\tb\t"), b"a\tb");
    }

    // ── handle_header: cookie jar population ─────────────────────────────

    /// Helper: HPACK-encode a set of headers and call `handle_header`, returning the kawa stream.
    fn decode_request_headers(
        pool: &mut crate::pool::Pool,
        headers: &[(&[u8], &[u8])],
        end_stream: bool,
    ) -> GenericHttpStream {
        let mut encoder = loona_hpack::Encoder::new();
        let mut encoded = Vec::new();
        for &(name, value) in headers {
            encoder
                .encode_header_into((name, value), &mut encoded)
                .unwrap();
        }

        let mut decoder = loona_hpack::Decoder::new();
        let mut prioriser = Prioriser::default();
        let mut kawa = make_generic_kawa(pool, Kind::Request);

        struct NoOpCallbacks;
        impl kawa::h1::ParserCallbacks<crate::pool::Checkout> for NoOpCallbacks {
            fn on_headers(&mut self, _kawa: &mut GenericHttpStream) {}
        }
        let mut callbacks = NoOpCallbacks;

        let result = handle_header(
            &mut decoder,
            &mut prioriser,
            1,
            &mut kawa,
            &encoded,
            end_stream,
            &mut callbacks,
            crate::protocol::mux::h2::MAX_HEADER_LIST_SIZE as u32,
            false,
        );
        assert!(result.is_ok(), "handle_header failed: {:?}", result.err());
        kawa
    }

    #[test]
    fn test_h2_cookies_stored_in_jar() {
        // RFC 9113 §8.2.3: H2 cookies should be stored in detached.jar
        // so H1 converter concatenates them into a single Cookie header.
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let kawa = decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"cookie", b"a=1; b=2; c=3"),
            ],
            true,
        );

        // All three cookies should be in the jar
        assert_eq!(kawa.detached.jar.len(), 3);
        let buf = kawa.storage.buffer();
        assert_eq!(kawa.detached.jar[0].key.data(buf), b"a");
        assert_eq!(kawa.detached.jar[0].val.data(buf), b"1");
        assert_eq!(kawa.detached.jar[1].key.data(buf), b"b");
        assert_eq!(kawa.detached.jar[1].val.data(buf), b"2");
        assert_eq!(kawa.detached.jar[2].key.data(buf), b"c");
        assert_eq!(kawa.detached.jar[2].val.data(buf), b"3");

        // Should have exactly one Block::Cookies in the blocks
        let cookie_blocks: Vec<_> = kawa
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Cookies))
            .collect();
        assert_eq!(cookie_blocks.len(), 1);
    }

    #[test]
    fn test_h2_multiple_cookie_headers_merged_in_jar() {
        // Multiple separate cookie HPACK headers should all go to the same jar
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let kawa = decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"cookie", b"a=1"),
                (b"cookie", b"b=2"),
                (b"cookie", b"c=3"),
            ],
            true,
        );

        assert_eq!(kawa.detached.jar.len(), 3);
        let buf = kawa.storage.buffer();
        assert_eq!(kawa.detached.jar[0].key.data(buf), b"a");
        assert_eq!(kawa.detached.jar[0].val.data(buf), b"1");
        assert_eq!(kawa.detached.jar[1].key.data(buf), b"b");
        assert_eq!(kawa.detached.jar[1].val.data(buf), b"2");
        assert_eq!(kawa.detached.jar[2].key.data(buf), b"c");
        assert_eq!(kawa.detached.jar[2].val.data(buf), b"3");

        // Still only one Block::Cookies marker
        let cookie_blocks: Vec<_> = kawa
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Cookies))
            .collect();
        assert_eq!(cookie_blocks.len(), 1);
    }

    #[test]
    fn test_h2_cookie_empty_value() {
        // Cookie with empty value after '=' (e.g., intercom-session=)
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let kawa = decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"cookie", b"session=; PHPSESSID=abc123"),
            ],
            true,
        );

        assert_eq!(kawa.detached.jar.len(), 2);
        let buf = kawa.storage.buffer();
        assert_eq!(kawa.detached.jar[0].key.data(buf), b"session");
        assert_eq!(kawa.detached.jar[0].val.data(buf), b"");
        assert_eq!(kawa.detached.jar[1].key.data(buf), b"PHPSESSID");
        assert_eq!(kawa.detached.jar[1].val.data(buf), b"abc123");
    }

    #[test]
    fn test_h2_cookie_without_equals() {
        // Cookie without '=' (bare name, no value)
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let kawa = decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"cookie", b"bare_name"),
            ],
            true,
        );

        assert_eq!(kawa.detached.jar.len(), 1);
        let buf = kawa.storage.buffer();
        assert_eq!(kawa.detached.jar[0].key.data(buf), b"bare_name");
        assert_eq!(kawa.detached.jar[0].val.data(buf), b"");
    }

    // ── has_invalid_value_byte / CTL rejection ───────────────────────────

    #[test]
    fn test_has_invalid_value_byte_ctl_and_del() {
        // RFC 9110 §5.5 / RFC 9113 §8.2.1: CR, LF, NUL, other C0 CTLs, DEL
        // are forbidden in field values.
        assert!(has_invalid_value_byte(b"a\rb"));
        assert!(has_invalid_value_byte(b"a\nb"));
        assert!(has_invalid_value_byte(b"a\r\nb"));
        assert!(has_invalid_value_byte(b"a\x00b"));
        assert!(has_invalid_value_byte(b"a\x01b"));
        assert!(has_invalid_value_byte(b"a\x7fb"));
        assert!(has_invalid_value_byte(b"a\x1fb"));
    }

    #[test]
    fn test_has_invalid_value_byte_permits_tab_sp_vchar() {
        // HTAB and visible ASCII (SP..=~) are permitted.
        assert!(!has_invalid_value_byte(b"a\tb"));
        assert!(!has_invalid_value_byte(b"a b"));
        assert!(!has_invalid_value_byte(b"normal-value"));
        // obs-text (0x80..=0xFF) is permitted by the value ABNF
        assert!(!has_invalid_value_byte(&[0x80, 0xFF]));
        // Empty slice is allowed by this helper (empty-check is done elsewhere)
        assert!(!has_invalid_value_byte(b""));
    }

    #[test]
    fn test_is_invalid_h2_header_rejects_ctl_in_value() {
        assert!(is_invalid_h2_header(b"x-evil", b"a\r\nb"));
        assert!(is_invalid_h2_header(b"x-evil", b"a\nb"));
        assert!(is_invalid_h2_header(b"x-evil", b"a\rb"));
        assert!(is_invalid_h2_header(b"x-evil", b"a\x00b"));
        assert!(is_invalid_h2_header(b"x-evil", b"a\x7fb"));
        assert!(is_invalid_h2_header(b"x-evil", b"a\x01b"));
        assert!(!is_invalid_h2_header(b"x-evil", b"a\tb"));
        assert!(!is_invalid_h2_header(b"x-evil", b"a b"));
    }

    // ── store_pseudo_header: empty & CTL rejection ────────────────────────

    #[test]
    fn test_store_pseudo_header_rejects_empty_value() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = make_generic_kawa(&mut pool, Kind::Request);
        // RFC 9113 §8.3.1: pseudo-header values MUST NOT be empty.
        let result = store_pseudo_header(&Store::Empty, false, &mut kawa, b"");
        assert!(result.is_err());
    }

    #[test]
    fn test_store_pseudo_header_rejects_ctl_in_value() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = make_generic_kawa(&mut pool, Kind::Request);
        let result = store_pseudo_header(&Store::Empty, false, &mut kawa, b"/foo\r\n");
        assert!(result.is_err());
        let result = store_pseudo_header(&Store::Empty, false, &mut kawa, b"a\x00b");
        assert!(result.is_err());
    }

    // ── handle_header: request pseudo-header & CTL/smuggling rejection ────

    /// HPACK-encode a header list and call `handle_header`, returning the
    /// resulting `Result` so callers can assert on rejections.
    fn try_decode_request_headers(
        pool: &mut crate::pool::Pool,
        headers: &[(&[u8], &[u8])],
        end_stream: bool,
    ) -> Result<(), (H2Error, bool)> {
        let mut encoder = loona_hpack::Encoder::new();
        let mut encoded = Vec::new();
        for &(name, value) in headers {
            encoder
                .encode_header_into((name, value), &mut encoded)
                .unwrap();
        }
        let mut decoder = loona_hpack::Decoder::new();
        let mut prioriser = Prioriser::default();
        let mut kawa = make_generic_kawa(pool, Kind::Request);
        struct NoOpCallbacks;
        impl kawa::h1::ParserCallbacks<crate::pool::Checkout> for NoOpCallbacks {
            fn on_headers(&mut self, _kawa: &mut GenericHttpStream) {}
        }
        let mut callbacks = NoOpCallbacks;
        handle_header(
            &mut decoder,
            &mut prioriser,
            1,
            &mut kawa,
            &encoded,
            end_stream,
            &mut callbacks,
            crate::protocol::mux::h2::MAX_HEADER_LIST_SIZE as u32,
            false,
        )
    }

    fn try_decode_response_headers(
        pool: &mut crate::pool::Pool,
        headers: &[(&[u8], &[u8])],
        end_stream: bool,
    ) -> Result<(), (H2Error, bool)> {
        let mut encoder = loona_hpack::Encoder::new();
        let mut encoded = Vec::new();
        for &(name, value) in headers {
            encoder
                .encode_header_into((name, value), &mut encoded)
                .unwrap();
        }
        let mut decoder = loona_hpack::Decoder::new();
        let mut prioriser = Prioriser::default();
        let mut kawa = make_generic_kawa(pool, Kind::Response);
        struct NoOpCallbacks;
        impl kawa::h1::ParserCallbacks<crate::pool::Checkout> for NoOpCallbacks {
            fn on_headers(&mut self, _kawa: &mut GenericHttpStream) {}
        }
        let mut callbacks = NoOpCallbacks;
        handle_header(
            &mut decoder,
            &mut prioriser,
            1,
            &mut kawa,
            &encoded,
            end_stream,
            &mut callbacks,
            crate::protocol::mux::h2::MAX_HEADER_LIST_SIZE as u32,
            false,
        )
    }

    #[test]
    fn test_handle_header_rejects_crlf_in_regular_header_value() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"x-evil", b"normal\r\nX-Smuggled: yes"),
            ],
            true,
        );
        assert!(err.is_err(), "CRLF-laden header value must be rejected");
    }

    #[test]
    fn test_handle_header_rejects_empty_path() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b""),
                (b":authority", b"example.com"),
            ],
            true,
        );
        assert!(err.is_err(), "empty :path must be rejected");
    }

    #[test]
    fn test_handle_header_rejects_non_origin_form_path() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        // absolute-form path (no leading `/`) must be rejected for non-OPTIONS
        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"http://attacker/"),
                (b":authority", b"example.com"),
            ],
            true,
        );
        assert!(err.is_err());

        // bare `*` is only valid for OPTIONS
        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"*"),
                (b":authority", b"example.com"),
            ],
            true,
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_handle_header_accepts_options_asterisk() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let ok = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"OPTIONS"),
                (b":scheme", b"https"),
                (b":path", b"*"),
                (b":authority", b"example.com"),
            ],
            true,
        );
        assert!(ok.is_ok(), "OPTIONS * must be accepted, got {ok:?}");
    }

    #[test]
    fn test_handle_header_rejects_bad_scheme() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"javascript"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
            ],
            true,
        );
        assert!(err.is_err());

        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"file"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
            ],
            true,
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_handle_header_rejects_crlf_in_path_and_authority() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/foo\r\nHost: attacker"),
                (b":authority", b"example.com"),
            ],
            true,
        );
        assert!(err.is_err(), "CRLF in :path must be rejected");

        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com\r\nX: y"),
            ],
            true,
        );
        assert!(err.is_err(), "CRLF in :authority must be rejected");
    }

    #[test]
    fn test_handle_header_rejects_bad_status() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        for bad in [&b"20"[..], b"abc", b"+200", b" 200", b"00200", b"2000"] {
            let err = try_decode_response_headers(
                &mut pool,
                &[(b":status", bad), (b"content-type", b"text/html")],
                true,
            );
            assert!(err.is_err(), ":status {bad:?} must be rejected");
        }
    }

    #[test]
    fn test_handle_header_rejects_empty_status() {
        // Codex G5 concern: response pseudo-header emptiness could slip through
        // if the three-digit guard were removed. Locks in `v.len() != 3` as
        // the rejection gate for an empty :status (HPACK allows zero-length
        // field values, so this must be enforced explicitly). A server
        // emitting :status = "" would otherwise produce a malformed H1
        // response ("HTTP/1.1  FromH2") on the backend side.
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let err = try_decode_response_headers(
            &mut pool,
            &[(b":status", b""), (b"content-type", b"text/html")],
            true,
        );
        assert!(err.is_err(), "empty :status must be rejected");
    }

    #[test]
    fn test_handle_header_rejects_missing_status() {
        // RFC 9113 §8.3.2: response HEADERS MUST contain :status. If no
        // :status pseudo-header is present at all, the post-loop
        // `status.is_empty()` check (line 851 of the production path) is the
        // backstop that converts the absence into a stream-level
        // PROTOCOL_ERROR. Without this test, silently-downgraded-to-H1
        // responses with no status line could slip through.
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let err = try_decode_response_headers(&mut pool, &[(b"content-type", b"text/html")], true);
        assert!(err.is_err(), "response without :status must be rejected");
    }

    #[test]
    fn test_handle_header_rejects_ctl_in_cookie_value() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"cookie", b"a=1\r\nX-Smuggled: yes"),
            ],
            true,
        );
        assert!(err.is_err(), "CRLF in cookie value must be rejected");
    }

    #[test]
    fn test_handle_header_rejects_ctl_in_cookie_key() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"cookie", b"a\rb=1"),
            ],
            true,
        );
        assert!(err.is_err(), "CR in cookie key must be rejected");
    }

    // ── write_regular_header: Content-Length digit-only parse ─────────────

    #[test]
    fn test_write_regular_header_content_length_digit_only() {
        // RFC 9113 §8.1.1: Content-Length value MUST be a non-empty sequence
        // of ASCII digits. write_regular_header must reject every non-digit
        // variant before calling usize::from_str so no parser-divergence path
        // reaches the backend.
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);

        let reject_cases: &[(&[u8], &str)] = &[
            (b"", "empty"),
            (b" 42", "leading whitespace"),
            (b"42 ", "trailing whitespace"),
            (b"+42", "unary plus"),
            (b"-42", "negative sign"),
            (b"4\xFF2", "non-ASCII byte"),
            (b"0x10", "non-digit hex chars"),
        ];
        for (val, label) in reject_cases {
            let mut kawa = make_generic_kawa(&mut pool, Kind::Request);
            let result = write_regular_header(&mut kawa, b"content-length", val);
            assert!(
                matches!(result, Err(RejectReason::DuplicateCl)),
                "content-length {label:?} ({val:?}) must yield DuplicateCl, got {result:?}",
            );
        }

        // Valid: a plain digit string must be accepted and set BodySize::Length.
        let mut kawa = make_generic_kawa(&mut pool, Kind::Request);
        let result = write_regular_header(&mut kawa, b"content-length", b"42");
        assert!(
            result.is_ok(),
            "valid content-length '42' was rejected: {result:?}"
        );
        assert_eq!(
            kawa.body_size,
            kawa::BodySize::Length(42),
            "body_size must be Length(42) after parsing '42'",
        );
    }

    #[test]
    fn test_handle_header_rejects_bad_content_length() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        // RFC 9110 §8.6 / RFC 9113 §8.1.1: Content-Length is 1*DIGIT.
        // Whitespace, sign, trailing garbage, and non-ASCII must be rejected
        // to avoid parser-divergence against backends.
        for bad in [
            &b" 42"[..], // leading whitespace
            b"+42",      // unary plus
            b"42 ",      // trailing whitespace
            b"-1",       // negative sign
            b"",         // empty
            b"0x10",     // non-digit chars
            b"4\xFF2",   // non-ASCII byte (RFC 9113 §8.1.1: must be digits only)
        ] {
            let err = try_decode_request_headers(
                &mut pool,
                &[
                    (b":method", b"POST"),
                    (b":scheme", b"https"),
                    (b":path", b"/"),
                    (b":authority", b"example.com"),
                    (b"content-length", bad),
                ],
                true,
            );
            assert!(err.is_err(), "content-length {bad:?} must be rejected");
        }

        // Sanity: a clean digit string is accepted.
        let ok = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"POST"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"content-length", b"42"),
            ],
            false,
        );
        assert!(ok.is_ok(), "valid content-length must be accepted: {ok:?}");
    }

    // ── handle_trailer: CTL rejection ─────────────────────────────────────

    #[test]
    fn test_handle_trailer_rejects_ctl_in_value() {
        use kawa::Buffer;
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let checkout = pool.checkout().expect("checkout");
        let mut kawa: GenericHttpStream = kawa::Kawa::new(Kind::Request, Buffer::new(checkout));
        // Fake: move out of the initial phase so handle_header routes to
        // handle_trailer. Easiest: push a StatusLine and move the head.
        kawa.push_block(Block::StatusLine);
        kawa.detached.status_line = StatusLine::Request {
            version: Version::V20,
            method: Store::Static(b"GET"),
            uri: Store::Static(b"/"),
            authority: Store::Static(b"example.com"),
            path: Store::Static(b"/"),
        };
        // HPACK-encode a trailer with LF in the value.
        let mut encoder = loona_hpack::Encoder::new();
        let mut encoded = Vec::new();
        encoder
            .encode_header_into((&b"x-trailer"[..], &b"val\nsmuggled"[..]), &mut encoded)
            .unwrap();
        let mut decoder = loona_hpack::Decoder::new();
        let err = handle_trailer(
            &mut kawa,
            &encoded,
            true,
            &mut decoder,
            crate::protocol::mux::h2::MAX_HEADER_LIST_SIZE as u32,
            false,
        );
        assert!(err.is_err(), "LF in trailer value must be rejected");
    }

    #[test]
    fn test_h2_cookie_value_with_equals() {
        // Cookie value containing '=' (e.g., base64-encoded token)
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let kawa = decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"cookie", b"token=abc=def=="),
            ],
            true,
        );

        assert_eq!(kawa.detached.jar.len(), 1);
        let buf = kawa.storage.buffer();
        assert_eq!(kawa.detached.jar[0].key.data(buf), b"token");
        // Split on FIRST '=' only, so value includes trailing '='
        assert_eq!(kawa.detached.jar[0].val.data(buf), b"abc=def==");
    }

    // ── host vs :authority reconciliation (RFC 9113 §8.3.1) ───────────

    #[test]
    fn test_strip_port_and_host_authority_match() {
        assert_eq!(strip_port(b"example.com"), b"example.com");
        assert_eq!(strip_port(b"example.com:8443"), b"example.com");
        assert_eq!(strip_port(b"example.com:abc"), b"example.com:abc");
        // IPv6 bracketed literals
        assert_eq!(strip_port(b"[::1]:8443"), b"[::1]");
        assert_eq!(strip_port(b"[::1]"), b"[::1]");
        assert_eq!(strip_port(b"[::1]:"), b"[::1]:");
        // Trailing colon with empty port must NOT strip (not a valid port)
        assert_eq!(strip_port(b"example.com:"), b"example.com:");

        // Both without port
        assert!(host_matches_authority(b"example.com", b"example.com"));
        // Case insensitive, same port
        assert!(host_matches_authority(b"Example.COM:80", b"example.com:80"));
        // Case insensitive, no port
        assert!(host_matches_authority(b"Example.com", b"example.com"));
        // Same explicit port
        assert!(host_matches_authority(b"example.com:80", b"example.com:80"));
        // One has port, one implicit
        assert!(host_matches_authority(b"example.com:80", b"example.com"));
        assert!(host_matches_authority(b"example.com", b"example.com:443"));
        // Different explicit ports → different origins (RFC 6454 §5)
        assert!(!host_matches_authority(
            b"example.com:80",
            b"example.com:443"
        ));
        assert!(!host_matches_authority(b"evil.com", b"example.com"));
        assert!(!host_matches_authority(b"example.com.evil", b"example.com"));
    }

    #[test]
    fn test_handle_header_host_matches_authority_is_deduplicated() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let kawa = decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"host", b"Example.com"),
            ],
            true,
        );
        // Literal ``host`` must not be emitted as a regular Header block —
        // kawa's H1 converter serialises Host: from :authority on its own.
        let buf = kawa.storage.buffer();
        for block in kawa.blocks.iter() {
            if let Block::Header(pair) = block {
                assert!(
                    !compare_no_case(pair.key.data(buf), b"host"),
                    "literal host header must be dropped when it matches :authority"
                );
            }
        }
    }

    #[test]
    fn test_handle_header_host_mismatch_rejected() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"host", b"evil.com"),
            ],
            true,
        );
        assert!(
            matches!(err, Err((H2Error::ProtocolError, _))),
            "host != :authority must yield PROTOCOL_ERROR, got {err:?}"
        );
    }

    #[test]
    fn test_handle_header_multiple_disagreeing_host_rejected() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"host", b"example.com"),
                (b"host", b"evil.com"),
            ],
            true,
        );
        assert!(
            matches!(err, Err((H2Error::ProtocolError, _))),
            "disagreeing host headers must yield PROTOCOL_ERROR, got {err:?}"
        );
    }

    #[test]
    fn test_handle_header_host_with_port_matches_authority() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let kawa = decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"host", b"example.com:8443"),
            ],
            true,
        );
        let buf = kawa.storage.buffer();
        for block in kawa.blocks.iter() {
            if let Block::Header(pair) = block {
                assert!(
                    !compare_no_case(pair.key.data(buf), b"host"),
                    "literal host header must be dropped (port-stripped match)"
                );
            }
        }
    }

    #[test]
    fn test_handle_header_host_crlf_rejected() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let err = try_decode_request_headers(
            &mut pool,
            &[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":path", b"/"),
                (b":authority", b"example.com"),
                (b"host", b"example.com\r\nX-Smuggled: yes"),
            ],
            true,
        );
        assert!(err.is_err(), "CRLF in host header must be rejected");
    }

    // ── H2→H1 trailer / Transfer-Encoding invariants ──────────────────────

    /// H2 request without END_STREAM and without Content-Length MUST gain
    /// `Transfer-Encoding: chunked` so the H1 backend can frame the body
    /// and any subsequent trailer HEADERS frame.
    #[test]
    fn test_h2_to_h1_body_without_content_length_forces_chunked() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let kawa = decode_request_headers(
            &mut pool,
            &[
                (b":method", b"POST"),
                (b":scheme", b"https"),
                (b":path", b"/upload"),
                (b":authority", b"example.com"),
            ],
            false, // body follows
        );
        assert_eq!(kawa.body_size, BodySize::Chunked);
        let buf = kawa.storage.buffer();
        let has_te_chunked = kawa.blocks.iter().any(|b| {
            matches!(b, Block::Header(Pair { key, val })
                if key.data(buf).eq_ignore_ascii_case(b"Transfer-Encoding")
                    && val.data(buf).eq_ignore_ascii_case(b"chunked"))
        });
        assert!(
            has_te_chunked,
            "H2→H1 chunked body must emit Transfer-Encoding: chunked for trailer support"
        );
    }
    // ── handle_trailer: spoof-vector elision per RFC 9110 §6.5 ───────────

    /// Helper: HPACK-encode a set of trailer pairs and run them through
    /// `handle_trailer`. Returns the kawa stream so the caller can
    /// inspect which trailer headers landed in `kawa.blocks`.
    fn decode_trailer(
        pool: &mut crate::pool::Pool,
        trailers: &[(&[u8], &[u8])],
    ) -> GenericHttpStream {
        let mut encoder = loona_hpack::Encoder::new();
        let mut encoded = Vec::new();
        for &(name, value) in trailers {
            encoder
                .encode_header_into((name, value), &mut encoded)
                .unwrap();
        }

        let mut decoder = loona_hpack::Decoder::new();
        let mut kawa = make_generic_kawa(pool, Kind::Request);

        let result = handle_trailer(
            &mut kawa,
            &encoded,
            true, // end_stream
            &mut decoder,
            crate::protocol::mux::h2::MAX_HEADER_LIST_SIZE as u32,
            false, // elide_x_real_ip — listener flag, not the trailer-side gate
        );
        assert!(result.is_ok(), "handle_trailer failed: {:?}", result.err());
        kawa
    }

    /// Returns the lowercased trailer keys that survived the elision filter.
    fn surviving_trailer_keys(kawa: &GenericHttpStream) -> Vec<Vec<u8>> {
        let buf = kawa.storage.buffer();
        kawa.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Header(Pair { key, .. }) => Some(key.data(buf).to_vec()),
                _ => None,
            })
            .collect()
    }

    /// RFC 9110 §6.5 forbids trailers from carrying message-routing
    /// fields. The initial-HEADERS path rewrites four client-attribution
    /// headers (`X-Real-IP`, `X-Forwarded-For`, `Forwarded`,
    /// `X-Request-Id`). An H2 client must not be able to smuggle a
    /// spoofed copy as a trailer, since H2 backends that merge trailers
    /// into their header view would observe the attacker-controlled
    /// value. Drop them unconditionally.
    #[test]
    fn handle_trailer_elides_spoof_vector_headers() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let kawa = decode_trailer(
            &mut pool,
            &[
                (b"x-real-ip", b"1.2.3.4"),
                (b"x-forwarded-for", b"5.6.7.8"),
                (b"forwarded", b"for=9.10.11.12"),
                (b"x-request-id", b"attacker-correlation-id"),
                (b"x-trailer-keep", b"please-keep-me"),
            ],
        );
        let surviving = surviving_trailer_keys(&kawa);
        assert_eq!(
            surviving,
            vec![b"x-trailer-keep".to_vec()],
            "only non-spoof trailers must survive the RFC 9110 §6.5 elision"
        );
    }

    /// Each of the four spoof-vector headers must be dropped on its own,
    /// so a single-trailer attempt cannot bypass the filter just because
    /// the others are absent.
    #[test]
    fn handle_trailer_drops_each_spoof_header_individually() {
        for &name in &[
            b"x-real-ip" as &[u8],
            b"x-forwarded-for",
            b"forwarded",
            b"x-request-id",
        ] {
            let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
            let kawa = decode_trailer(&mut pool, &[(name, b"v")]);
            let surviving = surviving_trailer_keys(&kawa);
            assert!(
                surviving.is_empty(),
                "trailer with only {} should leave no surviving block",
                std::str::from_utf8(name).unwrap()
            );
        }
    }

    /// Sanity: an unrelated trailer (e.g. `grpc-status`) is still kept.
    /// Without this we'd be silently breaking gRPC semantics.
    #[test]
    fn handle_trailer_keeps_grpc_status() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let kawa = decode_trailer(&mut pool, &[(b"grpc-status", b"0")]);
        let surviving = surviving_trailer_keys(&kawa);
        assert_eq!(surviving, vec![b"grpc-status".to_vec()]);
    }

    /// H2 request declaring Content-Length keeps the Length framing: the
    /// H1 backend cannot receive trailers in this case (RFC 9110 §6.5 —
    /// trailers require chunked transfer-coding), and we document that
    /// the caller must drop trailers rather than retro-fit Transfer-Encoding
    /// once the request headers have already gone out on the wire.
    #[test]
    fn test_h2_to_h1_body_with_content_length_keeps_length_framing() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let kawa = decode_request_headers(
            &mut pool,
            &[
                (b":method", b"POST"),
                (b":scheme", b"https"),
                (b":path", b"/upload"),
                (b":authority", b"example.com"),
                (b"content-length", b"42"),
            ],
            false,
        );
        assert_eq!(kawa.body_size, BodySize::Length(42));
        let buf = kawa.storage.buffer();
        let has_te = kawa.blocks.iter().any(|b| {
            matches!(b, Block::Header(Pair { key, .. })
                if key.data(buf).eq_ignore_ascii_case(b"Transfer-Encoding"))
        });
        assert!(
            !has_te,
            "Transfer-Encoding must not be retro-fitted when Content-Length was declared"
        );
    }
}
