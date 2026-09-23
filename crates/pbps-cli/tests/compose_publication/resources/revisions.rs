//! Real retirement and external edits at production read/pass boundaries.

use super::*;
use std::os::unix::fs::MetadataExt;
use std::sync::{Mutex, atomic::AtomicUsize};

fn preserved(repo: &Repository) -> Vec<Vec<u8>> {
    let path = |name| {
        PathBuf::from(
            String::from_utf8(git(
                &repo.root,
                &["rev-parse", "--path-format=absolute", "--git-path", name],
            ))
            .unwrap()
            .trim(),
        )
    };
    vec![
        fs::read(repo.project.join("schema/t.yml")).unwrap(),
        fs::read(repo.project.join("schema.ids.json")).unwrap(),
        fs::read(path("index")).unwrap(),
        fs::read(path("HEAD")).unwrap(),
        user_refs(&repo.root),
    ]
}

fn private_pins(f: &Fixture) -> Vec<u8> {
    git(
        &f.repo.root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/pbps-compose/",
        ],
    )
}

fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut values = fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect::<Vec<_>>();
    values.sort();
    values
}

#[test]
fn discovery_cannot_return_a_cached_preview_after_actual_retirement() {
    for linked in [false, true] {
        let f = Fixture::new(&format!("revision-retirement-{linked}"));
        let other = linked.then(|| {
            let path = f.repo.root.with_extension("linked");
            git(
                &f.repo.root,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    "-q",
                    path.to_str().unwrap(),
                    "master",
                ],
            );
            let repo = Repository {
                project: path.join("project"),
                root: path,
            };
            repo.table(RENAMED);
            repo
        });
        let source = other.as_ref().unwrap_or(&f.repo);
        let mut candidates = source.store();
        let preview = candidates.preview(request(), SystemTime::now()).unwrap();
        let candidates = Arc::new(Mutex::new(candidates));
        let before = preserved(source);
        let original_source = preserved(&f.repo);
        let snapshots = root(&f).join("snapshots");
        let directory = snapshots.clone();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let armed = Arc::new(AtomicBool::new(false));
        let enabled = armed.clone();
        let observer = ResourceObserver::new(move |at| {
            // The private-ref census has completed before snapshot enumeration.
            // Mutating during its owner read would already trip its second pass.
            if enabled.load(Ordering::SeqCst)
                && at.operation == ResourceOperation::Read
                && !at.after
                && at.path == directory
                && !flag.swap(true, Ordering::SeqCst)
            {
                let mut invalid = request();
                invalid.remote = "missing-replacement-endpoint".into();
                // Actual public preview replacement retires the old candidate;
                // the missing endpoint then refuses before acquiring a new one.
                assert!(
                    candidates
                        .lock()
                        .unwrap()
                        .preview(invalid, SystemTime::now())
                        .is_err()
                );
            }
            true
        });
        let publisher =
            Publications::open_with_resources(&source.root, Duration::from_secs(30), observer)
                .unwrap();
        let record = root(&f).join(format!("resources/{}.json", preview.operation_id));
        armed.store(true, Ordering::SeqCst);
        let result = publisher.resource_reports();
        assert!(fired.load(Ordering::SeqCst));
        let bytes = fs::read(&record).unwrap();
        let state: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(state["state"], "spent");
        assert!(names(&snapshots).is_empty());
        assert!(private_pins(&f).is_empty());
        assert!(
            result.is_err(),
            "discovery returned a stale cached lifecycle: {result:?}"
        );
        drop(publisher);
        let restarted = Publications::open(&source.root, Duration::from_secs(30)).unwrap();
        let reports = restarted.resource_reports().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].operation_id, preview.operation_id);
        assert_eq!(reports[0].state, ResourceState::Spent);
        assert!(!reports[0].cleanup_pending);
        assert_eq!(fs::read(record).unwrap(), bytes);
        assert_eq!(preserved(source), before);
        assert_eq!(preserved(&f.repo), original_source);
    }
}

