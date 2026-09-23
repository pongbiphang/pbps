//! Lifecycle evidence no transition writes is refused, not consumed.
//!
//! Every bad record here starts as an actual Git-backed record and then has
//! fields edited into a combination the acquisition and retirement code cannot
//! produce. These are synthetic state fixtures: they prove the validator, not
//! interruption handling, which #748 qualifies across real process boundaries.

use super::admission::{names, pins, store};
use super::*;
use pbps_ui::compose::ResourceBoundary;
use std::collections::BTreeMap;

/// The shared store: a linked worktree's `.git` is a file, not the store.
fn common(f: &Fixture) -> PathBuf {
    let path = git(
        &f.repo.root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    PathBuf::from(String::from_utf8(path).unwrap().trim()).join("pbps-compose-v2")
}

fn file(f: &Fixture, id: &str) -> PathBuf {
    common(f).join(format!("resources/{id}.json"))
}

fn edit(f: &Fixture, id: &str, change: impl FnOnce(&mut serde_json::Value)) {
    let path = file(f, id);
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    change(&mut value);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn remove_pin(f: &Fixture, id: &str, kind: &str) {
    git(
        &f.repo.root,
        &[
            "update-ref",
            "-d",
            &format!("refs/pbps-compose/{id}/{kind}"),
        ],
    );
}

fn failing(
    f: &Fixture,
    fail: impl Fn(&ResourceBoundary) -> bool + Send + Sync + 'static,
) -> String {
    let observer = ResourceObserver::new(move |at| !fail(at));
    assert!(
        store(f, observer)
            .preview(request(), SystemTime::now())
            .is_err()
    );
    let reports = f.publisher().resource_reports().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].state, ResourceState::Capturing);
    reports[0].operation_id.clone()
}

/// An actual capture interrupted after its base pin was acknowledged.
fn capturing(f: &Fixture) -> String {
    let id = failing(f, |at| {
        at.operation == ResourceOperation::Write
            && !at.after
            && at.path.ends_with("project/schema/t.yml")
    });
    let record = read(f, &id);
    assert_eq!(record["pins"]["base"]["owned"], true);
    id
}

/// An actual capture interrupted before its base pin was acknowledged.
fn unacknowledged_base(f: &Fixture) -> String {
    let id = failing(f, |at| {
        at.operation == ResourceOperation::Create
            && !at.after
            && at.path.ends_with("base")
            && at.path.to_string_lossy().contains("refs/pbps-compose/")
    });
    assert_eq!(read(f, &id)["pins"]["base"]["owned"], false);
    id
}

fn retained(f: &Fixture) -> String {
    let (_store, preview, candidate) = f.ready();
    let mut publisher = f.publisher();
    assert_eq!(publisher.confirm(&candidate).status, Status::Delivered);
    assert!(!publisher.cleanup(&preview.operation_id).cleanup_pending);
    assert_eq!(read(f, &preview.operation_id)["state"], "retained");
    preview.operation_id
}

fn read(f: &Fixture, id: &str) -> serde_json::Value {
    serde_json::from_slice(&fs::read(file(f, id)).unwrap()).unwrap()
}

/// Admission, discovery and the direct recovery action each refuse, and none
/// of them creates, retires or rewrites anything. `witness` is the main
/// worktree, whose index and HEAD files a linked worktree does not have.
fn refused_everywhere(label: &str, f: &Fixture, id: &str, witness: &Fixture) {
    let target = file(f, id);
    let bytes = fs::read(&target).unwrap();
    let records = common(f).join("resources");
    let snapshots = common(f).join("snapshots");
    let old_names = names(&records);
    let old_snapshots = names(&snapshots);
    let old_pins = pins(f);
    let before = witness.repo.preserved();
    let created = Arc::new(AtomicBool::new(false));
    let flag = created.clone();
    let (directory, private) = (records.clone(), snapshots.clone());
    let observer = ResourceObserver::new(move |at| {
        if at.operation == ResourceOperation::Create
            && (at.path.parent() == Some(directory.as_path())
                || at.path.parent() == Some(private.as_path())
                || at.path.to_string_lossy().contains("refs/pbps-compose/"))
        {
            flag.store(true, Ordering::SeqCst);
        }
        true
    });
    // Direct preview first: no discovery call has classified the record.
    let error = store(f, observer)
        .preview(request(), SystemTime::now())
        .map(|preview| preview.operation_id)
        .expect_err(&format!("{label}: admission accepted impossible evidence"))
        .to_string();
    assert!(error.contains("preserve"), "{label}: {error}");
    assert!(!created.load(Ordering::SeqCst), "{label}: acquired anyway");
    assert!(
        f.publisher().resource_reports().is_err(),
        "{label}: discovery"
    );
    assert!(
        f.publisher().recover_resources(id).is_err(),
        "{label}: direct recovery consumed the evidence"
    );
    assert_eq!(fs::read(&target).unwrap(), bytes, "{label}");
    assert_eq!(names(&records), old_names, "{label}");
    assert_eq!(names(&snapshots), old_snapshots, "{label}");
    assert_eq!(pins(f), old_pins, "{label}");
    assert_eq!(witness.repo.preserved(), before, "{label}");
}

