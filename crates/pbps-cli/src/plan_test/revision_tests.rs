use super::*;
use pbps_config::test_manifest::Candidate;
use std::process::Command;

const IMAGE: &str =
    "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280";

/// A git repository holding a project in `sub` (empty for the repository root).
struct Repo {
    dir: PathBuf,
    project: PathBuf,
}

type Tables<'a> = &'a [(&'a str, &'a [&'a str])];

impl Repo {
    fn new(sub: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("pbps-revisions-{:032x}", rand::random::<u128>()));
        let project = dir.join(sub);
        std::fs::create_dir_all(&project).unwrap();
        let repo = Self { dir, project };
        repo.git(&["init", "-q"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
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

    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let path = self.project.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn commit(&self, message: &str) -> CommitId {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.head()
    }

    fn head(&self) -> CommitId {
        CommitId::parse(&self.git(&["rev-parse", "HEAD"])).unwrap()
    }

    /// `pbps.yml` and one declaration per table; `ids` brings the identity
    /// file up to date with them, as `plan` would.
    fn declare(&self, tables: Tables<'_>, ids: bool) {
        self.write("pbps.yml", "dialect: postgres\n");
        let dir = self.project.join("schema");
        let _ = std::fs::remove_dir_all(&dir);
        for (name, columns) in tables {
            let columns: String = columns
                .iter()
                .map(|c| format!("  {c}: {{type: integer, nullable: false}}\n"))
                .collect();
            self.write(
                &format!("schema/{name}.yml"),
                format!(
                    "table: {name}\ncolumns:\n{columns}primary_key: [{}]\n",
                    columns_first(tables, name)
                ),
            );
        }
        if ids {
            self.refresh_ids();
        }
    }

    fn refresh_ids(&self) {
        let loaded = pbps_load::load_schema_dir(&self.project.join("schema")).unwrap();
        let path = self.project.join("schema.ids.json");
        let current: pbps_model::IdsFile = std::fs::read_to_string(&path)
            .ok()
            .map(|text| serde_json::from_str(&text).unwrap())
            .unwrap_or_default();
        let ctx = pbps_diff::Context {
            operator: "t".into(),
            today: "2026-01-01".into(),
        };
        let resolved = pbps_diff::resolve_with_annotations(
            &loaded.schema,
            &current,
            &loaded.intents,
            loaded.intents.len(),
            &ctx,
        )
        .expect("the test declarations resolve");
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&resolved.ids).unwrap()),
        )
        .unwrap();
    }

    /// The manifest at `tests/pbps-tests.yml`, with a fixture and an assertion file.
    fn manifest(&self, base: &CommitId, fixtures: &[(&str, &CommitId)], candidate: Option<&str>) {
        self.write("tests/a.sql", "SELECT 1 WHERE false\n");
        let fixtures = if fixtures.is_empty() {
            "[]".to_owned()
        } else {
            fixtures
                .iter()
                .map(|(name, at)| {
                    self.write(&format!("tests/{name}.sql"), "INSERT INTO t VALUES (1);\n");
                    format!("\n  - {{name: {name}, file: {name}.sql, at: {at}}}")
                })
                .collect()
        };
        let candidate = candidate
            .map(|c| format!("candidate: \"{c}\"\n"))
            .unwrap_or_default();
        self.write(
            "tests/pbps-tests.yml",
            format!(
                "version: 1\ndialect: postgres\nbase: {base}\n{candidate}engine:\n  image: {IMAGE}\n\
                 fixtures: {fixtures}\nassertions:\n  - {{name: a, file: a.sql}}\n"
            ),
        );
    }

    fn validate(&self) -> Result<Inputs, Vec<Refusal>> {
        validate_inputs(&self.project, &self.project.join("tests/pbps-tests.yml"))
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn columns_first<'a>(tables: Tables<'a>, name: &str) -> &'a str {
    tables.iter().find(|(n, _)| *n == name).unwrap().1[0]
}

const ORDERS: Tables<'static> = &[("app.orders", &["id", "amount"])];
const MORE: Tables<'static> = &[("app.orders", &["id", "amount"]), ("app.lines", &["id"])];

/// Pins at `c1`, base `c2`, working tree on top: the ordinary shape.
fn standard() -> (Repo, CommitId, CommitId) {
    let repo = Repo::new("");
    repo.declare(ORDERS, true);
    let c1 = repo.commit("one");
    repo.declare(MORE, true);
    let c2 = repo.commit("two");
    repo.manifest(&c2, &[("f", &c1)], None);
    (repo, c1, c2)
}

fn at<'a>(refusals: &'a [Refusal], commit: &CommitId) -> Vec<&'a RevisionProblem> {
    refusals
        .iter()
        .filter_map(|r| match r {
            Refusal::At {
                revision: Revision::Commit(c),
                problem,
            } if c == commit => Some(problem),
            Refusal::At { .. }
            | Refusal::Manifest(_)
            | Refusal::Input { .. }
            | Refusal::DuplicateFile { .. }
            | Refusal::Changed { .. }
            | Refusal::Git { .. }
            | Refusal::Chain(_)
            | Refusal::HeadMoved { .. } => None,
        })
        .collect()
}

