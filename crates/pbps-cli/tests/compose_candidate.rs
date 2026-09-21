//! Production capture against real Git and the ordinary CLI, with scheduled
//! editor changes. The publisher is deliberately still absent (#746).
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use pbps_ui::compose::{Candidates, CaptureBoundary, Config, Intent, Request};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");
const ORIGINAL: &str = "table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\n";
const RENAMED: &str = "table: dbo.t\ncolumns:\n  ident: {type: int, nullable: false}\n";

fn checked(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    checked(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "commit.gpgSign=false"])
            .args(args)
            .output()
            .unwrap(),
    )
}

struct Repository {
    root: PathBuf,
    project: PathBuf,
}

impl Repository {
    fn new(name: &str, sub: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pbps-candidate-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let project = root.join(sub);
        fs::create_dir_all(project.join("schema")).unwrap();
        fs::write(project.join("pbps.yml"), "dialect: mssql\n").unwrap();
        fs::write(project.join("schema/t.yml"), ORIGINAL).unwrap();
        git(&root, &["init", "-q", "-b", "master"]);
        git(&root, &["config", "user.name", "compose-test"]);
        git(&root, &["config", "user.email", "compose@example.test"]);
        git(&root, &["config", "commit.gpgSign", "false"]);
        let repo = Self { root, project };
        checked(repo.cli(&["plan"]));
        repo.commit();
        // Capture does not contact the remote. This is a real, contained local
        // endpoint with a committed base, used by later publication tests too.
        git(
            &repo.root,
            &["remote", "add", "origin", repo.root.to_str().unwrap()],
        );
        repo
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["--no-input", "--project"])
            .arg(&self.project)
            .args(args)
            .output()
            .unwrap()
    }

    fn commit(&self) {
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "-qm", "fixture"]);
    }
    fn table(&self, content: &str) {
        fs::write(self.project.join("schema/t.yml"), content).unwrap();
    }
    fn store(&self) -> Candidates {
        Candidates::new(Config {
            executable: BIN.into(),
            project: self.project.clone(),
            deadline: Duration::from_secs(30),
        })
    }
    fn preserved(&self) -> Vec<Vec<u8>> {
        vec![
            fs::read(self.project.join("schema/t.yml")).unwrap(),
            fs::read(self.project.join("schema.ids.json")).unwrap(),
            fs::read(self.root.join(".git/index")).unwrap(),
            fs::read(self.root.join(".git/HEAD")).unwrap(),
            git(&self.root, &["show-ref"]),
        ]
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn request() -> Request {
    Request {
        intent: Intent::Rename {
            from: "dbo.t.id".into(),
            to: "ident".into(),
        },
        message: "Rename the identifier".into(),
        remote: "origin".into(),
        remote_base_ref: "refs/heads/master".into(),
    }
}

#[test]
fn confirmation_keeps_the_reviewed_tree_and_preserves_source_and_staged_work() {
    let repo = Repository::new("frozen", "project");
    repo.table(RENAMED);
    fs::write(repo.root.join("unrelated"), "staged unrelated change").unwrap();
    git(&repo.root, &["add", "unrelated"]);
    fs::write(repo.root.join("untracked"), "untracked content").unwrap();
    // The ordinary read commands must not need any of the source publication
    // locks. Another Git writer's lock files are neither ours to use nor remove.
    for path in [
        ".git/index.lock",
        ".git/HEAD.lock",
        ".git/refs/heads/master.lock",
    ] {
        fs::write(repo.root.join(path), "foreign owner").unwrap();
    }
    let before = repo.preserved();
    let mut store = repo.store();
    let now = SystemTime::now();
    let preview = store.preview(request(), now).unwrap();
    assert_eq!(repo.preserved(), before);
    assert!(preview.diff.contains("+  ident:"));
    assert!(!preview.diff.contains("staged unrelated"));
    assert!(preview.output_ref.ends_with(&preview.operation_id));
    repo.table(&format!("{RENAMED}  unreviewed: {{type: int}}\n"));
    let later = repo.preserved();
    let candidate = store.confirm(&preview.candidate_id, now).unwrap();
    assert_eq!(
        candidate.preview().tree,
        preview.tree,
        "confirmation must keep the reviewed tree"
    );
    let tree_file = git(
        &candidate.snapshot_repository(),
        &["show", &format!("{}:project/schema/t.yml", preview.tree)],
    );
    assert_eq!(tree_file, RENAMED.as_bytes());
    assert_eq!(candidate.preview().binding, preview.binding);
    // A controlled publisher consumes exactly the candidate's tree/parent,
    // without reading the edited source or publishing a source ref.
    let snapshot = candidate.snapshot_repository();
    git(&snapshot, &["config", "user.email", "compose@example.test"]);
    let commit = String::from_utf8(git(
        &snapshot,
        &[
            "commit-tree",
            &preview.tree,
            "-p",
            &preview.base,
            "-m",
            &candidate.request().message,
        ],
    ))
    .unwrap();
    assert_eq!(
        String::from_utf8(git(
            &snapshot,
            &["show", "-s", "--format=%T", commit.trim()]
        ))
        .unwrap()
        .trim(),
        preview.tree
    );
    assert_eq!(repo.preserved(), later);
    for path in [
        ".git/index.lock",
        ".git/HEAD.lock",
        ".git/refs/heads/master.lock",
    ] {
        assert_eq!(
            fs::read_to_string(repo.root.join(path)).unwrap(),
            "foreign owner"
        );
    }
    assert_eq!(
        fs::read_to_string(repo.root.join("unrelated")).unwrap(),
        "staged unrelated change"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("untracked")).unwrap(),
        "untracked content"
    );
    assert!(Arc::ptr_eq(
        &candidate,
        &store.confirm(&preview.candidate_id, now).unwrap()
    ));
    assert!(store.confirm("invented-tree-or-handle", now).is_err());
    assert!(store.preview(request(), now).is_err());
}

