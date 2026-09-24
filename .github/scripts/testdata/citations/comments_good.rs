// Clean fixture for the Rust-comment line-number rule: the form it asks for,
// and the one thing it deliberately leaves alone.

/// The form the rule requires. `Sample::readable` (`sample.rs`) names the item
/// and carries no number, so no edit above it can move it.
pub struct SymbolForm;

/// A citation into a pinned external dependency names no file in this tree, so
/// this repository's churn cannot move it and this rule is not its owner:
/// `kawa/src/protocol/h1/parser/primitives.rs:194-203` is left alone, and the
/// abbreviated `.../h1/parser/mod.rs:249-258` beside it too.
pub struct External;
