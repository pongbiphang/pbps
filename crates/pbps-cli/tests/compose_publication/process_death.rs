//! Actual process death at the publication boundaries after commit creation,
//! and recovery that is itself killed and restarted (#748).
//!
//! A child test process runs the real publisher and parks at one boundary;
//! the parent kills it with SIGKILL, so no destructor, unwind or cleanup runs,
//! then recovers with a fresh publisher. The boundaries are named by
//! `PublicationBoundary`, so each kill lands on the operation it names rather
//! than on a phase that merely shares its name.

use super::http::Http;
use super::*;
use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};

const CHILD: &str = "publication::process_death::publication_stop_child";

#[test]
#[ignore = "synchronized subprocess used by the actual publication SIGKILL regressions"]
fn publication_stop_child() {
    let variable = |name: &str| std::env::var(format!("PBPS_PUBLICATION_TEST_{name}")).unwrap();
    let repository = PathBuf::from(variable("REPOSITORY"));
    let socket = PathBuf::from(variable("SOCKET"));
    let mode = variable("MODE");
    assert_no_core_dumps();
    let park = |id: &str| -> ! {
        let mut connection = UnixStream::connect(&socket).unwrap();
        writeln!(connection, "{id}").unwrap();
        loop {
            std::thread::park();
        }
    };
    let publisher = || Publications::open(&repository, Duration::from_secs(15)).unwrap();
    let recovery = match mode.as_str() {
        "recover-local" => Some(Boundary::BeforePersist(DurableStage::LocalPublished)),
        "recover-delivered" => Some(Boundary::BeforePersist(DurableStage::RemoteDelivered)),
        _ => None,
    };
    if let Some(stop) = recovery {
        let id = variable("OPERATION");
        publisher().recover_observed(&id, &|at| {
            if at == stop {
                park(&id);
            }
            true
        });
        panic!("{mode}: the recovery boundary was not reached");
    }
    let stop = match mode.as_str() {
        "commit-created" => Boundary::CommitCreated,
        "ref-installed" => Boundary::RefInstalled,
        "remote-attempt" => Boundary::AfterPersist(DurableStage::RemoteAttempt),
        "push-finished" => Boundary::PushFinished,
        _ => unreachable!("{mode}"),
    };
    let mut candidates = Candidates::new(Config {
        executable: BIN.into(),
        project: repository.join("project"),
        deadline: Duration::from_secs(30),
    });
    let now = SystemTime::now();
    let preview = candidates.preview(request(), now).unwrap();
    let candidate = candidates.confirm(&preview.candidate_id, now).unwrap();
    publisher().confirm_observed(&candidate, &|at| {
        if at == stop {
            park(&preview.operation_id);
        }
        true
    });
    panic!("{mode}: the publication boundary was not reached");
}

/// The stopped child must not be able to write a core file.
pub(super) fn assert_no_core_dumps() {
    let limits = fs::read_to_string("/proc/self/limits").unwrap();
    let core = limits
        .lines()
        .find(|line| line.starts_with("Max core file size"))
        .unwrap();
    assert_eq!(core.split_whitespace().nth(4), Some("0"), "{core}");
}

/// Runs the child in `mode` until it parks, then SIGKILLs it. Core dumps are
/// disabled; SIGKILL writes none, and a child that aborts instead must not
/// leave one behind either. Returns the operation the child reported.
fn killed_at(f: &Fixture, mode: &str, operation: &str) -> String {
    let socket = f.repo.root.join(format!("stop-{mode}.sock"));
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let log_path = f.repo.root.join(format!("child-{mode}.log"));
    let log = fs::File::create(&log_path).unwrap();
    let mut child = Command::new("sh")
        .args(["-c", "ulimit -c 0 && exec \"$0\" \"$@\""])
        .arg(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", CHILD, "--nocapture"])
        .env("PBPS_PUBLICATION_TEST_REPOSITORY", &f.repo.root)
        .env("PBPS_PUBLICATION_TEST_SOCKET", &socket)
        .env("PBPS_PUBLICATION_TEST_MODE", mode)
        .env("PBPS_PUBLICATION_TEST_OPERATION", operation)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    let connection = loop {
        if let Ok((connection, _)) = listener.accept() {
            break connection;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "{mode}: child exited {status}: {}",
                fs::read_to_string(&log_path).unwrap()
            );
        }
        if started.elapsed() > Duration::from_secs(60) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{mode}: child missed its synchronized boundary");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    connection
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut id = String::new();
    BufReader::new(connection).read_line(&mut id).unwrap();
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    let id = id.trim().to_owned();
    assert_eq!(id.len(), 64, "{mode}");
    // A Git child of the killed process sees EOF only after its parent dies.
    let lock = f
        .repo
        .root
        .join(format!(".git/refs/heads/pbps-compose/{id}.lock"));
    let waiting = std::time::Instant::now();
    while lock.exists() && waiting.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !lock.exists(),
        "{mode}: a real Git transaction kept its lock"
    );
    id
}

