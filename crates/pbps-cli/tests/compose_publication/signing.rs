//! The reviewed signing selectors, not whatever configuration commit-tree
//! reads later, sign the exact commit (#770).

use super::*;

fn key(f: &Fixture, name: &str) -> PathBuf {
    let key = f.repo.root.join(format!(".git/{name}"));
    checked(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .output()
            .unwrap(),
    );
    key
}

/// Required SSH signing with `reviewed`; only that key is an allowed signer,
/// so `verify-commit` fails for a commit signed by any other key.
fn signed(f: &Fixture) -> (PathBuf, PathBuf) {
    let (reviewed, other) = (key(f, "reviewed-key"), key(f, "other-key"));
    let allowed = f.repo.root.join(".git/allowed-signers");
    fs::write(
        &allowed,
        format!(
            "compose@example.test {}",
            fs::read_to_string(reviewed.with_extension("pub")).unwrap()
        ),
    )
    .unwrap();
    for (name, value) in [
        ("gpg.format", "ssh"),
        ("user.signingkey", reviewed.to_str().unwrap()),
        ("commit.gpgSign", "true"),
        ("gpg.ssh.allowedSignersFile", allowed.to_str().unwrap()),
    ] {
        git(&f.repo.root, &["config", name, value]);
    }
    (reviewed, other)
}

fn signature(f: &Fixture, commit: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(&f.repo.root)
        .args(["verify-commit", commit])
        .output()
        .unwrap()
        .status
        .success()
}

/// Confirms with `change` applied after the last policy check and the durable
/// commit intent, immediately before commit-tree reads configuration.
fn confirm_changed(f: &Fixture, change: impl Fn()) -> (Preview, Outcome) {
    let (_store, preview, candidate) = f.ready();
    let before = f.repo.preserved();
    let fired = std::cell::Cell::new(false);
    let result = f.publisher().confirm_observed(&candidate, &|at| {
        if at == Boundary::AfterPersist(DurableStage::Preparing) {
            change();
            fired.set(true);
        }
        true
    });
    assert!(fired.get());
    f.source_unchanged(&before);
    (preview, result)
}

fn delivered_as_reviewed(f: &Fixture, preview: &Preview, result: &Outcome, signed: bool) {
    assert_eq!(result.status, Status::Delivered, "{result:?}");
    let exact = commit(result);
    assert_eq!(signature(f, exact), signed, "{exact}");
    assert_eq!(
        git(&f.repo.root, &["rev-parse", &format!("{exact}^{{tree}}")]),
        format!("{}\n", preview.tree).as_bytes()
    );
    assert_eq!(
        git(&f.repo.root, &["rev-parse", &format!("{exact}^")]),
        format!("{}\n", preview.base).as_bytes()
    );
    assert_eq!(f.remote_ref(&preview.output_ref).as_deref(), Some(exact));
    assert_eq!(
        serde_json::to_value(&preview.signing).unwrap()["required"],
        signed
    );
    // Restart returns the recorded commit; nothing is signed again.
    assert_eq!(&f.publisher().retry(&preview.operation_id), result);
}

#[test]
fn a_selector_changed_after_the_policy_check_does_not_choose_the_signature() {
    for change in ["key", "format", "unset"] {
        let f = Fixture::new(&format!("signing-{change}"));
        let (_, other) = signed(&f);
        let root = f.repo.root.clone();
        let (preview, result) = confirm_changed(&f, || {
            let _ = match change {
                "key" => git(
                    &root,
                    &["config", "user.signingkey", other.to_str().unwrap()],
                ),
                "format" => git(&root, &["config", "gpg.format", "openpgp"]),
                _ => {
                    git(&root, &["config", "--unset", "user.signingkey"]);
                    git(&root, &["config", "--unset", "gpg.format"])
                }
            };
        });
        delivered_as_reviewed(&f, &preview, &result, true);
    }
}

#[test]
fn an_unavailable_reviewed_key_never_yields_an_unsigned_commit() {
    let f = Fixture::new("signing-unavailable");
    let (reviewed, _) = signed(&f);
    let (preview, result) = confirm_changed(&f, || fs::remove_file(&reviewed).unwrap());
    assert_ne!(result.status, Status::Delivered, "{result:?}");
    assert_eq!(
        result.problem,
        Some(Problem::CommitUnavailable),
        "{result:?}"
    );
    assert_eq!(result.details.as_ref().unwrap().commit, None);
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    assert_eq!(refs(&f), b"", "no output ref may name a replacement commit");
}

#[test]
fn a_reviewed_unsigned_policy_is_not_upgraded_by_later_configuration() {
    let f = Fixture::new("signing-default");
    let reviewed = key(&f, "late-key");
    let root = f.repo.root.clone();
    let (preview, result) = confirm_changed(&f, || {
        for (name, value) in [
            ("gpg.format", "ssh"),
            ("user.signingkey", reviewed.to_str().unwrap()),
            ("commit.gpgSign", "true"),
        ] {
            git(&root, &["config", name, value]);
        }
    });
    delivered_as_reviewed(&f, &preview, &result, false);
}

fn refs(f: &Fixture) -> Vec<u8> {
    git(
        &f.repo.root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/pbps-compose/",
        ],
    )
}
