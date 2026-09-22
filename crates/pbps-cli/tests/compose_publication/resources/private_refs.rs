//! Missing ownership evidence never turns a surviving Git root into no work.

use super::*;

#[test]
fn orphan_commit_pins_remain_visible_after_resource_loss_and_gc() {
    for packed in [false, true] {
        let f = Fixture::new(&format!("orphan-commit-{packed}"));
        let (_store, preview, candidate) = f.ready();
        let mut publisher = f.publisher();
        let delivered = publisher.confirm(&candidate);
        assert_eq!(delivered.status, Status::Delivered);
        assert!(!publisher.cleanup(&preview.operation_id).cleanup_pending);
        let reference = format!("refs/pbps-compose/{}/commit", preview.operation_id);
        let pin = git(&f.repo.root, &["rev-parse", &reference]);
        if packed {
            git(&f.repo.root, &["pack-refs", "--all"]);
        }
        assert_eq!(f.repo.root.join(".git").join(&reference).exists(), !packed);
        fs::remove_file(root(&f).join(format!("resources/{}.json", preview.operation_id))).unwrap();
        drop(publisher);
        // Keep each physical ref representation under test. The private tag
        // itself has no public ref even though its commit was delivered.
        git(
            &f.repo.root,
            &["-c", "gc.packRefs=false", "gc", "--prune=now"],
        );
        assert_eq!(
            String::from_utf8(git(
                &f.repo.root,
                &["rev-parse", &format!("{reference}^{{commit}}")]
            ))
            .unwrap()
            .trim(),
            commit(&delivered)
        );
        let receipt = fs::read(f.record(&preview.operation_id)).unwrap();
        let foreign = f.repo.root.join(".git/index.lock");
        fs::write(&foreign, "foreign owner").unwrap();
        let before = f.repo.preserved();
        for _ in 0..2 {
            let mut restarted = f.publisher();
            let error = restarted.resource_reports().unwrap_err().to_string();
            assert!(error.contains(&reference), "{error}");
            assert!(error.contains("ownership"), "{error}");
            assert!(restarted.recover_resources(&preview.operation_id).is_err());
            let error = f
                .repo
                .store()
                .preview(request(), SystemTime::now())
                .unwrap_err();
            assert!(error.to_string().contains(&reference), "{error}");
            assert_eq!(fs::read_dir(root(&f).join("resources")).unwrap().count(), 0);
            assert_eq!(fs::read_dir(root(&f).join("snapshots")).unwrap().count(), 0);
            assert_eq!(git(&f.repo.root, &["rev-parse", &reference]), pin);
            assert_eq!(fs::read(f.record(&preview.operation_id)).unwrap(), receipt);
            assert_eq!(fs::read_to_string(&foreign).unwrap(), "foreign owner");
            assert_eq!(f.repo.preserved(), before);
            assert_eq!(
                f.remote_ref(&preview.output_ref).as_deref(),
                Some(commit(&delivered))
            );
        }
    }
}