fn receipt(f: &Fixture, id: &str) -> serde_json::Value {
    serde_json::from_slice(&fs::read(f.record(id)).unwrap()).unwrap()
}

fn local_ref(f: &Fixture, id: &str) -> Option<String> {
    let result = Command::new("git")
        .arg("-C")
        .arg(&f.repo.root)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/pbps-compose/{id}"))
        .output()
        .unwrap();
    result
        .status
        .success()
        .then(|| String::from_utf8(result.stdout).unwrap().trim().to_owned())
}

fn commits(f: &Fixture) -> usize {
    let listed = git(
        &f.repo.root,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objecttype)",
        ],
    );
    String::from_utf8(listed)
        .unwrap()
        .lines()
        .filter(|kind| *kind == "commit")
        .count()
}

/// The source checkout is exactly as it was and stays ordinary to edit.
fn source_intact(f: &Fixture, before: &[Vec<u8>]) {
    f.source_unchanged(before);
    assert!(!f.repo.root.join(".git/index.lock").exists());
    fs::write(f.repo.root.join("after-process-death"), "ordinary edit").unwrap();
    git(&f.repo.root, &["add", "after-process-death"]);
}

#[test]
fn death_after_commit_creation_never_yields_a_second_commit() {
    let f = Fixture::new("death-commit-created");
    let before = f.repo.preserved();
    let id = killed_at(&f, "commit-created", "");
    // commit-tree ran; nothing durable names its result yet.
    assert_eq!(receipt(&f, &id)["state"]["phase"], "preparing");
    // A repeated commit-tree with the same inputs within the same second
    // would reproduce the same OID. A new committer identity makes any
    // further invocation a new object, so the count can observe it.
    git(
        &f.repo.root,
        &["config", "user.email", "after-death@example.test"],
    );
    let objects = commits(&f);
    let mut publisher = f.publisher();
    let first = publisher.recover(&id);
    assert_eq!(first.status, Status::PreparationUnknown, "{first:?}");
    assert_eq!(first.details.as_ref().unwrap().commit, None);
    for _ in 0..2 {
        assert_eq!(publisher.retry(&id), first);
    }
    assert_eq!(commits(&f), objects, "retry invoked commit creation again");
    // The detector itself: one more invocation right now is a new object.
    git(
        &f.repo.root,
        &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "probe"],
    );
    assert_eq!(commits(&f), objects + 1);
    assert_eq!(local_ref(&f, &id), None);
    assert_eq!(f.remote_ref(&format!("refs/heads/pbps-compose/{id}")), None);
    assert_eq!(receipt(&f, &id)["state"]["phase"], "preparing");
    source_intact(&f, &before);
}

#[test]
fn death_after_the_local_ref_is_installed_resumes_that_exact_commit() {
    let f = Fixture::new("death-ref-installed");
    let before = f.repo.preserved();
    let id = killed_at(&f, "ref-installed", "");
    let state = receipt(&f, &id)["state"].clone();
    assert_eq!(state["phase"], "local_attempt");
    let exact = state["commit"].as_str().unwrap().to_owned();
    assert_eq!(local_ref(&f, &id).as_deref(), Some(exact.as_str()));
    let output_ref = format!("refs/heads/pbps-compose/{id}");
    assert_eq!(f.remote_ref(&output_ref), None);
    let objects = commits(&f);
    let mut publisher = f.publisher();
    let recovered = publisher.recover(&id);
    assert_eq!(recovered.status, Status::Published, "{recovered:?}");
    assert_eq!(commit(&recovered), exact);
    assert_eq!(receipt(&f, &id)["state"]["phase"], "local_published");
    let delivered = publisher.retry(&id);
    assert_eq!(delivered.status, Status::Delivered, "{delivered:?}");
    assert_eq!(commit(&delivered), exact);
    assert_eq!(f.remote_ref(&output_ref).as_deref(), Some(exact.as_str()));
    assert_eq!(commits(&f), objects);
    source_intact(&f, &before);
}

/// The destination is the loopback smart-HTTP server, which logs every
/// request: a push, even one with nothing left to update, is observable.
fn over_http(f: &Fixture) -> Http {
    let http = Http::new(f);
    git(
        &f.repo.root,
        &["remote", "set-url", "origin", &http.endpoint()],
    );
    http
}

