use std::{io::Write, str::from_utf8_unchecked};

use crate::protocol::http::parser::compare_no_case;

use super::GenericHttpStream;

pub fn handle_header(kawa: &mut GenericHttpStream, input: &[u8], decoder: &mut hpack::Decoder) {
    println!("{input:?}");
    kawa.push_block(kawa::Block::StatusLine);
    kawa.detached.status_line = match kawa.kind {
        kawa::Kind::Request => {
            let mut method = kawa::Store::Empty;
            let mut authority = kawa::Store::Empty;
            let mut path = kawa::Store::Empty;
            let mut scheme = kawa::Store::Empty;
            decoder
                .decode_with_cb(input, |k, v| {
                    let start = kawa.storage.end as u32;
                    kawa.storage.write(&k).unwrap();
                    kawa.storage.write(&v).unwrap();
                    let len_key = k.len() as u32;
                    let len_val = v.len() as u32;
                    let key = kawa::Store::Slice(kawa::repr::Slice {
                        start,
                        len: len_key,
                    });
                    let val = kawa::Store::Slice(kawa::repr::Slice {
                        start: start + len_key,
                        len: len_val,
                    });

                    if compare_no_case(&k, b":method") {
                        method = val;
                    } else if compare_no_case(&k, b":authority") {
                        authority = val;
                    } else if compare_no_case(&k, b":path") {
                        path = val;
                    } else if compare_no_case(&k, b":scheme") {
                        scheme = val;
                    } else {
                        kawa.push_block(kawa::Block::Header(kawa::Pair { key, val }));
                    }
                })
                .unwrap();
            let buffer = kawa.storage.data();
            let uri = unsafe {
                format!(
                    "{}://{}{}",
                    from_utf8_unchecked(scheme.data(buffer)),
                    from_utf8_unchecked(authority.data(buffer)),
                    from_utf8_unchecked(path.data(buffer))
                )
            };
            println!("Reconstructed URI: {uri}");
            kawa::StatusLine::Request {
                version: kawa::Version::V20,
                method,
                uri: kawa::Store::from_string(uri),
                authority,
                path,
            }
        }
        kawa::Kind::Response => {
            let mut code = 0;
            let mut status = kawa::Store::Empty;
            decoder
                .decode_with_cb(input, |k, v| {
                    let start = kawa.storage.end as u32;
                    kawa.storage.write(&k).unwrap();
                    kawa.storage.write(&v).unwrap();
                    let len_key = k.len() as u32;
                    let len_val = v.len() as u32;
                    let key = kawa::Store::Slice(kawa::repr::Slice {
                        start,
                        len: len_key,
                    });
                    let val = kawa::Store::Slice(kawa::repr::Slice {
                        start: start + len_key,
                        len: len_val,
                    });

                    if compare_no_case(&k, b":status") {
                        status = val;
                        code = unsafe {
                            std::str::from_utf8_unchecked(&k)
                                .parse::<u16>()
                                .ok()
                                .unwrap()
                        }
                    } else {
                        kawa.push_block(kawa::Block::Header(kawa::Pair { key, val }));
                    }
                })
                .unwrap();
            kawa::StatusLine::Response {
                version: kawa::Version::V20,
                code,
                status,
                reason: kawa::Store::Empty,
            }
        }
    };

    // everything has been parsed
    kawa.storage.head = kawa.storage.end;
    kawa.parsing_phase = kawa::ParsingPhase::Chunks { first: true };
}