#[test]
fn capturing_evidence_with_a_retired_base_pin_is_refused_in_either_worktree() {
    for foreign in [false, true] {
        let f = Fixture::new(&format!("lifecycle-retired-base-{foreign}"));
        let linked = f.repo.root.with_extension("linked");
        let owner = if foreign {
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
                    root: linked.clone(),
                },
                remote: f.remote.clone(),
            };
            other.repo.table(RENAMED);
            Some(other)
        } else {
            None
        };
        let source = owner.as_ref().unwrap_or(&f);
        let id = capturing(source);
        // Retirement intent was never recorded, yet the pin reads retired and
        // no physical root survives: the #802 record.
        edit(&f, &id, |r| r["pins"]["base"]["retired"] = true.into());
        remove_pin(&f, &id, "base");
        refused_everywhere(&format!("retired-base foreign={foreign}"), &f, &id, &f);
        if let Some(other) = owner {
            // The owning worktree gets no more authority over it than a peer.
            refused_everywhere("retired-base owner", &other, &id, &f);
        }
    }
}

#[test]
fn every_state_refuses_pin_and_keep_commit_combinations_no_transition_writes() {
    type Case = (&'static str, fn(&Fixture) -> String, fn(&Fixture, &str));
    let cases: [Case; 7] = [
        ("unowned-base-retired", unacknowledged_base, |f, id| {
            edit(f, id, |r| r["pins"]["base"]["retired"] = true.into())
        }),
        ("capturing-keep-commit", capturing, |f, id| {
            edit(f, id, |r| r["keep_commit"] = true.into())
        }),
        (
            "sealed-keep-commit",
            |f| {
                f.repo
                    .store()
                    .preview(request(), SystemTime::now())
                    .unwrap()
                    .operation_id
            },
            |f, id| edit(f, id, |r| r["keep_commit"] = true.into()),
        ),
        (
            "confirmed-keep-commit",
            |f| {
                let (_store, preview, _candidate) = f.ready();
                assert_eq!(read(f, &preview.operation_id)["state"], "confirmed");
                preview.operation_id
            },
            |f, id| edit(f, id, |r| r["keep_commit"] = true.into()),
        ),
        (
            // Retirement removes the snapshot before any pin.
            "retiring-pin-before-snapshot",
            |f| {
                f.repo
                    .store()
                    .preview(request(), SystemTime::now())
                    .unwrap()
                    .operation_id
            },
            |f, id| {
                edit(f, id, |r| {
                    r["state"] = "retiring".into();
                    r["pins"]["base"]["retired"] = true.into();
                });
                remove_pin(f, id, "base");
            },
        ),
        (
            // Retirement clears the snapshot and inventory as it marks them.
            "retiring-retired-snapshot-kept",
            |f| {
                f.repo
                    .store()
                    .preview(request(), SystemTime::now())
                    .unwrap()
                    .operation_id
            },
            |f, id| {
                edit(f, id, |r| {
                    r["state"] = "retiring".into();
                    r["snapshot_retired"] = true.into();
                })
            },
        ),
        (
            // Retirement removes the base pin before the commit pin.
            "retiring-commit-before-base",
            retained,
            |f, id| {
                edit(f, id, |r| {
                    r["state"] = "retiring".into();
                    r["keep_commit"] = false.into();
                    r["pins"]["base"]["retired"] = false.into();
                    r["pins"]["commit"]["retired"] = true.into();
                });
                remove_pin(f, id, "commit");
            },
        ),
    ];
    for (label, acquire, corrupt) in cases {
        let f = Fixture::new(&format!("lifecycle-{label}"));
        let id = acquire(&f);
        corrupt(&f, &id);
        refused_everywhere(label, &f, &id, &f);
    }
}

#[test]
fn legitimate_incomplete_and_retired_states_remain_admissible() {
    // Positive controls for the refusals above: an unacknowledged base intent,
    // an unacknowledged commit intent, a retained receipt root and a spent
    // tombstone are all states the transitions write.
    let f = Fixture::new("lifecycle-legitimate");
    let base = unacknowledged_base(&f);
    let retained = retained(&f);
    let (_store, preview, candidate) = f.ready();
    let observer = ResourceObserver::new(|at| {
        !(at.operation == ResourceOperation::Create
            && !at.after
            && at.path.ends_with("commit")
            && at.path.to_string_lossy().contains("refs/pbps-compose/"))
    });
    let outcome = observed(&f, observer).confirm(&candidate);
    assert_ne!(outcome.status, Status::Delivered, "{outcome:?}");
    let intent = read(&f, &preview.operation_id);
    assert_eq!(intent["state"], "confirmed");
    assert_eq!(intent["pins"]["commit"]["owned"], false);
    let spent = f
        .repo
        .store()
        .preview(request(), SystemTime::now())
        .unwrap()
        .operation_id;
    f.publisher().discard_preview(&spent).unwrap();
    let states = |f: &Fixture| {
        f.publisher()
            .resource_reports()
            .unwrap()
            .into_iter()
            .map(|report| (report.operation_id, report.state))
            .collect::<BTreeMap<_, _>>()
    };
    let expected = BTreeMap::from([
        (base, ResourceState::Capturing),
        (retained, ResourceState::Retained),
        (preview.operation_id, ResourceState::Confirmed),
        (spent, ResourceState::Spent),
    ]);
    assert_eq!(states(&f), expected);
    let admitted = f
        .repo
        .store()
        .preview(request(), SystemTime::now())
        .unwrap();
    let mut after = states(&f);
    assert_eq!(
        after.remove(&admitted.operation_id),
        Some(ResourceState::Sealed)
    );
    assert_eq!(after, expected);
}
