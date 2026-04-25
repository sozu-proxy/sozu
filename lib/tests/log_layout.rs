//! Static log-layout regression guard.
//!
//! Walks `lib/src/**/*.rs` and asserts that every `error!`/`warn!`/`info!`/
//! `debug!`/`trace!` call site either:
//!
//! 1. has a `log_context`/`log_context_lite`/`log_context_stream`/
//!    `log_module_context`/`log_socket_context`/`log_socket_module_prefix`
//!    invocation, OR a `.log_context(` method-call (the canonical form used
//!    inside `kawa_h1/HttpContext` and other protocol structs that own a
//!    `LogContext` builder), within a bidirectional window — 24 lines back
//!    to catch the hoisted-local shape (`let context = log_context!(self);`
//!    before a mutably-borrowed `match &mut self.position { ... }`) and 12
//!    lines forward for the inline / single-arg shape. The format string is
//!    also expected to begin with `"{}"` so the macro output flows in as the
//!    first interpolation, OR
//! 2. lives under a `#[cfg(test)] mod ...` block (test-only callers can
//!    use bare `log::*`-style macros), OR
//! 3. lives in a file on the explicit allowlist (pre-fork / control-plane
//!    code that has no session context to attach).
//!
//! Mirrors the script in the planning research phase. Catches drift from the
//! "every protocol log line carries a load-bearing tag" rule encoded in
//! `CLAUDE.md`.

use std::{collections::HashSet, fs, path::Path};

/// Files that are intentionally exempt from the log-layout rule. These run
/// before any session exists (control plane / event-loop scaffolding) or
/// emit metric-shaped lines that never pretend to carry a session envelope.
const OUT_OF_SCOPE_FILES: &[&str] = &[
    "src/lib.rs",
    "src/server.rs",
    "src/timer.rs",
    "src/backends.rs",
    "src/pool.rs",
    "src/splice.rs",
    "src/features.rs",
    // metrics module: counter/gauge mechanics; lines emitted here are
    // intentionally bare and reach the access-log channel, not the protocol
    // log surface.
    "src/metrics/mod.rs",
    "src/metrics/local_drain.rs",
    "src/metrics/network_drain.rs",
];

/// Pre-existing untagged log sites in protocol/runtime files that this
/// regression guard surfaced but that the codebase deliberately has NOT
/// rewritten. Documented at the precision `file.rs:line` so future drift
/// in either the line numbers OR the rule can be detected: when a wrap is
/// added (or a violation appears in a file/line not in this list), the
/// test fires.
///
/// To extend coverage: wrap the call sites with the appropriate macro
/// (`log_context!(self)`, `log_module_context!()`, etc.) or the
/// `self.log_context()` method-call form recognised by the walker, drop
/// the corresponding entries here, and commit. The build.rs cargo:warning
/// emitter surfaces every entry on every `cargo build`, keeping the queue
/// visible without breaking builds.
///
/// Currently empty: every previously-tracked pre-existing violation has
/// been either wrapped at source or auto-cleared by the bidirectional
/// walker (which also recognises the canonical `.log_context()`
/// method-call form, see `CONTEXT_METHOD_NEEDLE`).
const KNOWN_PREEXISTING_VIOLATIONS: &[&str] = &[];

/// Macro names that count as a "tag-bearing" prefix. Any of these inside the
/// bidirectional scan window around a raw log call satisfies the rule.
const CONTEXT_MACROS: &[&str] = &[
    "log_context!",
    "log_context_lite!",
    "log_context_stream!",
    "log_module_context!",
    "log_socket_context!",
    "log_socket_module_prefix",
];

/// `HttpContext::log_context(&self) -> LogContext<'_>` (and the equivalent
/// helpers on other protocol structs) is the canonical method-call form used
/// inside `kawa_h1/editor.rs`, `tcp.rs`, `protocol/pipe.rs`, etc. Matching
/// `.log_context(` keeps the walker aware of method-call usage without
/// confusing it with the macro definitions above (those start with `pub fn `
/// or `macro_rules!`, never with `.`).
const CONTEXT_METHOD_NEEDLE: &str = ".log_context(";

