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
fn is_connection_specific_header(name: &[u8]) -> bool {
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
    kawa.detached.status_line = match kawa.kind {
        Kind::Request => {
            let mut method = Store::Empty;
            let mut authority = Store::Empty;
            let mut path = Store::Empty;
            let mut scheme = Store::Empty;
            let mut invalid_headers = false;
            let mut regular_headers = false;
            let decode_status = decoder.decode_with_cb(input, |k, v| {
                // RFC 9113 §8.2: reject header field names with uppercase ASCII
                if has_uppercase_ascii(&k) {
                    invalid_headers = true;
                    return;
                }

                // RFC 9113 §8.2.2: reject connection-specific headers
                if is_connection_specific_header(&k) {
                    invalid_headers = true;
                    return;
                }

                // RFC 9113 §8.2.2: TE header is only allowed with value "trailers"
                if compare_no_case(&k, b"te") && is_invalid_te_value(&v) {
                    invalid_headers = true;
                    return;
                }

                let len_key = k.len() as u32;
                let len_val = v.len() as u32;

                if compare_no_case(&k, b":method") {
                    if !method.is_empty() || regular_headers {
                        invalid_headers = true;
                        return;
                    }
                    let start = kawa.storage.end as u32;
                    if kawa.storage.write_all(&v).is_err() {
                        invalid_headers = true;
                        return;
                    }
                    method = Store::Slice(Slice {
                        start,
                        len: len_val,
                    });
                } else if compare_no_case(&k, b":scheme") {
                    if !scheme.is_empty() || regular_headers {
                        invalid_headers = true;
                        return;
                    }
                    let start = kawa.storage.end as u32;
                    if kawa.storage.write_all(&v).is_err() {
                        invalid_headers = true;
                        return;
                    }
                    scheme = Store::Slice(Slice {
                        start,
                        len: len_val,
                    });
                } else if compare_no_case(&k, b":path") {
                    if !path.is_empty() || regular_headers {
                        invalid_headers = true;
                        return;
                    }
                    let start = kawa.storage.end as u32;
                    if kawa.storage.write_all(&v).is_err() {
                        invalid_headers = true;
                        return;
                    }
                    path = Store::Slice(Slice {
                        start,
                        len: len_val,
                    });
                } else if compare_no_case(&k, b":authority") {
                    if !authority.is_empty() || regular_headers {
                        invalid_headers = true;
                        return;
                    }
                    let start = kawa.storage.end as u32;
                    if kawa.storage.write_all(&v).is_err() {
                        invalid_headers = true;
                        return;
                    }
                    authority = Store::Slice(Slice {
                        start,
                        len: len_val,
                    });
                } else if k.starts_with(b":") {
                    invalid_headers = true;
                } else if compare_no_case(&k, b"cookie") {
                    regular_headers = true;
                    // RFC 9113 §8.2.3: split combined cookie headers into individual pairs.
                    // Each cookie-pair separated by "; " becomes a separate cookie header.
                    // Only the split pairs are written to storage (not the full combined value).
                    for cookie_pair in v.split(|&b| b == b';') {
                        let trimmed = trim_ows(cookie_pair);
                        if trimmed.is_empty() {
                            continue;
                        }
                        let pair_start = kawa.storage.end as u32;
                        if kawa.storage.write_all(trimmed).is_err()
                            || kawa.storage.write_all(&k).is_err()
                        {
                            invalid_headers = true;
                            return;
                        }
                        let pair_len = trimmed.len() as u32;
                        let pair_val = Store::Slice(Slice {
                            start: pair_start,
                            len: pair_len,
                        });
                        let pair_key = Store::Slice(Slice {
                            start: pair_start + pair_len,
                            len: len_key,
                        });
                        kawa.push_block(Block::Header(Pair {
                            key: pair_key,
                            val: pair_val,
                        }));
                    }
                } else {
                    regular_headers = true;
                    let start = kawa.storage.end as u32;
                    if kawa.storage.write_all(&v).is_err() {
                        invalid_headers = true;
                        return;
                    }
                    let val = Store::Slice(Slice {
                        start,
                        len: len_val,
                    });
                    if compare_no_case(&k, b"content-length") {
                        if let Some(length) =
                            from_utf8(&v).ok().and_then(|v| v.parse::<usize>().ok())
                        {
                            kawa.body_size = BodySize::Length(length);
                        } else {
                            invalid_headers = true;
                        }
                    } else if compare_no_case(&k, b"priority") {
                        // todo!("decode priority");
                        prioriser.push_priority(
                            stream_id,
                            PriorityPart::Rfc9218 {
                                urgency: 0,
                                incremental: false,
                            },
                        );
                    }
                    if kawa.storage.write_all(&k).is_err() {
                        invalid_headers = true;
                        return;
                    }
                    let key = Store::Slice(Slice {
                        start: start + len_val,
                        len: len_key,
                    });
                    kawa.push_block(Block::Header(Pair { key, val }));
                }
            });
            if let Err(error) = decode_status {
                error!("INVALID FRAGMENT: {:?}", error);
                return Err((H2Error::CompressionError, true));
            }
            // Note: Store::is_empty() only matches Store::Empty, not Store::Slice { len: 0 }.
            // We must use len() == 0 to catch empty pseudo-header values like `:path: ""`.
            // RFC 9113 §8.3.1 requires all four pseudo-headers to be present and non-empty.
            // Note: CONNECT requests (RFC 9113 §8.5) only need :method + :authority,
            // but we don't advertise SETTINGS_ENABLE_CONNECT_PROTOCOL so CONNECT is
            // intentionally unsupported for now.
            #[allow(clippy::len_zero)]
            if invalid_headers
                || method.len() == 0
                || authority.len() == 0
                || path.len() == 0
                || scheme.len() == 0
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
            let mut regular_headers = false;
            let decode_status = decoder.decode_with_cb(input, |k, v| {
                // RFC 9113 §8.2: reject header field names with uppercase ASCII
                if has_uppercase_ascii(&k) {
                    invalid_headers = true;
                    return;
                }

                // RFC 9113 §8.2.2: reject connection-specific headers
                if is_connection_specific_header(&k) {
                    invalid_headers = true;
                    return;
                }

                // RFC 9113 §8.2.2: TE header is only allowed with value "trailers"
                if compare_no_case(&k, b"te") && is_invalid_te_value(&v) {
                    invalid_headers = true;
                    return;
                }

                let len_key = k.len() as u32;
                let len_val = v.len() as u32;

                if compare_no_case(&k, b":status") {
                    if !status.is_empty() || regular_headers {
                        invalid_headers = true;
                        return;
                    }
                    let start = kawa.storage.end as u32;
                    if kawa.storage.write_all(&v).is_err() {
                        invalid_headers = true;
                        return;
                    }
                    status = Store::Slice(Slice {
                        start,
                        len: len_val,
                    });
                    if let Some(parsed_code) =
                        from_utf8(&v).ok().and_then(|v| v.parse::<u16>().ok())
                    {
                        code = parsed_code;
                    } else {
                        invalid_headers = true;
                    }
                } else if k.starts_with(b":") {
                    invalid_headers = true;
                } else {
                    regular_headers = true;
                    let start = kawa.storage.end as u32;
                    if kawa.storage.write_all(&v).is_err() {
                        invalid_headers = true;
                        return;
                    }
                    let val = Store::Slice(Slice {
                        start,
                        len: len_val,
                    });
                    if compare_no_case(&k, b"content-length") {
                        if let Some(length) =
                            from_utf8(&v).ok().and_then(|v| v.parse::<usize>().ok())
                        {
                            kawa.body_size = BodySize::Length(length);
                        } else {
                            invalid_headers = true;
                        }
                    }
                    if kawa.storage.write_all(&k).is_err() {
                        invalid_headers = true;
                        return;
                    }
                    let key = Store::Slice(Slice {
                        start: start + len_val,
                        len: len_key,
                    });
                    kawa.push_block(Block::Header(Pair { key, val }));
                }
            });
            if let Err(error) = decode_status {
                error!("INVALID FRAGMENT: {:?}", error);
                return Err((H2Error::CompressionError, true));
            }
            #[allow(clippy::len_zero)]
            if invalid_headers || status.len() == 0 {
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
            kawa.body_size = BodySize::Length(0);
            kawa.push_block(Block::Header(Pair {
                key: Store::Static(b"Content-Length"),
                val: Store::Static(b"0"),
            }));
        }
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
