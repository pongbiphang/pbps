use super::{declared_path, forget_observations, runs};

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

#[test]
fn a_second_scratch_starts_without_the_first_ones_observations() {
    let mut data = crate::plan_test::report::TestData::for_repin();
    data.run_id = Some("run-1".into());
    data.acquired_image = Some("image".into());
    data.engine_build = Some("PostgreSQL 16".into());
    forget_observations(&mut data);
    assert!(data.run_id.is_none() && data.acquired_image.is_none() && data.engine_build.is_none());
}