#[test]
fn edits_during_capture_are_refused_and_never_become_post_diff_evidence() {
    for boundary in [
        CaptureBoundary::InputsRead,
        CaptureBoundary::IntentRecorded,
        CaptureBoundary::TreeBuilt,
    ] {
        let repo = Repository::new(&format!("race-{boundary:?}"), "");
        repo.table(RENAMED);
        let before = repo.preserved();
        let mut store = repo.store();
        let result = store.preview_observed(request(), SystemTime::now(), &|at| {
            if at == boundary {
                repo.table(&format!("{RENAMED}  unseen: {{type: int}}\n"));
            }
        });
        assert!(
            result.is_err(),
            "capture accepted a concurrent edit at {boundary:?}"
        );
        assert_eq!(&repo.preserved()[1..], &before[1..]);
        assert!(
            fs::read_to_string(repo.project.join("schema/t.yml"))
                .unwrap()
                .contains("unseen")
        );
        assert!(store.confirm("any", SystemTime::now()).is_err());
    }
}

#[test]
fn configuration_cannot_redirect_intent_writes_after_path_admission() {
    let repo = Repository::new("config-admission-race", "");
    repo.table(RENAMED);
    let ids = fs::read(repo.project.join("schema.ids.json")).unwrap();
    let index = fs::read(repo.root.join(".git/index")).unwrap();
    let result = repo
        .store()
        .preview_observed(request(), SystemTime::now(), &|at| {
            if at == CaptureBoundary::PathsResolved {
                // If this later configuration were overlaid into the snapshot,
                // ordinary `rename` would write the source's identity file before
                // the final capture recheck ever had a chance to refuse it.
                fs::write(
                    repo.project.join("pbps.yml"),
                    format!(
                        "dialect: mssql\nids_file: {}\n",
                        repo.project.join("schema.ids.json").display()
                    ),
                )
                .unwrap();
            }
        });
    assert!(result.is_err());
    assert_eq!(
        fs::read(repo.project.join("schema.ids.json")).unwrap(),
        ids,
        "path admission must protect the actual configuration used by the intent CLI"
    );
    assert_eq!(fs::read(repo.root.join(".git/index")).unwrap(), index);
}

