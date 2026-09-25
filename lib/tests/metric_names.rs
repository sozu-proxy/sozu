//! Static metric-name regression guard.
//!
//! Walks `lib/src/**/*.rs` — this crate's `src` only, NOT the repository —
//! and asserts that no `gauge!` / `gauge_add!` / `incr!` / `count!` / `decr!`
//! call site there passes its metric key as a string literal. Keys must
//! reference a constant from [`sozu_lib::metrics::names`] (directly, or
//! through a helper that returns one), which is the rule
//! `lib/src/metrics/names.rs` states in its own module documentation:
//!
//! > Every metric string emitted by Sōzu and consumed by the StatsD/
//! > Prometheus/TUI surface should reference a constant declared here rather
//! > than being repeated as a literal.
//!
//! A literal key is not a style nit: it is how `h2.streams.ready_incremental
//! .by_urgency` reached production as a per-connection `gauge!` on a global
//! key with no entry in `names.rs`, invisible to every reader of that file.
//!
//! The scan is deliberately allowlist-free. A new literal key in `lib/src` is
//! a test failure, not a queue entry.
//!
//! ## What this does NOT cover
//!
//! The gate closes the dominant shape — a key written out at its emit site —
//! and does not make a literal key impossible. Known gaps, all pre-existing:
//!
//! - **Other crates.** `bin/` and `command/` are outside `CARGO_MANIFEST_DIR`
//!   and are not scanned. `load_state` (`bin/src/command/requests.rs`) emits
//!   `count!("config.load_skipped_invalid", 1)` at two sites — a literal key
//!   with no `names.rs` entry — and this test says nothing about them.
//! - **Keys assembled by a helper.** `reject_metric_key!` and
//!   `h2_error_metric_key!` `concat!` a literal prefix with a variant suffix,
//!   and `metric_for_rst_stream_received` ends in a bare
//!   `.unwrap_or("h2.rst_stream.received.unknown_error")`. The first argument
//!   at the emit site is a call, so the scan passes it by construction.
//! - **Indirection.** A key bound to a local `const` or `let` first, then
//!   passed by name, reads as an identifier.
//! - **Non-parenthesised invocation.** `incr!["k"]` / `incr!{"k"}` are legal
//!   macro syntax; only `(` is matched.
//!
//! Closing any of these means parsing Rust, not scanning it. The narrow scan
//! is the trade; the list above is the honest statement of its edge.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Metric macros whose FIRST argument is the metric key. `decr!` forwards to
/// `count!` and `time!`/`record_metric` take the same key position, but only
/// the five that can name a counter or gauge key are gated here.
const KEY_MACROS: &[&str] = &["gauge_add", "gauge", "incr", "count", "decr"];

