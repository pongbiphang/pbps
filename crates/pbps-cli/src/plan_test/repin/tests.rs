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

#[test]
fn a_replaced_manifest_is_never_reported_as_not_moved() {
    let fresh = || ManifestWrite {
        status: "not_attempted".into(),
        old_sha256: "old".into(),
        new_sha256: None,
        reason: None,
    };
    let mut m = fresh();
    let (_, message) = record_blocked(Blocked::Write(anyhow::anyhow!("no space")), "new", &mut m);
    assert_eq!(
        (m.status.as_str(), m.new_sha256.as_deref()),
        ("failed", None)
    );
    assert!(message.starts_with("the pin was not moved"), "{message}");
    // Replaced and read back, but not synced: the pin did move.
    let mut m = fresh();
    let replaced = Blocked::Replaced {
        error: anyhow::anyhow!("sync failed"),
        confirmed: true,
    };
    let (_, message) = record_blocked(replaced, "new", &mut m);
    assert_eq!(
        (m.status.as_str(), m.new_sha256.as_deref()),
        ("written", Some("new"))
    );
    assert!(
        message.contains("was replaced") && !message.contains("not moved"),
        "{message}"
    );
    // Replaced but not read back: the bytes are unknown, and the report says so.
    let mut m = fresh();
    let replaced = Blocked::Replaced {
        error: anyhow::anyhow!("unreadable"),
        confirmed: false,
    };
    let (_, message) = record_blocked(replaced, "new", &mut m);
    assert_eq!(
        (m.status.as_str(), m.new_sha256.as_deref()),
        ("failed", None)
    );
    assert!(
        message.contains("was replaced") && !message.contains("not moved"),
        "{message}"
    );
}

