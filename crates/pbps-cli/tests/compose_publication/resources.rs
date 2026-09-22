//! Actual Git GC, attributable retirement, and production resource boundaries.

use super::*;
use pbps_ui::compose::{ResourceObserver, ResourceOperation, ResourceState};
use std::os::unix::fs::FileTypeExt;
use std::sync::atomic::{AtomicBool, Ordering};

fn observed(f: &Fixture, observer: ResourceObserver) -> Publications {
    Publications::open_with_resources(&f.repo.root, Duration::from_secs(15), observer).unwrap()
}

fn root(f: &Fixture) -> PathBuf {
    f.repo.root.join(".git/pbps-compose-v2")
}

#[test]
fn forced_source_gc_preserves_the_frozen_base_and_exact_unpublished_commit() {
    let f = Fixture::new("gc-roots");
    let (store, preview, candidate) = f.ready();
    // Remove every ordinary source ref's reachability to the reviewed base.
    let tree = String::from_utf8(git(&f.repo.root, &["rev-parse", "HEAD^{tree}"])).unwrap();
    let unrelated = String::from_utf8(git(
        &f.repo.root,
        &["commit-tree", tree.trim(), "-m", "independent root"],
    ))
    .unwrap();
    git(
        &f.repo.root,
        &["update-ref", "refs/heads/master", unrelated.trim()],
    );
    git(&f.repo.root, &["reflog", "expire", "--expire=now", "--all"]);
    git(&f.repo.root, &["gc", "--prune=now"]);
    git(
        &candidate.snapshot_repository(),
        &["cat-file", "-e", &format!("{}^{{commit}}", preview.base)],
    );
    git(
        &candidate.snapshot_repository(),
        &["cat-file", "-e", &preview.tree],
    );

    let mut publisher = f.publisher();
    let prepared = publisher.confirm_observed(&candidate, &|at| {
        at != Boundary::AfterPersist(DurableStage::Prepared)
    });
    let exact = commit(&prepared).to_owned();
    assert_eq!(prepared.status, Status::RecoveryRequired, "{prepared:?}");
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    drop(publisher);
    drop(candidate);
    drop(store);
    git(&f.repo.root, &["reflog", "expire", "--expire=now", "--all"]);
    git(&f.repo.root, &["gc", "--prune=now"]);
    git(
        &f.repo.root,
        &["cat-file", "-e", &format!("{exact}^{{commit}}")],
    );
    let mut publisher = f.publisher();
    let recovered = publisher.recover(&preview.operation_id);
    assert_eq!(recovered.status, Status::Prepared, "{recovered:?}");
    let delivered = publisher.retry(&preview.operation_id);
    assert_eq!(delivered.status, Status::Delivered, "{delivered:?}");
    assert_eq!(commit(&delivered), exact);
    assert_eq!(publisher.recover(&preview.operation_id), delivered);
}

#[test]
fn cleanup_retains_the_receipt_root_and_forgetting_revokes_every_old_handle() {
    let f = Fixture::new("forget");
    let (mut store, preview, candidate) = f.ready();
    let snapshot = candidate
        .snapshot_repository()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut publisher = f.publisher();
    let delivered = publisher.confirm(&candidate);
    assert_eq!(delivered.status, Status::Delivered, "{delivered:?}");
    let cleaned = publisher.cleanup(&preview.operation_id);
    assert_eq!(cleaned.status, Status::Delivered, "{cleaned:?}");
    assert!(!cleaned.cleanup_pending);
    assert!(!snapshot.exists());
    assert!(f.record(&preview.operation_id).exists());
    assert_eq!(publisher.cleanup(&preview.operation_id), cleaned);
    assert!(
        store
            .confirm(&preview.candidate_id, SystemTime::now())
            .is_ok()
    );
    let before = f.repo.preserved();
    let forgotten = publisher.forget(&preview.operation_id).unwrap();
    assert_eq!(forgotten.state, ResourceState::Spent);
    assert!(!forgotten.cleanup_pending);
    assert!(!f.record(&preview.operation_id).exists());
    assert_eq!(
        f.remote_ref(&preview.output_ref).as_deref(),
        Some(commit(&delivered))
    );
    assert_eq!(f.repo.preserved(), before);
    assert_eq!(
        publisher.forget(&preview.operation_id).unwrap().state,
        ResourceState::Spent
    );
    let tombstone =
        fs::read(root(&f).join(format!("resources/{}.json", preview.operation_id))).unwrap();
    assert!(tombstone.len() < 2048);
    git(&f.remote, &["update-ref", "-d", &preview.output_ref]);
    git(&f.repo.root, &["update-ref", "-d", &preview.output_ref]);
    assert!(
        store
            .confirm(&preview.candidate_id, SystemTime::now())
            .is_err()
    );
    assert_eq!(publisher.confirm(&candidate).status, Status::Refused);
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    fs::write(f.repo.root.join("after-recovery"), "ordinary editing").unwrap();
    git(&f.repo.root, &["add", "after-recovery"]);
}