fn refused(result: Result<Inputs, Vec<Refusal>>) -> Vec<Refusal> {
    match result {
        Ok(_) => panic!("expected refusals"),
        Err(refusals) => refusals,
    }
}

#[test]
fn the_ordinary_shape_validates_and_reports_its_chain_and_identity() {
    let (repo, c1, c2) = standard();
    let inputs = repo.validate().unwrap();
    assert_eq!(inputs.chain.groups.len(), 2);
    assert_eq!(inputs.chain.groups[0].commit, c1);
    assert_eq!(inputs.chain.groups[0].fixtures, [0]);
    assert_eq!(inputs.chain.groups[1].commit, c2);
    assert_eq!(inputs.chain.hops.len(), 1);
    assert_eq!(inputs.chain.hops[0].distance, 1);
    assert_eq!(inputs.revisions.len(), 2);
    // What a replay takes from each revision.
    assert_eq!(inputs.candidate.revision, Revision::Working);
    assert_eq!(
        inputs.candidate.config.dialect,
        pbps_config::DialectName::Postgres
    );
    assert_eq!(inputs.candidate.loaded.schema.tables.len(), 2);
    assert_eq!(inputs.candidate.ids.tables.len(), 2);
    assert_eq!(inputs.revisions[&c1].revision, Revision::Commit(c1.clone()));
    assert_eq!(
        inputs.revisions[&c1].ids.tables.len(),
        1,
        "the pin has the table it had then"
    );
    let id = &inputs.identity;
    assert_eq!(
        (id.base.clone(), id.pins[0].commit.clone()),
        (c2.clone(), c1)
    );
    assert_eq!(id.candidate.kind, CandidateKind::WorkingTree);
    assert!(id.candidate.uncommitted);
    assert_eq!(id.candidate.commit, c2, "the captured HEAD");
    assert!(id.files.windows(2).all(|w| w[0] <= w[1]), "sorted");
    let roles: BTreeSet<_> = id.files.iter().map(|f| f.role).collect();
    assert_eq!(roles.len(), 6, "{roles:?}");
}

use std::collections::BTreeSet;

#[test]
fn a_commit_and_a_working_tree_with_the_same_bytes_have_the_same_hashes() {
    let (repo, _, c2) = standard();
    let inputs = repo.validate().unwrap();
    let shas = |revision: Revision| -> BTreeSet<(String, Role, String)> {
        inputs
            .identity
            .files
            .iter()
            .filter(|f| f.revision == revision)
            .map(|f| (f.path.clone(), f.role, f.sha256.clone()))
            .collect()
    };
    let working = shas(Revision::Working);
    let committed = shas(Revision::Commit(c2));
    let project: BTreeSet<_> = working
        .iter()
        .filter(|(_, role, _)| matches!(role, Role::Config | Role::Declaration | Role::Ids))
        .cloned()
        .collect();
    assert_eq!(project, committed);
}

#[test]
fn a_working_tree_is_uncommitted_before_any_commit_and_lists_new_and_changed_inputs() {
    let (repo, _, _) = standard();
    // A new, untracked declaration and a changed identity file.
    repo.declare(
        &[
            ("app.orders", &["id", "amount"]),
            ("app.lines", &["id"]),
            ("app.notes", &["id"]),
        ],
        true,
    );
    let inputs = repo.validate().unwrap();
    let working: Vec<_> = inputs
        .identity
        .files
        .iter()
        .filter(|f| f.revision == Revision::Working && f.role == Role::Declaration)
        .map(|f| f.path.as_str())
        .collect();
    assert!(working.contains(&"schema/app.notes.yml"), "{working:?}");
    assert!(inputs.identity.candidate.uncommitted);
}

