// Broken fixture for the Rust-comment line-number rule. Every citation below
// must be reported: each names a line in a file this fixture tree really has,
// which is exactly the form the rule forbids.

/// Repo-root-relative, the shape sozu-proxy/sozu#1466 and #1473 both found on
/// main: `sample.rs:3` resolves, is not blank, reads like a real place in the
/// file, and rots the moment anything above line 3 moves.
pub struct RootRelative;

/// A RANGE and a GROUP, so the span forms `CITATION` carries are reported here
/// too and not only the single-line one: `sample.rs:6-8` is one citation and
/// `sample.rs:7/8` is another.
pub struct Spans;

/// The historical witness, exempt under the fixture table and reported under
/// the default one: `drift.rs:4`. Without a fixture that MOVES when the table
/// changes, HISTORICAL_CITATIONS is a constant no assertion can reach.
pub struct Historical;