#[test]
fn unknown_snapshot_entries_and_changed_owned_files_keep_retirement_pending() {
    for replace in [false, true] {
        let f = Fixture::new(&format!("foreign-cleanup-{replace}"));
        let (_store, preview, candidate) = f.ready();
        let path = if replace {
            candidate.snapshot_repository().join("project/schema/t.yml")
        } else {
            candidate
                .snapshot_repository()
                .parent()
                .unwrap()
                .join("foreign-file")
        };
        let mut publisher = f.publisher();
        let delivered = publisher.confirm(&candidate);
        fs::write(&path, b"third-party evidence").unwrap();
        let refused = publisher.cleanup(&preview.operation_id);
        assert_eq!(refused.status, Status::RecoveryRequired, "{refused:?}");
        assert!(refused.cleanup_pending);
        assert_eq!(fs::read(&path).unwrap(), b"third-party evidence");
        assert!(f.record(&preview.operation_id).exists());
        assert_eq!(
            f.remote_ref(&preview.output_ref).as_deref(),
            Some(commit(&delivered))
        );
        let again = publisher.cleanup(&preview.operation_id);
        assert!(again.cleanup_pending);
        assert_eq!(fs::read(&path).unwrap(), b"third-party evidence");
        // Manual removal of the conflicting evidence allows only the already
        // recorded retirement to finish; it never recreates public refs.
        fs::remove_file(&path).unwrap();
        assert_eq!(
            publisher
                .recover_resources(&preview.operation_id)
                .unwrap()
                .state,
            ResourceState::Retained
        );
        assert!(!publisher.recover(&preview.operation_id).cleanup_pending);
    }
}

#[test]
fn interrupted_retirement_restarts_without_erasing_foreign_locks_or_receipts() {
    for after in [false, true] {
        let f = Fixture::new(&format!("retire-interrupt-{after}"));
        let (_store, preview, candidate) = f.ready();
        let marker = candidate.snapshot_repository().join("project/schema/t.yml");
        let trigger = marker.clone();
        let armed = Arc::new(AtomicBool::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        let (enabled, hit) = (armed.clone(), fired.clone());
        let observer = ResourceObserver::new(move |point| {
            if enabled.load(Ordering::SeqCst)
                && point.operation == ResourceOperation::Remove
                && point.after == after
                && point.path == trigger
            {
                hit.store(true, Ordering::SeqCst);
                return false;
            }
            true
        });
        let mut publisher = observed(&f, observer);
        let delivered = publisher.confirm(&candidate);
        assert_eq!(delivered.status, Status::Delivered);
        let lock = f.repo.root.join(".git/index.lock");
        fs::write(&lock, b"foreign index owner").unwrap();
        armed.store(true, Ordering::SeqCst);
        let interrupted = publisher.cleanup(&preview.operation_id);
        assert!(fired.load(Ordering::SeqCst));
        assert_eq!(interrupted.status, Status::RecoveryRequired);
        assert!(interrupted.cleanup_pending);
        assert!(f.record(&preview.operation_id).exists());
        assert_eq!(fs::read(&lock).unwrap(), b"foreign index owner");
        drop(publisher);
        let mut publisher = f.publisher();
        assert_eq!(
            publisher
                .recover_resources(&preview.operation_id)
                .unwrap()
                .state,
            ResourceState::Retained
        );
        assert_eq!(
            publisher
                .recover_resources(&preview.operation_id)
                .unwrap()
                .state,
            ResourceState::Retained
        );
        assert_eq!(fs::read(&lock).unwrap(), b"foreign index owner");
        fs::remove_file(&lock).unwrap();
        fs::write(f.repo.root.join("after-stop"), "editor content").unwrap();
        git(&f.repo.root, &["add", "after-stop"]);
        assert_eq!(
            f.remote_ref(&preview.output_ref).as_deref(),
            Some(commit(&delivered))
        );
    }
}

#[test]
fn legacy_and_unknown_evidence_refuse_without_modifying_retained_data() {
    let f = Fixture::new("legacy");
    let legacy = f.repo.root.join(".git/pbps-ui/composing");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("old.json"), b"legacy retained originals").unwrap();
    let before = f.repo.preserved();
    let error = f
        .repo
        .store()
        .preview(request(), SystemTime::now())
        .unwrap_err();
    assert!(error.to_string().contains("matching experimental binary"));
    assert!(Publications::open(&f.repo.root, Duration::from_secs(5)).is_err());
    assert_eq!(
        fs::read(legacy.join("old.json")).unwrap(),
        b"legacy retained originals"
    );
    assert!(!root(&f).exists());
    assert_eq!(f.repo.preserved(), before);

    let f = Fixture::new("unknown-resource");
    let (_store, _, _) = f.ready();
    let unknown = root(&f).join("pending-unknown-owner");
    fs::write(&unknown, b"do not guess ownership").unwrap();
    assert!(Publications::open(&f.repo.root, Duration::from_secs(5)).is_err());
    assert_eq!(fs::read(&unknown).unwrap(), b"do not guess ownership");
}