#[cfg(unix)]
#[test]
fn a_directory_that_cannot_be_opened_refuses_before_anything_is_replaced() {
    let dir = Dir::new();
    let path = dir.manifest();
    // Write and search, but no read: files can be replaced, the directory cannot be opened.
    std::fs::set_permissions(&dir.0, std::os::unix::fs::PermissionsExt::from_mode(0o300)).unwrap();
    if std::fs::File::open(&dir.0).is_ok() {
        std::fs::set_permissions(&dir.0, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        return;
    }
    let blocked = replace(&path, b"new\n", || Ok(()));
    std::fs::set_permissions(&dir.0, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    assert!(matches!(blocked, Err(Blocked::Write(_))));
    assert_eq!(std::fs::read(&path).unwrap(), b"old\n");
    assert!(dir.leftovers().is_empty());
}

const FIXTURE_SPELLINGS: &[(&str, &str)] = &[
    ("orders", "orders"),
    ("orders\n\u{1b}[31m", "orders\\n\\u{1b}[31m"),
    (r"orders\n", r"orders\\n"),
];

impl Dir {
    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn git(&self, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.0)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn commit(&self) -> CommitId {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", "fixture"]);
        CommitId::parse(&self.git(&["rev-parse", "HEAD"])).unwrap()
    }
}

struct RepinRepo {
    dir: Dir,
    pin: CommitId,
    base: CommitId,
}

impl RepinRepo {
    fn new() -> Self {
        let dir = Dir::new();
        dir.git(&["init", "-q"]);
        dir.write("pbps.yml", "dialect: postgres\n");
        // Keep the empty declarations directory in both committed revisions.
        dir.write("schema/keep", "");
        let pin = dir.commit();
        let base = dir.commit();
        Self { dir, pin, base }
    }

    fn manifest(&self, stem: &str, fixtures: &[(&str, &str)]) -> std::path::PathBuf {
        let fixtures = fixtures
            .iter()
            .map(|(name, file)| {
                self.dir.write(&format!("tests/{file}"), "SELECT 1;\n");
                format!(
                    "\n  - {{name: {}, file: {}, at: {}}}",
                    serde_json::to_string(name).unwrap(),
                    serde_json::to_string(file).unwrap(),
                    self.pin
                )
            })
            .collect::<String>();
        self.dir
            .write("tests/assert.sql", "SELECT 1 WHERE false;\n");
        let path = format!("tests/{stem}.yml");
        self.dir.write(
            &path,
            format!(
                "version: 1\ndialect: postgres\nbase: {}\nengine:\n  image: \
                 postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280\n\
                 fixtures: {}\nassertions:\n  - {{name: a, file: assert.sql}}\n",
                self.base,
                if fixtures.is_empty() { "[]" } else { &fixtures }
            ),
        );
        self.dir.0.join(path)
    }

    fn baseline_refusals(&self, manifest: &Path) -> Vec<Refusal> {
        let inputs = super::super::validate_inputs(&self.dir.0, manifest)
            .unwrap_or_else(|r| panic!("invalid test inputs: {r:?}"));
        let head = CommitId::parse(&self.dir.git(&["rev-parse", "HEAD"])).unwrap();
        baseline(&inputs, &head, 0, &std::fs::read(manifest).unwrap())
            .expect_err("expected baseline refusals")
    }
}

fn assert_repin_refusals(found: Vec<Refusal>, expected: &[String]) {
    assert_eq!(
        found,
        expected
            .iter()
            .cloned()
            .map(Refusal::Repin)
            .collect::<Vec<_>>()
    );
    for refusal in found {
        let message = refusal.to_string();
        assert!(!message.chars().any(char::is_control), "{message:?}");
    }
}

#[test]
fn unknown_fixture_refusals_escape_the_requested_name_and_every_known_name() {
    for &(name, shown) in FIXTURE_SPELLINGS {
        let repo = RepinRepo::new();
        let manifest = repo.manifest("manifest", &[(name, "f.sql"), ("other", "other.sql")]);
        let requested = format!("missing-{name}");
        let found = validate(
            &repo.dir.0,
            &manifest,
            &requested,
            repo.base.as_str(),
            &mut TestData::for_repin(),
        )
        .err()
        .expect("expected an unknown fixture");
        assert_repin_refusals(
            found,
            &[format!(
                "the manifest has no fixture named `missing-{shown}`; it has `{shown}`, `other`"
            )],
        );

        let manifest = repo.manifest("manifest", &[]);
        let found = validate(
            &repo.dir.0,
            &manifest,
            &requested,
            repo.base.as_str(),
            &mut TestData::for_repin(),
        )
        .err()
        .expect("expected an empty fixture list");
        assert_repin_refusals(
            found,
            &[format!(
                "the manifest has no fixture named `missing-{shown}`; it has none"
            )],
        );
    }
}

#[test]
fn already_pinned_refusals_escape_the_fixture_name() {
    for &(name, shown) in FIXTURE_SPELLINGS {
        let repo = RepinRepo::new();
        let manifest = repo.manifest("manifest", &[(name, "f.sql")]);
        repo.dir.commit();
        let found = validate(
            &repo.dir.0,
            &manifest,
            name,
            repo.pin.as_str(),
            &mut TestData::for_repin(),
        )
        .err()
        .expect("expected an unchanged pin");
        assert_repin_refusals(
            found,
            &[format!(
                "fixture `{shown}` is already pinned at {}",
                repo.pin
            )],
        );
    }
}

#[test]
fn baseline_refusals_escape_fixture_labels_and_fixture_and_manifest_paths() {
    for &(name, shown) in FIXTURE_SPELLINGS {
        // Windows file names cannot hold the controls, but fixture names can.
        let (stem, shown_stem) = if cfg!(unix) {
            (name, shown)
        } else {
            ("f", "f")
        };
        let repo = RepinRepo::new();
        let file = format!("{stem}.sql");
        let manifest = repo.manifest(stem, &[("selected", "selected.sql"), (name, &file)]);
        let labels = [
            format!("the manifest (`tests/{shown_stem}.yml`)"),
            "fixture `selected` (`tests/selected.sql`)".into(),
            format!("fixture `{shown}` (`tests/{shown_stem}.sql`)"),
        ];
        assert_repin_refusals(
            repo.baseline_refusals(&manifest),
            &labels
                .iter()
                .map(|label| format!("{label} is not committed at HEAD; commit the original first"))
                .collect::<Vec<_>>(),
        );

        repo.dir.commit();
        repo.dir.write(&format!("tests/{file}"), "SELECT 2;\n");
        let mut bytes = std::fs::read(&manifest).unwrap();
        bytes.extend_from_slice(b"# edited\n");
        std::fs::write(&manifest, bytes).unwrap();
        assert_repin_refusals(
            repo.baseline_refusals(&manifest),
            &[&labels[0], &labels[2]]
                .into_iter()
                .map(|label| {
                    format!(
                        "{label} has uncommitted changes; a re-pin edits one pin and one \
                 fixture, so commit or revert them first"
                    )
                })
                .collect::<Vec<_>>(),
        );
    }
}

#[test]
fn invalid_utf8_baseline_refusals_escape_the_committed_fixture_path() {
    for &(name, shown) in FIXTURE_SPELLINGS {
        let (stem, shown_stem) = if cfg!(unix) {
            (name, shown)
        } else {
            ("f", "f")
        };
        let repo = RepinRepo::new();
        let file = format!("{stem}.sql");
        let manifest = repo.manifest("manifest", &[(name, &file)]);
        repo.dir.write(&format!("tests/{file}"), [0xff]);
        repo.dir.commit();
        repo.dir.write(&format!("tests/{file}"), "SELECT 1;\n");
        assert_repin_refusals(
            repo.baseline_refusals(&manifest),
            &[format!(
                "the committed `tests/{shown_stem}.sql` is not valid UTF-8"
            )],
        );
    }
}

#[cfg(unix)]
#[test]
fn linked_fixture_refusals_escape_the_name_and_both_paths() {
    for &(name, shown) in FIXTURE_SPELLINGS {
        let repo = RepinRepo::new();
        let declared = format!("declared-{name}.sql");
        let resolved = format!("resolved-{name}.sql");
        let manifest = repo.manifest("manifest", &[(name, &declared)]);
        repo.dir.write(&format!("tests/{resolved}"), "SELECT 1;\n");
        let link = repo.dir.0.join("tests").join(&declared);
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&resolved, &link).unwrap();
        repo.dir.commit();
        assert_repin_refusals(
            repo.baseline_refusals(&manifest),
            &[format!(
                "fixture `{shown}` is declared at `tests/declared-{shown}.sql` but resolves to \
                 `tests/resolved-{shown}.sql`; a re-pin needs a plain file there, not a link"
            )],
        );
    }
}

#[test]
fn repin_machine_identities_keep_raw_names_and_paths_while_human_messages_escape_them() {
    for &(name, shown) in FIXTURE_SPELLINGS {
        let (stem, shown_stem) = if cfg!(unix) {
            (name, shown)
        } else {
            ("f", "f")
        };
        let repo = RepinRepo::new();
        let file = format!("{stem}.sql");
        let manifest = repo.manifest(stem, &[(name, &file)]);
        repo.dir.commit();
        let mut data = TestData::for_repin();
        let plan = validate(&repo.dir.0, &manifest, name, repo.base.as_str(), &mut data)
            .unwrap_or_else(|r| panic!("valid re-pin refused: {r:?}"));
        data.input = Some(plan.inputs.identity.clone());
        let report = data.finish();
        let data = report.data.as_ref().unwrap();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["data"]["repin"]["fixture"], name);
        assert_eq!(json["data"]["repin"]["path"], format!("tests/{file}"));
        assert_eq!(json["data"]["input"]["pins"][0]["fixture"], name);
        assert_eq!(
            json["data"]["input"]["manifest"]["path"],
            format!("tests/{stem}.yml")
        );
        for side in ["original_inputs", "proposed_inputs"] {
            assert!(
                json["data"]["repin"][side]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| { f["role"] == "fixture" && f["path"] == format!("tests/{file}") })
            );
        }
        let summary = data.summary(true);
        assert_eq!(
            summary
                .iter()
                .find(|f| f.id == "test.repin")
                .unwrap()
                .message,
            format!(
                "fixture `{shown}` (tests/{shown_stem}.sql): {} -> {}",
                repo.pin, repo.base
            )
        );
        assert!(
            summary
                .iter()
                .all(|f| !f.message.chars().any(char::is_control))
        );
        assert_eq!(serde_json::to_value(&report).unwrap(), json);
    }
}