#[test]
fn an_explicit_commit_candidate_is_labelled_committed_and_head_is_resolved_once() {
    let (repo, _, c2) = standard();
    repo.manifest(&repo.head(), &[], Some("HEAD"));
    let inputs = repo.validate().unwrap();
    assert_eq!(inputs.identity.candidate.kind, CandidateKind::Commit);
    assert!(!inputs.identity.candidate.uncommitted);
    assert_eq!(inputs.identity.candidate.commit, c2);
    assert!(
        inputs
            .identity
            .files
            .iter()
            .all(|f| f.revision != Revision::Working
                || f.role == Role::Manifest
                || f.role == Role::Fixture
                || f.role == Role::Assertion)
    );
    assert!(matches!(
        &Candidate::Revision("HEAD".into()),
        Candidate::Revision(_)
    ));
}

#[test]
fn a_candidate_revision_that_does_not_exist_names_the_input() {
    let (repo, _, c2) = standard();
    repo.manifest(&c2, &[], Some("no-such-branch"));
    let found = refused(repo.validate());
    assert!(
        found.iter().any(|r| matches!(r, Refusal::Git { what, problem: GitProblem::Missing { .. } } if what.contains("no-such-branch"))),
        "{found:?}"
    );
}

#[test]
fn a_name_that_is_both_a_branch_and_a_tag_is_refused_not_silently_resolved() {
    let (repo, c1, c2) = standard();
    repo.git(&["tag", "v1", &c1.to_string()]);
    repo.git(&["branch", "v1", &c2.to_string()]);
    repo.manifest(&c2, &[], Some("v1"));
    let found = refused(repo.validate());
    assert!(
        found.iter().any(|r| matches!(
            r,
            Refusal::Git {
                problem: GitProblem::Ambiguous(_),
                ..
            }
        )),
        "{found:?}"
    );
}

#[test]
fn a_pin_that_is_not_in_the_repository_is_refused_and_the_rest_is_still_checked() {
    let (repo, _, c2) = standard();
    let ghost = CommitId::parse(&"ab".repeat(20)).unwrap();
    repo.manifest(&c2, &[("f", &ghost)], None);
    let found = refused(repo.validate());
    assert!(
        found.iter().any(|r| matches!(r, Refusal::Git { what, problem: GitProblem::Missing { shallow: false } } if what.contains("fixture `f`"))),
        "{found:?}"
    );
}

#[test]
fn a_repository_with_no_commits_refuses_head_and_the_pins_together() {
    let repo = Repo::new("");
    repo.declare(ORDERS, true);
    let fake = CommitId::parse(&"cd".repeat(20)).unwrap();
    repo.manifest(&fake, &[], None);
    let found = refused(repo.validate());
    assert!(
        found
            .iter()
            .any(|r| matches!(r, Refusal::Git { what, .. } if what.starts_with("HEAD"))),
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .any(|r| matches!(r, Refusal::Git { what, .. } if what == "the base")),
        "{found:?}"
    );
}

#[test]
fn a_pin_reached_only_through_a_merge_is_an_ancestor_though_not_on_the_first_parent_line() {
    let repo = Repo::new("");
    repo.declare(ORDERS, true);
    let root = repo.commit("root");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("side.txt", "side\n");
    let side = repo.commit("side");
    repo.git(&["checkout", "-q", "-"]);
    repo.write("main.txt", "main\n");
    repo.commit("main");
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    let merged = repo.head();
    repo.manifest(&merged, &[("a", &side), ("b", &root)], None);
    let inputs = repo.validate().unwrap();
    let order: Vec<_> = inputs
        .chain
        .groups
        .iter()
        .map(|g| g.commit.clone())
        .collect();
    assert_eq!(order, [root, side, merged]);
}

#[test]
fn two_pins_on_diverging_branches_are_refused_by_name() {
    let repo = Repo::new("");
    repo.declare(ORDERS, true);
    repo.commit("root");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("side.txt", "side\n");
    let side = repo.commit("side");
    repo.git(&["checkout", "-q", "-"]);
    repo.write("main.txt", "main\n");
    let main = repo.commit("main");
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    let merged = repo.head();
    repo.manifest(&merged, &[("a", &side), ("b", &main)], None);
    let found = refused(repo.validate());
    assert!(
        found.iter().any(|r| matches!(r, Refusal::Chain(ChainProblem::Incomparable { a, b }) if (a == &side && b == &main) || (a == &main && b == &side))),
        "{found:?}"
    );
}