#[test]
fn every_receipt_and_retirement_io_failure_preserves_unresolved_evidence() {
    for kind in [
        "open",
        "read",
        "create",
        "write",
        "file-sync",
        "directory-sync",
        "rename",
        "remove",
        "pin-remove",
        "unlock",
    ] {
        for after in [false, true] {
            let f = Fixture::new(&format!("io-{kind}-{after}"));
            let (_store, preview, candidate) = f.ready();
            let armed = Arc::new(AtomicBool::new(false));
            let fired = Arc::new(AtomicBool::new(false));
            let (enabled, hit) = (armed.clone(), fired.clone());
            let record_path = root(&f).join(format!("resources/{}.json", preview.operation_id));
            let records_path = root(&f).join("resources");
            let snapshot_file = candidate.snapshot_repository().join("project/schema/t.yml");
            let pin_path = f.repo.root.join(format!(
                ".git/refs/pbps-compose/{}/base",
                preview.operation_id
            ));
            let observer = ResourceObserver::new(move |point| {
                let selected = match kind {
                    "open" => {
                        point.operation == ResourceOperation::Open
                            && point.path.ends_with("resources.lock")
                    }
                    "read" => {
                        point.operation == ResourceOperation::Read && point.path == record_path
                    }
                    "create" => {
                        point.operation == ResourceOperation::Create
                            && point.path.parent() == Some(records_path.as_path())
                    }
                    "write" => {
                        point.operation == ResourceOperation::Write
                            && point.path.parent() == Some(records_path.as_path())
                    }
                    "file-sync" => {
                        point.operation == ResourceOperation::Sync
                            && point.path.parent() == Some(records_path.as_path())
                    }
                    "directory-sync" => {
                        point.operation == ResourceOperation::Sync && point.path == records_path
                    }
                    "rename" => {
                        point.operation == ResourceOperation::Rename && point.path == record_path
                    }
                    "remove" => {
                        point.operation == ResourceOperation::Remove && point.path == snapshot_file
                    }
                    "pin-remove" => {
                        point.operation == ResourceOperation::Remove && point.path == pin_path
                    }
                    "unlock" => {
                        point.operation == ResourceOperation::Unlock
                            && point.path.ends_with("resources.lock")
                    }
                    _ => unreachable!(),
                };
                !(enabled.load(Ordering::SeqCst)
                    && selected
                    && point.after == after
                    && !hit.swap(true, Ordering::SeqCst))
            });
            let mut publisher = observed(&f, observer);
            let delivered = publisher.confirm(&candidate);
            assert_eq!(
                delivered.status,
                Status::Delivered,
                "{kind}/{after}: {delivered:?}"
            );
            armed.store(true, Ordering::SeqCst);
            let stopped = publisher.cleanup(&preview.operation_id);
            assert!(fired.load(Ordering::SeqCst), "unexercised {kind}/{after}");
            assert_eq!(
                stopped.status,
                Status::RecoveryRequired,
                "{kind}/{after}: {stopped:?}"
            );
            assert!(stopped.cleanup_pending);
            assert!(f.record(&preview.operation_id).exists());
            assert_eq!(
                f.remote_ref(&preview.output_ref).as_deref(),
                Some(commit(&delivered))
            );
            drop(publisher);
            let pending: Vec<_> = fs::read_dir(root(&f).join("resources"))
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("pending-")
                })
                .collect();
            let mut publisher = f.publisher();
            for _ in 0..2 {
                let recovered = publisher.cleanup(&preview.operation_id);
                if pending.is_empty() {
                    assert_eq!(
                        recovered.status,
                        Status::Delivered,
                        "{kind}/{after}: {recovered:?}"
                    );
                    assert!(!recovered.cleanup_pending);
                } else {
                    assert_eq!(recovered.status, Status::RecoveryRequired);
                    assert!(recovered.cleanup_pending);
                    assert!(
                        pending.iter().all(|p| p.exists()),
                        "unacknowledged temporary ownership was guessed"
                    );
                }
                assert!(f.record(&preview.operation_id).exists());
            }
        }
    }
}