#[test]
fn refresh_replaces_the_handle_and_expiry_requires_another_preview() {
    let repo = Repository::new("refresh", "");
    repo.table(RENAMED);
    let mut store = repo.store();
    let now = SystemTime::now();
    let first = store.preview(request(), now).unwrap();
    repo.table(&format!("{RENAMED}  added: {{type: int}}\n"));
    let second = store.preview(request(), now).unwrap();
    assert_ne!(first.tree, second.tree);
    assert_ne!(first.candidate_id, second.candidate_id);
    assert_ne!(first.operation_id, second.operation_id);
    assert!(second.diff.contains("+  added:"));
    assert!(store.confirm(&first.candidate_id, now).is_err());
    assert!(
        store
            .confirm(&second.candidate_id, now + Duration::from_secs(86400))
            .is_err()
    );
    assert!(store.confirm(&second.candidate_id, now).is_err());
    let third = store.preview(request(), now).unwrap();
    repo.table("not valid: [");
    assert!(store.preview(request(), now).is_err());
    assert!(store.confirm(&third.candidate_id, now).is_err());
    repo.table(RENAMED);
    let fourth = store.preview(request(), now).unwrap();
    assert!(
        store
            .confirm(&fourth.candidate_id, now - Duration::from_secs(1))
            .is_err()
    );
    assert!(store.confirm(&fourth.candidate_id, now).is_err());
}

