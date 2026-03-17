use std::{io::Write, str::from_utf8};

use kawa::{
    Block, BodySize, Flags, Kind, Pair, ParsingPhase, StatusLine, Store, Version,
    h1::ParserCallbacks, repr::Slice,
};

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

/// Returns true if the header name contains any uppercase ASCII letter (A-Z).
/// RFC 9113 section 8.2 requires all header field names to be lowercase in HTTP/2.
fn has_uppercase_ascii(name: &[u8]) -> bool {
    name.iter().any(|b| b.is_ascii_uppercase())
}

/// Returns true if the header name is a connection-specific header field
/// that MUST NOT appear in HTTP/2 (RFC 9113 section 8.2.2).
pub(super) fn is_connection_specific_header(name: &[u8]) -> bool {
    compare_no_case(name, b"connection")
        || compare_no_case(name, b"proxy-connection")
        || compare_no_case(name, b"transfer-encoding")
        || compare_no_case(name, b"upgrade")
        || compare_no_case(name, b"keep-alive")
}

/// Returns true if the TE header has a value other than "trailers".
/// RFC 9113 section 8.2.2: the only acceptable value for TE in HTTP/2 is "trailers".
fn is_invalid_te_value(value: &[u8]) -> bool {
    !compare_no_case(value, b"trailers")
}

