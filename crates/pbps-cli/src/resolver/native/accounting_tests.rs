//! Namespace accounting must not requalify an already-accounted executable.

use super::*;
use std::process::{Command, Stdio};

struct OwnedChild(std::process::Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires a root observer and private mount/IPC/PID namespaces"]
fn known_pid_members_need_no_second_executable_qualification() {
    if std::env::var_os("PBPS_ACCOUNTING_MEMBER_FIXTURE").is_some() {
        // The normal-user-built test executable deliberately cannot earn a
        // ProcessLease, but can wait in the anchor's actual namespaces.
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).unwrap();
        return;
    }
    assert_eq!(rustix::process::geteuid().as_raw(), 0);
    let binary = std::env::current_exe().unwrap();
    assert_ne!(
        binary.metadata().unwrap().uid(),
        0,
        "build as the ordinary user"
    );
    let anchor = OwnedChild(spawned_and_execed(
        Command::new("/usr/bin/unshare").args(["--mount", "--ipc", "--", "/usr/bin/sleep", "60"]),
        "sleep",
    ));
    let lease = ProcessLease::capture(anchor.0.id()).unwrap();
    let mut member = OwnedChild(
        Command::new("/usr/bin/nsenter")
            .args(["--target", &anchor.0.id().to_string(), "--mount", "--ipc", "--"])
            .arg(&binary)
            .args(["--ignored", "--exact", "resolver::native::accounting_tests::known_pid_members_need_no_second_executable_qualification"])
            .env("PBPS_ACCOUNTING_MEMBER_FIXTURE", "1")
            .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn().unwrap(),
    );
    wait_for_exec(member.0.id(), binary.file_name().unwrap().to_str().unwrap());
    assert!(member.0.try_wait().unwrap().is_none());
    assert!(ProcessLease::capture(member.0.id()).is_err());
    let member_directory = open_process(member.0.id()).unwrap();
    for namespace in ["pid", "mnt", "ipc"] {
        let handle = File::open(proc_base(&member_directory).join("ns").join(namespace)).unwrap();
        assert!(lease.owns_namespace(namespace, &handle).unwrap());
    }
    for namespace in ["mnt", "ipc"] {
        let result = for_each_foreign_occupant(&lease, namespace, |occupant| {
            if lease.same_namespace(occupant, "pid")? {
                Ok(())
            } else {
                Err(UnqualifiedProcess)
            }
        });
        assert!(
            result.is_ok(),
            "known {namespace} members must not need executable authority"
        );
    }
    // A real foreign PID namespace sharing either namespace is still a
    // refusal. unshare --kill-child owns the nested sleep even on unwind.
    let mut foreign = OwnedChild(
        Command::new("/usr/bin/nsenter")
            .args([
                "--target",
                &anchor.0.id().to_string(),
                "--mount",
                "--ipc",
                "--",
            ])
            .args([
                "/usr/bin/unshare",
                "--pid",
                "--fork",
                "--kill-child",
                "--",
                "/usr/bin/sleep",
                "60",
            ])
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let foreign_lease = loop {
        assert!(foreign.0.try_wait().unwrap().is_none());
        let children =
            std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", foreign.0.id()))
                .unwrap();
        let candidate = children
            .split_whitespace()
            .filter_map(|id| id.parse().ok())
            .find_map(|id| ProcessLease::capture(id).ok());
        if let Some(candidate) = candidate
            && candidate.executable_path().ends_with("sleep")
            && !lease.same_namespace(&candidate, "pid").unwrap()
        {
            break candidate;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    for namespace in ["mnt", "ipc"] {
        assert!(lease.same_namespace(&foreign_lease, namespace).unwrap());
        let mut observed_foreign = false;
        let result = for_each_foreign_occupant(&lease, namespace, |occupant| {
            if lease.same_namespace(occupant, "pid")? {
                return Ok(());
            }
            observed_foreign = true;
            Err(UnqualifiedProcess)
        });
        assert!(
            observed_foreign && result.is_err(),
            "foreign {namespace} membership must refuse"
        );
    }
    drop(foreign);
    // Loss of the selected anchor can never be treated as an empty census.
    drop(member);
    let mut anchor = anchor;
    anchor.0.kill().unwrap();
    anchor.0.wait().unwrap();
    assert!(for_each_foreign_occupant(&lease, "mnt", |_| Ok(())).is_err());
    eprintln!(
        "accounting: known executable-independent members accepted; foreign mnt/ipc and anchor loss refused"
    );
}
