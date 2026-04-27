//! Compile-time log-layout regression watcher.
//!
//! Runs the scan from `log_scanner.rs` at build time and emits
//! `cargo:warning=` lines for each raw log call missing the canonical
//! envelope. Non-fatal by design — any cosmetic shift in macro authoring
//! would otherwise tank `cargo build` for unrelated work. The integration
//! test at `tests/log_layout.rs` is the gating check; this watcher is the
//! always-visible terminal feedback.

include!("log_scanner.rs");

fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=log_scanner.rs");

    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    scan_violations(&manifest_dir, &mut |rel, lineno, _snippet| {
        println!(
            "cargo:warning={rel}:{lineno}: raw log call missing log_context!/log_module_context! envelope (lookbehind {LOOKBEHIND_LINES} / lookahead {LOOKAHEAD_LINES} lines)"
        );
    });
}
