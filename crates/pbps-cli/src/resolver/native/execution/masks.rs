//! Effective optional masks: a missing mount row is never evidence of absence.

use super::{Mount, ProcessLease, UnqualifiedProcess, mount_id, proc_base};
use crate::resolver::native::open_within;
use rustix::fs::OFlags;
use std::collections::BTreeMap;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

/// One list drives the workload/forwarder recipe and their live verification.
pub(crate) const MASKED_PROC_PATHS: &[&str] = &[
    "/proc/asound",
    "/proc/acpi",
    "/proc/interrupts",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/proc/sched_debug",
    "/proc/scsi",
];

pub(super) fn check(
    process: &ProcessLease,
    mounts: &BTreeMap<&str, Mount<'_>>,
) -> Result<(), UnqualifiedProcess> {
    let root = process.root()?;
    let flags = OFlags::PATH | OFlags::NOFOLLOW;
    // Pin and identify the parent before interpreting a leaf's ENOENT. A
    // missing/inaccessible parent or a dangling symlink is not an absent
    // kernel interface. The enclosing lease brackets these reads with process
    // continuity checks; this is not a promise of continuous mount observation.
    let proc =
        open_within(&root, "proc", flags | OFlags::DIRECTORY).map_err(|_| UnqualifiedProcess)?;
    if mount_id(&proc)? != mounts.get("/proc").ok_or(UnqualifiedProcess)?.id {
        return Err(UnqualifiedProcess);
    }
    let null = open_within(&root, "dev/null", flags).map_err(|_| UnqualifiedProcess)?;
    let null = null.metadata().map_err(|_| UnqualifiedProcess)?;
    if !null.file_type().is_char_device() || null.rdev() != 0x103 {
        return Err(UnqualifiedProcess);
    }
    for path in MASKED_PROC_PATHS {
        let leaf = path.strip_prefix("/proc/").ok_or(UnqualifiedProcess)?;
        let file = match open_within(&proc, leaf, flags) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::NOENT.raw_os_error()) => {
                if mounts.contains_key(path) {
                    return Err(UnqualifiedProcess);
                }
                continue;
            }
            Err(_) => return Err(UnqualifiedProcess),
        };
        let mount = mounts.get(path).ok_or(UnqualifiedProcess)?;
        let metadata = file.metadata().map_err(|_| UnqualifiedProcess)?;
        if mount.kind != "tmpfs" || mount_id(&file)? != mount.id {
            return Err(UnqualifiedProcess);
        }
        if metadata.is_dir() {
            if mount.root != "/" || !mount.options.contains("ro") {
                return Err(UnqualifiedProcess);
            }
            let mut entries =
                std::fs::read_dir(proc_base(&file)).map_err(|_| UnqualifiedProcess)?;
            // An iterator error is unknown, not an empty directory.
            if entries.next().is_some() {
                return Err(UnqualifiedProcess);
            }
        } else if !metadata.file_type().is_char_device()
            || metadata.rdev() != 0x103
            || (metadata.dev(), metadata.ino()) != (null.dev(), null.ino())
            || mount.root != "/null"
        {
            // Docker binds this run's own /dev/null. A different tmpfs node
            // (even one at a correctly named mountpoint) cannot certify it.
            return Err(UnqualifiedProcess);
        }
    }
    Ok(())
}