#[test]
fn foreign_pin_locks_and_nonregular_receipts_are_never_cleared_or_waited_on() {
    let f = Fixture::new("foreign-pin-lock");
    let (_store, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    let delivered = publisher.confirm(&candidate);
    let lock = f.repo.root.join(format!(
        ".git/refs/pbps-compose/{}/base.lock",
        preview.operation_id
    ));
    fs::write(&lock, b"foreign pin owner").unwrap();
    let stopped = publisher.cleanup(&preview.operation_id);
    assert_eq!(stopped.status, Status::RecoveryRequired);
    assert_eq!(fs::read(&lock).unwrap(), b"foreign pin owner");
    assert!(f.record(&preview.operation_id).exists());
    fs::remove_file(&lock).unwrap();
    assert_eq!(
        publisher.cleanup(&preview.operation_id).status,
        Status::Delivered
    );
    assert_eq!(
        f.remote_ref(&preview.output_ref).as_deref(),
        Some(commit(&delivered))
    );

    let receipt = f.record(&preview.operation_id);
    let saved = f.repo.root.join("saved-receipt");
    fs::rename(&receipt, &saved).unwrap();
    checked(Command::new("mkfifo").arg(&receipt).output().unwrap());
    let started = std::time::Instant::now();
    assert_eq!(
        publisher.recover(&preview.operation_id).status,
        Status::RecoveryRequired
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        fs::symlink_metadata(&receipt)
            .unwrap()
            .file_type()
            .is_fifo()
    );
    fs::remove_file(&receipt).unwrap();
    fs::rename(saved, receipt).unwrap();
}

#[test]
fn replacing_a_receipt_or_lock_inode_cannot_transfer_ownership() {
    for lease in [false, true] {
        let f = Fixture::new(&format!("replace-owned-{lease}"));
        let (_store, preview, candidate) = f.ready();
        let armed = Arc::new(AtomicBool::new(false));
        let target = if lease {
            root(&f).join("owner.lock")
        } else {
            root(&f).join(format!("resources/{}.json", preview.operation_id))
        };
        let saved = f.repo.root.join("original-evidence");
        let trigger = target.clone();
        let backup = saved.clone();
        let active = armed.clone();
        let observer = ResourceObserver::new(move |point| {
            if !lease
                && point.operation == ResourceOperation::Rename
                && !point.after
                && point.path == trigger
                && active.swap(false, Ordering::SeqCst)
            {
                fs::rename(&trigger, &backup).unwrap();
                fs::write(&trigger, b"foreign replacement").unwrap();
            }
            true
        });
        let mut publisher = observed(&f, observer);
        assert_eq!(publisher.confirm(&candidate).status, Status::Delivered);
        if lease {
            fs::rename(&target, &saved).unwrap();
            fs::write(&target, b"foreign lock inode").unwrap();
        } else {
            armed.store(true, Ordering::SeqCst);
        }
        let refused = publisher.cleanup(&preview.operation_id);
        assert_eq!(refused.status, Status::RecoveryRequired, "{refused:?}");
        assert!(refused.cleanup_pending);
        assert_eq!(
            fs::read(&target).unwrap(),
            if lease {
                b"foreign lock inode".as_slice()
            } else {
                b"foreign replacement".as_slice()
            }
        );
        assert!(saved.exists());
        assert!(f.record(&preview.operation_id).exists());
    }
}

#[test]
fn a_replaced_new_directory_is_never_acknowledged_as_the_acquired_snapshot() {
    let f = Fixture::new("replace-new-snapshot");
    let moved = f.repo.root.join("original-snapshot");
    let retained = moved.clone();
    let observer = ResourceObserver::new(move |point| {
        if point.operation == ResourceOperation::Create
            && point.after
            && point
                .path
                .parent()
                .is_some_and(|p| p.ends_with("snapshots"))
        {
            fs::rename(&point.path, &retained).unwrap();
            fs::create_dir(&point.path).unwrap();
            fs::write(point.path.join("foreign"), b"other owner").unwrap();
        }
        true
    });
    let mut candidates = Candidates::with_resources(
        Config {
            executable: BIN.into(),
            project: f.repo.project.clone(),
            deadline: Duration::from_secs(15),
        },
        observer,
    );
    assert!(candidates.preview(request(), SystemTime::now()).is_err());
    assert!(moved.exists());
    let publisher = f.publisher();
    let reports = publisher.resource_reports().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].state, ResourceState::Capturing);
    let snapshot = root(&f).join("snapshots").join(&reports[0].operation_id);
    assert_eq!(fs::read(snapshot.join("foreign")).unwrap(), b"other owner");
    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(&reports[0].location).unwrap()).unwrap();
    assert!(raw["snapshot"].is_null());
}

#[test]
fn a_private_capture_write_failure_retains_unsealed_resources_and_source_state() {
    for after in [false, true] {
        let f = Fixture::new(&format!("capture-write-{after}"));
        let before = f.repo.preserved();
        let observer = ResourceObserver::new(move |point| {
            !(point.operation == ResourceOperation::Write
                && point.after == after
                && point.path.ends_with("project/schema/t.yml"))
        });
        let mut candidates = Candidates::with_resources(
            Config {
                executable: BIN.into(),
                project: f.repo.project.clone(),
                deadline: Duration::from_secs(15),
            },
            observer,
        );
        assert!(candidates.preview(request(), SystemTime::now()).is_err());
        let mut publisher = f.publisher();
        let reports = publisher.resource_reports().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, ResourceState::Capturing);
        assert!(reports[0].cleanup_pending);
        assert_eq!(
            publisher
                .recover_resources(&reports[0].operation_id)
                .unwrap()
                .state,
            ResourceState::Capturing
        );
        assert!(
            root(&f)
                .join("snapshots")
                .join(&reports[0].operation_id)
                .exists()
        );
        f.source_unchanged(&before);
        assert!(!f.repo.root.join(".git/index.lock").exists());
    }
}

