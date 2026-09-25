//! Mutate only this fixture's forwarder guards, preserving Docker's report.

use super::*;
use crate::resolver::native::for_each_namespace_task;
use rustix::process::{Pid, Resource, Rlimit, prlimit};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, Debug)]
enum Change {
    Higher,
    Missing,
    Unreadable,
}

// Restore the observer view and owned process even if another assertion fails.
// The host and the daemon never enter this test's private mount namespace.
struct ChangedGuard {
    pid: Pid,
    original: Option<Rlimit>,
    mounted: Option<PathBuf>,
    empty: Option<PathBuf>,
}

impl ChangedGuard {
    fn apply(guard: &ProcessLease, change: Change) -> Self {
        let pid = guard.observer_pid().unwrap();
        let mut changed = Self {
            pid: Pid::from_raw(pid.try_into().unwrap()).unwrap(),
            original: None,
            mounted: None,
            empty: None,
        };
        match change {
            Change::Higher => {
                changed.original = Some(
                    prlimit(
                        Some(changed.pid),
                        Resource::Nofile,
                        Rlimit {
                            current: Some(1024),
                            maximum: Some(2048),
                        },
                    )
                    .unwrap(),
                );
                assert_eq!(limits(guard), (1024, 2048));
            }
            Change::Missing | Change::Unreadable => {
                let source = if matches!(change, Change::Missing) {
                    let path = std::env::temp_dir()
                        .join(format!("pbps-empty-limits-{:032x}", rand::random::<u128>()));
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&path)
                        .unwrap();
                    changed.empty = Some(path.clone());
                    path
                } else {
                    // clear_refs has no read operation, unlike an empty file.
                    PathBuf::from(format!("/proc/{pid}/clear_refs"))
                };
                let target = PathBuf::from(format!("/proc/{pid}/limits"));
                assert!(
                    Command::new("/usr/bin/mount")
                        .arg("--bind")
                        .arg(&source)
                        .arg(&target)
                        .status()
                        .unwrap()
                        .success()
                );
                changed.mounted = Some(target);
                let observed = guard.read_proc("limits", 16384);
                if matches!(change, Change::Missing) {
                    assert_eq!(observed.unwrap(), "");
                } else {
                    assert!(observed.is_err());
                }
            }
        }
        guard.check().expect("only the limit evidence changes");
        changed
    }

    fn restore(&mut self) {
        if let Some(original) = self.original.take() {
            prlimit(Some(self.pid), Resource::Nofile, original).unwrap();
        }
        if let Some(target) = self.mounted.as_ref() {
            assert!(
                Command::new("/usr/bin/umount")
                    .arg(target)
                    .status()
                    .unwrap()
                    .success()
            );
            self.mounted = None;
        }
        if let Some(empty) = self.empty.take() {
            std::fs::remove_file(empty).unwrap();
        }
    }
}

impl Drop for ChangedGuard {
    fn drop(&mut self) {
        if let Some(original) = self.original.take() {
            let _ = prlimit(Some(self.pid), Resource::Nofile, original);
        }
        if let Some(target) = self.mounted.take() {
            let _ = Command::new("/usr/bin/umount").arg(target).status();
        }
        if let Some(empty) = self.empty.take() {
            let _ = std::fs::remove_file(empty);
        }
    }
}

fn limits(process: &ProcessLease) -> (u64, u64) {
    let text = process.read_proc("limits", 16384).unwrap();
    let row = text
        .lines()
        .find(|line| line.starts_with("Max open files"))
        .unwrap();
    let fields: Vec<_> = row.split_whitespace().collect();
    (fields[3].parse().unwrap(), fields[4].parse().unwrap())
}

fn bounded_children(root: &ProcessLease) {
    let mut observed = 0;
    for_each_namespace_task(root, |_, process| {
        if !process.same_process(root)? {
            assert_eq!(limits(&process), (1024, 1024));
            observed += 1;
        }
        Ok(())
    })
    .unwrap();
    assert!(observed > 0, "the forwarder has live bounded children");
}

#[tokio::test]
#[ignore = "requires owned dedicated servers and a private observer mount namespace"]
async fn every_forwarder_guard_requires_effective_descriptor_evidence() {
    fixture();
    assert_eq!(
        std::env::var("PBPS_LIMITS_PRIVATE_PROC_FIXTURE").as_deref(),
        Ok("1")
    );
    assert_ne!(
        std::fs::read_link("/proc/self/ns/mnt").unwrap(),
        std::fs::read_link("/proc/1/ns/mnt").unwrap()
    );
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let marker = std::env::var("PBPS_SERVER_MARKER_DATABASE").unwrap();
    let mut target = native_target().await;
    let mut accepted = Vec::new();
    for control in [true, false] {
        for change in [Change::Higher, Change::Missing, Change::Unreadable] {
            let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
            let mut run = server
                .open_scratch(&scratch_recipe(&mut target).await)
                .await
                .unwrap();
            let connection = &mut run.scratch.as_mut().unwrap().connection;
            connection
                .execute("CREATE TABLE pbps_limit_table (id integer)")
                .await
                .unwrap();
            connection
                .execute("CREATE VIEW pbps_limit_view AS SELECT id FROM pbps_limit_table")
                .await
                .unwrap();
            run.check(&mut target)
                .await
                .expect("bounded forwarders support real DDL");
            let (mut changed, admission_refused) = {
                let session = if control {
                    run.inner.control.session.as_ref().unwrap()
                } else {
                    run.scratch.as_ref().unwrap()
                };
                assert_eq!(limits(&session.guard), (1024, 1024));
                bounded_children(&session.guard);
                let changed = ChangedGuard::apply(&session.guard, change);
                bounded_children(&session.guard);
                session
                    .forwarder
                    .check()
                    .await
                    .expect("Docker's requested limit is unchanged");
                // This is the same kernel admission routine Session::open uses,
                // before publishing a qualified session; no test-only bypass.
                let init = run.inner.analysis.as_ref().unwrap().runtime.init();
                let refused = session.check_kernel(init).is_err();
                (changed, refused)
            };
            let recheck_refused = run.check(&mut target).await.is_err();
            changed.restore();
            let terminal = run.check(&mut target).await.is_err() && run.inner.live().is_err();
            run.close()
                .await
                .expect("limit loss must not lose cleanup ownership");
            let mut admin = session(&configured, maintenance()).await;
            assert_eq!(
                exists(&mut admin.connection, "pbps_scratch_").await,
                (false, false)
            );
            assert_eq!(
                exists(&mut admin.connection, "pbps_run_").await,
                (false, false)
            );
            assert!(exists(&mut admin.connection, &marker).await.0);
            admin.close().await;
            eprintln!(
                "guard limits control={control} change={change:?}: admission_refused={admission_refused}, recheck_refused={recheck_refused}, terminal={terminal}, cleanup=confirmed"
            );
            if !admission_refused || !recheck_refused || !terminal {
                accepted.push((control, change));
            }
        }
    }
    target.check().await.unwrap();
    assert!(
        accepted.is_empty(),
        "forwarder guard evidence was accepted: {accepted:?}"
    );
}