#[test]
fn a_pin_newer_than_the_base_and_a_base_ahead_of_head_are_refused() {
    let (repo, c1, c2) = standard();
    repo.manifest(&c1, &[("f", &c2)], None);
    // base c1 is behind HEAD, but the pin c2 is newer than it.
    let found = refused(repo.validate());
    assert!(
        found
            .iter()
            .any(|r| matches!(r, Refusal::Chain(ChainProblem::AfterBase { .. }))),
        "{found:?}"
    );
    repo.manifest(&c2, &[], None);
    repo.git(&["checkout", "-q", "--detach", &c1.to_string()]);
    let found = refused(repo.validate());
    assert!(
        found.iter().any(|r| matches!(
            r,
            Refusal::Chain(ChainProblem::BaseNotBehindCandidate { .. })
        )),
        "{found:?}"
    );
}

#[test]
fn a_revision_distance_counts_merged_history() {
    let repo = Repo::new("");
    repo.declare(ORDERS, true);
    let root = repo.commit("root");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("n1.txt", "1");
    repo.commit("s1");
    repo.write("n2.txt", "2");
    repo.commit("s2");
    repo.git(&["checkout", "-q", "-"]);
    repo.write("m.txt", "m");
    repo.commit("m1");
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    let merged = repo.head();
    repo.manifest(&merged, &[("f", &root)], None);
    let inputs = repo.validate().unwrap();
    assert_eq!(inputs.chain.hops[0].distance, 4, "m1, s1, s2 and the merge");
}

#[test]
fn a_pin_with_no_project_configuration_is_refused_not_read_as_no_project_yet() {
    let repo = Repo::new("");
    repo.write("README", "no project here yet\n");
    let early = repo.commit("early");
    repo.declare(ORDERS, true);
    let base = repo.commit("project");
    repo.manifest(&base, &[("f", &early)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &early).contains(&&RevisionProblem::ConfigAbsent),
        "{found:?}"
    );
}

#[test]
fn a_dialect_that_differs_from_the_manifest_is_refused_per_revision() {
    let (repo, c1, c2) = standard();
    repo.write("pbps.yml", "dialect: mssql\n");
    let mssql = repo.commit("mssql");
    repo.manifest(&mssql, &[("f", &c1)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &mssql)
            .iter()
            .any(|p| matches!(p, RevisionProblem::DialectMismatch { .. })),
        "{found:?}"
    );
    assert!(at(&found, &c1).is_empty() && at(&found, &c2).is_empty());
}

#[test]
fn identity_that_is_absent_malformed_inconsistent_or_unreadable_are_four_refusals() {
    let (repo, c1, _) = standard();
    // Absent, with objects declared: the identity file records nothing.
    std::fs::remove_file(repo.project.join("schema.ids.json")).unwrap();
    let absent = repo.commit("no ids");
    repo.manifest(&absent, &[("f", &c1)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &absent).contains(&&RevisionProblem::IdsOutOfDate),
        "{found:?}"
    );

    repo.write("schema.ids.json", "{not json");
    let malformed = repo.commit("malformed");
    repo.manifest(&malformed, &[("f", &c1)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &malformed)
            .iter()
            .any(|p| matches!(p, RevisionProblem::IdsMalformed { .. })),
        "{found:?}"
    );

    repo.write("schema.ids.json", "{\"version\": 99}");
    let inconsistent = repo.commit("inconsistent");
    repo.manifest(&inconsistent, &[("f", &c1)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &inconsistent)
            .iter()
            .any(|p| matches!(p, RevisionProblem::IdsInconsistent { .. })),
        "{found:?}"
    );

    // A damaged object is unreadable, which `baseline::ids_from` would read
    // as an empty file (#1814); here it is a refusal that says so.
    repo.refresh_ids();
    let healthy = repo.commit("healthy");
    let blob = repo.git(&["rev-parse", &format!("{healthy}:schema.ids.json")]);
    let object = repo
        .dir
        .join(".git/objects")
        .join(&blob[..2])
        .join(&blob[2..]);
    let mut permissions = std::fs::metadata(&object).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&object, permissions).unwrap();
    std::fs::write(&object, b"damaged").unwrap();
    repo.manifest(&healthy, &[("f", &c1)], None);
    let found = refused(repo.validate());
    let unreadable = at(&found, &healthy);
    assert!(
        unreadable.iter().any(
            |p| matches!(p, RevisionProblem::Unreadable { path, .. } if path == "schema.ids.json")
        ),
        "{found:?}"
    );
    assert!(found.iter().any(|r| r.outcome() == Outcome::Unanswerable));
}

