use super::{declared_path, runs};

#[test]
fn the_second_side_runs_only_after_the_first_completed_and_was_cleaned() {
    assert!(runs(false, 0, false));
    assert!(runs(true, 1, true));
    // The first side did not complete, or its cleanup is unconfirmed.
    assert!(!runs(true, 0, true));
    assert!(!runs(true, 1, false));
    assert!(!runs(false, 1, true));
}

#[test]
fn a_declared_path_folds_dots_the_way_a_resolved_path_does() {
    assert_eq!(declared_path("tests", "a.sql"), "tests/a.sql");
    assert_eq!(declared_path("tests", "./sub/../a.sql"), "tests/a.sql");
    assert_eq!(declared_path("tests", "../shared/a.sql"), "shared/a.sql");
    assert_eq!(declared_path("", "a.sql"), "a.sql");
    assert_ne!(declared_path("tests", "a.sql"), "tests/b.sql");
}
