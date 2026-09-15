//! Effective mount and cgroup-v2 limits, read from the selected live process.
//! Configuration replies do not substitute for these kernel observations.

use super::{FileIdentity, ProcessLease, UnqualifiedProcess, proc_base, read_bounded};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

pub(crate) struct ExecutionProfile {
    pub memory: u64,
    pub nano_cpus: u64,
    pub pids: u64,
    pub storage_path: &'static str,
    pub storage_bytes: u64,
}

pub(crate) struct ExecutionLease {
    process: ProcessLease,
    cgroup: File,
    cgroup_path: PathBuf,
    profile: ExecutionProfile,
}

impl ExecutionLease {
    pub(crate) fn capture(pid: u32, profile: ExecutionProfile) -> Result<Self, UnqualifiedProcess> {
        let process = ProcessLease::capture(pid)?;
        let cgroup_path = cgroup_path(&process)?;
        let cgroup = File::open(&cgroup_path).map_err(|_| UnqualifiedProcess)?;
        let metadata = cgroup.metadata().map_err(|_| UnqualifiedProcess)?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(UnqualifiedProcess);
        }
        let lease = Self {
            process,
            cgroup,
            cgroup_path,
            profile,
        };
        lease.check()?;
        Ok(lease)
    }

    pub(crate) fn check(&self) -> Result<(), UnqualifiedProcess> {
        self.process.check()?;
        if cgroup_path(&self.process)? != self.cgroup_path
            || FileIdentity::of(&File::open(&self.cgroup_path).map_err(|_| UnqualifiedProcess)?)?
                != FileIdentity::of(&self.cgroup)?
        {
            return Err(UnqualifiedProcess);
        }
        let base = proc_base(&self.cgroup);
        for (name, expected) in [
            ("memory.max", self.profile.memory),
            ("memory.swap.max", 0),
            ("pids.max", self.profile.pids),
        ] {
            let actual: u64 = read_bounded(&base.join(name), 128)?
                .trim()
                .parse()
                .map_err(|_| UnqualifiedProcess)?;
            if actual != expected {
                return Err(UnqualifiedProcess);
            }
        }
        let cpu = read_bounded(&base.join("cpu.max"), 128)?;
        let fields: Vec<_> = cpu.split_whitespace().collect();
        if fields.len() != 2 {
            return Err(UnqualifiedProcess);
        }
        let quota: u64 = fields[0].parse().map_err(|_| UnqualifiedProcess)?;
        let period: u64 = fields[1].parse().map_err(|_| UnqualifiedProcess)?;
        if quota == 0
            || period == 0
            || u128::from(quota) * 1_000_000_000
                != u128::from(period) * u128::from(self.profile.nano_cpus)
        {
            return Err(UnqualifiedProcess);
        }
        let mounts = read_bounded(
            &proc_base(&self.process.directory).join("mountinfo"),
            1024 * 1024,
        )?;
        check_mounts(&mounts, &self.profile)?;
        // The later empty read-only /sys overmount must really hide the
        // underlying host sysfs/cgroup mounts still listed in mountinfo.
        let root = proc_base(&self.process.directory).join("root");
        for (path, mount) in parse_mounts(&mounts)? {
            if path.starts_with("/sys/") {
                continue;
            }
            // O_PATH opens the actual mountpoint without requiring read
            // permission on write-only proc controls. fdinfo identifies the
            // visible mount, so a hidden older safe mount cannot certify a
            // later overmount at the same pathname.
            let file = File::options()
                .read(true)
                .custom_flags(0x200000)
                .open(root.join(path.strip_prefix('/').ok_or(UnqualifiedProcess)?))
                .map_err(|_| UnqualifiedProcess)?;
            let info = read_bounded(
                &PathBuf::from(format!("/proc/self/fdinfo/{}", file.as_raw_fd())),
                4096,
            )?;
            let actual: u64 = info
                .lines()
                .find_map(|line| line.strip_prefix("mnt_id:"))
                .ok_or(UnqualifiedProcess)?
                .trim()
                .parse()
                .map_err(|_| UnqualifiedProcess)?;
            if actual != mount.id {
                return Err(UnqualifiedProcess);
            }
        }
        let mut sys = std::fs::read_dir(root.join("sys")).map_err(|_| UnqualifiedProcess)?;
        if sys.next().is_some() {
            return Err(UnqualifiedProcess);
        }
        for path in [
            "proc/kcore",
            "proc/keys",
            "proc/latency_stats",
            "proc/timer_list",
        ] {
            let metadata = std::fs::metadata(root.join(path)).map_err(|_| UnqualifiedProcess)?;
            if !metadata.file_type().is_char_device() || metadata.rdev() != 0x103 {
                // Linux device 1:3 is /dev/null. A reported masked path must
                // not still expose the original readable kernel object.
                return Err(UnqualifiedProcess);
            }
        }
        self.process.check()
    }
}

