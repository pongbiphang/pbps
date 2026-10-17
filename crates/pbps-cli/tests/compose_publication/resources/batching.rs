//! Count production discovery work independently of wall-clock timing.

use super::*;
use std::sync::Mutex;

#[derive(Clone, Debug, Default)]
struct Counts {
    resource_passes: usize,
    receipt_passes: usize,
    resource_reads: usize,
    receipt_reads: usize,
    identities: usize,
}

fn counter(f: &Fixture, admission: bool) -> (ResourceObserver, Arc<Mutex<Counts>>) {
    let directory = root(f);
    let resources = directory.join("resources");
    let counts = Arc::new(Mutex::new(Counts::default()));
    let seen = counts.clone();
    let stopped = AtomicBool::new(false);
    let observer = ResourceObserver::new(move |at| {
        // Measure admission before its first persistent record creation. Later
        // capture/retirement work has independent per-operation obligations.
        if admission
            && at.operation == ResourceOperation::Create
            && at.path.parent() == Some(resources.as_path())
        {
            stopped.store(true, Ordering::SeqCst);
        }
        if stopped.load(Ordering::SeqCst) || at.after {
            return true;
        }
        let mut c = seen.lock().unwrap();
        match at.operation {
            ResourceOperation::Identify => c.identities += 1,
            ResourceOperation::Read if at.path == resources => c.resource_passes += 1,
            ResourceOperation::Read if at.path == directory => c.receipt_passes += 1,
            ResourceOperation::Read if at.path.extension().is_some_and(|e| e == "json") => {
                if at.path.parent() == Some(resources.as_path()) {
                    c.resource_reads += 1;
                }
                if at.path.parent() == Some(directory.as_path()) {
                    c.receipt_reads += 1;
                }
            }
            ResourceOperation::Open
            | ResourceOperation::Create
            | ResourceOperation::Read
            | ResourceOperation::Write
            | ResourceOperation::Sync
            | ResourceOperation::Rename
            | ResourceOperation::Remove
            | ResourceOperation::Unlock => (),
        }
        true
    });
    (observer, counts)
}

