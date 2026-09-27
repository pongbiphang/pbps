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
    process: std::fs::File,
    original: Option<Rlimit>,
    mounted: Option<PathBuf>,
    empty: Option<PathBuf>,
}

impl ChangedGuard {
    fn apply(guard: &ProcessLease, change: Change) -> Self {
        let pid = guard.observer_pid().unwrap();
        let mut changed = Self {
            pid: Pid::from_raw(pid.try_into().unwrap()).unwrap(),
            process: std::fs::File::open(format!("/proc/{pid}")).unwrap(),
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

    fn process_is_gone(&self) -> std::io::Result<bool> {
        // The directory pins the original process, so PID reuse cannot make
        // absence refer to a different process. Other read failures are not
        // evidence that the kernel removed this process's procfs mounts.
        match rustix::fs::openat(
            &self.process,
            "stat",
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(_) => Ok(false),
            Err(rustix::io::Errno::SRCH | rustix::io::Errno::NOENT) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    fn restore(&mut self) -> std::io::Result<()> {
        if let Some(original) = self.original {
            if !self.process_is_gone()? {
                match prlimit(Some(self.pid), Resource::Nofile, original) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::SRCH) if self.process_is_gone()? => {}
                    Err(error) => return Err(error.into()),
                }
            }
            self.original = None;
        }
        if let Some(target) = self.mounted.as_ref() {
            // Refusing a scratch session retires its stream. Its forwarder
            // can exit before restoration, and procfs then removes the bind
            // mount itself. A live process's failed unmount is still a failure.
            if !self.process_is_gone()? {
                let status = Command::new("/usr/bin/umount").arg(target).status()?;
                if !status.success() && !self.process_is_gone()? {
                    return Err(std::io::Error::other(format!(
                        "restore live guard mount {}: {status}",
                        target.display()
                    )));
                }
            }
            let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
            // This generated /proc/<decimal pid>/limits path needs no
            // mountinfo escaping. Unreadable or remaining mounts must fail.
            if mounts
                .lines()
                .any(|line| line.split_whitespace().nth(4) == target.to_str())
            {
                return Err(std::io::Error::other(format!(
                    "guard mount remains at {}",
                    target.display()
                )));
            }
            self.mounted = None;
        }
        if let Some(empty) = self.empty.as_ref() {
            std::fs::remove_file(empty)?;
            self.empty = None;
        }
        Ok(())
    }
}

impl Drop for ChangedGuard {
    fn drop(&mut self) {
        let _ = self.restore();
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
            let mut run = open_when_exclusive(&mut target).await;
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
            changed.restore().expect("restore the owned guard fixture");
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

#[test]
#[ignore = "requires a private observer mount namespace"]
fn guard_restoration_distinguishes_process_exit_from_live_cleanup_failure() {
    use std::os::unix::fs::PermissionsExt as _;

    assert_eq!(
        std::env::var("PBPS_LIMITS_PRIVATE_PROC_FIXTURE").as_deref(),
        Ok("1")
    );
    assert_ne!(
        std::fs::read_link("/proc/self/ns/mnt").unwrap(),
        PathBuf::from(std::env::var("PBPS_LIMITS_PARENT_MOUNT_NAMESPACE").unwrap())
    );
    struct OwnedProcess {
        child: std::process::Child,
        directory: PathBuf,
    }
    impl OwnedProcess {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "pbps-guard-cleanup-{:032x}",
                rand::random::<u128>()
            ));
            std::fs::create_dir(&directory).unwrap();
            let executable = directory.join("sleep");
            // The fixture owns this root-installed executable in either the
            // native CI namespace or an isolated local user namespace.
            std::fs::copy("/bin/sleep", &executable).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
            let child = Command::new(&executable).arg("60").spawn().unwrap();
            let process = Self { child, directory };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let observed = std::fs::read_link(format!("/proc/{}/exe", process.child.id()));
                if observed.as_ref().is_ok_and(|path| path == &executable) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "owned child did not exec: {observed:?}"
                );
                std::thread::yield_now();
            }
            process
        }

        fn stop(&mut self) {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
        }
    }
    impl Drop for OwnedProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    // Raising a live hard limit back needs host CAP_SYS_RESOURCE and remains
    // covered by the engine fixture. These mount-lifetime cases also run in
    // an unprivileged user's private namespace.
    for (exited, change) in [
        (false, Change::Missing),
        (false, Change::Unreadable),
        (true, Change::Higher),
        (true, Change::Missing),
        (true, Change::Unreadable),
    ] {
        let mut process = OwnedProcess::new();
        let guard = ProcessLease::capture(process.child.id()).unwrap();
        let original = limits(&guard);
        let mut changed = ChangedGuard::apply(&guard, change);
        let empty = changed.empty.clone();
        if exited {
            process.stop();
        }
        changed.restore().unwrap();
        assert!(changed.original.is_none() && changed.mounted.is_none());
        if let Some(empty) = empty {
            assert!(!empty.exists());
        }
        if !exited {
            guard.check().unwrap();
            assert_eq!(limits(&guard), original);
        }
        eprintln!("guard restoration exited={exited} change={change:?}: confirmed");
    }

    let process = OwnedProcess::new();
    let guard = ProcessLease::capture(process.child.id()).unwrap();
    let mut changed = ChangedGuard::apply(&guard, Change::Missing);
    let target = changed.mounted.as_ref().unwrap();
    assert!(
        Command::new("/usr/bin/umount")
            .arg(target)
            .status()
            .unwrap()
            .success()
    );
    let error = changed
        .restore()
        .expect_err("a live unmount failure must not pass");
    assert!(
        error.to_string().contains("restore live guard mount"),
        "{error}"
    );
    guard.check().unwrap();
    // This negative case deliberately removed the mount itself. Restore the
    // remaining owned file without asking Drop to repeat the expected error.
    changed.mounted = None;
    changed.restore().unwrap();

    let mut process = OwnedProcess::new();
    let guard = ProcessLease::capture(process.child.id()).unwrap();
    let mut changed = ChangedGuard::apply(&guard, Change::Missing);
    process.stop();
    let own_proc = PathBuf::from(format!("/proc/{}", std::process::id()));
    let mountinfo = own_proc.join("mountinfo");
    assert!(
        Command::new("/usr/bin/mount")
            .arg("--bind")
            .arg(own_proc.join("clear_refs"))
            .arg(&mountinfo)
            .status()
            .unwrap()
            .success()
    );
    let unreadable = changed.restore();
    assert!(
        Command::new("/usr/bin/umount")
            .arg(&mountinfo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        unreadable.is_err(),
        "unreadable mount evidence must not mean absent"
    );
    assert!(
        changed.mounted.is_some(),
        "failed restoration retains ownership"
    );
    changed.restore().unwrap();
}