#[test]
fn new_deleted_recreated_paths_and_modes_are_part_of_the_candidate() {
    let repo = Repository::new("membership", "");
    fs::remove_file(repo.project.join("schema/t.yml")).unwrap();
    fs::write(repo.project.join("schema/renamed.yml"), RENAMED).unwrap();
    fs::set_permissions(
        repo.project.join("schema/renamed.yml"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let mut store = repo.store();
    let now = SystemTime::now();
    let preview = store.preview(request(), now).unwrap();
    // Recreate a deleted path only after the preview is sealed.
    fs::write(repo.project.join("schema/t.yml"), "editor recreation").unwrap();
    let candidate = store.confirm(&preview.candidate_id, now).unwrap();
    assert!(candidate.manifest().inputs["schema/t.yml"].is_none());
    assert_eq!(
        candidate.manifest().inputs["schema/renamed.yml"]
            .as_ref()
            .unwrap()
            .mode,
        0o100755
    );
    let listing = String::from_utf8(git(
        &candidate.snapshot_repository(),
        &["ls-tree", "-r", &preview.tree],
    ))
    .unwrap();
    assert!(!listing.contains("\tschema/t.yml"));
    assert!(
        listing
            .lines()
            .any(|l| l.starts_with("100755 ") && l.ends_with("schema/renamed.yml"))
    );
    assert_eq!(
        fs::read_to_string(repo.project.join("schema/t.yml")).unwrap(),
        "editor recreation"
    );
}

#[test]
fn force_added_declarations_use_captured_bytes_without_admitting_ignored_untracked_files() {
    for (ignored, staged) in [(true, false), (true, true), (false, false)] {
        let repo = Repository::new(&format!("ignored-{ignored}-{staged}"), "");
        if ignored {
            fs::write(repo.root.join(".gitignore"), "schema/new.yml\n").unwrap();
            repo.commit();
        }
        let name = "schema/new.yml";
        let staged_bytes = "table: dbo.new\ncolumns:\n  id: {type: int}\n";
        fs::write(repo.project.join(name), staged_bytes).unwrap();
        if staged {
            git(&repo.root, &["add", "-f", name]);
            assert_eq!(
                git(&repo.root, &["ls-files", "-v", "--", name]),
                b"H schema/new.yml\n"
            );
        }
        let captured_bytes = format!("{staged_bytes}  captured: {{type: int}}\n");
        fs::write(repo.project.join(name), &captured_bytes).unwrap();
        let before = repo.preserved();
        let mut action = request();
        action.intent = Intent::Declarations;
        let mut store = repo.store();
        let now = SystemTime::now();
        let result = store.preview(action, now);
        if ignored && !staged {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("new declaration is ignored")
            );
        } else {
            let preview = result.unwrap();
            let candidate = store.confirm(&preview.candidate_id, now).unwrap();
            assert_eq!(
                git(
                    &candidate.snapshot_repository(),
                    &["show", &format!("{}:{name}", preview.tree)]
                ),
                captured_bytes.as_bytes()
            );
            assert!(preview.diff.contains("+  captured:"));
        }
        assert_eq!(repo.preserved(), before);
        assert_eq!(
            fs::read(repo.project.join(name)).unwrap(),
            captured_bytes.as_bytes()
        );
    }
}

#[test]
fn captured_executable_modes_match_real_git_including_group_only_execute() {
    for mode in [0o654, 0o740] {
        let repo = Repository::new(&format!("git-mode-{mode:o}"), "");
        repo.table(RENAMED);
        fs::set_permissions(
            repo.project.join("schema/t.yml"),
            fs::Permissions::from_mode(mode),
        )
        .unwrap();
        git(&repo.root, &["add", "schema/t.yml"]);
        let entry = String::from_utf8(git(
            &repo.root,
            &["ls-files", "--stage", "--", "schema/t.yml"],
        ))
        .unwrap();
        let expected = u32::from_str_radix(entry.split_whitespace().next().unwrap(), 8).unwrap();
        let mut store = repo.store();
        let now = SystemTime::now();
        let preview = store.preview(request(), now).unwrap();
        let candidate = store.confirm(&preview.candidate_id, now).unwrap();
        assert_eq!(
            candidate.manifest().inputs["schema/t.yml"]
                .as_ref()
                .unwrap()
                .mode,
            expected
        );
    }
}

#[test]
fn a_new_path_or_mode_change_at_the_capture_barrier_is_not_omitted() {
    for mode_only in [true, false] {
        let repo = Repository::new(&format!("membership-race-{mode_only}"), "");
        repo.table(RENAMED);
        assert!(
            repo.store()
                .preview_observed(request(), SystemTime::now(), &|at| {
                    if at == CaptureBoundary::TreeBuilt {
                        if mode_only {
                            fs::set_permissions(
                                repo.project.join("schema/t.yml"),
                                fs::Permissions::from_mode(0o755),
                            )
                            .unwrap();
                        } else {
                            fs::write(
                                repo.project.join("schema/new.yml"),
                                "table: dbo.new\ncolumns:\n  id: {type: int}\n",
                            )
                            .unwrap();
                        }
                    }
                })
                .is_err()
        );
    }
}

#[test]
fn binary_attributes_cannot_hide_reviewed_declaration_or_identity_bytes() {
    let repo = Repository::new("forced-text", "");
    fs::write(
        repo.root.join(".gitattributes"),
        "schema/*.yml -diff\nschema.ids.json -diff\n",
    )
    .unwrap();
    repo.commit();
    repo.table(RENAMED);
    let preview = repo.store().preview(request(), SystemTime::now()).unwrap();
    assert!(preview.diff.contains("+  ident:"), "{}", preview.diff);
    assert!(preview.diff.contains("schema.ids.json"));
    assert!(!preview.diff.contains("Binary files"));
}

#[test]
fn uppercase_suffixes_are_not_declarations_and_do_not_enter_candidate_changes() {
    let repo = Repository::new("suffixes", "");
    fs::write(
        repo.project.join("schema/notes.YML"),
        "base non-declaration",
    )
    .unwrap();
    repo.commit();
    repo.table(RENAMED);
    fs::write(
        repo.project.join("schema/notes.YML"),
        "unreviewed non-declaration",
    )
    .unwrap();
    fs::write(repo.project.join("schema/new.YAML"), "not a declaration").unwrap();
    let mut store = repo.store();
    let now = SystemTime::now();
    let preview = store.preview(request(), now).unwrap();
    assert!(!preview.diff.contains("notes.YML") && !preview.diff.contains("new.YAML"));
    let candidate = store.confirm(&preview.candidate_id, now).unwrap();
    assert!(!candidate.manifest().inputs.contains_key("schema/notes.YML"));
    assert!(!candidate.manifest().inputs.contains_key("schema/new.YAML"));
    assert_eq!(
        git(
            &candidate.snapshot_repository(),
            &["show", &format!("{}:schema/notes.YML", preview.tree)]
        ),
        b"base non-declaration"
    );
    let listing = String::from_utf8(git(
        &candidate.snapshot_repository(),
        &["ls-tree", "-r", &preview.tree],
    ))
    .unwrap();
    assert!(!listing.contains("new.YAML"));
}

#[test]
fn git_boolean_aliases_cannot_admit_conversion_dependent_carriage_returns() {
    for value in [
        "TRUE", "yes", "on", "1", "input", "INPUT", "false", "off", "0",
    ] {
        let repo = Repository::new(&format!("autocrlf-{value}"), "");
        git(&repo.root, &["config", "core.autocrlf", value]);
        repo.table(&RENAMED.replace('\n', "\r\n"));
        let raw = git(&repo.root, &["hash-object", "--no-filters", "schema/t.yml"]);
        let ordinary = git(
            &repo.root,
            &["hash-object", "--path=schema/t.yml", "schema/t.yml"],
        );
        let result = repo.store().preview(request(), SystemTime::now());
        if raw != ordinary {
            assert!(result.is_err(), "accepted conversion under {value}");
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("must not require Git line-ending conversion")
            );
        } else {
            assert!(
                result.is_ok(),
                "refused raw-byte policy {value}: {}",
                result.err().unwrap()
            );
        }
    }
}

