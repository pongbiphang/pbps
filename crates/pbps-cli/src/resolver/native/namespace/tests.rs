use super::*;

#[test]
fn a_hidden_or_unidentified_procfs_is_not_an_empty_namespace() {
    let row = "42 1 0:9 / /proc rw,nosuid,nodev,noexec - proc proc rw\n";
    visible_mount(row, 42).unwrap();
    visible_mount(&row.replace("proc rw", "proc rw,subset=pid"), 42).unwrap();
    for text in [
        row.replace("proc rw", "proc rw,hidepid=2"),
        row.replace("proc rw", "proc rw,hidepid=ptraceable"),
        row.replace(" / /proc ", " /123 /proc "),
        row.replace(" /proc ", " /elsewhere "),
        row.replace("- proc proc", "- tmpfs tmpfs"),
        format!("{row}{row}"),
        String::new(),
    ] {
        assert!(visible_mount(&text, 42).is_err(), "{text}");
    }
    assert!(visible_mount(row, 43).is_err());
}

#[test]
fn namespace_coordinates_cannot_substitute_for_another_namespaces_task() {
    let first = NamespaceTaskId {
        namespace: FileIdentity {
            device: 4,
            inode: 10,
        },
        number: 1,
    };
    let second = NamespaceTaskId {
        namespace: FileIdentity {
            device: 4,
            inode: 11,
        },
        number: 1,
    };
    assert_eq!(first.number(), second.number());
    assert_ne!(first, second);
    assert_eq!(status_id("Pid:\t17\nTgid:\t13\n", "Pid:").unwrap(), 17);
    for bad in ["", "Pid: 0", "Pid: 17 18", "Pid: 17\nPid: 18"] {
        assert!(status_id(bad, "Pid:").is_err());
    }
}

#[test]
fn held_task_exit_does_not_reopen_a_numeric_coordinate() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let directory = File::open(format!("/proc/{}", child.id())).unwrap();
    let stat = read_stat(&directory).unwrap().unwrap();
    let (id, _, start) = task_stat(&stat).unwrap();
    assert!(task_alive(&directory, id, start).unwrap());
    assert!(task_alive(&directory, id + 1, start).is_err());
    assert!(task_alive(&directory, id, start + 1).is_err());
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!task_alive(&directory, id, start).unwrap());
    assert!(group_exited(&directory).unwrap());
}

