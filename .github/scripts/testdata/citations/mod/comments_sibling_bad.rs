// Broken fixture for the SIBLING half of the Rust-comment rule, and the
// discriminator that separates this rule from the finder grep in
// sozu-proxy/sozu#1473.

/// `h2.rs:2` carries no directory, so a grep keyed on a `lib/src/`-style
/// prefix walks straight past it — and it still names a line, because a cited
/// path binds to the citing file's OWN directory first. It must be reported
/// against `mod/h2.rs` (11 lines) and never the root `h2.rs` (3 lines): the
/// two resolve to different files, so only the target text separates them.
pub struct Sibling;