#[test]
fn legacy_and_modern_line_ending_attributes_follow_git_precedence() {
    for (index, (attribute, autocrlf)) in [
        ("crlf", "false"),
        ("crlf=input", "false"),
        ("crlf=unset", "true"),
        ("text=unset", "true"),
        ("text=unset", "false"),
        ("text=set", "false"),
        ("crlf=auto", "false"),
        ("-crlf", "true"),
        ("-text crlf", "true"),
        ("text -crlf", "false"),
        ("text=auto -crlf", "false"),
        ("-text eol=lf", "true"),
        ("text=bad crlf=input", "false"),
        ("crlf=bad", "false"),
        ("eol=bad", "false"),
        ("eol=lf", "false"),
    ]
    .into_iter()
    .enumerate()
    {
        let repo = Repository::new(&format!("eol-attributes-{index}"), "");
        git(&repo.root, &["config", "core.autocrlf", autocrlf]);
        fs::write(
            repo.root.join(".gitattributes"),
            format!("schema/*.yml {attribute}\nschema.ids.json {attribute}\n"),
        )
        .unwrap();
        repo.commit();
        repo.table(&RENAMED.replace('\n', "\r\n"));
        let ids = fs::read_to_string(repo.project.join("schema.ids.json")).unwrap();
        fs::write(
            repo.project.join("schema.ids.json"),
            ids.replace('\n', "\r\n"),
        )
        .unwrap();
        let before = repo.preserved();
        for path in ["schema/t.yml", "schema.ids.json"] {
            let raw = git(&repo.root, &["hash-object", "--no-filters", path]);
            let ordinary = git(
                &repo.root,
                &["hash-object", &format!("--path={path}"), path],
            );
            let result = repo.store().preview(request(), SystemTime::now());
            assert_eq!(
                result.is_ok(),
                raw == ordinary,
                "wrong capture admission for {path}: {attribute}, autocrlf={autocrlf}"
            );
            if raw != ordinary {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("must not require Git line-ending conversion")
                );
            }
        }
        assert_eq!(repo.preserved(), before);
    }
}

#[test]
fn only_actual_line_ending_conversion_refuses_a_valid_declaration() {
    for (index, (policy, autocrlf, content)) in [
        ("text", "false", RENAMED.replace('\n', "\r")),
        ("text", "false", RENAMED.replace('\n', "\r\n")),
        (
            "text=auto",
            "false",
            RENAMED.replacen('\n', "\r", 1).replace('\n', "\r\n"),
        ),
        ("text=auto", "false", RENAMED.replace('\n', "\r\n")),
        (
            "text",
            "false",
            RENAMED.replacen('\n', "\r", 1).replace('\n', "\r\n"),
        ),
        (
            "!text",
            "true",
            RENAMED.replacen('\n', "\r", 1).replace('\n', "\r\n"),
        ),
        ("!text", "input", RENAMED.replace('\n', "\r")),
    ]
    .into_iter()
    .enumerate()
    {
        let repo = Repository::new(&format!("actual-eol-{index}"), "");
        git(&repo.root, &["config", "core.autocrlf", autocrlf]);
        fs::write(
            repo.root.join(".gitattributes"),
            format!("schema/*.yml {policy}\n"),
        )
        .unwrap();
        repo.commit();
        repo.table(&content);
        checked(repo.cli(&["validate"]));
        let raw = git(&repo.root, &["hash-object", "--no-filters", "schema/t.yml"]);
        let converted = git(
            &repo.root,
            &["hash-object", "--path=schema/t.yml", "schema/t.yml"],
        );
        let before = repo.preserved();
        let mut store = repo.store();
        let now = SystemTime::now();
        let result = store.preview(request(), now);
        assert_eq!(
            result.is_ok(),
            raw == converted,
            "non-converting declaration refused or converting declaration admitted: {policy}, autocrlf={autocrlf}, {content:?}, error={:?}",
            result.as_ref().err()
        );
        if let Err(error) = &result {
            assert!(
                error
                    .to_string()
                    .contains("must not require Git line-ending conversion")
            );
        }
        if let Ok(preview) = result {
            let candidate = store.confirm(&preview.candidate_id, now).unwrap();
            assert_eq!(
                git(
                    &candidate.snapshot_repository(),
                    &["show", &format!("{}:schema/t.yml", preview.tree)]
                ),
                content.as_bytes()
            );
        }
        assert_eq!(repo.preserved(), before);
    }
}