fn changed_batch(closing: bool, kinds: &[&str]) {
    for &kind in kinds {
        for replace in [false, true] {
            let f = Fixture::new(&format!("revision-change-{closing}-{kind}-{replace}"));
            let mut candidates = f.repo.store();
            let now = SystemTime::now();
            let preview = candidates.preview(request(), now).unwrap();
            if kind == "receipt" {
                let candidate = candidates.confirm(&preview.candidate_id, now).unwrap();
                let mut publisher = f.publisher();
                assert_eq!(publisher.confirm(&candidate).status, Status::Delivered);
                assert!(!publisher.cleanup(&preview.operation_id).cleanup_pending);
            } else if !kind.starts_with("pinned-") {
                f.publisher()
                    .discard_preview(&preview.operation_id)
                    .unwrap();
            }
            let records = root(&f).join("resources");
            let directory = if kind == "receipt" {
                root(&f)
            } else {
                records.clone()
            };
            let target = directory.join(format!("{}.json", preview.operation_id));
            let original = fs::read(&target).unwrap();
            let corrupt = vec![b'!'; original.len()];
            let before = f.repo.preserved();
            let old_names = names(&directory);
            let old_records = names(&records);
            let old_snapshots = names(&root(&f).join("snapshots"));
            let old_pins = private_pins(&f);
            let fired = Arc::new(AtomicBool::new(false));
            let flag = fired.clone();
            let created = Arc::new(AtomicBool::new(false));
            let creation = created.clone();
            let armed = Arc::new(AtomicBool::new(false));
            let enabled = armed.clone();
            let revision = Arc::new(Mutex::new(None));
            let changed_revision = revision.clone();
            let file = target.clone();
            let dir = directory.clone();
            let resource_dir = records.clone();
            let snapshots = root(&f).join("snapshots");
            let passes = AtomicUsize::new(0);
            let observer = ResourceObserver::new(move |at| {
                if !enabled.load(Ordering::SeqCst) {
                    return true;
                }
                if at.operation == ResourceOperation::Create
                    && (at.path.parent() == Some(dir.as_path())
                        || at.path.parent() == Some(resource_dir.as_path())
                        || at.path.parent() == Some(snapshots.as_path()))
                {
                    creation.store(true, Ordering::SeqCst);
                }
                let boundary = if closing {
                    at.operation == ResourceOperation::Read
                        && !at.after
                        && at.path == dir
                        && passes.fetch_add(1, Ordering::SeqCst) == 1
                } else {
                    at.operation == ResourceOperation::Read && at.after && at.path == file
                };
                if boundary && !flag.swap(true, Ordering::SeqCst) {
                    // Synthetic external writer: same name, different inode or
                    // same-length in-place corruption; no phase is fabricated.
                    if replace {
                        let temporary = file.with_extension("replacement");
                        fs::write(&temporary, &corrupt).unwrap();
                        fs::rename(temporary, &file).unwrap();
                    } else {
                        fs::write(&file, &corrupt).unwrap();
                    }
                    let m = fs::metadata(&file).unwrap();
                    *changed_revision.lock().unwrap() = Some((m.dev(), m.ino(), m.len(), m.mode()));
                }
                true
            });
            let mut publisher = if kind.ends_with("admission") {
                None
            } else {
                Some(observed(&f, observer.clone()))
            };
            // Unchanged positive control uses the same production entry point.
            if let Some(publisher) = &mut publisher {
                if kind == "receipt" {
                    assert!(!publisher.list().unwrap().is_empty());
                } else {
                    assert!(!publisher.resource_reports().unwrap().is_empty());
                }
            }
            armed.store(true, Ordering::SeqCst);
            let refused = if let Some(publisher) = &mut publisher {
                if kind == "receipt" {
                    publisher.list().err().map(|e| e.to_string())
                } else {
                    publisher.resource_reports().err().map(|e| e.to_string())
                }
            } else {
                let mut fresh = Candidates::with_resources(
                    Config {
                        executable: BIN.into(),
                        project: f.repo.project.clone(),
                        deadline: Duration::from_secs(30),
                    },
                    observer,
                );
                fresh
                    .preview(request(), SystemTime::now())
                    .err()
                    .map(|e| e.to_string())
            };
            assert!(
                fired.load(Ordering::SeqCst),
                "{closing}/{kind}/{replace}: missed boundary"
            );
            let refused = refused
                .unwrap_or_else(|| panic!("{closing}/{kind}/{replace}: accepted changed record"));
            // The refusal names the changed record, not only that one changed.
            assert!(
                refused.contains(&target.display().to_string()),
                "{closing}/{kind}/{replace}: {refused}"
            );
            assert!(
                !created.load(Ordering::SeqCst),
                "{closing}/{kind}/{replace}: allocated or rewrote evidence"
            );
            let m = fs::metadata(&target).unwrap();
            assert_eq!(
                Some((m.dev(), m.ino(), m.len(), m.mode())),
                *revision.lock().unwrap()
            );
            assert_eq!(fs::read(&target).unwrap(), vec![b'!'; original.len()]);
            assert_eq!(names(&directory), old_names);
            assert_eq!(names(&records), old_records);
            assert_eq!(names(&root(&f).join("snapshots")), old_snapshots);
            assert_eq!(private_pins(&f), old_pins);
            assert_eq!(f.repo.preserved(), before);
        }
    }
}

#[test]
fn batch_reads_cannot_certify_revisions_observed_after_their_bytes() {
    changed_batch(
        false,
        &[
            "resource",
            "admission",
            "pinned-resource",
            "pinned-admission",
        ],
    );
}

#[test]
fn closing_batches_recheck_each_resource_revision() {
    changed_batch(
        true,
        &[
            "resource",
            "admission",
            "pinned-resource",
            "pinned-admission",
        ],
    );
}

#[test]
fn receipt_reads_cannot_certify_revisions_observed_after_their_bytes() {
    changed_batch(false, &["receipt"]);
}

#[test]
fn receipt_discovery_rechecks_revisions_before_reconciliation() {
    changed_batch(true, &["receipt"]);
}

#[test]
fn a_receipt_removed_during_discovery_is_named() {
    let f = Fixture::new("receipt-removed-during-discovery");
    let mut ids = Vec::new();
    for _ in 0..2 {
        let (_store, preview, candidate) = f.ready();
        assert_eq!(f.publisher().confirm(&candidate).status, Status::Delivered);
        ids.push(preview.operation_id);
    }
    ids.sort();
    let (first, second) = (
        root(&f).join(format!("{}.json", ids[0])),
        root(&f).join(format!("{}.json", ids[1])),
    );
    let preserved = f.repo.root.join("removed-receipt");
    let (read, moved, kept) = (first.clone(), second.clone(), preserved.clone());
    let fired = Arc::new(AtomicBool::new(false));
    let flag = fired.clone();
    let observer = ResourceObserver::new(move |at| {
        // An external writer takes the second receipt away after the name
        // pass listed it and before discovery opens it.
        if at.operation == ResourceOperation::Read
            && at.after
            && at.path == read
            && !flag.swap(true, Ordering::SeqCst)
        {
            fs::rename(&moved, &kept).unwrap();
        }
        true
    });
    let error = observed(&f, observer).list().unwrap_err().to_string();
    assert!(fired.load(Ordering::SeqCst));
    assert!(error.contains(&second.display().to_string()), "{error}");
    assert!(!error.contains(&ids[0]), "{error}");
    assert!(first.exists() && preserved.exists());
}
