use std::{io::Write, str::from_utf8_unchecked};

use kawa::{
    h1::ParserCallbacks, repr::Slice, Block, BodySize, Flags, Kind, Pair, ParsingPhase, StatusLine,
    Store, Version,
};

use crate::{
    pool::Checkout,
    protocol::{
        http::parser::compare_no_case,
        mux::{h2::Prioriser, parser::PriorityPart, GenericHttpStream, StreamId},
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
) where
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
                    method = val;
                } else if compare_no_case(&k, b":scheme") {
                    scheme = val;
                } else if compare_no_case(&k, b":path") {
                    path = val;
                } else if compare_no_case(&k, b":authority") {
                    authority = val;
                } else if compare_no_case(&k, b"cookie---") {
                    todo!("cookies should be split in pairs");
                } else if compare_no_case(&k, b"priority") {
                    todo!("decode priority");
                    prioriser.push_priority(
                        stream_id,
                        PriorityPart::Rfc9218 {
                            urgency: todo!(),
                            incremental: todo!(),
                        },
                    )
                } else {
                    if compare_no_case(&k, b"content-length") {
                        let length = unsafe { from_utf8_unchecked(&v).parse::<usize>().unwrap() };
                        kawa.body_size = BodySize::Length(length);
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
                println!("INVALID FRAGMENT: {error:?}");
                kawa.parsing_phase.error("Invalid header fragment".into());
            }
            if method.is_empty() || authority.is_empty() || path.is_empty() {
                println!("MISSING PSEUDO HEADERS");
                kawa.parsing_phase.error("Missing pseudo headers".into());
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
                    status = val;
                    code = unsafe { from_utf8_unchecked(&v).parse::<u16>().ok().unwrap() }
                } else {
                    kawa.storage.write_all(&k).unwrap();
                    let key = Store::Slice(Slice {
                        start: start + len_val,
                        len: len_key,
                    });
                    kawa.push_block(Block::Header(Pair { key, val }));
                }
            });
            if let Err(error) = decode_status {
                println!("INVALID FRAGMENT: {error:?}");
                kawa.parsing_phase.error("Invalid header fragment".into());
            }
            if status.is_empty() {
                println!("MISSING PSEUDO HEADERS");
                kawa.parsing_phase.error("Missing pseudo headers".into());
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
    println!(
        "index: {}/{}/{}",
        kawa.storage.start, kawa.storage.head, kawa.storage.end
    );

    if kawa.is_error() {
        return;
    }
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
        return;
    }

    kawa.parsing_phase = match kawa.body_size {
        BodySize::Chunked => ParsingPhase::Chunks { first: true },
        BodySize::Length(0) => ParsingPhase::Terminated,
        BodySize::Length(_) => ParsingPhase::Body,
        BodySize::Empty => ParsingPhase::Chunks { first: true },
    };
}

pub fn handle_trailer(
    kawa: &mut GenericHttpStream,
    input: &[u8],
    end_stream: bool,
    decoder: &mut hpack::Decoder,
) {
    decoder
        .decode_with_cb(input, |k, v| {
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
        })
        .unwrap();

    // assert!(end_stream);
    kawa.push_block(Block::Flags(Flags {
        end_body: end_stream,
        end_chunk: false,
        end_header: true,
        end_stream,
    }));
    kawa.parsing_phase = ParsingPhase::Terminated;
}