#[test]
#[ignore = "synchronized subprocess used by the actual SIGKILL regression"]
fn resource_stop_child() {
    use std::os::unix::net::UnixStream;
    let repository = PathBuf::from(std::env::var_os("PBPS_RESOURCE_TEST_REPOSITORY").unwrap());
    let socket = PathBuf::from(std::env::var_os("PBPS_RESOURCE_TEST_SOCKET").unwrap());
    let mode = std::env::var("PBPS_RESOURCE_TEST_MODE").unwrap();
    let operation = Arc::new(std::sync::Mutex::new(String::new()));
    let send = move |id: &str| -> ! {
        let mut connection = UnixStream::connect(&socket).unwrap();
        writeln!(connection, "{id}").unwrap();
        loop {
            std::thread::park();
        }
    };
    let send = Arc::new(send);
    let (resource_send, resource_id, resource_mode) =
        (send.clone(), operation.clone(), mode.clone());
    let observer = ResourceObserver::new(move |point| {
        let path = &point.path;
        let stop = match resource_mode.as_str() {
            "snapshot-created" => {
                point.operation == ResourceOperation::Create
                    && point.after
                    && path.parent().is_some_and(|p| p.ends_with("snapshots"))
            }
            "commit-root-created" => {
                point.operation == ResourceOperation::Create
                    && point.after
                    && path.ends_with("commit")
                    && path.to_string_lossy().contains("/refs/pbps-compose/")
            }
            "prepared-replaced" => {
                point.operation == ResourceOperation::Rename
                    && point.after
                    && path
                        .parent()
                        .is_some_and(|p| p.ends_with("pbps-compose-v2"))
                    && fs::read(path)
                        .ok()
                        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                        .is_some_and(|r| r["state"]["phase"] == "prepared")
            }
            "snapshot-removed-file" => {
                point.operation == ResourceOperation::Remove
                    && point.after
                    && path.ends_with("project/schema/t.yml")
            }
            _ => false,
        };
        if stop {
            let id = if resource_mode == "snapshot-created" {
                path.file_name().unwrap().to_str().unwrap().to_owned()
            } else {
                resource_id.lock().unwrap().clone()
            };
            resource_send(&id);
        }
        true
    });
    let mut candidates = Candidates::with_resources(
        Config {
            executable: BIN.into(),
            project: repository.join("project"),
            deadline: Duration::from_secs(15),
        },
        observer.clone(),
    );
    let preview = candidates.preview(request(), SystemTime::now()).unwrap();
    *operation.lock().unwrap() = preview.operation_id.clone();
    let candidate = candidates
        .confirm(&preview.candidate_id, SystemTime::now())
        .unwrap();
    let mut publisher =
        Publications::open_with_resources(&repository, Duration::from_secs(15), observer).unwrap();
    let delivered = publisher.confirm_observed(&candidate, &|at| {
        if mode == "ref-prepared" && at == Boundary::RefPrepared {
            send(&preview.operation_id);
        }
        true
    });
    assert_eq!(delivered.status, Status::Delivered, "{delivered:?}");
    if mode == "snapshot-removed-file" {
        publisher.cleanup(&preview.operation_id);
    }
    panic!("the selected resource boundary was not reached");
}

#[test]
fn actual_process_death_preserves_acquisition_ref_handoff_and_retirement_evidence() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    for mode in [
        "snapshot-created",
        "commit-root-created",
        "prepared-replaced",
        "ref-prepared",
        "snapshot-removed-file",
    ] {
        let f = Fixture::new(&format!("kill-{mode}"));
        let before = f.repo.preserved();
        let socket = f.repo.root.join("stop.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let log_path = f.repo.root.join("child.log");
        let log = fs::File::create(&log_path).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "publication::resources::resource_stop_child",
                "--nocapture",
            ])
            .env("PBPS_RESOURCE_TEST_REPOSITORY", &f.repo.root)
            .env("PBPS_RESOURCE_TEST_SOCKET", &socket)
            .env("PBPS_RESOURCE_TEST_MODE", mode)
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
            if started.elapsed() > Duration::from_secs(30) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{mode}: child missed its synchronized boundary");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        connection
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut operation = String::new();
        BufReader::new(connection)
            .read_line(&mut operation)
            .unwrap();
        let id = operation.trim();
        assert_eq!(id.len(), 64);
        // SIGKILL bypasses destructors. Git at RefPrepared receives EOF only
        // after its parent dies; its lock must be observed, never guessed ours.
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        let output_ref = format!("refs/heads/pbps-compose/{id}");
        let lock = f.repo.root.join(format!(".git/{output_ref}.lock"));
        let waiting = std::time::Instant::now();
        while lock.exists() && waiting.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut publisher = f.publisher();
        let reports = publisher.resource_reports().unwrap();
        assert!(
            reports
                .iter()
                .any(|r| r.operation_id == id && r.cleanup_pending)
        );
        match mode {
            "snapshot-created" => {
                assert_eq!(reports[0].state, ResourceState::Capturing);
                assert!(root(&f).join(format!("snapshots/{id}")).exists());
                assert_eq!(
                    publisher.recover_resources(id).unwrap().state,
                    ResourceState::Capturing
                );
                assert!(!f.record(id).exists());
            }
            "commit-root-created" => {
                let resource: serde_json::Value = serde_json::from_slice(
                    &fs::read(root(&f).join(format!("resources/{id}.json"))).unwrap(),
                )
                .unwrap();
                assert_eq!(resource["pins"]["commit"]["owned"], false);
                let first = publisher.recover(id);
                assert_eq!(first.status, Status::PreparationUnknown, "{first:?}");
                assert!(first.details.as_ref().unwrap().commit.is_some());
                assert_eq!(publisher.retry(id), first);
                assert_eq!(f.remote_ref(&output_ref), None);
                assert!(f.record(id).exists());
            }
            "prepared-replaced" | "ref-prepared" => {
                assert!(
                    !lock.exists(),
                    "the real Git transaction did not discharge its lock"
                );
                let recovered = publisher.recover(id);
                assert_eq!(recovered.status, Status::Prepared, "{mode}: {recovered:?}");
                let exact = commit(&recovered).to_owned();
                let delivered = publisher.retry(id);
                assert_eq!(delivered.status, Status::Delivered, "{mode}: {delivered:?}");
                assert_eq!(commit(&delivered), exact);
            }
            "snapshot-removed-file" => {
                assert_eq!(
                    publisher.recover_resources(id).unwrap().state,
                    ResourceState::Retained
                );
                assert_eq!(
                    publisher.recover_resources(id).unwrap().state,
                    ResourceState::Retained
                );
                assert_eq!(publisher.recover(id).status, Status::Delivered);
                assert!(f.record(id).exists());
            }
            _ => unreachable!(),
        }
        f.source_unchanged(&before);
        assert!(!f.repo.root.join(".git/index.lock").exists());
        fs::write(
            f.repo.root.join("after-process-stop"),
            "ordinary Git editing",
        )
        .unwrap();
        git(&f.repo.root, &["add", "after-process-stop"]);
    }
}