#[test]
fn proc_component_reads_reject_symlinks_and_parent_escape() {
    let root = std::env::temp_dir().join(format!("pbps-proc-view-{}", rand::random::<u64>()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("status"), "Pid: 1\n").unwrap();
    std::os::unix::fs::symlink("status", root.join("alias")).unwrap();
    let directory = File::open(&root).unwrap();
    assert_eq!(read(&directory, "status").unwrap(), "Pid: 1\n");
    assert!(read(&directory, "alias").is_err());
    assert!(read(&directory, "../status").is_err());
    assert!(read(&directory, "/proc/self/status").is_err());
    assert!(namespace_init(&directory).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn namespace_procfs_fixture_observes_external_parent_tasks_and_retains_identity() {
    let Ok(pid) = std::env::var("PBPS_NAMESPACE_FIXTURE_PID") else {
        return;
    };
    let anchor = ProcessLease::capture(pid.parse().unwrap()).unwrap();
    let view = NamespaceProcfs::capture(&anchor).unwrap();
    let first = view.observe().unwrap();
    assert!(first.iter().any(|task| task.id().number() == 1));
    assert!(
        first.iter().any(|task| match task.status().unwrap() {
            TaskReading::Live(status) =>
                task.id().number() != 1
                    && status
                        .lines()
                        .any(|line| line.strip_prefix("PPid:").is_some_and(|s| s.trim() == "0")),
            TaskReading::Exited => false,
        }),
        "the fixture must include a daemon-exec task outside the init's tree"
    );
    let second = view.observe().unwrap();
    assert!(
        first
            .iter()
            .all(|task| second.iter().any(|other| task.same_task(other).unwrap()))
    );
    let regular = File::open(std::env::temp_dir()).unwrap();
    assert_ne!(
        rustix::fs::fstatfs(&regular).unwrap().f_type,
        rustix::fs::PROC_SUPER_MAGIC
    );
    assert!(namespace_init(&regular).is_err());
    if let Ok(group) = std::env::var("PBPS_NAMESPACE_DEAD_GROUP") {
        let group: u32 = group.parse().unwrap();
        assert!(
            first
                .iter()
                .any(|task| task.group().number() == group && task.id().number() != group)
        );
        assert!(!first.iter().any(|task| task.id().number() == group));
    }
    if let Ok(other_pid) = std::env::var("PBPS_NAMESPACE_SECOND_PID") {
        let other = ProcessLease::capture(other_pid.parse().unwrap()).unwrap();
        let other_view = NamespaceProcfs::capture(&other).unwrap();
        let other_tasks = other_view.observe().unwrap();
        let this_init = first.iter().find(|task| task.id().number() == 1).unwrap();
        let other_init = other_tasks
            .iter()
            .find(|task| task.id().number() == 1)
            .unwrap();
        assert_ne!(this_init.id(), other_init.id());
        assert!(!this_init.same_task(other_init).unwrap());
    }
}

#[test]
fn namespace_mount_fixture_refuses_replaced_views_and_process_overmounts() {
    if std::env::var_os("PBPS_NAMESPACE_MOUNT_FIXTURE").is_none() {
        return;
    }
    assert_eq!(
        std::process::id(),
        1,
        "the harness must provide a private PID namespace"
    );
    // The harness runs this one test in a fresh mount/PID namespace. No
    // mount below is performed in the caller's namespace.
    let mut child = super::super::spawned_and_execed(
        std::process::Command::new("/bin/sleep").arg("30"),
        "sleep",
    );
    let anchor = ProcessLease::capture(child.id()).unwrap();
    let view = NamespaceProcfs::capture(&anchor).unwrap();
    let observations = view.observe().unwrap();
    let held = observations
        .iter()
        .find(|task| task.id().number() == child.id())
        .unwrap();
    let mount = |args: &[&str]| {
        let status = std::process::Command::new("mount")
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    };
    let unmount = |path: &str| {
        assert!(
            std::process::Command::new("umount")
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
    };
    mount(&["--bind", "/proc", "/proc"]);
    assert!(
        view.check().is_err(),
        "a different mount is not the held view"
    );
    unmount("/proc");
    view.check().unwrap();
    mount(&["-o", "remount,hidepid=2", "/proc"]);
    assert!(matches!(view.check(), Err(NamespaceError::View)));
    mount(&["-o", "remount,hidepid=0", "/proc"]);
    view.check().unwrap();
    // A different proc directory on the same filesystem passes an f_type
    // check. Opening the numeric entry must nevertheless reject its mount.
    let target = format!("/proc/{}", child.id());
    mount(&["--bind", "/proc/self", &target]);
    assert!(view.observe().is_err());
    unmount(&target);
    view.check().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        view.check().is_err(),
        "a retained procfs does not revive its anchor"
    );
    // Force reuse only inside this fixture's private PID namespace. The old
    // open proc inode must still report exit when the numeric path exists
    // again, including when the replacement starts in the same clock tick.
    let old_pid = child.id();
    std::fs::write("/proc/sys/kernel/ns_last_pid", (old_pid - 1).to_string()).unwrap();
    let mut replacement = super::super::spawned_and_execed(
        std::process::Command::new("/bin/sleep").arg("30"),
        "sleep",
    );
    assert_eq!(replacement.id(), old_pid);
    assert!(matches!(held.status().unwrap(), TaskReading::Exited));
    let new_anchor = ProcessLease::capture(replacement.id()).unwrap();
    let new_view = NamespaceProcfs::capture(&new_anchor).unwrap();
    let new_observations = new_view.observe().unwrap();
    let new_task = new_observations
        .iter()
        .find(|task| task.id() == held.id())
        .unwrap();
    assert!(!held.same_task(new_task).unwrap());
    replacement.kill().unwrap();
    replacement.wait().unwrap();
}
