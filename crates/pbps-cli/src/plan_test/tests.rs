use super::*;
use sha2::Digest as _;
use std::path::PathBuf;

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";
const IMAGE: &str =
    "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280";
/// SHA-256 of zero bytes.
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("pbps-plan-test-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(dir.join("project/tests")).unwrap();
        Self(dir)
    }
    fn root(&self) -> PathBuf {
        self.0.join("project")
    }
    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    fn manifest(&self) -> PathBuf {
        self.root().join("tests/pbps-tests.yml")
    }
    /// A manifest with the given `fixtures:` and `assertions:` bodies.
    fn declare(&self, fixtures: &str, assertions: &str) {
        self.write(
            "tests/pbps-tests.yml",
            format!(
                "version: 1\ndialect: postgres\nbase: {BASE}\nengine:\n  image: {IMAGE}\n\
                 fixtures: {fixtures}\nassertions: {assertions}\n"
            ),
        );
    }
    /// One fixture and one assertion, both present.
    fn standard(&self) -> Frozen {
        self.write("tests/f.sql", "INSERT INTO t VALUES (1);\n");
        self.write("tests/a.sql", "SELECT 1 WHERE false\n");
        self.declare(
            &format!("\n  - {{name: f, file: f.sql, at: {BASE}}}"),
            "\n  - {name: a, file: a.sql}",
        );
        freeze(&self.root(), &self.manifest()).unwrap()
    }
    fn freeze(&self) -> Result<Frozen, Vec<Refusal>> {
        freeze(&self.root(), &self.manifest())
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn refusals(result: Result<Frozen, Vec<Refusal>>) -> Vec<Refusal> {
    match result {
        Ok(_) => panic!("expected refusals"),
        Err(refusals) => refusals,
    }
}

fn problem(role: Role, name: &str, problem: Problem) -> impl Fn(&Refusal) -> bool {
    let name = name.to_owned();
    move |r| {
        matches!(r, Refusal::Input { role: got, name: Some(n), problem: p, .. }
            if *got == role && *n == name && *p == problem)
    }
}

#[test]
fn the_manifest_and_its_files_freeze_with_the_hashes_of_their_bytes() {
    let project = Project::new();
    let frozen = project.standard();
    let sha = |bytes: &[u8]| format!("{:x}", sha2::Sha256::digest(bytes));
    assert_eq!(frozen.manifest.base.as_str(), BASE);
    assert_eq!(
        (
            frozen.fixtures[0].name.as_str(),
            frozen.assertions[0].name.as_str()
        ),
        ("f", "a")
    );
    assert_eq!(frozen.fixtures[0].text, "INSERT INTO t VALUES (1);\n");
    assert_eq!(
        frozen.fixtures[0].file.sha256,
        sha(b"INSERT INTO t VALUES (1);\n")
    );
    assert_eq!(
        frozen.assertions[0].file.sha256,
        sha(b"SELECT 1 WHERE false\n")
    );
    let manifest = std::fs::read(project.manifest()).unwrap();
    assert_eq!(frozen.manifest_file.sha256, sha(&manifest));
    assert_eq!(
        frozen
            .consumed()
            .iter()
            .map(|f| (f.path.as_str(), f.role))
            .collect::<Vec<_>>(),
        [
            ("tests/a.sql", Role::Assertion),
            ("tests/f.sql", Role::Fixture),
            ("tests/pbps-tests.yml", Role::Manifest),
        ],
        "sorted by project path"
    );
}

#[test]
fn bytes_are_hashed_as_written_without_newline_normalisation() {
    let project = Project::new();
    project.write("tests/lf.sql", "SELECT 1\n");
    project.write("tests/crlf.sql", "SELECT 1\r\n");
    project.declare(
        "[]",
        "\n  - {name: lf, file: lf.sql}\n  - {name: crlf, file: crlf.sql}",
    );
    let frozen = project.freeze().unwrap();
    assert_ne!(
        frozen.assertions[0].file.sha256,
        frozen.assertions[1].file.sha256
    );
    assert_eq!(frozen.assertions[1].text, "SELECT 1\r\n");
}

#[test]
fn an_empty_file_is_an_empty_file_and_a_missing_file_is_not() {
    let project = Project::new();
    project.write("tests/empty.sql", "");
    project.declare(
        "[]",
        "\n  - {name: e, file: empty.sql}\n  - {name: gone, file: gone.sql}",
    );
    let found = refusals(project.freeze());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(problem(Role::Assertion, "gone", Problem::Missing)(
        &found[0]
    ));
    project.declare("[]", "\n  - {name: e, file: empty.sql}");
    assert_eq!(project.freeze().unwrap().assertions[0].file.sha256, EMPTY);
}

#[test]
fn missing_directory_and_non_utf8_are_three_different_refusals() {
    let project = Project::new();
    project.write("tests/dir/keep", "x");
    project.write("tests/binary.sql", [0xff, 0xfe, 0x00]);
    project.declare(
        "[]",
        "\n  - {name: gone, file: gone.sql}\n  - {name: dir, file: dir}\n  - {name: bin, file: binary.sql}",
    );
    let found = refusals(project.freeze());
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(problem(Role::Assertion, "gone", Problem::Missing)(
        &found[0]
    ));
    assert!(problem(Role::Assertion, "dir", Problem::NotRegular)(
        &found[1]
    ));
    assert!(problem(Role::Assertion, "bin", Problem::NotUtf8)(&found[2]));
    // A file that exists but cannot be opened is a fourth fact, never "missing".
    assert_eq!(found[0].outcome(), Outcome::Finding);
    assert_eq!(
        Refusal::Input {
            role: Role::Assertion,
            name: None,
            path: "x".into(),
            problem: Problem::Unreadable("denied".into())
        }
        .outcome(),
        Outcome::Unanswerable
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_is_unreadable_not_missing() {
    use std::os::unix::fs::PermissionsExt as _;
    let project = Project::new();
    project.write("tests/secret.sql", "SELECT 1");
    let path = project.root().join("tests/secret.sql");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&path).is_ok() {
        return; // running as a user who ignores modes
    }
    project.declare("[]", "\n  - {name: s, file: secret.sql}");
    let found = refusals(project.freeze());
    assert!(
        matches!(
            &found[..],
            [Refusal::Input {
                problem: Problem::Unreadable(_),
                ..
            }]
        ),
        "{found:?}"
    );
    assert_eq!(found[0].outcome(), Outcome::Unanswerable);
}

#[test]
fn a_path_may_climb_but_not_out_of_the_project() {
    let project = Project::new();
    project.write("fixtures/f.sql", "INSERT INTO t VALUES (1)");
    project.write("../outside.sql", "SELECT 1");
    project.write("tests/a.sql", "SELECT 1");
    project.declare(
        &format!("\n  - {{name: in, file: ../fixtures/f.sql, at: {BASE}}}"),
        "\n  - {name: a, file: a.sql}",
    );
    assert_eq!(
        project.freeze().unwrap().fixtures[0].file.path,
        "fixtures/f.sql"
    );
    project.declare("[]", "\n  - {name: out, file: ../../outside.sql}");
    let found = refusals(project.freeze());
    assert!(
        problem(Role::Assertion, "out", Problem::OutsideProject)(&found[0]),
        "{found:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_leaving_the_project_is_refused_and_one_staying_inside_is_read_through() {
    let project = Project::new();
    project.write("../elsewhere.sql", "SELECT 'outside'");
    project.write("tests/real.sql", "SELECT 'inside'");
    std::os::unix::fs::symlink(
        project.0.join("elsewhere.sql"),
        project.root().join("tests/escape.sql"),
    )
    .unwrap();
    std::os::unix::fs::symlink("real.sql", project.root().join("tests/alias.sql")).unwrap();
    project.declare(
        "[]",
        "\n  - {name: esc, file: escape.sql}\n  - {name: ok, file: alias.sql}",
    );
    let found = refusals(project.freeze());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(problem(Role::Assertion, "esc", Problem::OutsideProject)(
        &found[0]
    ));
    project.declare("[]", "\n  - {name: ok, file: alias.sql}");
    let frozen = project.freeze().unwrap();
    assert_eq!(frozen.assertions[0].text, "SELECT 'inside'");
    assert_eq!(
        frozen.assertions[0].file.path, "tests/real.sql",
        "named by what it resolves to"
    );
}

#[test]
fn one_file_spelled_two_ways_is_refused_within_a_list_and_allowed_across_lists() {
    let project = Project::new();
    project.write("tests/a.sql", "SELECT 1");
    project.declare(
        "[]",
        "\n  - {name: one, file: a.sql}\n  - {name: two, file: ./sub/../a.sql}",
    );
    project.write("tests/sub/keep", "");
    let found = refusals(project.freeze());
    assert!(
        matches!(&found[..], [Refusal::DuplicateFile { first, second, path, .. }]
            if first == "one" && second == "two" && path == "tests/a.sql"),
        "{found:?}"
    );
    project.declare(
        &format!("\n  - {{name: f, file: a.sql, at: {BASE}}}"),
        "\n  - {name: a, file: a.sql}",
    );
    assert!(
        project.freeze().is_ok(),
        "a fixture and an assertion may share a file"
    );
}

#[test]
fn every_refusal_is_reported_in_one_pass() {
    let project = Project::new();
    project.write("tests/bin.sql", [0xff]);
    project.write("tests/a.sql", "SELECT 1");
    project.declare(
        &format!("\n  - {{name: f, file: gone.sql, at: {BASE}}}"),
        "\n  - {name: bin, file: bin.sql}\n  - {name: a, file: a.sql}\n  - {name: a2, file: a.sql}",
    );
    let found = refusals(project.freeze());
    assert_eq!(found.len(), 3, "{found:?}");
}

#[test]
fn a_manifest_that_is_missing_outside_the_project_or_not_text_is_a_refusal_of_the_manifest() {
    let project = Project::new();
    let found = refusals(project.freeze());
    assert!(matches!(
        &found[..],
        [Refusal::Input {
            role: Role::Manifest,
            problem: Problem::Missing,
            ..
        }]
    ));
    project.write("tests/pbps-tests.yml", [0xff, 0xfe]);
    let found = refusals(project.freeze());
    assert!(matches!(
        &found[..],
        [Refusal::Input {
            role: Role::Manifest,
            problem: Problem::NotUtf8,
            ..
        }]
    ));
    project.write("../other.yml", "version: 1\n");
    let found = refusals(freeze(&project.root(), &project.0.join("other.yml")));
    assert!(matches!(
        &found[..],
        [Refusal::Input {
            role: Role::Manifest,
            problem: Problem::OutsideProject,
            ..
        }]
    ));
}

#[test]
fn manifest_errors_surface_typed_and_only_an_unknown_version_cannot_be_answered() {
    let project = Project::new();
    project.write("tests/pbps-tests.yml", "version: 9\nanything: goes\n");
    let found = refusals(project.freeze());
    assert!(matches!(
        &found[..],
        [Refusal::Manifest(ManifestError::UnsupportedVersion(_))]
    ));
    assert_eq!(found[0].outcome(), Outcome::Unanswerable);
    project.write("tests/pbps-tests.yml", "version: 1\nsurprise: 1\n");
    let found = refusals(project.freeze());
    assert!(matches!(
        &found[..],
        [Refusal::Manifest(ManifestError::Syntax(_))]
    ));
    assert_eq!(found[0].outcome(), Outcome::Finding);
}

#[test]
fn the_text_a_run_uses_is_the_text_that_was_hashed_even_if_the_file_changes_later() {
    let project = Project::new();
    let frozen = project.standard();
    project.write("tests/f.sql", "DELETE FROM t;\n");
    assert_eq!(frozen.fixtures[0].text, "INSERT INTO t VALUES (1);\n");
}

#[test]
fn reverify_passes_when_nothing_moved() {
    let project = Project::new();
    project.standard().reverify().unwrap();
}

#[test]
fn reverify_names_an_edited_a_deleted_and_a_replaced_input_separately() {
    let project = Project::new();
    let frozen = project.standard();
    project.write("tests/f.sql", "DELETE FROM t;\n");
    std::fs::remove_file(project.root().join("tests/a.sql")).unwrap();
    std::fs::create_dir(project.root().join("tests/a.sql")).unwrap();
    project.write(
        "tests/pbps-tests.yml",
        format!(
            "{}# edited\n",
            std::fs::read_to_string(project.manifest()).unwrap()
        ),
    );
    let found = frozen.reverify().unwrap_err();
    let change = |path: &str| {
        found
            .iter()
            .find_map(|r| match r {
                Refusal::Changed { path: p, change } if p == path => Some(change.clone()),
                Refusal::Changed { .. }
                | Refusal::Manifest(_)
                | Refusal::Input { .. }
                | Refusal::DuplicateFile { .. } => None,
            })
            .unwrap_or_else(|| panic!("no refusal for {path}: {found:?}"))
    };
    assert_eq!(change("tests/f.sql"), Change::Edited);
    assert_eq!(change("tests/a.sql"), Change::Unusable(Problem::NotRegular));
    assert_eq!(change("tests/pbps-tests.yml"), Change::Edited);
    assert!(found.iter().all(|r| r.outcome() == Outcome::Unanswerable));
    std::fs::remove_dir(project.root().join("tests/a.sql")).unwrap();
    let found = frozen.reverify().unwrap_err();
    assert!(found.iter().any(|r| matches!(r, Refusal::Changed { path, change: Change::Unusable(Problem::Missing) } if path == "tests/a.sql")));
}

#[cfg(unix)]
#[test]
fn reverify_notices_a_symlink_retargeted_to_a_file_with_the_same_bytes() {
    let project = Project::new();
    project.write("tests/one.sql", "SELECT 1");
    project.write("tests/two.sql", "SELECT 1");
    let link = project.root().join("tests/link.sql");
    std::os::unix::fs::symlink("one.sql", &link).unwrap();
    project.declare("[]", "\n  - {name: l, file: link.sql}");
    let frozen = project.freeze().unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("two.sql", &link).unwrap();
    let found = frozen.reverify().unwrap_err();
    assert!(
        matches!(
            &found[..],
            [Refusal::Changed {
                change: Change::Retargeted,
                ..
            }]
        ),
        "{found:?}"
    );
}

#[test]
fn refusal_messages_show_user_text_inert() {
    let shown = Refusal::Input {
        role: Role::Fixture,
        name: Some("a\nb".into()),
        path: "x\u{202e}y.sql".into(),
        problem: Problem::Missing,
    }
    .to_string();
    assert!(
        !shown.contains('\n') && !shown.contains('\u{202e}'),
        "{shown}"
    );
}

#[test]
fn the_project_path_is_slash_separated_and_never_absolute() {
    let project = Project::new();
    let frozen = project.standard();
    for file in frozen.consumed() {
        assert!(
            !file.path.starts_with('/') && !file.path.contains('\\'),
            "{file:?}"
        );
    }
}