#[test]
fn only_unconfirmed_sealed_previews_expire_or_allow_explicit_discard_after_restart() {
    for discard in [false, true] {
        let f = Fixture::new(&format!("sealed-expiry-{discard}"));
        let mut store = f.repo.store();
        let now = SystemTime::now();
        let preview = store.preview(request(), now).unwrap();
        drop(store);
        let mut publisher = f.publisher();
        assert_eq!(
            publisher
                .recover_resources_at(&preview.operation_id, now)
                .unwrap()
                .state,
            ResourceState::Sealed
        );
        let retired = if discard {
            publisher.discard_preview(&preview.operation_id).unwrap()
        } else {
            publisher
                .recover_resources_at(
                    &preview.operation_id,
                    now + Duration::from_secs(25 * 60 * 60),
                )
                .unwrap()
        };
        assert_eq!(retired.state, ResourceState::Spent);
        assert!(!retired.cleanup_pending);
        assert!(
            !root(&f)
                .join("snapshots")
                .join(&preview.operation_id)
                .exists()
        );
        assert!(!f.record(&preview.operation_id).exists());
        assert_eq!(
            publisher
                .recover_resources(&preview.operation_id)
                .unwrap()
                .state,
            ResourceState::Spent
        );
    }
    for attempt in [false, true] {
        let f = Fixture::new(&format!("confirmed-no-expiry-{attempt}"));
        let (_store, preview, candidate) = f.ready();
        let mut publisher = f.publisher();
        if attempt {
            let result = publisher.confirm_observed(&candidate, &|at| {
                at != Boundary::AfterPersist(DurableStage::RemoteAttempt)
            });
            assert_ne!(result.status, Status::Delivered);
        }
        let snapshot = root(&f).join("snapshots").join(&preview.operation_id);
        let manifest = fs::read(snapshot.join("manifest.json")).unwrap();
        assert!(publisher.discard_preview(&preview.operation_id).is_err());
        assert!(publisher.forget(&preview.operation_id).is_err());
        assert_ne!(
            publisher.cleanup(&preview.operation_id).status,
            Status::Delivered
        );
        drop(publisher);
        let mut publisher = f.publisher();
        assert_eq!(
            publisher
                .recover_resources_at(
                    &preview.operation_id,
                    SystemTime::now() + Duration::from_secs(365 * 24 * 60 * 60)
                )
                .unwrap()
                .state,
            ResourceState::Confirmed
        );
        assert_eq!(fs::read(snapshot.join("manifest.json")).unwrap(), manifest);
        assert_eq!(f.remote_ref(&preview.output_ref), None);
    }
}

#[test]
fn publication_requires_the_durable_manifest_that_was_reviewed() {
    let f = Fixture::new("manifest-binding");
    let (_store, preview, candidate) = f.ready();
    let path = candidate
        .snapshot_repository()
        .parent()
        .unwrap()
        .join("manifest.json");
    let original = fs::read(&path).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(manifest["tree"], preview.tree);
    assert_eq!(manifest["base"], preview.base);
    let resource: serde_json::Value = serde_json::from_slice(
        &fs::read(root(&f).join(format!("resources/{}.json", preview.operation_id))).unwrap(),
    )
    .unwrap();
    assert_eq!(resource["binding"], preview.binding);
    fs::write(&path, b"changed input evidence").unwrap();
    let mut publisher = f.publisher();
    assert_eq!(publisher.confirm(&candidate).status, Status::Refused);
    assert!(!f.record(&preview.operation_id).exists());
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    fs::write(&path, original).unwrap();
    let delivered = publisher.confirm(&candidate);
    assert_eq!(delivered.status, Status::Delivered, "{delivered:?}");
}

#[test]
fn unknown_resource_versions_and_impossible_states_preserve_all_evidence() {
    for field in ["version", "state"] {
        let f = Fixture::new(&format!("unknown-resource-{field}"));
        let (_store, preview, candidate) = f.ready();
        let path = root(&f).join(format!("resources/{}.json", preview.operation_id));
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        record[field] = if field == "version" {
            serde_json::json!(999)
        } else {
            serde_json::json!("spent")
        };
        let bytes = serde_json::to_vec(&record).unwrap();
        fs::write(&path, &bytes).unwrap();
        let mut publisher = f.publisher();
        assert!(publisher.resource_reports().is_err());
        assert!(publisher.recover_resources(&preview.operation_id).is_err());
        assert_eq!(publisher.confirm(&candidate).status, Status::Refused);
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert!(candidate.snapshot_repository().exists());
        assert_eq!(f.remote_ref(&preview.output_ref), None);
    }
}

