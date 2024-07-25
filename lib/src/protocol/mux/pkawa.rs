use std::{io::Write, str::from_utf8};

use kawa::{
    h1::ParserCallbacks, repr::Slice, Block, BodySize, Flags, Kind, Pair, ParsingPhase, StatusLine,
    Store, Version,
};

use crate::{
    pool::Checkout,
    protocol::{
        http::parser::compare_no_case,
        mux::{
            h2::Prioriser,
            parser::{H2Error, PriorityPart},
            GenericHttpStream, StreamId,
        },
    },
};

pub fn handle_header<C>(
    decoder: &mut hpack::Decoder,
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
    // kawa.detached.status_line = match kawa.kind {
    //     Kind::Request => StatusLine::Request {
    //         version: Version::V20,
    //         method: Store::Static(b"GET"),
    //         uri: Store::Static(b"/"),
    //         authority: Store::Static(b"lolcatho.st:8443"),
    //         path: Store::Static(b"/"),
    //     },
    //     Kind::Response => StatusLine::Response {
    //         version: Version::V20,
    //         code: 200,
    //         status: Store::Static(b"200"),
    //         reason: Store::Static(b"FromH2"),
    //     },
    // };
    kawa.detached.status_line = match kawa.kind {
        Kind::Request => {
            let mut method = Store::Empty;
            let mut authority = Store::Empty;
            let mut path = Store::Empty;
            let mut scheme = Store::Empty;
            let mut invalid_headers = false;
            let mut regular_headers = false;
            let decode_status = decoder.decode_with_cb(input, |k, v| {
                let start = kawa.storage.end as u32;
                kawa.storage.write_all(&v).unwrap();
                let len_key = k.len() as u32;
                let len_val = v.len() as u32;
                let val = Store::Slice(Slice {
                    start,
                    len: len_val,
                });

                if compare_no_case(&k, b":method") {
                    if !method.is_empty() || regular_headers {
                        invalid_headers = true;
                    }
                    method = val;
                } else if compare_no_case(&k, b":scheme") {
                    if !scheme.is_empty() || regular_headers {
                        invalid_headers = true;
                    }
                    scheme = val;
                } else if compare_no_case(&k, b":path") {
                    if !path.is_empty() || regular_headers {
                        invalid_headers = true;
                    }
                    path = val;
                } else if compare_no_case(&k, b":authority") {
                    if !authority.is_empty() || regular_headers {
                        invalid_headers = true;
                    }
                    authority = val;
                } else if k.starts_with(b":") {
                    invalid_headers = true;
                } else if compare_no_case(&k, b"cookie---") {
                    regular_headers = true;
                    todo!("cookies should be split in pairs");
                } else {
                    regular_headers = true;
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
                        warn!("DECODE PRIORITY: {}", unsafe {
                            std::str::from_utf8_unchecked(v.as_ref())
                        });
                        prioriser.push_priority(
                            stream_id,
                            PriorityPart::Rfc9218 {
                                urgency: 0,
                                incremental: false,
                            },
                        );
                    }
                    kawa.storage.write_all(&k).unwrap();
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
            if invalid_headers
                || method.len() == 0
                || authority.len() == 0
                || path.len() == 0
                || scheme.len() == 0
            {
                error!("INVALID HEADERS");
                return Err((H2Error::ProtocolError, false));
            }
            // uri is only used by H1 statusline, in most cases it only consists of the path
            // a better algorithm should be used though
            // let buffer = kawa.storage.data();
            // let uri = unsafe {
            //     format!(
            //         "{}://{}{}",
            //         from_utf8_unchecked(scheme.data(buffer)),
            //         from_utf8_unchecked(authority.data(buffer)),
            //         from_utf8_unchecked(path.data(buffer))
            //     )
            // };
            // println!("Reconstructed URI: {uri}");
            StatusLine::Request {
                version: Version::V20,
                method,
                uri: path.clone(), //Store::from_string(uri),
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
                let start = kawa.storage.end as u32;
                kawa.storage.write_all(&v).unwrap();
                let len_key = k.len() as u32;
                let len_val = v.len() as u32;
                let val = Store::Slice(Slice {
                    start,
                    len: len_val,
                });

                if compare_no_case(&k, b":status") {
                    if !status.is_empty() || regular_headers {
                        invalid_headers = true;
                    }
                    status = val;
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
                    kawa.storage.write_all(&k).unwrap();
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
    decoder: &mut hpack::Decoder,
) -> Result<(), (H2Error, bool)> {
    if !end_stream {
        return Err((H2Error::ProtocolError, false));
    }
    let decode_status = decoder.decode_with_cb(input, |k, v| {
        let start = kawa.storage.end as u32;
        kawa.storage.write_all(&k).unwrap();
        kawa.storage.write_all(&v).unwrap();
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
        println!("INVALID FRAGMENT: {error:?}");
        return Err((H2Error::CompressionError, true));
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
