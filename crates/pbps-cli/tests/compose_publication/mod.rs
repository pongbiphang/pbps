//! Real publication, lost acknowledgements and restart from durable receipts.

use super::*;
use pbps_ui::compose::{
    Candidate, DeliveryState, DurableStage, LocalState, Outcome, Preview, Problem,
    PublicationBoundary as Boundary, Publications, Status,
};

struct Fixture {
    repo: Repository,
    remote: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let repo = Repository::new(&format!("publication-{name}"), "project");
        let remote = repo.root.with_extension("remote.git");
        let _ = fs::remove_dir_all(&remote);
        git(
            &repo.root,
            &["clone", "--bare", "-q", ".", remote.to_str().unwrap()],
        );
        git(
            &repo.root,
            &["remote", "set-url", "origin", remote.to_str().unwrap()],
        );
        repo.table(RENAMED);
        Self { repo, remote }
    }
    fn ready(&self) -> (Candidates, Preview, Arc<Candidate>) {
        let mut candidates = self.repo.store();
        let now = SystemTime::now();
        let preview = candidates.preview(request(), now).unwrap();
        let candidate = candidates.confirm(&preview.candidate_id, now).unwrap();
        (candidates, preview, candidate)
    }
    fn publisher(&self) -> Publications {
        Publications::open(&self.repo.root, Duration::from_secs(15)).unwrap()
    }
    fn remote_ref(&self, reference: &str) -> Option<String> {
        let result = Command::new("git")
            .arg("-C")
            .arg(&self.remote)
            .args(["rev-parse", "--verify", "--quiet", reference])
            .output()
            .unwrap();
        result
            .status
            .success()
            .then(|| String::from_utf8(result.stdout).unwrap().trim().to_owned())
    }
    fn source_unchanged(&self, before: &[Vec<u8>]) {
        assert_eq!(&self.repo.preserved()[..4], &before[..4]);
        let old = String::from_utf8(before[4].clone()).unwrap();
        let master = old
            .lines()
            .find(|line| line.ends_with(" refs/heads/master"))
            .unwrap()
            .split(' ')
            .next()
            .unwrap();
        assert_eq!(
            String::from_utf8(git(&self.repo.root, &["rev-parse", "refs/heads/master"]))
                .unwrap()
                .trim(),
            master
        );
    }
    fn record(&self, operation: &str) -> PathBuf {
        self.repo
            .root
            .join(format!(".git/pbps-compose-v2/{operation}.json"))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.remote);
    }
}
fn commit(outcome: &Outcome) -> &str {
    outcome.details.as_ref().unwrap().commit.as_deref().unwrap()
}
fn generation(outcome: &Outcome) -> String {
    outcome
        .details
        .as_ref()
        .unwrap()
        .delivery_generation
        .clone()
        .unwrap()
}

#[test]
fn publication_uses_the_frozen_tree_and_restart_returns_the_same_receipt() {
    let f = Fixture::new("ordinary");
    let (_candidates, preview, candidate) = f.ready();
    f.repo.table(&format!("{RENAMED}  later: {{type: int}}\n"));
    fs::write(f.repo.root.join("staged"), "keep staged").unwrap();
    git(&f.repo.root, &["add", "staged"]);
    fs::write(f.repo.root.join(".git/index.lock"), "foreign lock").unwrap();
    let before = f.repo.preserved();
    let mut publisher = f.publisher();
    let result = publisher.confirm(&candidate);
    assert_eq!(result.status, Status::Delivered, "{result:?}");
    assert_eq!(result.local, LocalState::Present);
    assert_eq!(result.remote, DeliveryState::Delivered);
    assert_eq!(
        f.remote_ref(&preview.output_ref).as_deref(),
        Some(commit(&result))
    );
    assert_eq!(
        git(
            &f.repo.root,
            &["show", &format!("{}:project/schema/t.yml", commit(&result))]
        ),
        RENAMED.as_bytes()
    );
    assert_eq!(
        String::from_utf8(git(
            &f.repo.root,
            &["show", "-s", "--format=%T%n%P", commit(&result)]
        ))
        .unwrap(),
        format!("{}\n{}\n", preview.tree, preview.base)
    );
    f.source_unchanged(&before);
    assert_eq!(
        fs::read_to_string(f.repo.root.join(".git/index.lock")).unwrap(),
        "foreign lock"
    );
    assert_eq!(publisher.confirm(&candidate), result);
    assert!(Publications::open(&f.repo.root, Duration::from_secs(5)).is_err());
    drop(publisher);
    let mut restarted = f.publisher();
    assert_eq!(restarted.recover(&preview.operation_id), result);
    assert_eq!(restarted.list().unwrap(), vec![result]);
}