/// Byte-for-byte mask of `src`: comment bytes and the *contents* of string,
/// raw-string and char literals become `X`, every `\n` survives. Offsets and
/// line numbers therefore map 1:1 back onto `src`, while a macro name written
/// in prose (`[`gauge_add!`]`, ``count!()``) or inside a literal can no longer
/// be mistaken for a call site. The delimiting quotes are KEPT so a literal
/// first argument is still detectable.
fn mask(src: &[u8]) -> Vec<u8> {
    let mut out = vec![b' '; src.len()];
    let mut i = 0usize;
    // Copy `src[i]` through untouched (structural bytes the scanner reads).
    macro_rules! keep {
        () => {{
            out[i] = src[i];
            i += 1;
        }};
    }
    // Blank `src[i]` but preserve line structure.
    macro_rules! blank {
        ($fill:expr) => {{
            out[i] = if src[i] == b'\n' { b'\n' } else { $fill };
            i += 1;
        }};
    }
    while i < src.len() {
        match src[i] {
            b'/' if src.get(i + 1) == Some(&b'/') => {
                while i < src.len() && src[i] != b'\n' {
                    blank!(b' ');
                }
            }
            b'/' if src.get(i + 1) == Some(&b'*') => {
                let mut depth = 0usize;
                while i < src.len() {
                    if src[i] == b'/' && src.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        blank!(b' ');
                        if i < src.len() {
                            blank!(b' ');
                        }
                    } else if src[i] == b'*' && src.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        blank!(b' ');
                        if i < src.len() {
                            blank!(b' ');
                        }
                        if depth == 0 {
                            break;
                        }
                    } else {
                        blank!(b' ');
                    }
                }
            }
            b'"' => {
                keep!(); // opening quote
                while i < src.len() {
                    if src[i] == b'\\' {
                        blank!(b'X');
                        if i < src.len() {
                            blank!(b'X');
                        }
                    } else if src[i] == b'"' {
                        keep!(); // closing quote
                        break;
                    } else {
                        blank!(b'X');
                    }
                }
            }
            b'\'' => {
                // Char literal vs lifetime tick. A lifetime can never start
                // with a backslash, and a char literal is always exactly one
                // char (or one escape) wide.
                let after = i + 1;
                let is_char = match src.get(after) {
                    Some(b'\\') => true,
                    Some(_) => {
                        // Width of the single UTF-8 char at `after`.
                        let w = match src[after] {
                            0x00..=0x7f => 1,
                            0xc0..=0xdf => 2,
                            0xe0..=0xef => 3,
                            _ => 4,
                        };
                        src.get(after + w) == Some(&b'\'')
                    }
                    None => false,
                };
                if !is_char {
                    keep!(); // lifetime tick
                    continue;
                }
                keep!(); // opening tick
                while i < src.len() {
                    if src[i] == b'\\' {
                        blank!(b'X');
                        if i < src.len() {
                            blank!(b'X');
                        }
                    } else if src[i] == b'\'' {
                        keep!(); // closing tick
                        break;
                    } else {
                        blank!(b'X');
                    }
                }
            }
            b'r' | b'b' => {
                // Raw / byte-raw string: `r"`, `r#"`, `b"`, `br#"`, … Only
                // when the prefix does not continue an identifier.
                let starts_token = i == 0 || !is_ident_byte(src[i - 1]);
                let mut j = i;
                if starts_token && src[j] == b'b' {
                    j += 1;
                }
                if starts_token && src.get(j) == Some(&b'r') {
                    j += 1;
                    let hash_start = j;
                    while src.get(j) == Some(&b'#') {
                        j += 1;
                    }
                    let hashes = j - hash_start;
                    if src.get(j) == Some(&b'"') {
                        while i <= j {
                            keep!(); // prefix, hashes, opening quote
                        }
                        // Closing delimiter is `"` followed by `hashes` `#`.
                        while i < src.len() {
                            if src[i] == b'"'
                                && src[i + 1..]
                                    .iter()
                                    .take(hashes)
                                    .filter(|c| **c == b'#')
                                    .count()
                                    == hashes
                            {
                                for _ in 0..=hashes {
                                    keep!();
                                }
                                break;
                            }
                            blank!(b'X');
                        }
                        continue;
                    }
                }
                keep!();
            }
            _ => keep!(),
        }
    }
    out
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Every `lib/src/**/*.rs` path, sorted for a stable failure report.
fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"));
        for entry in entries {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// To SEE THIS RED: in `lib/src/protocol/mux/h2.rs`, replace
/// `names::h2::STREAMS_READY_INCREMENTAL_BY_URGENCY` in `record_metric`
/// with the literal `"h2.streams.ready_incremental.by_urgency"`. The scan then
/// reports that one site, byte-identical emission and all.
///
/// AND AGAIN in raw form, which is the same defect one character wider: in
/// `lib/src/protocol/mux/stream.rs`, replace `names::http::ERRORS` with
/// `r"http.errors"`. Matching on a bare `"` used to pass this — the `r` sat
/// where the quote was expected — so the prefix walk above is load-bearing,
/// not decoration. `b"…"`, `br#"…"#` and `r##"…"##` take the same path.
#[test]
fn lib_src_metric_keys_reference_names_constants() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_root = crate_root.join("src");

    let mut violations: Vec<String> = Vec::new();
    for path in rust_sources(&src_root) {
        let raw = fs::read(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let masked = mask(&raw);
        let rel = path
            .strip_prefix(&crate_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();

        for name in KEY_MACROS {
            let pat = name.as_bytes();
            let mut at = 0usize;
            while let Some(hit) = find(&masked[at..], pat) {
                let start = at + hit;
                at = start + pat.len();
                // `!` must follow the name immediately, and the name must not
                // be the tail of a longer identifier (`macro_rules! count`,
                // `gauge_add` when probing for `gauge`, …).
                if masked.get(at) != Some(&b'!') {
                    continue;
                }
                if start > 0 && is_ident_byte(masked[start - 1]) {
                    continue;
                }
                let mut k = at + 1;
                while masked.get(k).is_some_and(|c| c.is_ascii_whitespace()) {
                    k += 1;
                }
                if masked.get(k) != Some(&b'(') {
                    continue;
                }
                k += 1;
                while masked.get(k).is_some_and(|c| c.is_ascii_whitespace()) {
                    k += 1;
                }
                // A Rust string literal may carry a `b` and/or `r` prefix and,
                // when raw, any number of `#` before the opening quote:
                // `"k"`, `b"k"`, `r"k"`, `br#"k"#`, `r##"k"##`. Every one of
                // them emits the same key, so every one is the same defect.
                // `mask` keeps the whole delimiter, so the scan must step over
                // it — matching on the bare `"` let `incr!(r"http.errors", …)`
                // through.
                let literal_start = k;
                if masked.get(k) == Some(&b'b') {
                    k += 1;
                }
                let raw_prefix = masked.get(k) == Some(&b'r');
                if raw_prefix {
                    k += 1;
                }
                let hashes_start = k;
                if raw_prefix {
                    while masked.get(k) == Some(&b'#') {
                        k += 1;
                    }
                }
                let hashes = k - hashes_start;
                if masked.get(k) != Some(&b'"') {
                    continue;
                }
                // Recover the real key from the unmasked source. `mask` blanks
                // literal contents but keeps both delimiters, so the next `"`
                // in the MASKED buffer is the true closing quote — escapes and
                // embedded quotes inside a raw string cannot end it early.
                let content_start = k + 1;
                let end = masked[content_start..]
                    .iter()
                    .position(|c| *c == b'"')
                    .map(|p| content_start + p)
                    .unwrap_or(raw.len());
                // Echo the literal exactly as written, prefix and all, so the
                // report names the form that slipped through.
                let literal =
                    String::from_utf8_lossy(&raw[literal_start..(end + 1 + hashes).min(raw.len())])
                        .into_owned();
                let line = 1 + masked[..start].iter().filter(|c| **c == b'\n').count();
                violations.push(format!("{rel}:{line}: {name}!({literal}, …)"));
            }
        }
    }

    violations.sort();
    assert!(
        violations.is_empty(),
        "found {n} metric call site(s) keyed by a string literal instead of a \
         `names::` constant:\n{joined}\n\n\
         add the constant to the matching `lib/src/metrics/names.rs` submodule \
         and reference it at the emission site. A literal key is invisible to \
         every reader of `names.rs`, so nothing stops a per-connection \
         `gauge!` from silently clobbering a global key.",
        n = violations.len(),
        joined = violations.join("\n"),
    );
}

/// First occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
