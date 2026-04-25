//! Compile-time log-layout regression watcher.
//!
//! Runs the same scan as `lib/tests/log_layout.rs` but at build time and
//! emits `cargo:warning=` lines instead of failing the build. Keeps clean
//! checkouts quiet and surfaces regressions on every `cargo build` so the
//! signal lands in the developer's terminal even before they run the
//! integration test suite.
//!
//! Stays non-fatal by design: any cosmetic shift in macro authoring would
//! otherwise tank `cargo build` for unrelated work. The integration test
//! at `tests/log_layout.rs` is the gating check.

use std::{fs, path::Path};

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

const CONTEXT_MACROS: &[&str] = &[
    "log_context!",
    "log_context_lite!",
    "log_context_stream!",
    "log_module_context!",
    "log_socket_context!",
    "log_socket_module_prefix",
];

/// Lines after a `error!(`/`warn!(`/etc. opening to scan for a context macro.
const LOOKAHEAD_LINES: usize = 12;

/// Lines before a log call to scan for a context macro. Real code sometimes
/// hoists `let context = log_context!(self);` to a local *before* a
/// mutably-borrowed block (e.g. `match &mut self.position { ... }`) because
/// the macro reads fields of `self` that the match arm holds mutably. The
/// forward-only window misses that shape; this lookbehind window catches it.
const LOOKBEHIND_LINES: usize = 24;

fn main() {
    // Only re-run when source files change. Keeps incremental rebuilds fast.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = match std::env::var("CARGO_MANIFEST_DIR") {
        Ok(v) => v,
        Err(_) => return,
    };
    let src_root = Path::new(&manifest_dir).join("src");
    let allowlist: std::collections::HashSet<&str> = OUT_OF_SCOPE_FILES.iter().copied().collect();

    walk_dir(&src_root, &mut |path| {
        if path.extension().map(|e| e != "rs").unwrap_or(true) {
            return;
        }
        let rel = path
            .strip_prefix(&manifest_dir)
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
            // Bidirectional scan: forward `LOOKAHEAD_LINES` for the
            // single-line / single-arg form, backward `LOOKBEHIND_LINES`
            // for the hoisted-local form (`let context = log_context!(self);`
            // before a mutably-borrowed match).
            let start = idx.saturating_sub(LOOKBEHIND_LINES);
            let end = (idx + 1 + LOOKAHEAD_LINES).min(lines.len());
            let window = &lines[start..end];
            let has_context_macro = window
                .iter()
                .any(|l| CONTEXT_MACROS.iter().any(|m| l.contains(m)));
            if !has_context_macro {
                println!(
                    "cargo:warning={rel}:{lineno}: raw log call missing log_context!/log_module_context! envelope (lookbehind {LOOKBEHIND_LINES} / lookahead {LOOKAHEAD_LINES} lines)",
                    rel = rel,
                    lineno = idx + 1,
                    LOOKBEHIND_LINES = LOOKBEHIND_LINES,
                    LOOKAHEAD_LINES = LOOKAHEAD_LINES,
                );
            }
        }
    });
}

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