#[test]
fn failed_prerequisite_persistence_cannot_publish_and_retry_keeps_the_exact_commit() {
    let f = Fixture::new("authorization");
    let (_, preview, candidate) = f.ready();
    let before = f.repo.preserved();
    let mut publisher = f.publisher();
    let result = publisher.confirm_observed(&candidate, &|at| {
        at != Boundary::BeforePersist(DurableStage::LocalAttempt)
    });
    assert_eq!(result.local, LocalState::NotAttempted, "{result:?}");
    assert!(
        !f.repo
            .root
            .join(format!(".git/{}", preview.output_ref))
            .exists()
    );
    assert!(
        !f.repo
            .root
            .join(format!(".git/{}.lock", preview.output_ref))
            .exists()
    );
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    f.source_unchanged(&before);
    let retried = publisher.retry(&preview.operation_id);
    assert_eq!(retried.status, Status::Delivered, "{retried:?}");
    assert_eq!(commit(&result), commit(&retried));
}

#[test]
fn lost_local_acknowledgement_and_post_write_record_failure_share_recovery() {
    for boundary in [
        Boundary::RefInstalled,
        Boundary::BeforePersist(DurableStage::LocalPublished),
        Boundary::AfterPersist(DurableStage::LocalAttempt),
    ] {
        let f = Fixture::new(&format!("local-{boundary:?}"));
        let (_, preview, candidate) = f.ready();
        let before = f.repo.preserved();
        let mut publisher = f.publisher();
        let result = publisher.confirm_observed(&candidate, &|at| {
            at != boundary && at != Boundary::BeforeReconcile
        });
        assert!(result.details.as_ref().unwrap().commit.is_some());
        f.source_unchanged(&before);
        drop(publisher);
        let mut restarted = f.publisher();
        let recovered = restarted.recover(&preview.operation_id);
        assert_eq!(commit(&recovered), commit(&result));
        if boundary == Boundary::AfterPersist(DurableStage::LocalAttempt) {
            assert_eq!(recovered.status, Status::PublicationUnknown);
            assert_eq!(
                restarted.retry(&preview.operation_id).status,
                Status::PublicationUnknown
            );
            assert_eq!(f.remote_ref(&preview.output_ref), None);
        } else {
            assert_eq!(recovered.local, LocalState::Present, "{recovered:?}");
            assert_eq!(recovered.remote, DeliveryState::NotAttempted);
            assert_eq!(
                restarted.retry(&preview.operation_id).status,
                Status::Delivered
            );
        }
    }
}

#[test]
fn missing_commit_acknowledgement_never_generates_a_second_commit() {
    let f = Fixture::new("commit-ack");
    let (_, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    let first = publisher.confirm_observed(&candidate, &|at| at != Boundary::CommitCreated);
    assert_eq!(first.status, Status::PreparationUnknown);
    assert!(first.details.as_ref().unwrap().commit.is_none());
    let objects = git(
        &f.repo.root,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
    );
    assert_eq!(
        publisher.confirm(&candidate).status,
        Status::PreparationUnknown
    );
    drop(publisher);
    assert_eq!(
        f.publisher().retry(&preview.operation_id).status,
        Status::PreparationUnknown
    );
    assert_eq!(
        git(
            &f.repo.root,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname) %(objecttype)"
            ]
        ),
        objects
    );
    assert_eq!(f.remote_ref(&preview.output_ref), None);
}

