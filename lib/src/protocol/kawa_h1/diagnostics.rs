use std::fmt::Write;

use kawa::{ParsingErrorKind, ParsingPhase, ParsingPhaseMarker};

use super::GenericHttpStream;

#[cfg(feature = "tolerant-http1-parser")]
const CHARSET: &str = "all characters are LATIN-1 (no UTF-8 allowed)";
#[cfg(not(feature = "tolerant-http1-parser"))]
const CHARSET: &str = "all characters are UASCII (no UTF-8 allowed)";

fn hex_dump(buffer: &[u8], window: usize, start: usize, end: usize) -> String {
    let mut result = String::with_capacity(window * 3 + 10);
    result.push('\"');
    if end - start <= window {
        let slice = &buffer[start..end];
        for (i, c) in slice.iter().enumerate() {
            let _ = write!(result, "{c:02x}");
            if i < slice.len() - 1 {
                result.push(' ');
            }
        }
    } else {
        let half = window / 2;
        let slice1 = &buffer[start..start + half - 1];
        let slice2 = &buffer[end - half + 1..end];
        for c in slice1 {
            let _ = write!(result, "{c:02x} ");
        }
        result.push_str("… ");
        for (i, c) in slice2.iter().enumerate() {
            let _ = write!(result, "{c:02x}");
            if i < slice2.len() - 1 {
                result.push(' ');
            }
        }
    }
    result.push('\"');
    result
}

pub fn diagnostic_400_502(
    marker: ParsingPhaseMarker,
    kind: ParsingErrorKind,
    kawa: &GenericHttpStream,
) -> (String, String, String, String) {
    match kind {
        ParsingErrorKind::Consuming { index } => {
            let message = match marker {
                ParsingPhaseMarker::StatusLine => {
                    format!(
                        "The status line is invalid. Make sure it is well formated and {CHARSET}."
                    )
                }
                ParsingPhaseMarker::Headers | ParsingPhaseMarker::Trailers => {
                    let marker = if marker == ParsingPhaseMarker::Headers {
                        "header"
                    } else {
                        "trailer"
                    };
                    format!("A {marker} is invalid, make sure {CHARSET}.")
                }
                ParsingPhaseMarker::Cookies => {
                    format!("A cookie is invalid, make sure {CHARSET}.")
                }
                ParsingPhaseMarker::Body
                | ParsingPhaseMarker::Chunks
                | ParsingPhaseMarker::Terminated
                | ParsingPhaseMarker::Error => "The parser stopped in an unexpected phase.".into(),
            };
            let buffer = kawa.storage.buffer();
            let successfully_parsed = hex_dump(buffer, 32, kawa.storage.start, kawa.storage.head);
            let partially_parsed = hex_dump(buffer, 32, kawa.storage.head, index as usize);
            let invalid = hex_dump(buffer, 32, index as usize, kawa.storage.end);
            (message, successfully_parsed, partially_parsed, invalid)
        }
        ParsingErrorKind::Processing { message } => (
            format!(
                "The request is correctly structured but presents inconsistent or invalid values: {message}."
            ),
            "null".into(),
            "null".into(),
            "null".into(),
        ),
    }
}

pub fn diagnostic_413_507(parsing_phase: ParsingPhase) -> String {
    match parsing_phase {
        kawa::ParsingPhase::StatusLine => {
            "Status line is too long. Note that an URL should not exceed 2083 characters.".into()
        }
        kawa::ParsingPhase::Headers | kawa::ParsingPhase::Cookies { .. } => {
            "Headers are too long. All headers should fit in a single buffer.".into()
        }
        _ => "The parser stopped in an unexpected phase.".into(),
    }
}