fn retain(f: &Fixture) -> Preview {
    let (_store, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    assert_eq!(publisher.confirm(&candidate).status, Status::Delivered);
    assert!(!publisher.cleanup(&preview.operation_id).cleanup_pending);
    preview
}

#[test]
fn discovery_and_admission_have_bounded_passes_and_linear_record_reads() {
    fn updates(repository: &Path, refs: &[(String, String)]) {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repository)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["update-ref", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for (name, value) in refs {
            writeln!(stdin, "update {name} {value}").unwrap();
        }
        drop(stdin);
        checked(child.wait_with_output().unwrap());
    }
    fn expand(
        f: &Fixture,
        seed: &str,
        id: &str,
        local: &mut Vec<(String, String)>,
        remote: &mut Vec<(String, String)>,
    ) {
        let resources = root(f).join("resources");
        let mut resource: serde_json::Value =
            serde_json::from_slice(&fs::read(resources.join(format!("{seed}.json"))).unwrap())
                .unwrap();
        resource["operation"] = id.into();
        let target = resources.join(format!("{id}.json"));
        assert!(!target.exists());
        fs::write(target, serde_json::to_vec(&resource).unwrap()).unwrap();
        if resource["state"] == "retained" {
            let mut receipt: serde_json::Value =
                serde_json::from_slice(&fs::read(f.record(seed)).unwrap()).unwrap();
            receipt["description"]["operation_id"] = id.into();
            let output = format!("refs/heads/pbps-compose/{id}");
            receipt["description"]["output_ref"] = output.clone().into();
            assert!(!f.record(id).exists());
            fs::write(f.record(id), serde_json::to_vec(&receipt).unwrap()).unwrap();
            let pin = resource["pins"]["commit"]["value"]
                .as_str()
                .unwrap()
                .to_owned();
            local.push((format!("refs/pbps-compose/{id}/commit"), pin));
            let commit = resource["pins"]["commit"]["target"]
                .as_str()
                .unwrap()
                .to_owned();
            local.push((output.clone(), commit.clone()));
            remote.push((output, commit));
        }
    }
    let f = Fixture::new("batch-counts-replay");
    let retained = retain(&f);
    let mut store = f.repo.store();
    let spent = store.preview(request(), SystemTime::now()).unwrap();
    f.publisher().discard_preview(&spent.operation_id).unwrap();
    drop(store);
    let linked = f.repo.root.with_extension("linked");
    git(
        &f.repo.root,
        &[
            "worktree",
            "add",
            "--detach",
            "-q",
            linked.to_str().unwrap(),
            "master",
        ],
    );
    let other = Fixture {
        repo: Repository {
            project: linked.join("project"),
            root: linked,
        },
        remote: f.remote.clone(),
    };
    other.repo.table(RENAMED);
    let unrelated = retain(&other);
    let pristine = fixtures::Pristine::capture(&f);
    for n in [2, 5] {
        pristine.assert(&f);
        // Retained/spent/other-source shapes all come from real operations.
        // Copies have new operation/output identities and matching actual refs,
        // but share the immutable commit objects, not candidate capabilities.
        let (mut local, mut remote) = (Vec::new(), Vec::new());
        for number in 1..n {
            expand(
                &f,
                &retained.operation_id,
                &format!("{number:064x}"),
                &mut local,
                &mut remote,
            );
            expand(
                &f,
                &spent.operation_id,
                &format!("{:064x}", 0x100 + number),
                &mut local,
                &mut remote,
            );
            expand(
                &f,
                &unrelated.operation_id,
                &format!("{:064x}", 0x200 + number),
                &mut local,
                &mut remote,
            );
        }
        for number in 0..16 * n {
            expand(
                &f,
                &spent.operation_id,
                &format!("{:064x}", 0x1000 + number),
                &mut local,
                &mut remote,
            );
        }
        updates(&f.repo.root, &local);
        updates(&f.remote, &remote);
        let populated = fixtures::Pristine::capture(&f);
        populated.assert(&f);
        let before = f.repo.preserved();
        let (observer, counts) = counter(&f, false);
        let mut publisher = observed(&f, observer);
        *counts.lock().unwrap() = Counts::default();
        let started = std::time::Instant::now();
        assert_eq!(publisher.resource_reports().unwrap().len(), 18 * n);
        let c = counts.lock().unwrap().clone();
        eprintln!("resources n={n}: {c:?}, {:?}", started.elapsed());
        assert!(c.resource_passes <= 2, "{c:?}");
        assert!(c.identities <= 2, "{c:?}");
        assert_eq!(c.resource_reads, 19 * n, "{c:?}");
        *counts.lock().unwrap() = Counts::default();
        assert_eq!(publisher.list().unwrap().len(), n);
        let c = counts.lock().unwrap().clone();
        eprintln!("receipts n={n}: {c:?}");
        assert!(c.receipt_passes <= 2, "{c:?}");
        assert_eq!(c.receipt_reads, 2 * n, "{c:?}");
        // Reconciliation deliberately retains each operation's current
        // authority checks; only receipt enumeration is batched here.
        drop(publisher);
        let (observer, counts) = counter(&f, true);
        let mut store = Candidates::with_resources(
            Config {
                executable: BIN.into(),
                project: f.repo.project.clone(),
                deadline: Duration::from_secs(30),
            },
            observer,
        );
        let preview = store.preview(request(), SystemTime::now()).unwrap();
        let c = counts.lock().unwrap().clone();
        eprintln!("admission n={n}: {c:?}");
        assert!(c.resource_passes <= 2, "{c:?}");
        assert!(c.identities <= 3, "{c:?}");
        assert_eq!(c.resource_reads, 19 * n + 1, "{c:?}");
        f.publisher()
            .discard_preview(&preview.operation_id)
            .unwrap();
        assert_eq!(f.repo.preserved(), before);
        drop(store);
        pristine.restore(&f);
    }
    pristine.assert(&f);
}

#[test]
fn discovery_refuses_incomplete_or_changing_batches_without_consuming_evidence() {
    let f = Fixture::new("batch-negative-replay");
    let retained = retain(&f);
    let mut store = f.repo.store();
    let spent = store.preview(request(), SystemTime::now()).unwrap();
    f.publisher().discard_preview(&spent.operation_id).unwrap();
    drop(store);
    let pristine = fixtures::Pristine::capture(&f);
    for receipts in [false, true] {
        for mode in [
            "unknown",
            "malformed",
            "symlink",
            "unreadable",
            "changed-directory",
            "late-entry",
            "late-unknown",
            "interrupted-scan",
        ] {
            pristine.assert(&f);
            let directory = if receipts {
                root(&f)
            } else {
                root(&f).join("resources")
            };
            let id = if receipts {
                &retained.operation_id
            } else {
                &spent.operation_id
            };
            let target = directory.join(format!("{id}.json"));
            let original = fs::read(&target).unwrap();
            let before = f.repo.preserved();
            let armed = Arc::new(AtomicBool::new(false));
            let enabled = armed.clone();
            let injected = Arc::new(AtomicBool::new(false));
            let flag = injected.clone();
            let file = target.clone();
            let dir = directory.clone();
            let template = original.clone();
            let observer = ResourceObserver::new(move |at| {
                if !enabled.load(Ordering::SeqCst) {
                    return true;
                }
                if mode == "interrupted-scan"
                    && at.operation == ResourceOperation::Read
                    && !at.after
                    && at.path == dir
                {
                    flag.store(true, Ordering::SeqCst);
                    return false;
                }
                if at.operation != ResourceOperation::Read || at.path != file {
                    return true;
                }
                if mode == "unreadable" && !at.after {
                    flag.store(true, Ordering::SeqCst);
                    return false;
                }
                if mode == "changed-directory" && !at.after && !flag.swap(true, Ordering::SeqCst) {
                    fs::rename(&dir, dir.with_extension("moved")).unwrap();
                    fs::create_dir(&dir).unwrap();
                }
                if mode.starts_with("late-") && at.after && !flag.swap(true, Ordering::SeqCst) {
                    if mode == "late-unknown" {
                        fs::write(dir.join("unclassified-evidence"), "preserve").unwrap();
                    } else {
                        // Deliberate valid-state fixture: the new operation must
                        // be noticed even though it was not in the first pass.
                        let id = "e".repeat(64);
                        let mut value: serde_json::Value =
                            serde_json::from_slice(&template).unwrap();
                        if receipts {
                            value["description"]["operation_id"] = id.clone().into();
                            value["description"]["output_ref"] =
                                format!("refs/heads/pbps-compose/{id}").into();
                        } else {
                            value["operation"] = id.clone().into();
                        }
                        fs::write(
                            dir.join(format!("{id}.json")),
                            serde_json::to_vec(&value).unwrap(),
                        )
                        .unwrap();
                    }
                }
                true
            });
            let mut publisher = observed(&f, observer);
            match mode {
                "unknown" => {
                    fs::write(directory.join("unclassified-evidence"), "preserve").unwrap();
                }
                "malformed" => {
                    fs::write(&target, "{}").unwrap();
                }
                "symlink" => {
                    let backup = f.repo.root.join("record-evidence");
                    fs::rename(&target, &backup).unwrap();
                    symlink(&backup, &target).unwrap();
                }
                _ => (),
            }
            armed.store(true, Ordering::SeqCst);
            let refused = if receipts {
                publisher.list().is_err()
            } else {
                publisher.resource_reports().is_err()
            };
            assert!(refused, "{receipts}/{mode}");
            if !matches!(mode, "unknown" | "malformed" | "symlink") {
                assert!(injected.load(Ordering::SeqCst), "{receipts}/{mode}");
            }
            let evidence = if mode == "changed-directory" {
                directory
                    .with_extension("moved")
                    .join(target.file_name().unwrap())
            } else {
                target.clone()
            };
            assert!(evidence.symlink_metadata().is_ok(), "{receipts}/{mode}");
            if mode != "malformed" {
                assert_eq!(fs::read(evidence).unwrap(), original, "{receipts}/{mode}");
            }
            assert_eq!(f.repo.preserved(), before, "{receipts}/{mode}");
            drop(publisher);
            pristine.restore(&f);
        }
    }
    pristine.assert(&f);
}

#[test]
fn a_batch_cannot_outlive_its_repository_binding_or_hide_new_snapshots() {
    for mode in ["repository", "snapshot"] {
        let f = Fixture::new(&format!("batch-binding-{mode}"));
        let mut store = f.repo.store();
        let preview = store.preview(request(), SystemTime::now()).unwrap();
        f.publisher()
            .discard_preview(&preview.operation_id)
            .unwrap();
        let target = root(&f).join(format!("resources/{}.json", preview.operation_id));
        let original = fs::read(&target).unwrap();
        let before = f.repo.preserved();
        let alternate = f.repo.root.join("alternate.git");
        git(
            &f.repo.root,
            &["clone", "--bare", "-q", ".", alternate.to_str().unwrap()],
        );
        let common_file = f.repo.root.join(".git/commondir");
        let injected = Arc::new(AtomicBool::new(false));
        let flag = injected.clone();
        let file = target.clone();
        let common = common_file.clone();
        let snapshots = root(&f).join("snapshots");
        let observer = ResourceObserver::new(move |at| {
            if at.operation == ResourceOperation::Read
                && at.after
                && at.path == file
                && !flag.swap(true, Ordering::SeqCst)
            {
                if mode == "repository" {
                    fs::write(&common, format!("{}\n", alternate.display())).unwrap();
                } else {
                    fs::create_dir(snapshots.join("f".repeat(64))).unwrap();
                }
            }
            true
        });
        let publisher = observed(&f, observer);
        assert!(publisher.resource_reports().is_err(), "{mode}");
        assert!(injected.load(Ordering::SeqCst));
        if mode == "repository" {
            fs::remove_file(common_file).unwrap();
        }
        assert_eq!(fs::read(target).unwrap(), original);
        assert_eq!(f.repo.preserved(), before);
    }
}

#[test]
fn admission_refuses_a_changed_batch_before_creating_another_resource() {
    for mode in ["late-entry", "late-unknown", "repository"] {
        let f = Fixture::new(&format!("batch-admission-{mode}"));
        let retained = retain(&f);
        let mut old_store = f.repo.store();
        let spent = old_store.preview(request(), SystemTime::now()).unwrap();
        f.publisher().discard_preview(&spent.operation_id).unwrap();
        let resources = root(&f).join("resources");
        let target = resources.join(format!("{}.json", retained.operation_id));
        let template = fs::read(resources.join(format!("{}.json", spent.operation_id))).unwrap();
        let before = f.repo.preserved();
        let pins = git(
            &f.repo.root,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/pbps-compose/",
            ],
        );
        let alternate = f.repo.root.join("alternate.git");
        git(
            &f.repo.root,
            &["clone", "--bare", "-q", ".", alternate.to_str().unwrap()],
        );
        let common_file = f.repo.root.join(".git/commondir");
        let common = common_file.clone();
        let injected = Arc::new(AtomicBool::new(false));
        let flag = injected.clone();
        let created = Arc::new(AtomicBool::new(false));
        let creation = created.clone();
        let observer = ResourceObserver::new(move |at| {
            if at.operation == ResourceOperation::Create
                && at.path.parent() == Some(resources.as_path())
            {
                creation.store(true, Ordering::SeqCst);
            }
            if at.operation == ResourceOperation::Read
                && at.after
                && at.path == target
                && !flag.swap(true, Ordering::SeqCst)
            {
                match mode {
                    "repository" => {
                        fs::write(&common, format!("{}\n", alternate.display())).unwrap();
                    }
                    "late-unknown" => {
                        fs::write(resources.join("unclassified-evidence"), "preserve").unwrap();
                    }
                    _ => {
                        let id = "e".repeat(64);
                        let mut value: serde_json::Value =
                            serde_json::from_slice(&template).unwrap();
                        value["operation"] = id.clone().into();
                        fs::write(
                            resources.join(format!("{id}.json")),
                            serde_json::to_vec(&value).unwrap(),
                        )
                        .unwrap();
                    }
                }
            }
            true
        });
        let mut store = Candidates::with_resources(
            Config {
                executable: BIN.into(),
                project: f.repo.project.clone(),
                deadline: Duration::from_secs(30),
            },
            observer,
        );
        assert!(
            store.preview(request(), SystemTime::now()).is_err(),
            "{mode}"
        );
        assert!(injected.load(Ordering::SeqCst), "{mode}");
        assert!(
            !created.load(Ordering::SeqCst),
            "{mode}: admission created evidence after the batch changed"
        );
        if mode == "repository" {
            fs::remove_file(common_file).unwrap();
        }
        assert_eq!(
            git(
                &f.repo.root,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/pbps-compose/"
                ]
            ),
            pins
        );
        assert_eq!(f.repo.preserved(), before);
    }
}
