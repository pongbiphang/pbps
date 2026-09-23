//! A required push certificate is preflighted before the remote attempt,
//! bound to the push that follows, and never dropped (#775).

use super::*;
use std::os::unix::fs::PermissionsExt;

const SECRET: &str = "FAKE_SIGNER_SECRET";

/// A destination that records whether each accepted push carried a
/// certificate; `certificates` decides whether it advertises the capability.
fn destination(f: &Fixture, certificates: bool) {
    if certificates {
        git(
            &f.remote,
            &["config", "receive.certNonceSeed", "fixture-seed"],
        );
    }
    let hook = f.remote.join("hooks/pre-receive");
    fs::write(
        &hook,
        b"#!/bin/sh\ncat >/dev/null\nif [ -n \"$GIT_PUSH_CERT\" ]; then echo cert; else echo none; fi >> compose-pushes\n",
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o700)).unwrap();
}

fn pushes(f: &Fixture) -> Vec<String> {
    fs::read_to_string(f.remote.join("compose-pushes"))
        .map(|text| text.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// SSH signing for push certificates, with `mode` as `push.gpgSign`.
fn signer(f: &Fixture, mode: Option<&str>) {
    let key = f.repo.root.join(".git/push-key");
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
    if let Some(mode) = mode {
        git(&f.repo.root, &["config", "push.gpgSign", mode]);
    }
}

fn config(f: &Fixture) -> Vec<u8> {
    git(&f.repo.root, &["config", "--local", "--list"])
}

fn quiet(f: &Fixture, id: &str, result: &Outcome) {
    for text in [
        serde_json::to_string(result).unwrap(),
        fs::read_to_string(f.record(id)).unwrap_or_default(),
    ] {
        for marker in [SECRET, "receiving end", "fatal:"] {
            assert!(!text.contains(marker), "{marker} leaked: {text}");
        }
    }
    resources::private_bytes_exclude(f, SECRET);
}

fn refused_before_the_attempt(f: &Fixture, preview: &Preview, result: &Outcome) -> String {
    assert_eq!(result.local, LocalState::Present, "{result:?}");
    assert_eq!(result.remote, DeliveryState::NotAttempted, "{result:?}");
    assert_eq!(result.problem, Some(Problem::PushSigningUnsupported));
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    assert!(pushes(f).is_empty());
    quiet(f, &preview.operation_id, result);
    commit(result).to_owned()
}

#[test]
fn each_push_signing_mode_meets_its_destination_or_refuses_before_the_attempt() {
    for (mode, certificates, carried) in [
        (None, false, "none"),
        (Some("if-asked"), false, "none"),
        (Some("if-asked"), true, "cert"),
        (Some("true"), true, "cert"),
    ] {
        let f = Fixture::new(&format!("push-sign-{mode:?}-{certificates}"));
        destination(&f, certificates);
        signer(&f, mode);
        let (_store, preview, candidate) = f.ready();
        let (before, settings) = (f.repo.preserved(), config(&f));
        let result = f.publisher().confirm(&candidate);
        assert_eq!(result.status, Status::Delivered, "{mode:?} {result:?}");
        assert_eq!(
            f.remote_ref(&preview.output_ref).as_deref(),
            Some(commit(&result))
        );
        assert_eq!(pushes(&f), [carried], "{mode:?} {certificates}");
        assert_eq!(config(&f), settings);
        f.source_unchanged(&before);
    }
}

#[test]
fn an_unsupported_required_certificate_refuses_and_retries_the_same_commit_once_repaired() {
    for repair in ["capability", "configuration"] {
        let f = Fixture::new(&format!("push-sign-repair-{repair}"));
        destination(&f, false);
        signer(&f, Some("true"));
        let (_store, preview, candidate) = f.ready();
        let (before, settings) = (f.repo.preserved(), config(&f));
        let mut publisher = f.publisher();
        let refused = publisher.confirm(&candidate);
        let exact = refused_before_the_attempt(&f, &preview, &refused);
        assert_eq!(config(&f), settings);
        // Still unsupported: an ordinary retry refuses the same way.
        let again = publisher.retry(&preview.operation_id);
        assert_eq!(refused_before_the_attempt(&f, &preview, &again), exact);
        let carried = if repair == "capability" {
            git(
                &f.remote,
                &["config", "receive.certNonceSeed", "fixture-seed"],
            );
            "cert"
        } else {
            git(&f.repo.root, &["config", "push.gpgSign", "if-asked"]);
            "none"
        };
        let delivered = publisher.retry(&preview.operation_id);
        assert_eq!(delivered.status, Status::Delivered, "{delivered:?}");
        assert_eq!(commit(&delivered), exact);
        assert_eq!(pushes(&f), [carried]);
        f.source_unchanged(&before);
    }
}

#[test]
fn the_push_uses_the_requirement_its_preflight_checked() {
    let f = Fixture::new("push-sign-bound");
    destination(&f, true);
    signer(&f, Some("true"));
    let (_store, _preview, candidate) = f.ready();
    let root = f.repo.root.clone();
    let result = f.publisher().confirm_observed(&candidate, &|at| {
        if at == Boundary::BeforePersist(DurableStage::RemoteAttempt) {
            git(&root, &["config", "push.gpgSign", "false"]);
        }
        true
    });
    assert_eq!(result.status, Status::Delivered, "{result:?}");
    assert_eq!(pushes(&f), ["cert"]);
}

#[test]
fn a_failing_signer_is_uncertain_delivery_never_an_unsigned_push() {
    let f = Fixture::new("push-sign-signer");
    destination(&f, true);
    signer(&f, Some("true"));
    let program = f.repo.root.join(".git/failing-signer");
    fs::write(&program, format!("#!/bin/sh\necho {SECRET} >&2\nexit 1\n")).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    git(
        &f.repo.root,
        &["config", "gpg.ssh.program", program.to_str().unwrap()],
    );
    let (_store, preview, candidate) = f.ready();
    let before = f.repo.preserved();
    let mut publisher = f.publisher();
    let result = publisher.confirm(&candidate);
    // The preflight cannot sign, so the failure is after the attempt.
    assert_eq!(result.local, LocalState::Present, "{result:?}");
    assert_eq!(result.remote, DeliveryState::Unknown, "{result:?}");
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    assert!(pushes(&f).is_empty(), "an unsigned push reached the server");
    quiet(&f, &preview.operation_id, &result);
    let exact = commit(&result).to_owned();
    git(&f.repo.root, &["config", "--unset", "gpg.ssh.program"]);
    let diagnosed = publisher.recover(&preview.operation_id);
    let republished = publisher.republish(&preview.operation_id, &generation(&diagnosed));
    assert_eq!(republished.status, Status::Delivered, "{republished:?}");
    assert_eq!(commit(&republished), exact);
    assert_eq!(pushes(&f), ["cert"]);
    f.source_unchanged(&before);
}

#[test]
fn transport_loss_and_a_lost_acknowledgement_are_not_proof_of_no_write() {
    for lost in ["transport", "acknowledgement"] {
        let f = Fixture::new(&format!("push-sign-{lost}"));
        destination(&f, true);
        signer(&f, Some("true"));
        let (_store, preview, candidate) = f.ready();
        let moved = f.remote.with_extension("moved");
        let mut publisher = f.publisher();
        let result = publisher.confirm_observed(&candidate, &|at| match (lost, at) {
            ("transport", Boundary::BeforePush) => {
                fs::rename(&f.remote, &moved).unwrap();
                true
            }
            ("acknowledgement", Boundary::PushFinished) => false,
            _ => true,
        });
        assert_eq!(result.local, LocalState::Present, "{lost} {result:?}");
        let exact = commit(&result).to_owned();
        if lost == "acknowledgement" {
            // The push landed; reconciliation observes it rather than
            // replaying, so the certificate reached the server exactly once.
            assert_eq!(result.problem, Some(Problem::Interrupted));
            assert_eq!(result.remote, DeliveryState::Delivered, "{result:?}");
        } else {
            // A vanished destination is unavailable, never "not attempted".
            assert_eq!(result.remote, DeliveryState::Unavailable, "{result:?}");
            fs::rename(&moved, &f.remote).unwrap();
            assert!(pushes(&f).is_empty());
            let diagnosed = publisher.recover(&preview.operation_id);
            let delivered = publisher.republish(&preview.operation_id, &generation(&diagnosed));
            assert_eq!(delivered.status, Status::Delivered, "{delivered:?}");
            assert_eq!(commit(&delivered), exact);
        }
        assert_eq!(
            f.remote_ref(&preview.output_ref).as_deref(),
            Some(exact.as_str())
        );
        assert_eq!(pushes(&f), ["cert"]);
    }
}

fn refused_unsigned_or_unusable(
    f: &Fixture,
    preview: &Preview,
    result: &Outcome,
    problem: Problem,
) {
    assert_eq!(result.local, LocalState::Present, "{result:?}");
    assert_eq!(result.remote, DeliveryState::NotAttempted, "{result:?}");
    assert_eq!(result.problem, Some(problem), "{result:?}");
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    assert!(pushes(f).is_empty());
}

#[test]
fn every_push_signing_spelling_git_accepts_selects_its_mode() {
    // Against a server without certificates: a required spelling refuses
    // before the attempt, a disabled one delivers unsigned. Against one with
    // them, if-asked in any case sends a certificate.
    for (value, certificates, expected) in [
        ("yes", false, "required"),
        ("on", false, "required"),
        ("1", false, "required"),
        ("TRUE", false, "required"),
        ("bare", false, "required"),
        ("no", true, "none"),
        ("off", true, "none"),
        ("0", true, "none"),
        ("IF-ASKED", true, "cert"),
        ("If-Asked", false, "none"),
        ("bogus", true, "unusable"),
    ] {
        let f = Fixture::new(&format!("push-sign-spelling-{value}"));
        destination(&f, certificates);
        signer(&f, None);
        if value == "bare" {
            let config = f.repo.root.join(".git/config");
            let mut text = fs::read_to_string(&config).unwrap();
            text.push_str("[push]\n\tgpgSign\n");
            fs::write(&config, text).unwrap();
        } else {
            git(&f.repo.root, &["config", "push.gpgSign", value]);
        }
        let (_store, preview, candidate) = f.ready();
        let result = f.publisher().confirm(&candidate);
        match expected {
            "required" => {
                refused_unsigned_or_unusable(&f, &preview, &result, Problem::PushSigningUnsupported)
            }
            "unusable" => {
                refused_unsigned_or_unusable(&f, &preview, &result, Problem::SigningUnavailable)
            }
            carried => {
                assert_eq!(result.status, Status::Delivered, "{value}: {result:?}");
                assert_eq!(pushes(&f), [carried], "{value}");
            }
        }
    }
}

#[test]
#[ignore = "subprocess run under a counting git wrapper by the single-read regression"]
fn push_signing_count_child() {
    let repository = PathBuf::from(std::env::var("PBPS_PUSH_SIGNING_REPOSITORY").unwrap());
    let mut candidates = Candidates::new(Config {
        executable: BIN.into(),
        project: repository.join("project"),
        deadline: Duration::from_secs(30),
    });
    let now = SystemTime::now();
    let preview = candidates.preview(request(), now).unwrap();
    let candidate = candidates.confirm(&preview.candidate_id, now).unwrap();
    let result = Publications::open(&repository, Duration::from_secs(15))
        .unwrap()
        .confirm(&candidate);
    assert_eq!(result.status, Status::Delivered, "{result:?}");
}

#[test]
fn one_publication_reads_the_push_signing_mode_once() {
    // PATH is process-wide, so the counting wrapper applies only to a child.
    let f = Fixture::new("push-sign-single-read");
    destination(&f, true);
    signer(&f, Some("true"));
    let real = String::from_utf8(checked(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap(),
    ))
    .unwrap();
    let bin = f.repo.root.join(".git/counting-bin");
    fs::create_dir(&bin).unwrap();
    let log = f.repo.root.join(".git/push-signing-reads");
    fs::write(
        bin.join("git"),
        format!(
            "#!/bin/sh\ncase \" $* \" in *\" push.gpgSign \"*) echo read >> '{}';; esac\nexec '{}' \"$@\"\n",
            log.display(),
            real.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "publication::push_signing::push_signing_count_child",
            "--nocapture",
        ])
        .env("PATH", path)
        .env("PBPS_PUSH_SIGNING_REPOSITORY", &f.repo.root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(pushes(&f), ["cert"]);
    let reads = fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(reads.lines().count(), 1, "push.gpgSign reads: {reads:?}");
}
