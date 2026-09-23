//! Admission validates prior evidence even when discovery was never requested.

use super::*;
use std::os::unix::fs::MetadataExt;

fn spent(f: &Fixture) -> Preview {
    let preview = f
        .repo
        .store()
        .preview(request(), SystemTime::now())
        .unwrap();
    f.publisher()
        .discard_preview(&preview.operation_id)
        .unwrap();
    preview
}

pub(super) fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
}

pub(super) fn pins(f: &Fixture) -> Vec<u8> {
    git(
        &f.repo.root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/pbps-compose/",
        ],
    )
}

pub(super) fn store(f: &Fixture, observer: ResourceObserver) -> Candidates {
    Candidates::with_resources(
        Config {
            executable: BIN.into(),
            project: f.repo.project.clone(),
            deadline: Duration::from_secs(30),
        },
        observer,
    )
}

#[test]
fn direct_admission_preserves_bad_prior_evidence_without_allocating_resources() {
    for mode in [
        "truncated",
        "malformed",
        "version",
        "operation",
        "common",
        "source",
        "state",
        "unknown-field",
        "directory",
        "fifo",
        "symlink",
        "hardlink",
        "unreadable",
        "read-error",
    ] {
        // Root can read mode-000 files; injected read failure separately tests
        // unreadable I/O without claiming root follows ordinary DAC rules.
        if mode == "unreadable" && rustix::process::geteuid().is_root() {
            continue;
        }
        let f = Fixture::new(&format!("prior-record-{mode}"));
        let prior = spent(&f);
        let records = root(&f).join("resources");
        let target = records.join(format!("{}.json", prior.operation_id));
        let original = fs::read(&target).unwrap();
        match mode {
            "truncated" => fs::write(&target, b"{\"version\":").unwrap(),
            "malformed" => fs::write(&target, b"not-json").unwrap(),
            "directory" | "fifo" | "symlink" => {
                let backup = f.repo.root.join("preserved-record");
                fs::rename(&target, &backup).unwrap();
                match mode {
                    "directory" => fs::create_dir(&target).unwrap(),
                    "fifo" => {
                        checked(Command::new("mkfifo").arg(&target).output().unwrap());
                    }
                    _ => symlink(&backup, &target).unwrap(),
                }
            }
            "hardlink" => fs::hard_link(&target, f.repo.root.join("record-link")).unwrap(),
            "unreadable" => {
                fs::set_permissions(&target, fs::Permissions::from_mode(0o000)).unwrap()
            }
            "read-error" => (),
            _ => {
                let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
                match mode {
                    "version" => value["version"] = 999.into(),
                    "operation" => value["operation"] = "f".repeat(64).into(),
                    "common" => {
                        value["repository"]["common_inode"] =
                            (value["repository"]["common_inode"].as_u64().unwrap() + 1).into()
                    }
                    "source" => {
                        value["repository"]["source_inode"] =
                            (value["repository"]["source_inode"].as_u64().unwrap() + 1).into()
                    }
                    "state" => value["state"] = "sealed".into(),
                    _ => value["unknown"] = true.into(),
                }
                fs::write(&target, serde_json::to_vec(&value).unwrap()).unwrap();
            }
        }
        let metadata = fs::symlink_metadata(&target).unwrap();
        let identity = (
            metadata.dev(),
            metadata.ino(),
            metadata.mode(),
            metadata.nlink(),
            metadata.len(),
        );
        let bytes = if metadata.is_file() && mode != "unreadable" {
            Some(fs::read(&target).unwrap())
        } else {
            None
        };
        let before = f.repo.preserved();
        let old_names = names(&records);
        let old_snapshots = names(&root(&f).join("snapshots"));
        let old_pins = pins(&f);
        let created = Arc::new(AtomicBool::new(false));
        let flag = created.clone();
        let read_failed = Arc::new(AtomicBool::new(false));
        let read_flag = read_failed.clone();
        let file = target.clone();
        let directory = records.clone();
        let snapshots = root(&f).join("snapshots");
        let observer = ResourceObserver::new(move |at| {
            if at.operation == ResourceOperation::Create
                && (at.path.parent() == Some(directory.as_path())
                    || at.path.parent() == Some(snapshots.as_path()))
            {
                flag.store(true, Ordering::SeqCst);
            }
            if mode == "read-error"
                && at.operation == ResourceOperation::Read
                && !at.after
                && at.path == file
            {
                read_flag.store(true, Ordering::SeqCst);
                return false;
            }
            true
        });
        // No resource_reports/list call: direct preview owns the precondition.
        let result = store(&f, observer).preview(request(), SystemTime::now());
        assert!(
            result.is_err(),
            "{mode}: admission accepted bad prior evidence"
        );
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains(&target.display().to_string()) && error.contains("preserve"),
            "{mode}: {error}"
        );
        assert!(
            !created.load(Ordering::SeqCst),
            "{mode}: new acquisition started"
        );
        if mode == "read-error" {
            assert!(read_failed.load(Ordering::SeqCst));
        }
        assert_eq!(names(&records), old_names, "{mode}");
        assert_eq!(names(&root(&f).join("snapshots")), old_snapshots, "{mode}");
        assert_eq!(pins(&f), old_pins, "{mode}");
        let after = fs::symlink_metadata(&target).unwrap();
        assert_eq!(
            (
                after.dev(),
                after.ino(),
                after.mode(),
                after.nlink(),
                after.len()
            ),
            identity,
            "{mode}"
        );
        if let Some(bytes) = bytes {
            assert_eq!(fs::read(&target).unwrap(), bytes, "{mode}");
        }
        if mode == "unreadable" {
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(fs::read(&target).unwrap(), original);
        }
        if matches!(mode, "directory" | "fifo" | "symlink") {
            assert_eq!(
                fs::read(f.repo.root.join("preserved-record")).unwrap(),
                original
            );
        }
        if mode == "symlink" {
            assert_eq!(
                fs::read_link(&target).unwrap(),
                f.repo.root.join("preserved-record")
            );
        }
        assert_eq!(f.repo.preserved(), before, "{mode}");
    }
}