#[test]
fn attribute_sentinel_aliases_cannot_hide_a_changed_policy() {
    for during_capture in [false, true] {
        let repo = Repository::new(&format!("attribute-alias-{during_capture}"), "project");
        git(&repo.root, &["config", "core.autocrlf", "true"]);
        let path = repo.root.join(".gitattributes");
        fs::write(&path, "project/schema/*.yml -text\n").unwrap();
        repo.commit();
        repo.table(&RENAMED.replace('\n', "\r\n"));
        let attrs = git(
            &repo.root,
            &["check-attr", "--all", "-z", "--", "project/schema/t.yml"],
        );
        let change = || {
            fs::write(&path, "project/schema/*.yml text=unset\n").unwrap();
            assert_eq!(
                git(
                    &repo.root,
                    &["check-attr", "--all", "-z", "--", "project/schema/t.yml"]
                ),
                attrs
            );
        };
        if !during_capture {
            change();
        }
        let before = repo.preserved();
        let result = repo
            .store()
            .preview_observed(request(), SystemTime::now(), &|at| {
                if during_capture && at == CaptureBoundary::TreeBuilt {
                    change();
                }
            });
        assert!(
            result.is_err(),
            "attribute sentinel alias hid changed policy"
        );
        assert_eq!(repo.preserved(), before);
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "project/schema/*.yml text=unset\n"
        );
    }
}

#[test]
fn resolving_line_ending_policy_never_executes_a_configured_filter() {
    let repo = Repository::new("policy-filter", "");
    repo.table(RENAMED);
    let marker = repo.root.join("filter-ran");
    fs::write(
        repo.root.join(".git/info/attributes"),
        "schema/*.yml filter=probe\n",
    )
    .unwrap();
    git(
        &repo.root,
        &[
            "config",
            "filter.probe.clean",
            &format!("touch '{}'; cat", marker.display()),
        ],
    );
    let before = repo.preserved();
    assert!(repo.store().preview(request(), SystemTime::now()).is_err());
    assert!(!marker.exists(), "policy observation executed a filter");
    assert_eq!(repo.preserved(), before);
    // The fixture's helper is executable: ordinary filtered hashing runs it.
    git(
        &repo.root,
        &["hash-object", "--path=schema/t.yml", "schema/t.yml"],
    );
    assert!(marker.exists());
}

#[test]
fn private_attribute_indexes_do_not_create_source_split_index_files() {
    let repo = Repository::new("policy-split-index", "");
    git(&repo.root, &["config", "core.splitIndex", "true"]);
    git(&repo.root, &["update-index", "--split-index"]);
    repo.table(RENAMED);
    let shared_indexes = || {
        let mut names: Vec<_> = fs::read_dir(repo.root.join(".git"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with("sharedindex."))
            .collect();
        names.sort();
        names
    };
    let before = repo.preserved();
    let indexes = shared_indexes();
    assert!(!indexes.is_empty());
    repo.store().preview(request(), SystemTime::now()).unwrap();
    assert_eq!(repo.preserved(), before);
    assert_eq!(
        shared_indexes(),
        indexes,
        "private policy index wrote source split indexes"
    );
}

#[test]
fn contained_parent_components_resolve_to_the_same_reviewed_inputs() {
    for sub in ["", "project"] {
        let repo = Repository::new(&format!("contained-parents-{sub}"), sub);
        fs::write(
            repo.project.join("pbps.yml"),
            "dialect: mssql\nschema_dir: schema/../schema\nids_file: schema/../schema.ids.json\n",
        )
        .unwrap();
        checked(repo.cli(&["validate"]));
        repo.commit();
        repo.table(RENAMED);
        let before = repo.preserved();
        let mut store = repo.store();
        let now = SystemTime::now();
        let preview = store
            .preview(request(), now)
            .expect("contained parent path refused");
        let candidate = store.confirm(&preview.candidate_id, now).unwrap();
        assert!(preview.diff.contains("+  ident:"));
        assert!(
            candidate
                .manifest()
                .inputs
                .keys()
                .all(|name| !name.contains(".."))
        );
        assert_eq!(repo.preserved(), before);
        for config in [
            "dialect: mssql\nschema_dir: schema/../../outside\n",
            "dialect: mssql\nids_file: schema/../../outside.ids.json\n",
        ] {
            repo.table(ORIGINAL);
            fs::write(repo.project.join("pbps.yml"), config).unwrap();
            repo.commit();
            repo.table(RENAMED);
            let before = repo.preserved();
            let error = repo
                .store()
                .preview(request(), now)
                .unwrap_err()
                .to_string();
            assert!(error.contains("cannot leave the project"), "{error}");
            assert_eq!(repo.preserved(), before);
        }
    }
}

#[test]
fn links_unreadable_inputs_and_literal_unspecified_filters_are_named_refusals() {
    for variant in ["link", "unreadable", "filter"] {
        let repo = Repository::new(variant, "");
        if variant == "filter" {
            fs::write(
                repo.root.join(".gitattributes"),
                "schema/*.yml filter=unspecified\n",
            )
            .unwrap();
            repo.commit();
        }
        repo.table(RENAMED);
        if variant == "link" {
            fs::remove_file(repo.project.join("schema/t.yml")).unwrap();
            symlink("../schema.ids.json", repo.project.join("schema/t.yml")).unwrap();
        } else if variant == "unreadable" {
            fs::set_permissions(
                repo.project.join("schema/t.yml"),
                fs::Permissions::from_mode(0o0),
            )
            .unwrap();
        }
        let error = match repo.store().preview(request(), SystemTime::now()) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("accepted {variant}"),
        };
        assert!(
            error.contains("link") || error.contains("unreadable") || error.contains("filter"),
            "{error}"
        );
    }
}

