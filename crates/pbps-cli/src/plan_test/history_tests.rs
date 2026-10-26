use super::*;
use std::path::PathBuf;
use std::process::Command;

const IMAGE: &str =
    "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280";

/// A git repository holding a project in `sub` (empty for the repository root).
struct Repo {
    dir: PathBuf,
    project: PathBuf,
}

impl Repo {
    fn new(sub: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("pbps-history-{:032x}", rand::random::<u128>()));
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

    /// The manifest at `tests/pbps-tests.yml`, with one SQL file per fixture
    /// and one assertion.
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

    fn validate(&self) -> Result<History, Vec<Refusal>> {
        validate_history(&self.project, &self.project.join("tests/pbps-tests.yml"))
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Pins at `c1`, base `c2`: the ordinary shape.
fn standard() -> (Repo, CommitId, CommitId) {
    let repo = Repo::new("");
    repo.write("one.txt", "1");
    let c1 = repo.commit("one");
    repo.write("two.txt", "2");
    let c2 = repo.commit("two");
    repo.manifest(&c2, &[("f", &c1)], None);
    (repo, c1, c2)
}

fn refused(result: Result<History, Vec<Refusal>>) -> Vec<Refusal> {
    match result {
        Ok(_) => panic!("expected refusals"),
        Err(refusals) => refusals,
    }
}

#[test]
fn the_ordinary_shape_validates_and_reports_its_chain_and_candidate() {
    let (repo, c1, c2) = standard();
    let history = repo.validate().unwrap();
    assert_eq!(history.chain.groups.len(), 2);
    assert_eq!(history.chain.groups[0].commit, c1);
    assert_eq!(history.chain.groups[0].fixtures, [0]);
    assert_eq!(history.chain.groups[1].commit, c2);
    assert_eq!(history.chain.hops.len(), 1);
    assert_eq!(history.chain.hops[0].distance, 1);
    assert_eq!(history.base, c2);
    assert_eq!(history.pins[0].commit, c1);
    assert_eq!(history.candidate.kind, CandidateKind::WorkingTree);
    assert!(history.candidate.uncommitted);
    assert_eq!(history.candidate.commit, c2, "the captured HEAD");
}

#[test]
fn an_explicit_commit_candidate_is_labelled_committed() {
    let (repo, _, c2) = standard();
    repo.manifest(&c2, &[], Some("HEAD"));
    let history = repo.validate().unwrap();
    assert_eq!(history.candidate.kind, CandidateKind::Commit);
    assert!(!history.candidate.uncommitted);
    assert_eq!(history.candidate.commit, c2);
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
    repo.manifest(&c2, &[("f", &ghost)], Some("no-such-branch"));
    let found = refused(repo.validate());
    assert!(
        found.iter().any(|r| matches!(r, Refusal::Git { what, problem: GitProblem::Missing { shallow: false } } if what.contains("fixture `f`"))),
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .any(|r| matches!(r, Refusal::Git { what, .. } if what.contains("no-such-branch"))),
        "both are reported: {found:?}"
    );
}

#[test]
fn a_repository_with_no_commits_refuses_head_and_the_base_together() {
    let repo = Repo::new("");
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
    repo.write("root.txt", "r");
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
    let history = repo.validate().unwrap();
    let order: Vec<_> = history
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
    repo.write("root.txt", "r");
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
    repo.write("root.txt", "r");
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
    let history = repo.validate().unwrap();
    assert_eq!(
        history.chain.hops[0].distance, 4,
        "m1, s1, s2 and the merge"
    );
}

#[test]
fn a_project_below_the_repository_root_is_validated_the_same_way() {
    let repo = Repo::new("apps/db");
    repo.write("note.txt", "x");
    let c1 = repo.commit("one");
    repo.manifest(&c1, &[("f", &c1)], None);
    let history = repo.validate().unwrap();
    assert_eq!(history.chain.groups.len(), 1);
    assert_eq!(history.chain.groups[0].fixtures, [0]);
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
    let outcome = |problem| {
        Refusal::Git {
            what: "x".into(),
            problem,
        }
        .outcome()
    };
    assert_eq!(outcome(GitProblem::Incomplete), Outcome::Unanswerable);
    assert_eq!(
        outcome(GitProblem::Missing { shallow: true }),
        Outcome::Unanswerable
    );
    assert_eq!(
        outcome(GitProblem::Missing { shallow: false }),
        Outcome::Finding
    );
    let a = CommitId::parse(&"a1".repeat(20)).unwrap();
    let b = CommitId::parse(&"b2".repeat(20)).unwrap();
    assert_eq!(
        Refusal::Chain(ChainProblem::Unknown {
            ancestor: a.clone(),
            descendant: b.clone(),
            problem: GitProblem::Incomplete,
        })
        .outcome(),
        Outcome::Unanswerable
    );
    assert_eq!(
        Refusal::Chain(ChainProblem::Incomparable { a, b }).outcome(),
        Outcome::Finding
    );
}

#[test]
fn reverify_passes_when_nothing_moved_and_names_a_moved_head_and_an_edited_file() {
    let (repo, _, c2) = standard();
    let history = repo.validate().unwrap();
    history.reverify().unwrap();
    repo.write("tests/a.sql", "SELECT 2\n");
    let found = history.reverify().unwrap_err();
    assert!(
        found
            .iter()
            .any(|r| matches!(r, Refusal::Changed { path, .. } if path == "tests/a.sql")),
        "{found:?}"
    );
    repo.write("tests/a.sql", "SELECT 1 WHERE false\n");
    history.reverify().unwrap();
    repo.write("elsewhere.txt", "x");
    let now = repo.commit("another");
    let found = history.reverify().unwrap_err();
    assert_eq!(found, [Refusal::HeadMoved { captured: c2, now }]);
    assert_eq!(found[0].outcome(), Outcome::Unanswerable);
}

#[test]
fn a_commit_candidate_is_not_bound_to_head_after_it_is_resolved() {
    let (repo, _, c2) = standard();
    repo.manifest(&c2, &[], Some("HEAD"));
    let history = repo.validate().unwrap();
    repo.write("elsewhere.txt", "x");
    repo.commit("another");
    history.reverify().unwrap();
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
