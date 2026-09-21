use super::*;

#[test]
fn detached_coordinates_retain_one_namespace_handle_until_the_last_clone_drops() {
    if std::env::var_os("PBPS_NAMESPACE_COORDINATE_FIXTURE").is_none() {
        return;
    }
    assert_eq!(
        std::process::id(),
        1,
        "the fixture needs a private PID namespace"
    );
    let namespace = FileIdentity::of(&File::open("/proc/self/ns/pid").unwrap()).unwrap();
    let descriptors = || {
        use std::os::unix::fs::MetadataExt;
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .map(|entry| std::fs::metadata(entry.unwrap().path()).unwrap())
            .filter(|metadata| {
                FileIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                } == namespace
            })
            .count()
    };
    for group_coordinate in [false, true] {
        let before = descriptors();
        let mut child = super::super::spawned_and_execed(
            std::process::Command::new("/bin/sleep").arg("30"),
            "sleep",
        );
        let number = child.id();
        let anchor = ProcessLease::capture(number).unwrap();
        let view = NamespaceProcfs::capture(&anchor).unwrap();
        let observed = view.observe().unwrap();
        let task = observed
            .iter()
            .find(|task| task.id().number() == number)
            .unwrap();
        let id = if group_coordinate {
            task.group()
        } else {
            task.id()
        };
        assert_eq!(id, task.group());
        // Independent captures of this namespace must compare by kernel identity,
        // not by the address of the shared owner or its descriptor number.
        let other_view = NamespaceProcfs::capture(&anchor).unwrap();
        let others = other_view.observe().unwrap();
        let other = others
            .iter()
            .find(|task| task.id().number() == number)
            .unwrap()
            .id();
        assert_eq!(id, other);
        assert_eq!(id.cmp(&other), std::cmp::Ordering::Equal);
        drop(other);
        drop(others);
        drop(other_view);
        drop(observed);
        drop(view);
        drop(anchor);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(
            descriptors(),
            before + 1,
            "detached coordinates must retain the namespace capability"
        );

        let copies = vec![id.clone(); 1024];
        drop(id);
        assert!(copies.iter().all(|id| id.number() == number));
        assert_eq!(
            descriptors(),
            before + 1,
            "cloning coordinates must share one descriptor"
        );
        drop(copies);
        assert_eq!(
            descriptors(),
            before,
            "the final coordinate releases its namespace handle"
        );
    }
}

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
fn namespace_coordinates_distinguish_task_numbers_and_reject_malformed_ids() {
    let handle = Arc::new(File::open("/proc/self/ns/pid").unwrap());
    let namespace = FileIdentity::of(&handle).unwrap();
    let first = NamespaceTaskId {
        namespace,
        number: 1,
        handle: Arc::clone(&handle),
    };
    let second = NamespaceTaskId {
        namespace,
        number: 2,
        handle,
    };
    assert_ne!(first.number(), second.number());
    assert_ne!(first, second);
    assert!(first < second);
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
fn a_held_task_iterator_may_disappear_only_after_its_group_exits() {
    let mut child = super::super::spawned_and_execed(
        std::process::Command::new("/bin/sleep").arg("30"),
        "sleep",
    );
    let group = File::open(format!("/proc/{}", child.id())).unwrap();
    let tasks = open(&group, "task", OFlags::DIRECTORY).unwrap();
    assert!(task_ids(&group, &tasks).unwrap().contains(&child.id()));
    let not_a_directory = open(&group, "stat", OFlags::empty()).unwrap();
    assert!(
        task_ids(&group, &not_a_directory).is_err(),
        "a live unreadable group must refuse"
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        ids(&tasks).is_err(),
        "the kernel fixture must exercise the failed iterator"
    );
    assert!(task_ids(&group, &tasks).unwrap().is_empty());
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
    // Production consumers qualify the held task itself. The namespace's
    // init is the same live process even though this view's directory inode
    // differs from the observer's, and a worker survives its dead leader.
    let mut qualified = std::collections::BTreeSet::new();
    for_each_namespace_task(&anchor, |task, process| {
        assert!(process.observer_pid().is_err());
        assert_eq!(process.same_process(&anchor)?, task.id().number() == 1);
        super::super::security(&process, 999, 0)?;
        qualified.insert(task.id().number());
        Ok(())
    })
    .unwrap();
    assert_eq!(
        qualified,
        first.iter().map(|task| task.id().number()).collect()
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
        let held_group = open(&view.directory, &group.to_string(), OFlags::DIRECTORY).unwrap();
        let unreadable_tasks = open(&held_group, "stat", OFlags::empty()).unwrap();
        assert!(
            task_ids(&held_group, &unreadable_tasks).is_err(),
            "a dead leader with a live worker is not an exited group"
        );
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
        assert!(!anchor.same_process(&other).unwrap());
    }
}

#[test]
fn foreign_namespace_sharers_remain_visible_outside_the_container_pid_view() {
    let Ok(pid) = std::env::var("PBPS_NAMESPACE_FOREIGN_PID") else {
        return;
    };
    let namespace = std::env::var("PBPS_NAMESPACE_FOREIGN_KIND").unwrap();
    assert!(matches!(namespace.as_str(), "mnt" | "ipc"));
    let anchor = ProcessLease::capture(pid.parse().unwrap()).unwrap();
    let mut foreign = false;
    let result = super::super::for_each_occupant(&anchor, &namespace, |occupant| {
        if !anchor.same_namespace(occupant, "pid")? {
            foreign = true;
            Err(UnqualifiedProcess)
        } else {
            Ok(())
        }
    });
    assert!(
        foreign,
        "PID-scoped admission must retain the separate foreign-sharer census"
    );
    assert!(result.is_err());
}

#[test]
fn reparenting_during_qualification_keeps_the_held_grandchild() {
    let Ok(pid) = std::env::var("PBPS_NAMESPACE_ORPHAN_PID") else {
        return;
    };
    let anchor = ProcessLease::capture(pid.parse().unwrap()).unwrap();
    let ready = anchor.read_root_file("dev/shm/ready", 128).unwrap();
    let ids: Vec<u32> = ready
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    let mut grandchild = None;
    let mut parent_exited = false;
    for_each_namespace_task(&anchor, |task, process| {
        super::super::security(&process, 999, 0)?;
        if task.id().number() == ids[1] {
            grandchild = Some(process);
        } else if task.id().number() == ids[0] {
            // The fixture owns this container and its private signal file.
            // Release the parent only after its task has been inspected.
            use std::io::Write as _;
            // Open the existing fixture signal without O_CREAT: Linux's
            // protected_regular correctly refuses creating over another
            // user's file in a sticky directory, even for a root observer.
            std::fs::OpenOptions::new()
                .write(true)
                .open(super::super::proc_base(&anchor.directory).join("root/dev/shm/release"))
                .unwrap()
                .write_all(b"1")
                .unwrap();
            for _ in 0..1000 {
                if matches!(task.status().unwrap(), TaskReading::Exited) {
                    parent_exited = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(parent_exited);
        }
        Ok(())
    })
    .unwrap();
    assert!(parent_exited);
    let child = grandchild.expect("reparenting must not remove the live grandchild");
    child.check().unwrap();
    assert_eq!(
        status_id(&child.read_proc("status", 65536).unwrap(), "PPid:").unwrap(),
        1
    );
}

#[test]
fn a_containers_task_count_does_not_consume_the_observers_descriptor_budget() {
    let Ok(pid) = std::env::var("PBPS_NAMESPACE_MANY_PID") else {
        return;
    };
    let limits = std::fs::read_to_string("/proc/self/limits").unwrap();
    assert_eq!(
        limits
            .lines()
            .find_map(|line| line.strip_prefix("Max open files"))
            .and_then(|fields| fields.split_whitespace().next()),
        Some("64")
    );
    let anchor = ProcessLease::capture(pid.parse().unwrap()).unwrap();
    let mut count = 0;
    for_each_namespace_task(&anchor, |_, process| {
        super::super::security(&process, 999, 0)?;
        count += 1;
        Ok(())
    })
    .unwrap();
    assert!(
        count >= 98,
        "every worker and the container init must be checked"
    );
}

#[test]
fn departing_incidental_tasks_do_not_refuse_an_unchanged_container_profile() {
    let Ok(pid) = std::env::var("PBPS_NAMESPACE_CHURN_PID") else {
        return;
    };
    let anchor = ProcessLease::capture(pid.parse().unwrap()).unwrap();
    let started = std::time::Instant::now();
    for _ in 0..500 {
        for_each_namespace_task(&anchor, |_, process| {
            super::super::security(&process, 999, 0)
        })
        .unwrap();
    }
    eprintln!(
        "500 container qualifications under fork/exit churn: {:?}",
        started.elapsed()
    );
}

#[test]
fn container_membership_does_not_hide_wrong_credentials_or_foreign_cgroups() {
    let Ok(pid) = std::env::var("PBPS_NAMESPACE_NEGATIVE_PID") else {
        return;
    };
    let anchor = ProcessLease::capture(pid.parse().unwrap()).unwrap();
    let expectation = std::env::var("PBPS_NAMESPACE_NEGATIVE").unwrap();
    let bounded = super::super::cgroup_relative(&anchor).unwrap();
    let mut observed_violation = false;
    let result = for_each_namespace_task(&anchor, |task, process| match expectation.as_str() {
        "credentials" | "thread-credentials" => {
            let status = process.read_proc("status", 65536)?;
            let root = status.lines().any(|line| {
                line.strip_prefix("Uid:")
                    .is_some_and(|v| v.split_whitespace().all(|v| v == "0"))
            });
            if root && (expectation == "credentials" || task.id() != task.group()) {
                observed_violation = true;
            }
            super::super::security(&process, 999, 0)
        }
        "cgroup" => {
            let current = super::super::cgroup_relative(&process)?;
            if current == bounded || current.starts_with(&format!("{bounded}/")) {
                Ok(())
            } else {
                observed_violation = true;
                Err(UnqualifiedProcess)
            }
        }
        _ => panic!("unknown fixture expectation"),
    });
    assert!(
        observed_violation,
        "the intended task must reach its policy check"
    );
    assert!(
        result.is_err(),
        "a live out-of-profile task must refuse the container"
    );
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
    assert!(anchor.same_process(&new_anchor).is_err());
    replacement.kill().unwrap();
    replacement.wait().unwrap();
}
