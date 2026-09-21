// Broken fixture for the dead-test-name rule. Every identifier below must be
// reported.

/// This test cites `a_fixture_test_that_was_renamed_or_never_landed` as its
/// evidence. No function in the fixture tree carries that name.
#[test]
fn a_fixture_test_citing_a_name_that_is_gone() {}

/// This test cites `a_fixture_test_renamed_to_a_name_that_is_also_gone`,
/// which the fixture rename table forwards to a name that is itself absent.
#[test]
fn a_fixture_test_citing_a_rename_whose_target_is_gone() {}