fn cgroup_path(process: &ProcessLease) -> Result<PathBuf, UnqualifiedProcess> {
    let mounts = read_bounded(Path::new("/proc/self/mountinfo"), 1024 * 1024)?;
    let mounts = parse_mounts(&mounts)?;
    let root = mounts.get("/sys/fs/cgroup").ok_or(UnqualifiedProcess)?;
    if root.kind != "cgroup2" || root.root != "/" {
        return Err(UnqualifiedProcess);
    }
    let membership = read_bounded(&proc_base(&process.directory).join("cgroup"), 4096)?;
    let mut lines = membership.lines();
    let relative = lines
        .next()
        .and_then(|line| line.strip_prefix("0::/"))
        .ok_or(UnqualifiedProcess)?;
    if lines.next().is_some()
        || relative.is_empty()
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(UnqualifiedProcess);
    }
    Ok(Path::new("/sys/fs/cgroup").join(relative))
}

struct Mount<'a> {
    id: u64,
    root: &'a str,
    options: BTreeSet<&'a str>,
    kind: &'a str,
    super_options: BTreeSet<&'a str>,
}

fn parse_mounts(text: &str) -> Result<BTreeMap<&str, Mount<'_>>, UnqualifiedProcess> {
    let mut mounts = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let split = fields
            .iter()
            .position(|field| *field == "-")
            .ok_or(UnqualifiedProcess)?;
        if split < 6 || fields.len() != split + 4 {
            return Err(UnqualifiedProcess);
        }
        // Supported mountpoints contain no escaped whitespace. A later
        // overmount is the visible one; required masks are also read back.
        mounts.insert(
            fields[4],
            Mount {
                id: fields[0].parse().map_err(|_| UnqualifiedProcess)?,
                root: fields[3],
                options: fields[5].split(',').collect(),
                kind: fields[split + 1],
                super_options: fields[split + 3].split(',').collect(),
            },
        );
    }
    if mounts.is_empty() {
        return Err(UnqualifiedProcess);
    }
    Ok(mounts)
}

fn check_mounts(text: &str, profile: &ExecutionProfile) -> Result<(), UnqualifiedProcess> {
    let mounts = parse_mounts(text)?;
    let readonly_proc = [
        "/proc/bus",
        "/proc/fs",
        "/proc/irq",
        "/proc/sys",
        "/proc/sysrq-trigger",
    ];
    let masks = [
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
    for (path, mount) in &mounts {
        let flags = &mount.options;
        let permitted = match *path {
            "/" => flags.contains("ro"),
            "/sys" => mount.kind == "tmpfs" && flags.contains("ro"),
            "/proc" => {
                mount.kind == "proc"
                    && ["nosuid", "nodev", "noexec"]
                        .iter()
                        .all(|flag| flags.contains(flag))
            }
            "/sys/fs/cgroup" => mount.kind == "cgroup2" && flags.contains("ro"),
            "/dev" => mount.kind == "tmpfs" && flags.contains("nosuid") && size(mount)? == 67108864,
            "/dev/pts" => {
                mount.kind == "devpts" && flags.contains("nosuid") && flags.contains("noexec")
            }
            "/dev/mqueue" => {
                mount.kind == "mqueue"
                    && ["nosuid", "nodev", "noexec"]
                        .iter()
                        .all(|flag| flags.contains(flag))
            }
            "/etc/hostname" | "/etc/hosts" | "/etc/resolv.conf" => flags.contains("ro"),
            path if readonly_proc.contains(&path) => mount.kind == "proc" && flags.contains("ro"),
            path if masks.contains(&path) => mount.kind == "tmpfs",
            "/tmp" | "/dev/shm" => private_tmpfs(mount, 67108864)?,
            path if path == profile.storage_path => private_tmpfs(mount, profile.storage_bytes)?,
            _ => false,
        };
        if !permitted {
            return Err(UnqualifiedProcess);
        }
    }
    for required in [
        "/",
        "/proc",
        "/sys",
        "/dev",
        "/dev/shm",
        "/tmp",
        profile.storage_path,
    ]
    .into_iter()
    .chain(readonly_proc)
    {
        if !mounts.contains_key(required) {
            return Err(UnqualifiedProcess);
        }
    }
    Ok(())
}

fn private_tmpfs(mount: &Mount<'_>, expected: u64) -> Result<bool, UnqualifiedProcess> {
    Ok(mount.kind == "tmpfs"
        && ["rw", "nosuid", "nodev", "noexec"]
            .iter()
            .all(|flag| mount.options.contains(flag))
        && size(mount)? == expected)
}

fn size(mount: &Mount<'_>) -> Result<u64, UnqualifiedProcess> {
    let value = mount
        .super_options
        .iter()
        .find_map(|value| value.strip_prefix("size="))
        .ok_or(UnqualifiedProcess)?;
    let (number, factor) = if let Some(value) = value.strip_suffix('k') {
        (value, 1024)
    } else {
        (value, 1)
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|size| size.checked_mul(factor))
        .ok_or(UnqualifiedProcess)
}

#[cfg(test)]
mod tests;