#[test]
fn unknown_push_then_independent_deletion_requires_fresh_informed_authorization() {
    let f = Fixture::new("push-ack");
    let (_, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    let lost = |at| {
        if at == Boundary::PushFinished {
            git(&f.remote, &["update-ref", "-d", &preview.output_ref]);
            return false;
        }
        true
    };
    let first = publisher.confirm_observed(&candidate, &lost);
    assert_eq!(first.local, LocalState::Present, "{first:?}");
    assert_eq!(first.remote, DeliveryState::Unknown);
    let first_generation = generation(&first);
    assert_eq!(
        publisher.retry(&preview.operation_id).remote,
        DeliveryState::Unknown
    );
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    drop(publisher);
    let mut publisher = f.publisher();
    assert_eq!(
        publisher.recover(&preview.operation_id).remote,
        DeliveryState::Unknown
    );
    let second = publisher.republish_observed(&preview.operation_id, &first_generation, &lost);
    assert_eq!(commit(&first), commit(&second));
    assert_ne!(first_generation, generation(&second));
    assert_eq!(
        publisher
            .republish(&preview.operation_id, &first_generation)
            .remote,
        DeliveryState::Unknown
    );
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    let delivered = publisher.republish(&preview.operation_id, &generation(&second));
    assert_eq!(delivered.status, Status::Delivered, "{delivered:?}");
    assert_eq!(commit(&first), commit(&delivered));
    // Even an explicit no-op while already delivered consumes authorization.
    let token = generation(&delivered);
    let noop = publisher.republish(&preview.operation_id, &token);
    assert_eq!(noop.status, Status::Delivered);
    assert_ne!(generation(&noop), token);
    git(&f.remote, &["update-ref", "-d", &preview.output_ref]);
    assert_eq!(
        publisher.republish(&preview.operation_id, &token).remote,
        DeliveryState::Changed
    );
    assert_eq!(f.remote_ref(&preview.output_ref), None);
}

#[test]
fn deleted_local_publication_and_unreadable_receipts_are_never_empty_evidence() {
    let f = Fixture::new("local-deleted");
    let (_, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    let result = publisher.confirm_observed(&candidate, &|at| {
        if at == Boundary::RefInstalled {
            git(&f.repo.root, &["update-ref", "-d", &preview.output_ref]);
            return false;
        }
        true
    });
    assert_eq!(result.status, Status::PublicationUnknown);
    assert_eq!(
        publisher.retry(&preview.operation_id).status,
        Status::PublicationUnknown
    );
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    fs::write(f.record(&preview.operation_id), "invalid receipt").unwrap();
    assert_eq!(
        publisher.recover(&preview.operation_id).problem,
        Some(Problem::ReceiptUnavailable)
    );
    assert_eq!(
        publisher.confirm(&candidate).problem,
        Some(Problem::ReceiptUnavailable)
    );
    assert!(publisher.list().is_err());
    assert_eq!(
        fs::read_to_string(f.record(&preview.operation_id)).unwrap(),
        "invalid receipt"
    );
}

#[test]
fn direct_symbolic_and_unborn_checked_out_collisions_preserve_the_source() {
    for kind in ["direct", "symbolic", "unborn"] {
        let f = Fixture::new(kind);
        let (_, preview, candidate) = f.ready();
        match kind {
            "direct" => {
                git(
                    &f.repo.root,
                    &["update-ref", &preview.output_ref, &preview.base],
                );
            }
            "symbolic" => {
                git(
                    &f.repo.root,
                    &["symbolic-ref", &preview.output_ref, "refs/heads/missing"],
                );
            }
            "unborn" => {
                let other = f.repo.root.join("other-checkout");
                git(
                    &f.repo.root,
                    &["worktree", "add", "--detach", other.to_str().unwrap()],
                );
                git(&other, &["symbolic-ref", "HEAD", &preview.output_ref]);
            }
            _ => unreachable!(),
        }
        let before = f.repo.preserved();
        let result = f.publisher().confirm(&candidate);
        assert_eq!(
            result.problem,
            Some(Problem::RefCollision),
            "{kind}: {result:?}"
        );
        assert_eq!(f.repo.preserved(), before);
        assert_eq!(f.remote_ref(&preview.output_ref), None);
        assert!(
            !f.repo
                .root
                .join(format!(".git/{}.lock", preview.output_ref))
                .exists()
        );
    }
}

#[test]
fn unpublished_ancestry_and_required_signer_failure_refuse_without_unsigned_fallback() {
    let f = Fixture::new("ancestry");
    f.repo.table(ORIGINAL);
    fs::write(f.repo.root.join("unpublished"), "local history").unwrap();
    f.repo.commit();
    f.repo.table(RENAMED);
    let (_, preview, candidate) = f.ready();
    let result = f.publisher().confirm(&candidate);
    assert_eq!(result.problem, Some(Problem::RemoteBaseChanged));
    assert_eq!(result.status, Status::Refused);
    assert_eq!(f.remote_ref(&preview.output_ref), None);

    let f = Fixture::new("signing");
    let signer = f.repo.root.join("refuse-signing");
    fs::write(
        &signer,
        "#!/bin/sh\nprintf FAKE_SIGNER_SECRET >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&signer, fs::Permissions::from_mode(0o700)).unwrap();
    git(&f.repo.root, &["config", "commit.gpgSign", "true"]);
    git(
        &f.repo.root,
        &["config", "gpg.program", signer.to_str().unwrap()],
    );
    let (_, preview, candidate) = f.ready();
    let before = f.repo.preserved();
    let mut publisher = f.publisher();
    let result = publisher.confirm(&candidate);
    assert_eq!(
        result.problem,
        Some(Problem::CommitUnavailable),
        "{result:?}"
    );
    assert!(result.details.as_ref().unwrap().commit.is_none());
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    assert_eq!(f.repo.preserved(), before);
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("FAKE_SIGNER_SECRET")
    );
    assert!(
        !fs::read_to_string(f.record(&preview.operation_id))
            .unwrap()
            .contains("FAKE_SIGNER_SECRET")
    );
    assert_eq!(
        publisher.retry(&preview.operation_id).status,
        Status::PreparationUnknown
    );
}

#[test]
fn destination_changes_and_remote_base_changes_do_not_reuse_reviewed_authority() {
    let f = Fixture::new("destination");
    let (_, preview, candidate) = f.ready();
    git(
        &f.repo.root,
        &[
            "remote",
            "set-url",
            "origin",
            "https://other.example.test/repo",
        ],
    );
    assert_eq!(
        f.publisher().confirm(&candidate).problem,
        Some(Problem::DestinationChanged)
    );
    assert_eq!(f.remote_ref(&preview.output_ref), None);

    let f = Fixture::new("base-change");
    let (_, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    let result = publisher.confirm_observed(&candidate, &|at| {
        if at == Boundary::CommitCreated {
            git(&f.remote, &["update-ref", "-d", "refs/heads/master"]);
        }
        true
    });
    assert_eq!(
        result.problem,
        Some(Problem::RemoteBaseChanged),
        "{result:?}"
    );
    assert_eq!(result.local, LocalState::NotAttempted);
    assert_eq!(f.remote_ref(&preview.output_ref), None);
}

#[test]
fn an_explicit_alternative_owns_a_new_preview_and_commit_while_reconfirm_keeps_the_result() {
    let f = Fixture::new("alternative");
    let (mut candidates, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    let first = publisher.confirm(&candidate);
    assert_eq!(first.status, Status::Delivered);
    assert!(candidates.preview(request(), SystemTime::now()).is_err());
    assert!(
        publisher
            .start_alternative(&mut candidates, "invalid")
            .is_err()
    );
    assert_eq!(publisher.confirm(&candidate), first);
    publisher
        .start_alternative(&mut candidates, &preview.operation_id)
        .unwrap();
    let mut alternative = request();
    alternative.message = "A separately reviewed alternative".into();
    let second = candidates.preview(alternative, SystemTime::now()).unwrap();
    assert_ne!(preview.operation_id, second.operation_id);
    assert_eq!(preview.base, second.base);
    let candidate = candidates
        .confirm(&second.candidate_id, SystemTime::now())
        .unwrap();
    let result = publisher.confirm(&candidate);
    assert_eq!(result.status, Status::Delivered);
    assert_ne!(
        result.details.unwrap().commit,
        first.details.unwrap().commit
    );
    assert_eq!(publisher.list().unwrap().len(), 2);
}

mod http;

#[test]
fn unchanged_large_blobs_outside_the_project_are_not_repacked_for_publication() {
    use std::io::Read;
    let f = Fixture::new("large-base");
    let mut bytes = vec![0; 64 * 1024 * 1024 + 4096];
    fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut bytes)
        .unwrap();
    fs::write(f.repo.root.join("unrelated.bin"), &bytes).unwrap();
    drop(bytes);
    git(&f.repo.root, &["add", "unrelated.bin"]);
    git(&f.repo.root, &["commit", "-qm", "unrelated large base"]);
    git(&f.repo.root, &["push", "-q", "origin", "master"]);
    let (_store, _, candidate) = f.ready();
    let before = f.repo.preserved();
    let result = f.publisher().confirm(&candidate);
    assert_eq!(result.status, Status::Delivered, "{result:?}");
    f.source_unchanged(&before);
}

#[test]
fn required_ssh_signing_uses_the_reviewed_tree_and_never_repeats_on_restart() {
    let f = Fixture::new("ssh-sign");
    let key = f.repo.root.join(".git/fixture-key");
    checked(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .output()
            .unwrap(),
    );
    git(&f.repo.root, &["config", "gpg.format", "ssh"]);
    git(
        &f.repo.root,
        &["config", "user.signingkey", key.to_str().unwrap()],
    );
    git(&f.repo.root, &["config", "commit.gpgSign", "true"]);
    let allowed = f.repo.root.join(".git/allowed-signers");
    fs::write(
        &allowed,
        format!(
            "compose@example.test {}",
            fs::read_to_string(key.with_extension("pub")).unwrap()
        ),
    )
    .unwrap();
    git(
        &f.repo.root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            allowed.to_str().unwrap(),
        ],
    );
    let (_store, preview, candidate) = f.ready();
    assert_eq!(
        serde_json::to_value(&preview.signing).unwrap()["required"],
        true
    );
    let before = f.repo.preserved();
    let mut publisher = f.publisher();
    let first = publisher.confirm(&candidate);
    assert_eq!(first.status, Status::Delivered, "{first:?}");
    let commit = first.details.as_ref().unwrap().commit.as_ref().unwrap();
    git(&f.repo.root, &["verify-commit", commit]);
    assert_eq!(
        git(&f.repo.root, &["rev-parse", &format!("{commit}^{{tree}}")]),
        format!("{}\n", preview.tree).as_bytes()
    );
    drop(publisher);
    fs::remove_file(key).unwrap();
    assert_eq!(f.publisher().retry(&preview.operation_id), first);
    f.source_unchanged(&before);
}

#[test]
fn changed_signing_policy_before_commit_and_replaced_repository_refuse_publication() {
    let f = Fixture::new("signing-drift");
    let (_store, preview, candidate) = f.ready();
    let before = f.repo.preserved();
    let result = f.publisher().confirm_observed(&candidate, &|at| {
        if at == Boundary::BeforeCommit {
            git(&f.repo.root, &["config", "commit.gpgSign", "true"]);
        }
        true
    });
    assert_eq!(result.problem, Some(Problem::SigningChanged));
    assert_eq!(result.details.unwrap().commit, None);
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    f.source_unchanged(&before);

    let f = Fixture::new("repository-drift");
    let (_store, preview, candidate) = f.ready();
    let old = f.repo.root.with_extension("old");
    fs::rename(&f.repo.root, &old).unwrap();
    git(
        &f.remote,
        &["clone", "-q", ".", f.repo.root.to_str().unwrap()],
    );
    let result = f.publisher().confirm(&candidate);
    assert_eq!(result.problem, Some(Problem::RepositoryChanged));
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    fs::remove_dir_all(&f.repo.root).unwrap();
    fs::rename(old, &f.repo.root).unwrap();
}

#[test]
fn alternative_authorization_cannot_switch_projects_or_silently_follow_a_new_base() {
    let f = Fixture::new("alternative-base");
    let (mut candidates, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    assert_eq!(publisher.confirm(&candidate).status, Status::Delivered);
    let mut other = Candidates::new(Config {
        executable: BIN.into(),
        project: f.repo.root.clone(),
        deadline: Duration::from_secs(15),
    });
    assert!(
        publisher
            .start_alternative(&mut other, &preview.operation_id)
            .is_err()
    );
    publisher
        .start_alternative(&mut candidates, &preview.operation_id)
        .unwrap();
    git(
        &f.repo.root,
        &["commit", "--allow-empty", "-qm", "source moved"],
    );
    assert!(candidates.preview(request(), SystemTime::now()).is_err());
    assert_eq!(publisher.list().unwrap().len(), 1);
}

#[test]
fn malformed_ref_evidence_and_outcome_record_failures_preserve_known_commit_details() {
    let f = Fixture::new("ref-unreadable");
    let (_store, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    let first = publisher.confirm(&candidate);
    assert_eq!(first.status, Status::Delivered);
    let reference = f.repo.root.join(".git").join(&preview.output_ref);
    fs::write(&reference, b"unreadable object id\n").unwrap();
    let result = publisher.recover(&preview.operation_id);
    assert_eq!(result.local, LocalState::Unavailable);
    assert_eq!(result.details, first.details);
    assert_eq!(fs::read(reference).unwrap(), b"unreadable object id\n");

    let f = Fixture::new("delivered-persist");
    let (_store, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    let outcome = publisher.confirm_observed(&candidate, &|at| {
        at != Boundary::BeforePersist(DurableStage::RemoteDelivered)
    });
    assert_eq!(outcome.status, Status::RecoveryRequired);
    assert_eq!(outcome.local, LocalState::Present);
    assert_eq!(outcome.remote, DeliveryState::Delivered);
    let commit = outcome.details.unwrap().commit;
    drop(publisher);
    let recovered = f.publisher().recover(&preview.operation_id);
    assert_eq!(recovered.status, Status::Delivered);
    assert_eq!(recovered.details.unwrap().commit, commit);
}

#[test]
fn each_destination_identity_component_is_revalidated_before_network_contact() {
    let cases = [
        ("https://old.invalid/repo", "https://new.invalid/repo"),
        (
            "https://old.invalid:443/repo",
            "https://old.invalid:8443/repo",
        ),
        ("https://old.invalid/repo", "https://old.invalid/other"),
        ("ssh://git@old.invalid/repo", "ssh://other@old.invalid/repo"),
        ("git@old.invalid:repo", "git@old.invalid:other"),
    ];
    for (n, (old, new)) in cases.into_iter().enumerate() {
        let f = Fixture::new(&format!("destination-component-{n}"));
        git(&f.repo.root, &["remote", "set-url", "origin", old]);
        let (_store, _, candidate) = f.ready();
        let before = f.repo.preserved();
        git(&f.repo.root, &["remote", "set-url", "origin", new]);
        let result = f.publisher().confirm(&candidate);
        assert_eq!(result.status, Status::Refused);
        assert_eq!(result.problem, Some(Problem::DestinationChanged));
        f.source_unchanged(&before);
    }
}

#[test]
fn late_dangling_symbolic_collision_is_rejected_under_the_prepared_ref_lock() {
    let f = Fixture::new("late-symbolic");
    let (_store, preview, candidate) = f.ready();
    let before = f.repo.preserved();
    let result = f.publisher().confirm_observed(&candidate, &|at| {
        if at == Boundary::CommitCreated {
            git(
                &f.repo.root,
                &[
                    "symbolic-ref",
                    &preview.output_ref,
                    "refs/heads/foreign-missing",
                ],
            );
        }
        true
    });
    assert_eq!(result.problem, Some(Problem::RefCollision));
    assert_eq!(
        git(&f.repo.root, &["symbolic-ref", &preview.output_ref]),
        b"refs/heads/foreign-missing\n"
    );
    assert!(
        !f.repo
            .root
            .join(".git")
            .join(format!("{}.lock", preview.output_ref))
            .exists()
    );
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    f.source_unchanged(&before);
}