#[test]
fn git_output_terminators_cannot_normalize_an_invalid_destination() {
    let repo = Repository::new("destination-terminators", "");
    repo.table(RENAMED);
    let before = repo.preserved();
    for suffix in ["", "\n", "\r", "\r\n", "\n\n", "\t"] {
        let endpoint = format!("https://host.test/repo{suffix}");
        git(&repo.root, &["config", "remote.origin.pushurl", &endpoint]);
        // Real Git adds its own terminator after the literal config bytes.
        assert_eq!(
            git(
                &repo.root,
                &["remote", "get-url", "--push", "--all", "origin"]
            ),
            format!("{endpoint}\n").as_bytes()
        );
        let result = repo.store().preview(request(), SystemTime::now());
        assert_eq!(
            result.is_ok(),
            suffix.is_empty(),
            "destination control bytes {suffix:?} must not be normalized away"
        );
        assert_eq!(repo.preserved(), before);
    }
}

#[test]
fn changed_configuration_and_secret_bearing_destinations_do_not_seal() {
    let repo = Repository::new("settings", "");
    repo.table(RENAMED);
    for url in [
        "https://user:FAKE_SECRET@host.test/repo",
        "https://host.test/repo?token=FAKE_SECRET",
    ] {
        git(&repo.root, &["remote", "set-url", "origin", url]);
        let error = match repo.store().preview(request(), SystemTime::now()) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("accepted secret endpoint"),
        };
        assert!(!error.contains("FAKE_SECRET"));
    }
    git(
        &repo.root,
        &["remote", "set-url", "origin", repo.root.to_str().unwrap()],
    );
    fs::write(
        repo.project.join("pbps.yml"),
        "dialect: mssql\n# uncommitted\n",
    )
    .unwrap();
    assert!(repo.store().preview(request(), SystemTime::now()).is_err());
}

