// Fixture source for the dead-test-name half of the self-test. Not compiled:
// it lives under `testdata/`, which the checker skips when it walks the real
// tree. Nothing in THIS block is examined -- it carries no identifier long
// enough to be a candidate.

/// The citation `a_fixture_test_cited_by_the_name_it_still_carries` resolves,
/// because the test right below carries exactly that name.
#[test]
fn a_fixture_test_cited_by_the_name_it_still_carries() {}

/// Shape filter. This test block also names `max_connections_per_ip`, which
/// carries no function either -- but four underscore-separated segments is a
/// noun phrase, a configuration key or a struct field, not a sentence, so it
/// is never examined and the word above does not drag it in.
fn shape_filter_witness() {}

// Context filter. `a_sentence_shaped_identifier_naming_no_function_at_all` is
// sentence-shaped and would be a candidate anywhere else; this comment block
// never says the word, so the identifier is not examined here.
fn context_filter_witness() {}