#[test]
fn packed_output_refs_allow_reconciliation_after_an_unacknowledged_local_write() {
    let f = Fixture::new("packed-local-recovery");
    let (_store, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    let interrupted = publisher.confirm_observed(&candidate, &|at| {
        at != Boundary::RefInstalled && at != Boundary::BeforeReconcile
    });
    assert_ne!(interrupted.status, Status::Delivered);
    let exact = commit(&interrupted).to_owned();
    drop(publisher);
    git(&f.repo.root, &["pack-refs", "--all", "--prune"]);
    assert!(!f.repo.root.join(".git").join(&preview.output_ref).exists());
    let mut publisher = f.publisher();
    let recovered = publisher.recover(&preview.operation_id);
    assert_eq!(recovered.local, LocalState::Present);
    assert_eq!(recovered.status, Status::Published, "{recovered:?}");
    let delivered = publisher.retry(&preview.operation_id);
    assert_eq!(delivered.status, Status::Delivered, "{delivered:?}");
    assert_eq!(commit(&delivered), exact);
}

#[test]
fn orphan_snapshot_names_remain_visible_as_unattributable_evidence() {
    let f = Fixture::new("orphan-snapshot");
    let mut publisher = f.publisher();
    let orphan = root(&f).join("snapshots/unknown-original");
    fs::create_dir(&orphan).unwrap();
    fs::write(orphan.join("retained"), b"unknown ownership").unwrap();
    assert!(publisher.resource_reports().is_err());
    assert!(publisher.discard_preview("unknown-original").is_err());
    assert_eq!(
        fs::read(orphan.join("retained")).unwrap(),
        b"unknown ownership"
    );
}

pub(super) fn private_bytes_exclude(f: &Fixture, marker: &str) {
    fn walk(path: &Path, marker: &[u8]) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                walk(&path, marker);
            } else if metadata.is_file() {
                assert!(
                    !fs::read(&path)
                        .unwrap()
                        .windows(marker.len())
                        .any(|bytes| bytes == marker),
                    "private artifact leaked marker: {}",
                    path.display()
                );
            }
        }
    }
    if root(f).exists() {
        walk(&root(f), marker.as_bytes());
    }
}

#[test]
fn secret_bearing_endpoints_never_enter_private_acquisition_evidence() {
    let f = Fixture::new("secret-acquisition");
    for endpoint in [
        "https://user:FAKE_ENDPOINT_SECRET@host.test/repo",
        "https://host.test/repo?token=FAKE_ENDPOINT_SECRET",
        "https://host.test/repo#FAKE_ENDPOINT_SECRET",
    ] {
        git(&f.repo.root, &["remote", "set-url", "origin", endpoint]);
        let error = f
            .repo
            .store()
            .preview(request(), SystemTime::now())
            .err()
            .unwrap();
        assert!(!error.to_string().contains("FAKE_ENDPOINT_SECRET"));
        private_bytes_exclude(&f, "FAKE_ENDPOINT_SECRET");
        assert!(!root(&f).join("resources").exists());
    }
}

