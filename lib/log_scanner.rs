// Shared scan helpers for the log-layout regression guard.
//
// Used by both `lib/build.rs` (compile-time `cargo:warning=` emitter) and
// `lib/tests/log_layout.rs` (test-time gating check). Brought into each
// via `include!`. Keeps the regex, bidirectional window, out-of-scope
// allowlist, and walker logic in one place so the build-time signal and
// the test-time gate cannot drift.
//
// This file is NOT a normal Rust module: it is included verbatim into
// `build.rs` and the integration-test crate, so it intentionally has no
// `pub` modifiers (everything is in the includer's scope) and pulls in
// `std::{collections::HashSet, fs, path::Path}` once for both consumers.

#[allow(unused_imports)]
use std::{collections::HashSet, fs, path::Path};

/// Files exempt from the log-layout rule. They run before any session
/// exists (control plane / event-loop scaffolding) or emit metric-shaped
/// lines that never pretend to carry a session envelope.
const OUT_OF_SCOPE_FILES: &[&str] = &[
    "src/lib.rs",
    "src/server.rs",
    "src/timer.rs",
    "src/backends.rs",
    "src/pool.rs",
    "src/splice.rs",
    "src/features.rs",
    "src/metrics/mod.rs",
    "src/metrics/local_drain.rs",
    "src/metrics/network_drain.rs",
];

/// Macro names that count as a tag-bearing prefix. Any of these appearing
/// inside the bidirectional scan window around a raw log call satisfies
/// the rule.
const CONTEXT_MACROS: &[&str] = &[
    "log_context!",
    "log_context_lite!",
    "log_context_stream!",
    "log_module_context!",
    "log_socket_context!",
    "log_socket_module_prefix",
];

/// `HttpContext::log_context(&self) -> LogContext<'_>` (and equivalent
/// helpers on other protocol structs) is the canonical method-call form
/// used inside `kawa_h1/editor.rs`, `tcp.rs`, `protocol/pipe.rs`, etc.
/// Matching `.log_context(` keeps the walker aware of method-call usage
/// without confusing it with the macro definitions in `CONTEXT_MACROS`
/// (those start with `pub fn ` or `macro_rules!`, never with `.`).
const CONTEXT_METHOD_NEEDLE: &str = ".log_context(";

/// Forward window after a log-call opening. Sized to cover the inline
/// `error!("{} ...", log_context!(self), ...)` shape and short multi-line
/// forms; 12 covers every existing well-formed call site at the time of
/// writing with margin.
const LOOKAHEAD_LINES: usize = 12;

/// Backward window before a log call. Covers the hoisted-local shape
/// (`let context = log_context!(self);` before a mutably-borrowed
/// `match &mut self.position { ... }` — see `mux/h2.rs:5468`-`5480` for
/// the canonical case where the macro reads `self.position` while the
/// match arm holds it mutably). 24 = longest observed hoist (~12 lines)
/// plus a comfort margin so future re-shuffling of those blocks does not
/// have to revisit this constant.
const LOOKBEHIND_LINES: usize = 24;

/// True iff `trimmed` starts with one of the level-keyed log macros and
/// the next byte opens the format-string argument list. Comments and
/// string literals are not handled — false positives are caught by the
/// `KNOWN_PREEXISTING_VIOLATIONS` allowlist in the integration test.
#[allow(dead_code)]
fn is_log_call_open(trimmed: &str) -> bool {
    const LEVELS: &[&str] = &["error!", "warn!", "info!", "debug!", "trace!"];
    LEVELS.iter().any(|prefix| {
        trimmed.starts_with(prefix)
            && trimmed
                .as_bytes()
                .get(prefix.len())
                .map(|b| *b == b'(')
                .unwrap_or(false)
    })
}

#[allow(dead_code)]
fn walk_dir(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_dir(&path, visit);
        } else {
            visit(&path);
        }
    }
}

/// Walk `<manifest_dir>/src/**/*.rs` and invoke `on_violation` for each
/// raw log call that is not covered by a `log_context*!` macro inside the
/// bidirectional window or by a `.log_context()` method call. Skips files
/// in `OUT_OF_SCOPE_FILES` and call sites inside `#[cfg(test)] mod ...`
/// blocks. Callback args: repo-relative path, 1-indexed line number,
/// trimmed source snippet for diagnostics.
#[allow(dead_code)]
fn scan_violations(manifest_dir: &str, on_violation: &mut dyn FnMut(&str, usize, &str)) {
    let src_root = Path::new(manifest_dir).join("src");
    let allowlist: HashSet<&str> = OUT_OF_SCOPE_FILES.iter().copied().collect();

    walk_dir(&src_root, &mut |path| {
        if path.extension().map(|e| e != "rs").unwrap_or(true) {
            return;
        }
        let rel = path
            .strip_prefix(manifest_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if allowlist.contains(rel.as_str()) {
            return;
        }
        let Ok(contents) = fs::read_to_string(path) else {
            return;
        };
        let lines: Vec<&str> = contents.lines().collect();
        let mut in_test_cfg_depth: i32 = 0;
        for (idx, raw_line) in lines.iter().enumerate() {
            let trimmed = raw_line.trim_start();
            if trimmed.starts_with("#[cfg(test)]") || trimmed.starts_with("#[cfg(any(test") {
                for next in lines.iter().skip(idx + 1).take(4) {
                    let next_trim = next.trim();
                    if next_trim.starts_with("mod ") && next_trim.ends_with('{') {
                        in_test_cfg_depth += 1;
                        break;
                    }
                    if !next_trim.is_empty() && !next_trim.starts_with("//") {
                        break;
                    }
                }
                continue;
            }
            if in_test_cfg_depth > 0 && raw_line.starts_with('}') {
                in_test_cfg_depth -= 1;
                continue;
            }
            if in_test_cfg_depth > 0 {
                continue;
            }
            if !is_log_call_open(trimmed) {
                continue;
            }
            let start = idx.saturating_sub(LOOKBEHIND_LINES);
            let end = (idx + 1 + LOOKAHEAD_LINES).min(lines.len());
            let window = &lines[start..end];
            let has_context = window.iter().any(|l| {
                CONTEXT_MACROS.iter().any(|m| l.contains(m)) || l.contains(CONTEXT_METHOD_NEEDLE)
            });
            if !has_context {
                on_violation(&rel, idx + 1, trimmed);
            }
        }
    });
}
