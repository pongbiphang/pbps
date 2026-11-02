use super::*;

struct Dir(std::path::PathBuf);
impl Dir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("pbps-repin-{:x}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn manifest(&self) -> std::path::PathBuf {
        let path = self.0.join("pbps-tests.yml");
        std::fs::write(&path, "old\n").unwrap();
        path
    }
    fn leftovers(&self) -> Vec<String> {
        std::fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "pbps-tests.yml")
            .collect()
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ =
            std::fs::set_permissions(&self.0, std::os::unix::fs::PermissionsExt::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn changed() -> Vec<Refusal> {
    vec![Refusal::Repin("an input moved".into())]
}

#[test]
#[cfg(unix)]
fn a_replacement_publishes_whole_bytes_keeps_the_mode_and_leaves_no_temporary() {
    let dir = Dir::new();
    let path = dir.manifest();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o640)).unwrap();
    replace(&path, b"new\n", || Ok(())).unwrap_or_else(|_| panic!("replace failed"));
    assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o640);
    assert!(dir.leftovers().is_empty(), "{:?}", dir.leftovers());
}

#[test]
fn a_changed_input_at_the_last_check_leaves_the_manifest_and_no_temporary() {
    let dir = Dir::new();
    let path = dir.manifest();
    let blocked = replace(&path, b"new\n", || Err(changed()));
    assert!(matches!(blocked, Err(Blocked::Input(_))));
    assert_eq!(std::fs::read(&path).unwrap(), b"old\n");
    assert!(dir.leftovers().is_empty(), "{:?}", dir.leftovers());
}

#[test]
#[cfg(unix)]
fn an_unwritable_directory_refuses_without_touching_the_manifest() {
    let dir = Dir::new();
    let path = dir.manifest();
    std::fs::set_permissions(&dir.0, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
    // A superuser is not stopped by the mode; the property needs an ordinary user.
    if std::fs::File::create(dir.0.join("probe")).is_ok() {
        return;
    }
    let blocked = replace(&path, b"new\n", || Ok(()));
    assert!(matches!(blocked, Err(Blocked::Write(_))));
    assert_eq!(std::fs::read(&path).unwrap(), b"old\n");
}

#[test]
fn a_missing_target_refuses_and_the_check_never_runs() {
    let dir = Dir::new();
    let path = dir.0.join("absent.yml");
    let blocked = replace(&path, b"new\n", || panic!("the check ran"));
    assert!(matches!(blocked, Err(Blocked::Write(_))));
    assert!(dir.leftovers().is_empty(), "{:?}", dir.leftovers());
}

fn passed(name: &str) -> Phase {
    let mut p = Phase::new(name, "x");
    p.outcome = Status::Passed;
    p
}

fn finished() -> TestData {
    let mut data = TestData::for_repin();
    data.cleanup_confirmed = true;
    data.inputs_reverified = true;
    data.phases = vec![passed("input"), passed("compare_rows"), passed("cleanup")];
    data
}

#[test]
fn the_pin_moves_only_when_every_phase_passed_and_cleanup_is_confirmed() {
    assert!(may_publish(&finished()));
    let mut data = finished();
    data.cleanup_confirmed = false;
    assert!(!may_publish(&data), "unconfirmed cleanup");
    let mut data = finished();
    data.inputs_reverified = false;
    assert!(!may_publish(&data), "inputs not reverified");
    for outcome in [Status::Failed, Status::Unanswerable, Status::Skipped] {
        let mut data = finished();
        data.phases[1].outcome = outcome;
        assert!(!may_publish(&data), "{outcome:?}");
    }
    let mut data = finished();
    data.phases.push(Phase::new("manifest_write", "m"));
    assert!(
        may_publish(&data),
        "the write phase itself is not a precondition"
    );
}

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
