use std::fmt::Write;

use kawa::{Block, Pair, ParsingErrorKind, ParsingPhase, ParsingPhaseMarker};

use super::GenericHttpStream;

#[cfg(feature = "tolerant-http1-parser")]
const CHARSET: &str = "all characters are LATIN-1 (no UTF-8 allowed)";
#[cfg(not(feature = "tolerant-http1-parser"))]
const CHARSET: &str = "all characters are UASCII (no UTF-8 allowed)";

fn hex_dump(buffer: &[u8], window: usize, start: usize, end: usize) -> String {
    let mut result = String::with_capacity(window * 4 * 2 + 10);
    if end - start <= window {
        let slice = &buffer[start..end];
        for c in slice {
            let _ = write!(result, " {c:02x}");
        }
        result.push_str(
            &std::iter::repeat(' ')
                .take((window + start - end) * 3 + 4)
                .collect::<String>(),
        );
        result.push_str(&String::from_utf8_lossy(slice).escape_debug().to_string());
    } else {
        let slice1 = &buffer[start..start + window - 1];
        let slice2 = &buffer[end - window + 1..end];
        for c in slice1 {
            let _ = write!(result, " {c:02x}");
        }
        result.push_str(" ..    ");
        result.push_str(&String::from_utf8_lossy(slice1).escape_debug().to_string());
        result.push_str("..\n ..");
        for c in slice2 {
            let _ = write!(result, " {c:02x}");
        }
        result.push_str("    ..");
        result.push_str(&String::from_utf8_lossy(slice2).escape_debug().to_string());
    }
    result
}

pub fn diagnostic_400_502(
    marker: ParsingPhaseMarker,
    kind: ParsingErrorKind,
    kawa: &GenericHttpStream,
) -> (String, String) {
    match kind {
        ParsingErrorKind::Consuming { index } => {
            let message = match marker {
                ParsingPhaseMarker::StatusLine => {
                    format!(
                        "The request line is invalid. Make sure it is well formated and {CHARSET}."
                    )
                }
                ParsingPhaseMarker::Headers | ParsingPhaseMarker::Trailers => {
                    let marker = if marker == ParsingPhaseMarker::Headers {
                        "header"
                    } else {
                        "trailer"
                    };
                    if let Some(Block::Header(Pair { key, .. })) = kawa.blocks.back() {
                        format!(
                            "A {marker} is invalid, make sure {CHARSET}. Last valid {marker} is: {:?}.",
                            String::from_utf8_lossy(key.data(kawa.storage.buffer())),
                        )
                    } else {
                        format!("The first {marker} is invalid, make sure {CHARSET}.")
                    }
                }
                ParsingPhaseMarker::Cookies => {
                    if kawa.detached.jar.len() > 1 {
                        let Pair { key, .. } = &kawa.detached.jar[kawa.detached.jar.len() - 2];
                        format!(
                            "A cookie is invalid, make sure {CHARSET}. Last valid cookie is: {:?}.",
                            String::from_utf8_lossy(key.data(kawa.storage.buffer())),
                        )
                    } else {
                        format!("The first cookie is invalid, make sure {CHARSET}.")
                    }
                }
                ParsingPhaseMarker::Body
                | ParsingPhaseMarker::Chunks
                | ParsingPhaseMarker::Terminated
                | ParsingPhaseMarker::Error => {
                    format!("Parsing phase {marker:?} should not produce 400 error.")
                }
            };
            let buffer = kawa.storage.buffer();
            let details = format!(
                "\
Parsed successfully:
{}
Partially parsed (valid):
{}
Invalid:
{}",
                hex_dump(buffer, 32, kawa.storage.start, kawa.storage.head),
                hex_dump(buffer, 32, kawa.storage.head, index as usize),
                hex_dump(buffer, 32, index as usize, kawa.storage.end),
            );
            (message, details)
        }
        ParsingErrorKind::Processing { message } => (
            "The request was successfully parsed but presents inconsistent or invalid values."
                .into(),
            message.to_owned(),
        ),
    }
}

pub fn diagnostic_413_507(parsing_phase: ParsingPhase) -> String {
    match parsing_phase {
        kawa::ParsingPhase::StatusLine => {
            format!("Request line is too long. Note that an URL should not exceed 2083 characters.")
        }
        kawa::ParsingPhase::Headers | kawa::ParsingPhase::Cookies { .. } => {
            format!("Headers are too long. All headers should fit in a single buffer.")
        }
        phase => format!("Unexpected parsing phase: {phase:?}"),
    }
}