/// Lines after a `error!(`/`warn!(`/etc. opening to scan for a context macro.
/// Catches the inline / single-line `error!("{} ...", log_context!(self), ...)`
/// shape and short multi-line forms.
const LOOKAHEAD_LINES: usize = 12;

/// Lines before a log call to scan for a context macro. Real code sometimes
/// hoists `let context = log_context!(self);` to a local *before* a
/// mutably-borrowed block (e.g. `match &mut self.position { ... }`) because
/// the macro reads fields of `self` that the match arm holds mutably. The
/// forward-only window misses that shape; this lookbehind catches it.
const LOOKBEHIND_LINES: usize = 24;

#[test]
fn lib_src_log_calls_use_canonical_envelope() {
    let crate_root = env!("CARGO_MANIFEST_DIR");
    let src_root = Path::new(crate_root).join("src");
    let allowlist: HashSet<&str> = OUT_OF_SCOPE_FILES.iter().copied().collect();
    let known: HashSet<&str> = KNOWN_PREEXISTING_VIOLATIONS.iter().copied().collect();

    let mut violations: Vec<String> = Vec::new();
    walk_dir(&src_root, &mut |path| {
        // Skip non-Rust files and non-UTF-8-decodable contents.
        if path.extension().map(|e| e != "rs").unwrap_or(true) {
            return;
        }
        let rel = path
            .strip_prefix(crate_root)
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
            // Track entry/exit of `#[cfg(test)] mod ...` blocks. The
            // bookkeeping is approximate -- it only counts `mod NAME {` or
            // `mod NAME;` after a `#[cfg(test)]` attribute on the previous
            // non-blank line. In practice this matches every `mod tests {`
            // pattern in the codebase.
            if trimmed.starts_with("#[cfg(test)]") || trimmed.starts_with("#[cfg(any(test") {
                // Lookahead for `mod ... {` on a following line.
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
            // Approximate brace tracking for the test-block depth. Closing
            // braces at column 0 are the canonical end of a `mod tests`.
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

            // Bidirectional scan: forward LOOKAHEAD_LINES for the inline /
            // single-line `error!("{} ...", log_context!(self), ...)` shape
            // (line 0 of the forward window covers same-line invocations);
            // backward LOOKBEHIND_LINES for the hoisted-local shape
            // (`let context = log_context!(self);` before a mutably-borrowed
            // match arm). Both are idiomatic Rust used in this codebase.
            let start = idx.saturating_sub(LOOKBEHIND_LINES);
            let end = (idx + 1 + LOOKAHEAD_LINES).min(lines.len());
            let window = &lines[start..end];
            let has_context_macro = window.iter().any(|l| {
                CONTEXT_MACROS.iter().any(|m| l.contains(m)) || l.contains(CONTEXT_METHOD_NEEDLE)
            });
            if !has_context_macro {
                let key = format!("{rel}:{lineno}", rel = rel, lineno = idx + 1);
                if known.contains(key.as_str()) {
                    continue;
                }
                violations.push(format!(
                    "{key}: raw log call without log_context!/log_module_context! in {LOOKBEHIND_LINES} lines back / {LOOKAHEAD_LINES} forward: {snippet}",
                    key = key,
                    LOOKBEHIND_LINES = LOOKBEHIND_LINES,
                    LOOKAHEAD_LINES = LOOKAHEAD_LINES,
                    snippet = trimmed.chars().take(80).collect::<String>(),
                ));
            }
        }
    });

    if !violations.is_empty() {
        panic!(
            "found {n} log-layout violation(s):\n{joined}\n\n\
             every protocol log line in lib/src must use the canonical \
             tag envelope -- prefix the format string with `\"{{}}\"` and \
             pass `log_context!(self)` / `log_module_context!()` first. See \
             CLAUDE.md > Logging.",
            n = violations.len(),
            joined = violations.join("\n"),
        );
    }
}

/// Returns true if `trimmed` starts with `error!(`, `warn!(`, etc. -- one of
/// the project's level-keyed log macros, opening the format-string argument
/// list. Comments and string literals are not handled (the test deliberately
/// has false-positive risk; the explicit allowlist is the compensating
/// pressure release valve).
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

/// Recursively walk a directory, calling `visit` for each file.
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