#[test]
fn death_after_the_remote_attempt_intent_needs_informed_republish() {
    let f = Fixture::new("death-remote-attempt");
    let http = over_http(&f);
    let before = f.repo.preserved();
    let id = killed_at(&f, "remote-attempt", "");
    let state = receipt(&f, &id)["state"].clone();
    assert_eq!(state["remote"]["phase"], "attempted");
    let exact = state["commit"].as_str().unwrap().to_owned();
    let output_ref = format!("refs/heads/pbps-compose/{id}");
    // The push never ran, but the durable intent says it may have.
    assert_eq!(http.push_requests(), 0);
    assert_eq!(http.reviewed_ref(&output_ref), None);
    let mut publisher = f.publisher();
    let recovered = publisher.recover(&id);
    // The local result stands; absence after an authorized attempt is unknown
    // delivery, never "not attempted".
    assert_eq!(recovered.status, Status::Published, "{recovered:?}");
    assert_eq!(recovered.remote, DeliveryState::Unknown, "{recovered:?}");
    // Ordinary retry reconciles; it never replays an authorized attempt.
    assert_eq!(publisher.retry(&id), recovered);
    assert_eq!(http.push_requests(), 0, "recovery or retry pushed");
    let republished = publisher.republish(&id, &generation(&recovered));
    assert_eq!(republished.status, Status::Delivered, "{republished:?}");
    assert_eq!(commit(&republished), exact);
    assert!(http.push_requests() > 0);
    assert_eq!(
        http.reviewed_ref(&output_ref).as_deref(),
        Some(exact.as_str())
    );
    source_intact(&f, &before);
}

#[test]
fn death_after_the_push_finds_the_delivered_commit_without_pushing_again() {
    let f = Fixture::new("death-push-finished");
    let http = over_http(&f);
    let before = f.repo.preserved();
    let id = killed_at(&f, "push-finished", "");
    let state = receipt(&f, &id)["state"].clone();
    assert_eq!(state["remote"]["phase"], "attempted");
    let exact = state["commit"].as_str().unwrap().to_owned();
    let output_ref = format!("refs/heads/pbps-compose/{id}");
    assert_eq!(
        http.reviewed_ref(&output_ref).as_deref(),
        Some(exact.as_str())
    );
    let pushed = http.push_requests();
    assert!(pushed > 0);
    let recovered = f.publisher().recover(&id);
    assert_eq!(recovered.status, Status::Delivered, "{recovered:?}");
    assert_eq!(commit(&recovered), exact);
    assert_eq!(receipt(&f, &id)["state"]["remote"]["phase"], "delivered");
    assert_eq!(http.push_requests(), pushed, "recovery pushed again");
    // The detector itself: an up-to-date push still reaches the server.
    git(
        &f.repo.root,
        &[
            "push",
            "-q",
            &http.endpoint(),
            &format!("{exact}:{output_ref}"),
        ],
    );
    assert!(http.push_requests() > pushed);
    source_intact(&f, &before);
}

#[test]
fn recovery_killed_before_its_own_receipt_write_restarts_to_the_same_result() {
    for (first, recovery, phase) in [
        ("ref-installed", "recover-local", "local_attempt"),
        ("push-finished", "recover-delivered", "local_published"),
    ] {
        let f = Fixture::new(&format!("death-{recovery}"));
        let before = f.repo.preserved();
        let id = killed_at(&f, first, "");
        let untouched = fs::read(f.record(&id)).unwrap();
        // Recovery itself dies just before persisting what it reconciled.
        assert_eq!(killed_at(&f, recovery, &id), id);
        assert_eq!(fs::read(f.record(&id)).unwrap(), untouched, "{recovery}");
        assert_eq!(receipt(&f, &id)["state"]["phase"], phase);
        let exact = receipt(&f, &id)["state"]["commit"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut publisher = f.publisher();
        let recovered = publisher.recover(&id);
        // Recovery is repeatable: a second pass reports the same facts.
        assert_eq!(publisher.recover(&id), recovered, "{recovery}");
        assert_eq!(commit(&recovered), exact);
        let expected = if first == "ref-installed" {
            Status::Published
        } else {
            Status::Delivered
        };
        assert_eq!(recovered.status, expected, "{recovery}: {recovered:?}");
        let delivered = publisher.retry(&id);
        assert_eq!(
            delivered.status,
            Status::Delivered,
            "{recovery}: {delivered:?}"
        );
        assert_eq!(commit(&delivered), exact);
        source_intact(&f, &before);
    }
}