#[test]
fn declarations_that_are_absent_and_declarations_that_are_empty_are_different() {
    let repo = Repo::new("");
    repo.write("pbps.yml", "dialect: postgres\n");
    let absent = repo.commit("config only");
    repo.write("schema/.gitkeep", "");
    let empty = repo.commit("empty schema directory");
    repo.manifest(&empty, &[("f", &absent)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &absent)
            .iter()
            .any(|p| matches!(p, RevisionProblem::DeclarationsAbsent(_))),
        "{found:?}"
    );
    assert!(
        at(&found, &empty).is_empty(),
        "an empty schema loads: {found:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_among_the_declarations_is_refused_and_so_is_text_that_is_not_utf8() {
    let (repo, c1, _) = standard();
    std::os::unix::fs::symlink("app.orders.yml", repo.project.join("schema/link.yml")).unwrap();
    let linked = repo.commit("link");
    repo.manifest(&linked, &[("f", &c1)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &linked).iter().any(
            |p| matches!(p, RevisionProblem::NotRegular { path, .. } if path == "schema/link.yml")
        ),
        "{found:?}"
    );
    std::fs::remove_file(repo.project.join("schema/link.yml")).unwrap();
    repo.write("schema/bad.yml", [0xff, 0xfe]);
    let binary = repo.commit("binary");
    repo.manifest(&binary, &[("f", &c1)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &binary)
            .iter()
            .any(|p| matches!(p, RevisionProblem::NotUtf8 { path } if path == "schema/bad.yml")),
        "{found:?}"
    );
}

#[test]
fn a_rename_with_no_recorded_intent_and_a_stale_identity_file_are_refused_with_their_decisions() {
    let (repo, c1, _) = standard();
    // `amount` is gone and `total` is new, and nothing says it is a rename.
    repo.declare(
        &[("app.orders", &["id", "total"]), ("app.lines", &["id"])],
        false,
    );
    let renamed = repo.commit("rename without intent");
    repo.manifest(&renamed, &[("f", &c1)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &renamed)
            .iter()
            .any(|p| matches!(p, RevisionProblem::Blockers(b) if !b.is_empty())),
        "{found:?}"
    );
    // A new table the identity file has never seen.
    repo.declare(
        &[
            ("app.orders", &["id", "amount"]),
            ("app.lines", &["id"]),
            ("app.new", &["id"]),
        ],
        false,
    );
    let unrecorded = repo.commit("stale ids");
    repo.manifest(&unrecorded, &[("f", &c1)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &unrecorded).contains(&&RevisionProblem::IdsOutOfDate),
        "{found:?}"
    );
}

#[test]
fn declaration_policies_bind_the_candidate_and_not_an_older_pin() {
    let repo = Repo::new("");
    let policy = "policies:\n  rules:\n    naming.table: {severity: error, pattern: \"^ok_\"}\n";
    repo.declare(ORDERS, true);
    repo.write("pbps.yml", format!("dialect: postgres\n{policy}"));
    let old = repo.commit("old");
    repo.manifest(&old, &[("f", &old)], None);
    let found = refused(repo.validate());
    assert!(
        found.iter().any(|r| matches!(
            r,
            Refusal::At {
                revision: Revision::Working,
                problem: RevisionProblem::Policy(_)
            }
        )),
        "the candidate violates the rule: {found:?}"
    );
    assert!(
        at(&found, &old).is_empty(),
        "the pin is not judged by it: {found:?}"
    );
    // The candidate as a commit is judged the same way.
    repo.manifest(&old, &[], Some("HEAD"));
    let found = refused(repo.validate());
    assert!(
        found.iter().any(|r| matches!(
            r,
            Refusal::At {
                problem: RevisionProblem::Policy(_),
                ..
            }
        )),
        "{found:?}"
    );
}

#[test]
fn problems_at_several_revisions_arrive_in_one_pass() {
    let (repo, c1, _) = standard();
    repo.write("schema.ids.json", "{not json");
    let bad_base = repo.commit("bad base");
    repo.write("pbps.yml", "dialect: mssql\n");
    repo.manifest(
        &bad_base,
        &[
            ("f", &c1),
            ("g", &CommitId::parse(&"ef".repeat(20)).unwrap()),
        ],
        None,
    );
    let found = refused(repo.validate());
    assert!(
        found.iter().any(|r| matches!(r, Refusal::Git { .. })),
        "{found:?}"
    );
    assert!(!at(&found, &bad_base).is_empty(), "{found:?}");
    assert!(
        found.iter().any(|r| matches!(
            r,
            Refusal::At {
                revision: Revision::Working,
                ..
            }
        )),
        "{found:?}"
    );
}

#[test]
fn a_project_below_the_repository_root_names_its_files_relative_to_itself() {
    let repo = Repo::new("apps/db");
    repo.declare(ORDERS, true);
    repo.write("../shared/note.txt", "x");
    let c1 = repo.commit("one");
    repo.manifest(&c1, &[("f", &c1)], None);
    let inputs = repo.validate().unwrap();
    let committed: BTreeSet<_> = inputs
        .identity
        .files
        .iter()
        .filter(|f| f.revision == Revision::Commit(c1.clone()))
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(
        committed,
        BTreeSet::from(["pbps.yml", "schema.ids.json", "schema/app.orders.yml"])
    );
}

#[test]
fn declarations_kept_beside_the_project_are_named_with_a_climb() {
    let repo = Repo::new("apps/db");
    repo.write(
        "pbps.yml",
        "dialect: postgres\nschema_dir: ../shared/schema\nids_file: ../shared/ids.json\n",
    );
    repo.write(
        "../shared/schema/app.orders.yml",
        "table: app.orders\ncolumns:\n  id: {type: integer, nullable: false}\nprimary_key: [id]\n",
    );
    let loaded = pbps_load::load_schema_dir(&repo.project.join("../shared/schema")).unwrap();
    let ctx = pbps_diff::Context {
        operator: "t".into(),
        today: "2026-01-01".into(),
    };
    let ids = pbps_diff::resolve_with_annotations(
        &loaded.schema,
        &Default::default(),
        &loaded.intents,
        0,
        &ctx,
    )
    .unwrap()
    .ids;
    repo.write(
        "../shared/ids.json",
        serde_json::to_string_pretty(&ids).unwrap(),
    );
    let c1 = repo.commit("one");
    repo.manifest(&c1, &[], None);
    let inputs = repo.validate().unwrap();
    let paths: BTreeSet<_> = inputs
        .identity
        .files
        .iter()
        .filter(|f| f.role == Role::Declaration || f.role == Role::Ids)
        .map(|f| f.path.as_str())
        .collect();
    assert!(
        paths.contains("../shared/schema/app.orders.yml") && paths.contains("../shared/ids.json"),
        "{paths:?}"
    );
}

#[test]
fn reverify_passes_when_nothing_moved_and_names_each_kind_of_movement() {
    let (repo, _, _) = standard();
    let inputs = repo.validate().unwrap();
    inputs.reverify().unwrap();
    let change_of = |inputs: &Inputs| -> Vec<(String, String)> {
        inputs
            .reverify()
            .unwrap_err()
            .iter()
            .map(|r| match r {
                Refusal::Changed { path, change } => (path.clone(), change.to_string()),
                Refusal::HeadMoved { .. } => ("HEAD".to_owned(), "moved".to_owned()),
                other @ (Refusal::Manifest(_)
                | Refusal::Input { .. }
                | Refusal::DuplicateFile { .. }
                | Refusal::Git { .. }
                | Refusal::Chain(_)
                | Refusal::At { .. }) => (String::new(), other.to_string()),
            })
            .collect()
    };
    // Added.
    repo.write(
        "schema/app.extra.yml",
        "table: app.extra\ncolumns:\n  id: {type: integer, nullable: false}\nprimary_key: [id]\n",
    );
    assert!(change_of(&inputs).contains(&("schema/app.extra.yml".into(), "was added".into())));
    std::fs::remove_file(repo.project.join("schema/app.extra.yml")).unwrap();
    // Changed bytes in a declaration.
    let path = repo.project.join("schema/app.orders.yml");
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{original}# note\n")).unwrap();
    assert!(change_of(&inputs).contains(&("schema/app.orders.yml".into(), "changed".into())));
    std::fs::remove_file(&path).unwrap();
    // Removed.
    assert!(change_of(&inputs).contains(&("schema/app.orders.yml".into(), "was removed".into())));
    std::fs::write(&path, original).unwrap();
    inputs.reverify().unwrap();
    // The identity file.
    let ids = repo.project.join("schema.ids.json");
    let text = std::fs::read_to_string(&ids).unwrap();
    std::fs::write(&ids, format!("{text} ")).unwrap();
    assert!(change_of(&inputs).contains(&("schema.ids.json".into(), "changed".into())));
    std::fs::write(&ids, text).unwrap();
    // HEAD moves.
    repo.write("elsewhere.txt", "x");
    repo.commit("another");
    assert!(change_of(&inputs).contains(&("HEAD".into(), "moved".into())));
}

#[test]
fn the_fixture_and_assertion_files_are_rechecked_with_the_candidate() {
    let (repo, _, _) = standard();
    let inputs = repo.validate().unwrap();
    repo.write("tests/a.sql", "SELECT 2\n");
    let found = inputs.reverify().unwrap_err();
    assert!(
        found
            .iter()
            .any(|r| matches!(r, Refusal::Changed { path, .. } if path == "tests/a.sql")),
        "{found:?}"
    );
}

#[test]
fn what_git_cannot_say_is_never_turned_into_an_answer() {
    use crate::baseline::ancestry::Evidence;
    assert_eq!(GitProblem::from_evidence(Evidence::Contains), None);
    assert_eq!(GitProblem::from_evidence(Evidence::Diverged), None);
    assert_eq!(
        GitProblem::from_evidence(Evidence::Incomplete),
        Some(GitProblem::Incomplete)
    );
    assert_eq!(
        GitProblem::from_evidence(Evidence::Missing { shallow: true }),
        Some(GitProblem::Missing { shallow: true })
    );
    assert_eq!(
        GitProblem::from_evidence(Evidence::NoCheckout),
        Some(GitProblem::NoCheckout)
    );
    // A shallow clone's missing or incomplete history cannot be answered.
    let unanswerable = |problem| {
        Refusal::Git {
            what: "x".into(),
            problem,
        }
        .outcome()
    };
    assert_eq!(unanswerable(GitProblem::Incomplete), Outcome::Unanswerable);
    assert_eq!(
        unanswerable(GitProblem::Missing { shallow: true }),
        Outcome::Unanswerable
    );
    assert_eq!(
        unanswerable(GitProblem::Missing { shallow: false }),
        Outcome::Finding
    );
}

#[test]
fn a_configuration_that_does_not_parse_at_a_pin_is_refused_at_that_pin() {
    let (repo, c1, _) = standard();
    repo.write("pbps.yml", "dialect: postgres\nsurprise: 1\n");
    let broken = repo.commit("broken config");
    repo.manifest(&broken, &[("f", &c1)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &broken)
            .iter()
            .any(|p| matches!(p, RevisionProblem::Config(_))),
        "{found:?}"
    );
}

/// A gone alternate object store leaves git unable to say "no such commit":
/// `rev-parse --quiet` still exits 1 for a full id, with the complaint on
/// stderr, and exits 0 for `HEAD` with the same complaint.
#[test]
fn a_failing_object_store_is_unreadable_not_a_missing_commit() {
    let (repo, _, c2) = standard();
    let alternates = repo.dir.join(".git/objects/info/alternates");
    std::fs::write(&alternates, "/nonexistent-pbps-objects\n").unwrap();
    let ghost = "ab".repeat(20);
    repo.manifest(&c2, &[], Some(&ghost));
    let found = refused(repo.validate());
    let candidate = found
        .iter()
        .find(|r| matches!(r, Refusal::Git { what, .. } if what.contains(&ghost)))
        .unwrap_or_else(|| panic!("{found:?}"));
    assert!(
        matches!(
            candidate,
            Refusal::Git {
                problem: GitProblem::Unreadable(_),
                ..
            }
        ),
        "{candidate:?}"
    );
    assert_eq!(candidate.outcome(), Outcome::Unanswerable);
    // The same complaint beside a successful answer is not an ambiguity.
    repo.manifest(&c2, &[], None);
    let found = refused(repo.validate());
    assert!(
        found.iter().any(|r| matches!(r, Refusal::Git { what, problem: GitProblem::Unreadable(_) } if what.starts_with("HEAD"))),
        "{found:?}"
    );
    assert!(!found.iter().any(|r| matches!(
        r,
        Refusal::Git {
            problem: GitProblem::Ambiguous(_),
            ..
        }
    )));
}

#[test]
fn a_directory_git_cannot_look_for_is_unreadable_not_absent() {
    let (repo, _, c2) = standard();
    let root = repo.dir.canonicalize().unwrap();
    assert_eq!(
        revision::git_objects_tree_exists(&root, &c2, "schema"),
        Ok(true)
    );
    assert_eq!(
        revision::git_objects_tree_exists(&root, &c2, "nowhere"),
        Ok(false)
    );
    std::fs::write(
        repo.dir.join(".git/objects/info/alternates"),
        "/nonexistent-pbps-objects\n",
    )
    .unwrap();
    let found = revision::git_objects_tree_exists(&root, &c2, "nowhere");
    assert!(
        matches!(found, Err(RevisionProblem::Unreadable { .. })),
        "{found:?}"
    );
}

#[test]
fn a_submodule_is_refused_wherever_it_stands_among_the_inputs_never_read_as_nothing() {
    let (repo, c1, c2) = standard();
    let link = |path: &str| {
        repo.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{c2},{path}"),
        ]);
    };
    let not_regular = |commit: &CommitId, path: &str| {
        let found = refused(repo.validate());
        let problems = at(&found, commit);
        assert!(
            problems.iter().any(|p| matches!(p, RevisionProblem::NotRegular { path: p, what } if p == path && what.contains("submodule"))),
            "{path}: {found:?}"
        );
    };
    // Named like a declaration, and named like nothing: `plan` would read
    // what is checked out under either, which a commit cannot give.
    for (path, label) in [
        ("schema/vendored.yml", "declaration-named"),
        ("schema/vendor", "plain"),
        ("schema/nested/vendor", "nested"),
        ("schema.ids.json", "identity file"),
        ("pbps.yml", "configuration"),
    ] {
        repo.git(&["rm", "-q", "--cached", "-r", "--ignore-unmatch", path]);
        link(path);
        // Not `Repo::commit`: `add -A` would stage the missing gitlink away.
        repo.git(&["commit", "-q", "-m", label]);
        let commit = repo.head();
        repo.manifest(&commit, &[("f", &c1)], None);
        not_regular(&commit, path);
        repo.git(&["reset", "-q", "--hard", &c2.to_string()]);
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_to_a_directory_is_refused_whatever_it_is_called() {
    let (repo, c1, _) = standard();
    std::fs::create_dir_all(repo.project.join("schema/real")).unwrap();
    repo.write(
        "schema/real/extra.yml",
        "table: app.extra\ncolumns:\n  id: {type: integer, nullable: false}\nprimary_key: [id]\n",
    );
    std::os::unix::fs::symlink("real", repo.project.join("schema/views")).unwrap();
    let linked = repo.commit("a link to a directory of declarations");
    repo.manifest(&linked, &[("f", &c1)], None);
    let found = refused(repo.validate());
    assert!(
        at(&found, &linked).iter().any(
            |p| matches!(p, RevisionProblem::NotRegular { path, .. } if path == "schema/views")
        ),
        "{found:?}"
    );
}

#[cfg(unix)]
#[test]
fn working_declarations_whose_names_are_not_utf8_are_refused_not_merged_by_lossy_display() {
    use std::os::unix::ffi::OsStrExt as _;
    let (repo, _, _) = standard();
    let name = std::ffi::OsStr::from_bytes(b"\xff.yml");
    std::fs::write(repo.project.join("schema").join(name), "table: app.x\n").unwrap();
    std::fs::write(repo.project.join("schema/\u{fffd}.yml"), "table: app.y\n").unwrap();
    let found = refused(repo.validate());
    assert!(
        found.iter().any(|r| matches!(
            r,
            Refusal::At {
                revision: Revision::Working,
                problem: RevisionProblem::NotUtf8 { .. }
            }
        )),
        "{found:?}"
    );
}
