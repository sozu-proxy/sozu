//! Static log-layout regression guard.
//!
//! Walks `lib/src/**/*.rs` and asserts that every `error!`/`warn!`/`info!`/
//! `debug!`/`trace!` call site either:
//!
//! 1. has a `log_context*!()` macro invocation OR a `.log_context()`
//!    method call within a 24-back / 12-forward bidirectional window, OR
//! 2. lives under `#[cfg(test)] mod ...`, OR
//! 3. lives in a file on `OUT_OF_SCOPE_FILES`.
//!
//! Catches drift from the "every protocol log line carries a load-bearing
//! tag" rule encoded in `CLAUDE.md > Logging`.
//!
//! Scan logic is shared with `lib/build.rs` via `include!("../log_scanner.rs")`
//! so the test gate and the cargo:warning emitter cannot diverge.

include!("../log_scanner.rs");

/// Pre-existing untagged log sites that the codebase deliberately has NOT
/// rewritten yet. The list is the pressure-release valve that lets a
/// bulk-conversion PR retire entries incrementally without forcing the
/// entire surface to be wrapped in one go: the build.rs `cargo:warning=`
/// emitter still surfaces every entry on every `cargo build`, keeping the
/// queue visible even while the test stays green.
///
/// Currently empty — every historical pre-existing violation has been
/// wrapped at source (commits `e346d656`, `760089ca`, `7632768c`,
/// `a7c382c4`, `881e9316`, `fe84345e`, `361e7473`, `55609f37`, `ff659ab6`)
/// or auto-cleared by the bidirectional walker introduced in `361e7473`.
const KNOWN_PREEXISTING_VIOLATIONS: &[&str] = &[];

#[test]
fn lib_src_log_calls_use_canonical_envelope() {
    let crate_root = env!("CARGO_MANIFEST_DIR");
    let known: HashSet<&str> = KNOWN_PREEXISTING_VIOLATIONS.iter().copied().collect();

    let mut violations: Vec<String> = Vec::new();
    scan_violations(crate_root, &mut |rel, lineno, snippet| {
        let key = format!("{rel}:{lineno}");
        if known.contains(key.as_str()) {
            return;
        }
        let snippet: String = snippet.chars().take(80).collect();
        violations.push(format!(
            "{key}: raw log call without log_context!/log_module_context! in {LOOKBEHIND_LINES} lines back / {LOOKAHEAD_LINES} forward: {snippet}"
        ));
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