/// Returns true if the header violates HTTP/2 field requirements (RFC 9113 §8.2):
/// - uppercase ASCII characters in the name
/// - connection-specific header fields (RFC 9113 §8.2.2)
/// - TE header with a value other than "trailers"
fn is_invalid_h2_header(name: &[u8], value: &[u8]) -> bool {
    name.is_empty()
        || has_uppercase_ascii(name)
        || is_connection_specific_header(name)
        || (compare_no_case(name, b"te") && is_invalid_te_value(value))
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
/// Returns `Some(Store::Slice)` on success, or `None` if the pseudo-header was
/// already set (`dest` is non-empty), regular headers have already appeared, or
/// the write to storage fails. Callers should set `invalid_headers = true` on `None`.
fn store_pseudo_header(
    dest: &Store,
    regular_headers: bool,
    kawa: &mut GenericHttpStream,
    value: &[u8],
) -> Option<Store> {
    if !dest.is_empty() || regular_headers {
        return None;
    }
    let start = kawa.storage.end as u32;
    if kawa.storage.write_all(value).is_err() {
        return None;
    }
    Some(Store::Slice(Slice {
        start,
        len: value.len() as u32,
    }))
}

/// Write a regular (non-pseudo) header into kawa storage and push a `Block::Header`.
///
/// Also handles `content-length` validation: if the header is `content-length`, its
/// value is parsed and checked for consistency with any previously seen value.
/// Returns `false` if content-length parsing fails or conflicts (caller should set
/// `invalid_headers = true`).
fn write_regular_header(kawa: &mut GenericHttpStream, key: &[u8], value: &[u8]) -> bool {
    let len_key = key.len() as u32;
    let len_val = value.len() as u32;
    let start = kawa.storage.end as u32;
    if kawa.storage.write_all(value).is_err() {
        return false;
    }
    let val = Store::Slice(Slice {
        start,
        len: len_val,
    });
    if compare_no_case(key, b"content-length") {
        if let Some(length) = from_utf8(value).ok().and_then(|v| v.parse::<usize>().ok()) {
            if !set_content_length(&mut kawa.body_size, length) {
                return false;
            }
        } else {
            return false;
        }
    }
    if kawa.storage.write_all(key).is_err() {
        return false;
    }
    let key = Store::Slice(Slice {
        start: start + len_val,
        len: len_key,
    });
    kawa.push_block(Block::Header(Pair { key, val }));
    true
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
fn parse_rfc9218_priority(value: &[u8]) -> (u8, bool) {
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

pub fn handle_header<C>(
    decoder: &mut loona_hpack::Decoder<'static>,
    prioriser: &mut Prioriser,
    stream_id: StreamId,
    kawa: &mut GenericHttpStream,
    input: &[u8],
    end_stream: bool,
    callbacks: &mut C,
) -> Result<(), (H2Error, bool)>
where
    C: ParserCallbacks<Checkout>,
{
    if !kawa.is_initial() {
        return handle_trailer(kawa, input, end_stream, decoder);
    }
    kawa.push_block(Block::StatusLine);
    let max_decoded_bytes = crate::protocol::mux::h2::MAX_HEADER_LIST_SIZE as usize;
    kawa.detached.status_line = match kawa.kind {
        Kind::Request => {
            let mut method = Store::Empty;
            let mut authority = Store::Empty;
            let mut path = Store::Empty;
            let mut scheme = Store::Empty;
            let mut invalid_headers = false;
            let mut budget_exceeded = false;
            let mut decoded_bytes: usize = 0;
            let mut regular_headers = false;
            let mut cookies_added = false;
            let decode_status = decoder.decode_with_cb(input, |k, v| {
                if invalid_headers || budget_exceeded {
                    return;
                }

                decoded_bytes = decoded_bytes.saturating_add(k.len() + v.len());
                if decoded_bytes > max_decoded_bytes {
                    budget_exceeded = true;
                    return;
                }

                if is_invalid_h2_header(&k, &v) {
                    invalid_headers = true;
                    return;
                }

                if compare_no_case(&k, b":method") {
                    match store_pseudo_header(&method, regular_headers, kawa, &v) {
                        Some(s) => method = s,
                        None => invalid_headers = true,
                    }
                } else if compare_no_case(&k, b":scheme") {
                    match store_pseudo_header(&scheme, regular_headers, kawa, &v) {
                        Some(s) => scheme = s,
                        None => invalid_headers = true,
                    }
                } else if compare_no_case(&k, b":path") {
                    match store_pseudo_header(&path, regular_headers, kawa, &v) {
                        Some(s) => path = s,
                        None => invalid_headers = true,
                    }
                } else if compare_no_case(&k, b":authority") {
                    match store_pseudo_header(&authority, regular_headers, kawa, &v) {
                        Some(s) => authority = s,
                        None => invalid_headers = true,
                    }
                } else if k.starts_with(b":") {
                    invalid_headers = true;
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
                        let (cookie_key, cookie_val) = match trimmed.iter().position(|&b| b == b'=')
                        {
                            Some(eq_pos) => (&trimmed[..eq_pos], &trimmed[eq_pos + 1..]),
                            None => (trimmed, &b""[..]),
                        };
                        let key_start = kawa.storage.end as u32;
                        if kawa.storage.write_all(cookie_key).is_err() {
                            invalid_headers = true;
                            return;
                        }
                        let key_len = cookie_key.len() as u32;
                        let val_start = kawa.storage.end as u32;
                        if kawa.storage.write_all(cookie_val).is_err() {
                            invalid_headers = true;
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
                } else {
                    regular_headers = true;
                    if !write_regular_header(kawa, &k, &v) {
                        invalid_headers = true;
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
            });
            if let Err(error) = decode_status {
                error!("INVALID FRAGMENT: {:?}", error);
                return Err((H2Error::CompressionError, true));
            }
            if budget_exceeded {
                error!(
                    "HPACK decoded header size {} exceeds MAX_HEADER_LIST_SIZE {}",
                    decoded_bytes, max_decoded_bytes
                );
                return Err((H2Error::EnhanceYourCalm, false));
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
                error!("INVALID HEADERS");
                return Err((H2Error::ProtocolError, false));
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
            let mut invalid_headers = false;
            let mut budget_exceeded = false;
            let mut decoded_bytes: usize = 0;
            let mut regular_headers = false;
            let decode_status = decoder.decode_with_cb(input, |k, v| {
                if invalid_headers || budget_exceeded {
                    return;
                }

                decoded_bytes = decoded_bytes.saturating_add(k.len() + v.len());
                if decoded_bytes > max_decoded_bytes {
                    budget_exceeded = true;
                    return;
                }

                if is_invalid_h2_header(&k, &v) {
                    invalid_headers = true;
                    return;
                }

                if compare_no_case(&k, b":status") {
                    match store_pseudo_header(&status, regular_headers, kawa, &v) {
                        Some(s) => {
                            status = s;
                            if let Some(parsed_code) =
                                from_utf8(&v).ok().and_then(|v| v.parse::<u16>().ok())
                            {
                                code = parsed_code;
                            } else {
                                invalid_headers = true;
                            }
                        }
                        None => invalid_headers = true,
                    }
                } else if k.starts_with(b":") {
                    invalid_headers = true;
                } else {
                    regular_headers = true;
                    if !write_regular_header(kawa, &k, &v) {
                        invalid_headers = true;
                    }
                }
            });
            if let Err(error) = decode_status {
                error!("INVALID FRAGMENT: {:?}", error);
                return Err((H2Error::CompressionError, true));
            }
            if budget_exceeded {
                error!(
                    "HPACK decoded header size {} exceeds MAX_HEADER_LIST_SIZE {}",
                    decoded_bytes, max_decoded_bytes
                );
                return Err((H2Error::EnhanceYourCalm, false));
            }
            if invalid_headers || status.is_empty() {
                error!("INVALID HEADERS");
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
        "index: {}/{}/{}",
        kawa.storage.start, kawa.storage.head, kawa.storage.end
    );

    callbacks.on_headers(kawa);

    if end_stream {
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

pub fn handle_trailer(
    kawa: &mut GenericHttpStream,
    input: &[u8],
    end_stream: bool,
    decoder: &mut loona_hpack::Decoder<'static>,
) -> Result<(), (H2Error, bool)> {
    if !end_stream {
        return Err((H2Error::ProtocolError, false));
    }
    let mut invalid_trailers = false;
    let decode_status = decoder.decode_with_cb(input, |k, v| {
        // RFC 9113 §8.1: Trailers MUST NOT contain pseudo-header fields.
        if k.starts_with(b":") {
            invalid_trailers = true;
            return;
        }
        // RFC 9113 §8.2: reject uppercase header names in HTTP/2
        if has_uppercase_ascii(&k) {
            invalid_trailers = true;
            return;
        }
        let start = kawa.storage.end as u32;
        if kawa.storage.write_all(&k).is_err() || kawa.storage.write_all(&v).is_err() {
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
        error!("INVALID FRAGMENT: {:?}", error);
        return Err((H2Error::CompressionError, true));
    }
    if invalid_trailers {
        error!("INVALID TRAILERS");
        return Err((H2Error::ProtocolError, false));
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
        assert!(result.is_some());

        // Second store with non-empty dest fails (duplicate pseudo-header)
        let dest = result.unwrap();
        let result = store_pseudo_header(&dest, false, &mut kawa, b"POST");
        assert!(result.is_none());
    }

    #[test]
    fn test_store_pseudo_header_rejects_after_regular_headers() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = make_generic_kawa(&mut pool, Kind::Request);

        let dest = Store::Empty;
        // regular_headers = true means regular headers have already appeared
        let result = store_pseudo_header(&dest, true, &mut kawa, b"GET");
        assert!(result.is_none());
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
}