#[test]
fn ordinary_drop_and_strategy_intents_are_resolved_only_in_the_snapshot() {
    let repo = Repository::new("drop", "");
    repo.table(&format!("{ORIGINAL}  old: {{type: int}}\n"));
    checked(repo.cli(&["plan"]));
    repo.commit();
    repo.table(ORIGINAL);
    let before = repo.preserved();
    let mut action = request();
    action.intent = Intent::Drop {
        column: "dbo.t.old".into(),
        reason: "Retired by the owner".into(),
    };
    let mut store = repo.store();
    let preview = store.preview(action, SystemTime::now()).unwrap();
    assert!(preview.diff.contains("Retired by the owner"));
    assert_eq!(repo.preserved(), before);

    let repo = Repository::new("strategy", "");
    fs::write(
        repo.project.join("pbps.yml"),
        "dialect: mssql\ndev:\n  url_env: PBPS_COMPOSE_UNAVAILABLE_DATABASE\n",
    )
    .unwrap();
    repo.commit();
    repo.table("table: dbo.t\nstrategy:\n  online: true\ncolumns:\n  id: {type: int, nullable: false}\nindexes:\n  ix_id:\n    columns: [id]\n");
    let before = repo.preserved();
    let mut action = request();
    action.intent = Intent::Declarations;
    let preview = repo.store().preview(action, SystemTime::now()).unwrap();
    assert!(preview.diff.contains("+  online: true"));
    assert_eq!(repo.preserved(), before);
    // Ordinary CLI control: configured rehearsal is unavailable, while the
    // explicit opt-out still performs the file-side plan and writes ids.
    assert!(!repo.cli(&["plan"]).status.success());
    checked(repo.cli(&["plan", "--no-dev"]));
    checked(repo.cli(&["plan", "--check"]));
    for flags in [
        vec![
            "plan",
            "--no-dev",
            "--dev",
            "docker://unused",
            "--format=json",
        ],
        vec!["plan", "--no-dev", "--db", "unused", "--format=json"],
    ] {
        let output = repo.cli(&flags);
        assert!(!output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["findings"][0]["id"], "flags.conflicting");
    }
}

#[test]
fn base_destination_and_signing_changes_during_capture_require_a_fresh_candidate() {
    for variant in ["base", "destination", "signing"] {
        let repo = Repository::new(&format!("bound-{variant}"), "");
        repo.table(RENAMED);
        let result = repo
            .store()
            .preview_observed(request(), SystemTime::now(), &|at| {
                if at != CaptureBoundary::TreeBuilt {
                    return;
                }
                match variant {
                    "base" => {
                        git(
                            &repo.root,
                            &["commit", "--allow-empty", "-qm", "concurrent commit"],
                        );
                    }
                    "destination" => {
                        git(
                            &repo.root,
                            &["remote", "set-url", "origin", "https://changed.test/repo"],
                        );
                    }
                    _ => {
                        git(&repo.root, &["config", "commit.gpgSign", "true"]);
                    }
                }
            });
        assert!(result.is_err(), "accepted changed {variant}");
    }
    let repo = Repository::new("message", "");
    repo.table(RENAMED);
    let mut store = repo.store();
    let now = SystemTime::now();
    let old = store.preview(request(), now).unwrap();
    let mut edited = request();
    edited.message = "Another message".into();
    let new = store.preview(edited, now).unwrap();
    assert_ne!(old.binding, new.binding);
    assert!(store.confirm(&old.candidate_id, now).is_err());
    assert_eq!(
        store
            .confirm(&new.candidate_id, now)
            .unwrap()
            .request()
            .message,
        "Another message"
    );
}

#[test]
fn paths_only_resolves_before_loading_declarations_and_never_contacts_a_database() {
    let repo = Repository::new("paths-only", "");
    fs::write(repo.project.join("pbps.yml"), "dialect: mssql\nschema_dir: ../unreadable-outside\nenvironments:\n  unreachable:\n    url_env: PBPS_COMPOSE_UNAVAILABLE_DATABASE\n").unwrap();
    let data: serde_json::Value = serde_json::from_slice(&checked(repo.cli(&[
        "doctor",
        "--paths-only",
        "--format=json",
    ])))
    .unwrap();
    assert_eq!(data["data"]["mode"], "paths-only");
    assert!(
        data["data"]["declarations"]
            .as_str()
            .unwrap()
            .ends_with("../unreadable-outside")
    );
    assert!(data["data"].get("tables").is_none());
    assert!(
        !repo
            .cli(&["doctor", "--paths-only", "--env", "unreachable"])
            .status
            .success()
    );
    assert!(
        !repo
            .cli(&["doctor", "--paths-only", "--db", "somewhere"])
            .status
            .success()
    );
    repo.commit();
    assert!(repo.store().preview(request(), SystemTime::now()).is_err());
}

#[test]
fn the_browser_cannot_supply_replacement_evidence_or_arbitrary_commands() {
    let mut value = serde_json::to_value(request()).unwrap();
    value["tree"] = "forged".into();
    assert!(serde_json::from_value::<Request>(value).is_err());
    assert!(serde_json::from_str::<Intent>(r#"{"kind":"exec","command":"anything"}"#).is_err());
}