#[test]
fn pre_receipt_refusals_keep_retained_or_unreadable_resources_pending() {
    for reason in ["manifest", "resource", "signing"] {
        let f = Fixture::new(&format!("refused-pending-{reason}"));
        let (_store, preview, candidate) = f.ready();
        match reason {
            "manifest" => fs::write(
                candidate
                    .snapshot_repository()
                    .parent()
                    .unwrap()
                    .join("manifest.json"),
                b"changed evidence",
            )
            .unwrap(),
            "resource" => fs::write(
                root(&f).join(format!("resources/{}.json", preview.operation_id)),
                b"unreadable ownership",
            )
            .unwrap(),
            "signing" => {
                git(
                    &f.repo.root,
                    &["config", "user.signingKey", "changed-public-selector"],
                );
            }
            _ => unreachable!(),
        }
        let mut publisher = f.publisher();
        for _ in 0..2 {
            let refused = publisher.confirm(&candidate);
            assert_eq!(refused.status, Status::Refused);
            assert!(refused.cleanup_pending, "{reason}: {refused:?}");
            assert_eq!(refused.local, LocalState::NotAttempted);
            assert!(refused.details.as_ref().unwrap().commit.is_none());
            assert!(candidate.snapshot_repository().exists());
            assert!(!f.record(&preview.operation_id).exists());
            assert_eq!(f.remote_ref(&preview.output_ref), None);
        }
    }
    let f = Fixture::new("spent-not-pending");
    let (_store, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    assert_eq!(publisher.confirm(&candidate).status, Status::Delivered);
    publisher.forget(&preview.operation_id).unwrap();
    let refused = publisher.confirm(&candidate);
    assert_eq!(refused.status, Status::Refused);
    assert!(!refused.cleanup_pending, "{refused:?}");
}

#[test]
fn an_owned_commit_root_prevents_repreparation_after_receipt_loss() {
    let f = Fixture::new("known-root-missing-receipt");
    let (_store, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    let prepared = publisher.confirm_observed(&candidate, &|at| {
        at != Boundary::AfterPersist(DurableStage::Prepared) && at != Boundary::BeforeReconcile
    });
    let exact = commit(&prepared).to_owned();
    let resource = root(&f).join(format!("resources/{}.json", preview.operation_id));
    let original = fs::read(&resource).unwrap();
    // Deliberate evidence-loss fixture after real preparation, not power loss.
    fs::remove_file(f.record(&preview.operation_id)).unwrap();
    let invoked = std::cell::Cell::new(false);
    let repeated = publisher.confirm_observed(&candidate, &|at| {
        if at == Boundary::CommitCreated {
            invoked.set(true);
        }
        true
    });
    assert!(
        !invoked.get(),
        "an existing exact root permitted another commit invocation"
    );
    assert_eq!(repeated.status, Status::RecoveryRequired, "{repeated:?}");
    assert_eq!(repeated.problem, Some(Problem::ReceiptUnavailable));
    assert!(repeated.cleanup_pending);
    assert!(!f.record(&preview.operation_id).exists());
    assert_eq!(fs::read(resource).unwrap(), original);
    assert_eq!(f.remote_ref(&preview.output_ref), None);
    git(&f.repo.root, &["cat-file", "-e", &exact]);
    assert_eq!(publisher.confirm(&candidate), repeated);
}

#[test]
fn linked_worktrees_list_only_their_own_valid_receipts_and_resources() {
    let f = Fixture::new("worktree-scopes");
    let (_store, original, candidate) = f.ready();
    let mut publisher = f.publisher();
    assert_eq!(publisher.confirm(&candidate).status, Status::Delivered);
    let original_resource = root(&f).join(format!("resources/{}.json", original.operation_id));
    let original_bytes = fs::read(&original_resource).unwrap();
    let source = f.repo.preserved();
    drop(publisher);
    let linked = Repository {
        root: f.repo.root.with_extension("linked"),
        project: f.repo.root.with_extension("linked").join("project"),
    };
    git(
        &f.repo.root,
        &[
            "worktree",
            "add",
            "--detach",
            "-q",
            linked.root.to_str().unwrap(),
            &original.output_ref,
        ],
    );
    let mut service = Publications::open(&linked.root, Duration::from_secs(15)).unwrap();
    assert!(service.resource_reports().unwrap().is_empty());
    assert!(service.list().unwrap().is_empty());
    assert!(service.recover_resources(&original.operation_id).is_err());
    assert!(service.forget(&original.operation_id).is_err());
    assert_eq!(fs::read(&original_resource).unwrap(), original_bytes);
    linked.table(&RENAMED.replace("ident", "next_ident"));
    let mut store = linked.store();
    let mut action = request();
    action.intent = Intent::Rename {
        from: "dbo.t.ident".into(),
        to: "next_ident".into(),
    };
    action.remote_base_ref = original.output_ref.clone();
    let next = store.preview(action, SystemTime::now()).unwrap();
    let candidate = store
        .confirm(&next.candidate_id, SystemTime::now())
        .unwrap();
    assert_eq!(service.confirm(&candidate).status, Status::Delivered);
    assert_eq!(
        service
            .resource_reports()
            .unwrap()
            .iter()
            .map(|r| r.operation_id.as_str())
            .collect::<Vec<_>>(),
        [&next.operation_id]
    );
    assert_eq!(
        service
            .list()
            .unwrap()
            .iter()
            .map(|r| r.operation_id.as_str())
            .collect::<Vec<_>>(),
        [&next.operation_id]
    );
    drop(service);
    let mut main = f.publisher();
    assert_eq!(
        main.resource_reports()
            .unwrap()
            .iter()
            .map(|r| r.operation_id.as_str())
            .collect::<Vec<_>>(),
        [&original.operation_id]
    );
    assert_eq!(
        main.list()
            .unwrap()
            .iter()
            .map(|r| r.operation_id.as_str())
            .collect::<Vec<_>>(),
        [&original.operation_id]
    );
    // Only a fully validated record can be classified as another worktree's.
    for path in [
        root(&f).join(format!("resources/{}.json", next.operation_id)),
        f.record(&next.operation_id),
    ] {
        let saved = fs::read(&path).unwrap();
        let mut bad: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        bad["version"] = serde_json::json!(999);
        fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
        if path.parent().unwrap().ends_with("resources") {
            assert!(main.resource_reports().is_err());
        } else {
            assert!(main.list().is_err());
        }
        fs::write(path, saved).unwrap();
    }
    let mut changed: serde_json::Value = serde_json::from_slice(&original_bytes).unwrap();
    changed["repository"]["source_inode"] =
        serde_json::json!(changed["repository"]["source_inode"].as_u64().unwrap() + 1);
    fs::write(&original_resource, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(
        main.resource_reports().is_err(),
        "a replaced source at the same path was hidden as foreign"
    );
    fs::write(&original_resource, &original_bytes).unwrap();
    assert_eq!(main.resource_reports().unwrap().len(), 1);
    assert_eq!(fs::read(&original_resource).unwrap(), original_bytes);
    f.source_unchanged(&source);
}