#[test]
fn orphan_base_pins_block_admission_without_retiring_the_snapshot() {
    let f = Fixture::new("orphan-base");
    let (_store, preview, _candidate) = f.ready();
    let reference = format!("refs/pbps-compose/{}/base", preview.operation_id);
    let pin = git(&f.repo.root, &["rev-parse", &reference]);
    let snapshot = root(&f).join("snapshots").join(&preview.operation_id);
    fs::remove_file(root(&f).join(format!("resources/{}.json", preview.operation_id))).unwrap();
    let before = f.repo.preserved();
    for _ in 0..2 {
        let publisher = f.publisher();
        let error = publisher.resource_reports().unwrap_err().to_string();
        assert!(error.contains(&reference), "{error}");
        assert!(error.contains("ownership"), "{error}");
        assert!(
            f.repo
                .store()
                .preview(request(), SystemTime::now())
                .is_err()
        );
        assert!(snapshot.is_dir());
        assert_eq!(fs::read_dir(root(&f).join("resources")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(root(&f).join("snapshots")).unwrap().count(), 1);
        assert_eq!(git(&f.repo.root, &["rev-parse", &reference]), pin);
        assert_eq!(f.repo.preserved(), before);
    }
}

#[test]
fn hidden_or_malformed_private_ref_evidence_never_disappears_from_discovery() {
    for kind in [
        "operation",
        "kind",
        "symbolic",
        "dangling",
        "symlink",
        "fifo",
        "directory",
        "packed-name",
        "packed-broken",
        "packed-symlink",
    ] {
        let f = Fixture::new(&format!("private-ref-{kind}"));
        let (_store, preview, _candidate) = f.ready();
        let reference = format!("refs/pbps-compose/{}/base", preview.operation_id);
        let pin = git(&f.repo.root, &["rev-parse", &reference]);
        let namespace = f.repo.root.join(".git/refs/pbps-compose");
        let base = f.repo.root.join(".git").join(&reference);
        let packed = f.repo.root.join(".git/packed-refs");
        match kind {
            "operation" => fs::write(namespace.join("unknown"), &pin).unwrap(),
            "kind" => fs::write(base.with_file_name("unknown"), &pin).unwrap(),
            "symbolic" | "dangling" => {
                fs::write(
                    &base,
                    if kind == "symbolic" {
                        "ref: refs/heads/master\n"
                    } else {
                        "ref: refs/heads/missing\n"
                    },
                )
                .unwrap();
            }
            "symlink" => {
                fs::remove_file(&base).unwrap();
                std::os::unix::fs::symlink("../../heads/master", &base).unwrap();
            }
            "fifo" => {
                fs::remove_file(&base).unwrap();
                rustix::fs::mknodat(
                    rustix::fs::CWD,
                    &base,
                    rustix::fs::FileType::Fifo,
                    rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                    0,
                )
                .unwrap();
            }
            "directory" => {
                fs::remove_file(&base).unwrap();
                fs::create_dir(&base).unwrap();
            }
            "packed-name" => fs::write(
                &packed,
                format!(
                    "{} refs/pbps-compose/unknown/base\n",
                    String::from_utf8(pin.clone()).unwrap().trim()
                ),
            )
            .unwrap(),
            "packed-broken" => fs::write(&packed, "incomplete table\n").unwrap(),
            "packed-symlink" => std::os::unix::fs::symlink("HEAD", &packed).unwrap(),
            _ => unreachable!(),
        }
        let resource = root(&f).join(format!("resources/{}.json", preview.operation_id));
        let bytes = fs::read(&resource).unwrap();
        let preserved = || {
            vec![
                fs::read(f.repo.project.join("schema/t.yml")).unwrap(),
                fs::read(f.repo.project.join("schema.ids.json")).unwrap(),
                fs::read(f.repo.root.join(".git/index")).unwrap(),
                fs::read(f.repo.root.join(".git/HEAD")).unwrap(),
                fs::read(f.repo.root.join(".git/refs/heads/master")).unwrap(),
            ]
        };
        let before = preserved();
        for _ in 0..2 {
            assert!(f.publisher().resource_reports().is_err(), "{kind}");
            assert!(
                f.repo
                    .store()
                    .preview(request(), SystemTime::now())
                    .is_err(),
                "{kind}"
            );
            assert_eq!(fs::read(&resource).unwrap(), bytes, "{kind}");
            assert_eq!(
                fs::read_dir(root(&f).join("resources")).unwrap().count(),
                1,
                "{kind}"
            );
            assert_eq!(
                fs::read_dir(root(&f).join("snapshots")).unwrap().count(),
                1,
                "{kind}"
            );
            assert_eq!(preserved(), before, "{kind}");
        }
    }
}

#[test]
fn incomplete_or_changing_private_ref_enumeration_cannot_certify_absence() {
    for after in [false, true] {
        let f = Fixture::new(&format!("private-ref-read-{after}"));
        let (_store, _preview, _candidate) = f.ready();
        let namespace = f.repo.root.join(".git/refs/pbps-compose");
        let fired = Arc::new(AtomicBool::new(false));
        let mark = fired.clone();
        let service = observed(
            &f,
            ResourceObserver::new(move |at| {
                if at.operation == ResourceOperation::Read
                    && at.path == namespace
                    && at.after == after
                {
                    mark.store(true, Ordering::SeqCst);
                    return false;
                }
                true
            }),
        );
        assert!(service.resource_reports().is_err());
        assert!(fired.load(Ordering::SeqCst));
        drop(service);
        assert_eq!(f.publisher().resource_reports().unwrap().len(), 1);
    }

    let f = Fixture::new("private-ref-census-changed");
    let (_store, preview, _candidate) = f.ready();
    let reference = format!("refs/pbps-compose/{}/base", preview.operation_id);
    let pin = git(&f.repo.root, &["rev-parse", &reference]);
    let namespace = f.repo.root.join(".git/refs/pbps-compose");
    let orphan_id = "a".repeat(64);
    let orphan = namespace.join(&orphan_id);
    let fired = Arc::new(AtomicBool::new(false));
    let mark = fired.clone();
    let passes = std::sync::atomic::AtomicUsize::new(0);
    let service = observed(
        &f,
        ResourceObserver::new(move |at| {
            if at.operation == ResourceOperation::Read
                && !at.after
                && at.path == namespace
                && passes.fetch_add(1, Ordering::SeqCst) == 1
            {
                mark.store(true, Ordering::SeqCst);
                fs::create_dir(&orphan).unwrap();
                fs::write(orphan.join("base"), &pin).unwrap();
            }
            true
        }),
    );
    let error = service.resource_reports().unwrap_err().to_string();
    assert!(fired.load(Ordering::SeqCst));
    assert!(error.contains("changed during"), "{error}");
    drop(service);
    let error = f.publisher().resource_reports().unwrap_err().to_string();
    assert!(error.contains(&orphan_id), "{error}");
    assert!(error.contains("ownership"), "{error}");
}