#[test]
fn admission_validates_healthy_foreign_records_without_granting_cleanup_authority() {
    let f = Fixture::new("prior-record-foreign");
    // The first acquisition also proves the empty-store positive case.
    let own = spent(&f);
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
    let foreign = spent(&other);
    let records = root(&f).join("resources");
    let old = [&own, &foreign].map(|p| {
        let file = records.join(format!("{}.json", p.operation_id));
        let bytes = fs::read(&file).unwrap();
        (file, bytes)
    });
    let before = f.repo.preserved();
    let preview = f
        .repo
        .store()
        .preview(request(), SystemTime::now())
        .unwrap();
    for (file, bytes) in &old {
        assert_eq!(fs::read(file).unwrap(), *bytes);
    }
    assert_eq!(f.repo.preserved(), before);
    assert!(
        f.publisher()
            .discard_preview(&foreign.operation_id)
            .is_err()
    );
    f.publisher()
        .discard_preview(&preview.operation_id)
        .unwrap();
    // A healthy other source is accepted only after common-store/schema validation.
    let mut bad: serde_json::Value = serde_json::from_slice(&old[1].1).unwrap();
    bad["version"] = 999.into();
    fs::write(&old[1].0, serde_json::to_vec(&bad).unwrap()).unwrap();
    let names_before = names(&records);
    let bad_bytes = fs::read(&old[1].0).unwrap();
    assert!(
        f.repo
            .store()
            .preview(request(), SystemTime::now())
            .is_err()
    );
    assert_eq!(names(&records), names_before);
    assert_eq!(fs::read(&old[1].0).unwrap(), bad_bytes);
    assert_eq!(f.repo.preserved(), before);
}

#[test]
fn admission_refuses_changed_unpinned_evidence_before_acquisition() {
    for late_entry in [false, true] {
        let f = Fixture::new(&format!("prior-record-change-{late_entry}"));
        spent(&f);
        spent(&f);
        let records = root(&f).join("resources");
        let old_names = names(&records);
        let first = records.join(&old_names[0]);
        let second = records.join(&old_names[1]);
        let template = fs::read(&first).unwrap();
        let before = f.repo.preserved();
        let old_pins = pins(&f);
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let created = Arc::new(AtomicBool::new(false));
        let creation = created.clone();
        let directory = records.clone();
        let injected = if late_entry {
            records.join(format!("{}.json", "e".repeat(64)))
        } else {
            second
        };
        let changed = injected.clone();
        let observer = ResourceObserver::new(move |at| {
            if at.operation == ResourceOperation::Create
                && at.path.parent() == Some(directory.as_path())
            {
                creation.store(true, Ordering::SeqCst);
            }
            if at.operation == ResourceOperation::Read
                && at.after
                && at.path == first
                && !flag.swap(true, Ordering::SeqCst)
            {
                if late_entry {
                    let mut value: serde_json::Value = serde_json::from_slice(&template).unwrap();
                    value["operation"] = "e".repeat(64).into();
                    fs::write(&changed, serde_json::to_vec(&value).unwrap()).unwrap();
                } else {
                    fs::write(&changed, b"truncated evidence").unwrap();
                }
            }
            true
        });
        assert!(
            store(&f, observer)
                .preview(request(), SystemTime::now())
                .is_err()
        );
        assert!(fired.load(Ordering::SeqCst));
        assert!(!created.load(Ordering::SeqCst));
        assert_eq!(
            names(&records).len(),
            old_names.len() + usize::from(late_entry)
        );
        assert!(injected.exists());
        if !late_entry {
            assert_eq!(fs::read(injected).unwrap(), b"truncated evidence");
        }
        assert!(names(&root(&f).join("snapshots")).is_empty());
        assert_eq!(pins(&f), old_pins);
        assert_eq!(f.repo.preserved(), before);
    }
}
