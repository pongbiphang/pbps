//! Filesystem identity comes from a held kernel object, not a mount type.

use super::{FileIdentity, ProcessLease, UnqualifiedProcess};
use rustix::fd::AsFd as _;
use rustix::mount::{FsMountFlags, FsOpenFlags, MountAttrFlags, fsconfig_create, fsmount, fsopen};
use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};
use std::fs::File;

/// Pins the one mqueue filesystem belonging to the process's held IPC namespace.
/// This descriptor is never attached to a path or used to read queue contents.
pub(crate) struct MqueueLease(File);

impl MqueueLease {
    pub(crate) fn capture(process: &ProcessLease) -> Result<Self, UnqualifiedProcess> {
        process.check()?;
        let namespace = process
            .namespaces
            .iter()
            .find(|(name, _, _)| *name == "ipc")
            .map(|(_, file, _)| file)
            .ok_or(UnqualifiedProcess)?;
        let lease = Self(read_namespace(namespace)?);
        lease.check(process)?;
        Ok(lease)
    }

    pub(crate) fn check(&self, process: &ProcessLease) -> Result<(), UnqualifiedProcess> {
        process.check()?;
        same_root(&self.0, &process.open_in_root("dev/mqueue")?)?;
        process.check()
    }
}

fn read_namespace(namespace: &File) -> Result<File, UnqualifiedProcess> {
    // DEC-643.1: unlike devpts, mqueue has one superblock per IPC namespace.
    // A fresh thread enters only that held namespace. The observer's worker
    // never moves, and the detached read-only view neither attaches a mount
    // nor writes target state. Closing the descriptor releases the view.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("pbps-mqueue".into())
            .spawn_scoped(scope, || {
                move_into_link_name_space(
                    namespace.as_fd(),
                    Some(LinkNameSpaceType::InterProcessCommunication),
                )
                .map_err(|_| UnqualifiedProcess)?;
                let context = fsopen("mqueue", FsOpenFlags::FSOPEN_CLOEXEC)
                    .map_err(|_| UnqualifiedProcess)?;
                fsconfig_create(&context).map_err(|_| UnqualifiedProcess)?;
                fsmount(
                    &context,
                    FsMountFlags::FSMOUNT_CLOEXEC,
                    MountAttrFlags::MOUNT_ATTR_RDONLY,
                )
                .map(File::from)
                .map_err(|_| UnqualifiedProcess)
            })
            .map_err(|_| UnqualifiedProcess)?
            .join()
            .map_err(|_| UnqualifiedProcess)?
    })
}

/// Cgroup v2 instances share a device, so the root inode is essential too.
/// The caller retains the expected directory and brackets the visible read
/// with its process/cgroup continuity checks.
pub(super) fn same_root(expected: &File, visible: &File) -> Result<(), UnqualifiedProcess> {
    if FileIdentity::of(expected)? != FileIdentity::of(visible)? {
        return Err(UnqualifiedProcess);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_ipc_evidence_does_not_move_the_observer() {
        let before = std::fs::read_link("/proc/thread-self/ns/ipc").unwrap();
        assert!(read_namespace(&File::open("/dev/null").unwrap()).is_err());
        assert_eq!(
            std::fs::read_link("/proc/thread-self/ns/ipc").unwrap(),
            before
        );
    }

    #[test]
    fn equal_filesystem_devices_do_not_identify_the_same_directory_root() {
        let expected = File::open("/proc/self").unwrap();
        let reopened = File::open("/proc/self").unwrap();
        let other = File::open("/proc/self/task").unwrap();
        assert_eq!(
            FileIdentity::of(&expected).unwrap().device,
            FileIdentity::of(&other).unwrap().device
        );
        assert!(same_root(&expected, &reopened).is_ok());
        assert!(same_root(&expected, &other).is_err());
    }

    #[test]
    #[ignore = "requires scripts/live-resolver-pseudo.py in an owned user/mount/IPC namespace"]
    fn a_foreign_mqueue_mount_invalidates_admission_and_the_retained_lease() {
        use rustix::mount::{UnmountFlags, mount_bind, unmount};
        let foreign = std::env::var("PBPS_PSEUDO_FOREIGN_MQUEUE").unwrap();
        // The runner maps only one UID in its new user namespace; its mount
        // operations cannot mutate an initial-user-namespace mount.
        let map = std::fs::read_to_string("/proc/self/uid_map").unwrap();
        let fields: Vec<_> = map.split_whitespace().collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0], "0");
        assert_eq!(fields[2], "1");
        let pid = std::env::var("PBPS_PSEUDO_TARGET_PID")
            .map(|value| value.parse().unwrap())
            .unwrap_or_else(|_| std::process::id());
        let process = ProcessLease::capture(pid).unwrap();
        let observer = std::fs::read_link("/proc/thread-self/ns/ipc").unwrap();
        let original = File::open("/dev/mqueue").unwrap();
        let foreign_file = File::open(&foreign).unwrap();
        assert_ne!(
            FileIdentity::of(&original).unwrap(),
            FileIdentity::of(&foreign_file).unwrap()
        );
        let lease = MqueueLease::capture(&process).unwrap();
        lease.check(&process).unwrap();
        mount_bind(&foreign, "/dev/mqueue").unwrap();
        let retained_refused = lease.check(&process).is_err();
        let admission_refused = MqueueLease::capture(&process).is_err();
        unmount("/dev/mqueue", UnmountFlags::empty()).unwrap();
        lease.check(&process).unwrap();
        assert_eq!(
            std::fs::read_link("/proc/thread-self/ns/ipc").unwrap(),
            observer
        );
        assert!(
            retained_refused,
            "a foreign mount must invalidate the retained lease"
        );
        assert!(
            admission_refused,
            "filesystem type cannot admit a foreign IPC filesystem"
        );
    }
}
