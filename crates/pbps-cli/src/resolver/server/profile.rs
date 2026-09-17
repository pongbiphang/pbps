//! Named, versioned dedicated-server profiles, one per engine.
//!
//! A supplied server is not qualified by its URL, its separate database or an
//! operator's assertion. The operator names the externally enforced runtime
//! their container already provides; admission reads the daemon's record of
//! it and then measures the kernel. An unnamed or unimplemented layout is
//! refused by name, never assumed to work.
//!
//! The profile is a **known layout**, not an arbitrary one. Everything in the
//! container's mount table has to be a row this file names — the runtime's
//! own pseudo-filesystems and masks, the read-only image root, the tmpfs
//! storage — and a row it does not name is refused as that row. That is the
//! whole difference from proving containment of whatever an operator built:
//! equality with a layout is decidable, and the layouts of Docker and Podman
//! were measured rather than reasoned about (DECISIONS 514).

use crate::resolver::native::{MountEntry, ResourceCeilings};
use pbps_db::Driver;
use serde_json::Value;
use std::collections::BTreeSet;
use std::time::Duration;

pub(crate) struct ServerProfile {
    pub name: &'static str,
    pub driver: Driver,
    /// The engine service's main executable, as installed by root.
    pub executable: &'static str,
    pub uid: u32,
    /// Measured on Docker 28 and Podman 4.9: `--user 10001` resolves to the
    /// image's matching group, and the PostgreSQL recipe's `setpriv` clears
    /// the supplementary set outright. A process that kept a group such as
    /// `docker` or `disk` would reach what that group owns.
    pub gid: u32,
    pub capabilities: u64,
    /// The loopback port the engine listens on inside its private namespace.
    pub port: u16,
    /// The one writable place the engine keeps its files: a tmpfs the
    /// container runtime created for it, so no host path is in the container.
    pub storage_path: &'static str,
    pub resources: ResourceCeilings,
    /// The bound the whole run may occupy on the supplied server. Past it
    /// the next check refuses and the caller's exit path removes the
    /// resources. It is not a watchdog: a caller that never returns at all
    /// leaves them until a human removes them (#641).
    pub lifetime: Duration,
    /// The administrative database the supplied credentials connect to.
    pub maintenance_database: &'static str,
}

/// Every supported profile. The list is deliberately short: each entry names
/// controls that were measured against a real engine on a real kernel.
static PROFILES: &[ServerProfile] = &[
    ServerProfile {
        name: "linux-dedicated-v1",
        driver: Driver::Postgres,
        executable: "postgres",
        uid: 999,
        gid: 999,
        capabilities: 0,
        port: 5432,
        storage_path: "/var/lib/postgresql",
        resources: ResourceCeilings {
            memory: 8 * 1024 * 1024 * 1024,
            nano_cpus: 4_000_000_000,
            pids: 2048,
        },
        lifetime: Duration::from_secs(3600),
        maintenance_database: "postgres",
    },
    ServerProfile {
        name: "linux-dedicated-v1",
        driver: Driver::Mssql,
        executable: "sqlservr",
        uid: 10001,
        // Run as `10001:10001` with supplementary group `10001`: the mssql
        // user's own group in the Microsoft image, which `--user 10001` (the
        // natural unprivileged way to run it) resolves to. Measured on the
        // pinned image; the Docker profile's `--regid=0` is a different
        // deployment and not what a supplied container gets.
        gid: 10001,
        // NET_BIND_SERVICE: the Linux image keeps it for the engine's listener.
        capabilities: 0x400,
        port: 1433,
        storage_path: "/var/opt/mssql",
        resources: ResourceCeilings {
            memory: 8 * 1024 * 1024 * 1024,
            nano_cpus: 4_000_000_000,
            pids: 2048,
        },
        lifetime: Duration::from_secs(3600),
        maintenance_database: "master",
    },
];

/// Looks up the named profile for this engine. A name implemented for the
/// other engine is still unsupported here: the controls are engine-specific.
pub(crate) fn supported(name: &str, driver: Driver) -> Option<&'static ServerProfile> {
    PROFILES
        .iter()
        .find(|profile| profile.name == name && profile.driver == driver)
}

/// The implemented names, for a refusal that says what does exist.
pub(crate) fn implemented(driver: Driver) -> Vec<&'static str> {
    let mut names: Vec<_> = PROFILES
        .iter()
        .filter(|profile| profile.driver == driver)
        .map(|profile| profile.name)
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// The daemon's record of the container, judged before the kernel is asked.
///
/// This is an early, operator-readable refusal and not the measurement: a
/// record can say `NetworkMode: none` about a process that has since joined
/// another namespace, which is why every one of these has a kernel reading
/// behind it. What it adds is a name — the key the operator's `docker run`
/// got wrong — where the kernel reading would say only which premise failed.
pub(crate) fn configuration(observed: &Value) -> Result<(), &'static str> {
    let host = &observed["HostConfig"];
    if host["Privileged"] != false {
        return Err("Privileged");
    }
    if host["PublishAllPorts"] != false {
        return Err("PublishAllPorts");
    }
    if host["NetworkMode"] != "none" {
        return Err("NetworkMode");
    }
    if host["ReadonlyRootfs"] != true {
        return Err("ReadonlyRootfs");
    }
    // Docker reports `""` for a private namespace and Podman `"private"`.
    // `host` and `container:<id>` are somebody else's namespace, and so is
    // Podman's default `shareable` IPC namespace: it is this container's
    // until another container joins it, which the recipe's `--ipc private`
    // is what forbids (finding on #640).
    for key in ["PidMode", "UTSMode", "UsernsMode", "CgroupnsMode"] {
        match &host[key] {
            Value::Null => (),
            Value::String(mode) if matches!(mode.as_str(), "" | "private") => (),
            Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)
            | Value::Object(_) => return Err(key),
        }
    }
    // IpcMode is required to be an explicit private value, not absent: a
    // missing field is no evidence the namespace is private, and the default
    // on some runtimes is `shareable`, which another container can join.
    // Refused here rather than trusted, though the kernel IPC accounting is
    // the enforcement (finding on #640).
    match &host["IpcMode"] {
        Value::String(mode) if matches!(mode.as_str(), "" | "private") => (),
        Value::Null
        | Value::Bool(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Array(_)
        | Value::Object(_) => return Err("IpcMode"),
    }
    for key in [
        "Binds",
        "Devices",
        "DeviceRequests",
        "DeviceCgroupRules",
        "PortBindings",
        "VolumesFrom",
        "Links",
        "ExtraHosts",
    ] {
        match &host[key] {
            Value::Null => (),
            Value::Array(values) if values.is_empty() => (),
            Value::Object(values) if values.is_empty() => (),
            Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)
            | Value::Object(_) => return Err(key),
        }
    }
    match &host["RestartPolicy"]["Name"] {
        Value::Null => (),
        Value::String(name) if matches!(name.as_str(), "" | "no") => (),
        Value::Bool(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Array(_)
        | Value::Object(_) => return Err("RestartPolicy"),
    }
    // Docker lists its tmpfs mounts here; Podman lists none. Anything with a
    // source on the host — a volume, a bind — is not a tmpfs.
    if let Some(mounts) = observed["Mounts"].as_array()
        && mounts.iter().any(|mount| mount["Type"] != "tmpfs")
    {
        return Err("Mounts");
    }
    for key in ["Memory", "PidsLimit"] {
        if host[key].as_u64().is_none_or(|value| value == 0) {
            return Err(key);
        }
    }
    let state = &observed["State"];
    if state["Running"] != true || state["Restarting"] != false || state["Paused"] != false {
        return Err("State");
    }
    Ok(())
}

/// A `/proc` file a runtime remounts read-only.
const READONLY_PROC: &[&str] = &[
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// A `/proc` path a runtime masks. A directory is masked with an empty
/// read-only tmpfs by both runtimes; a file is masked by binding a `/null`
/// node over it — Docker's from the container's own `/dev` tmpfs, Podman's
/// from the host's devtmpfs. All three measured.
const MASKED_PROC: &[&str] = &[
    "/proc/acpi",
    "/proc/asound",
    "/proc/interrupts",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/proc/timer_list",
    "/proc/timer_stats",
];

/// A `/sys` path a runtime masks with an empty read-only tmpfs.
const MASKED_SYS: &[&str] = &[
    "/sys/dev/block",
    "/sys/devices/virtual/powerpc",
    "/sys/firmware",
    "/sys/fs/selinux",
];

/// The character devices Podman binds one by one from the host's devtmpfs.
/// Docker creates them as nodes in its `/dev` tmpfs instead, so it has no
/// such rows. A block device bound at one of these names is not this.
const DEVICE_NODES: &[&str] = &[
    "/dev/full",
    "/dev/null",
    "/dev/random",
    "/dev/tty",
    "/dev/urandom",
    "/dev/zero",
];

/// The files a runtime writes for the container and binds in read-only.
const RUNTIME_FILES: &[&str] = &[
    "/etc/hostname",
    "/etc/hosts",
    "/etc/resolv.conf",
    "/run/.containerenv",
];

/// The private tmpfs mounts a runtime lays out or the recipe asks for.
const PRIVATE_TMPFS: &[&str] = &["/dev/shm", "/run", "/tmp", "/var/tmp"];

/// The kinds a kernel keeps one instance of per container, or none at all.
/// A file the runtime bound in can be on any ordinary filesystem, but never
/// on one of these.
const PSEUDO: &[&str] = &["cgroup2", "devpts", "devtmpfs", "mqueue", "proc", "sysfs"];

/// Every row of the container's mount table must be one this profile names.
///
/// The table is taken uncollapsed: two rows at one target are two mounts
/// stacked, which no runtime lays out and which is how a hidden row would
/// otherwise certify the visible one (finding on #640). A refusal names the
/// row, since the containers are gone by the time anyone reads the fixture.
pub(crate) fn contained(rows: &[MountEntry], profile: &ServerProfile) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for entry in rows {
        if !seen.insert(entry.target.as_str()) {
            return Err(format!("{} is mounted more than once", entry.target));
        }
    }
    let device = |target: &str| {
        rows.iter()
            .find(|entry| entry.target == target)
            .map(|entry| entry.device.as_str())
    };
    let proc_device = device("/proc").ok_or_else(|| "/proc is not mounted".to_owned())?;
    let dev_device = device("/dev").ok_or_else(|| "/dev is not mounted".to_owned())?;
    for entry in rows {
        let has = |flag: &str| entry.options.contains(flag);
        let all = |flags: &[&str]| flags.iter().all(|flag| has(flag));
        let tmpfs = entry.kind == "tmpfs" && entry.root == "/";
        let ordinary = !PSEUDO.contains(&entry.kind.as_str());
        let target = entry.target.as_str();
        let permitted = match target {
            // The image root. Its filesystem cannot be constrained — the
            // engine's executable lives on it, and what qualifies that is the
            // root-owned executable the process lease requires — but it is
            // read-only, which is the recipe's `--read-only`.
            "/" => has("ro"),
            "/proc" => {
                entry.kind == "proc" && entry.root == "/" && all(&["nosuid", "nodev", "noexec"])
            }
            // A real read-only sysfs: the anchor reads the loopback device
            // through it, which a mask could never show.
            "/sys" => entry.kind == "sysfs" && entry.root == "/" && has("ro"),
            "/sys/fs/cgroup" => entry.kind == "cgroup2" && has("ro"),
            "/dev" => tmpfs && has("nosuid"),
            "/dev/pts" => entry.kind == "devpts" && all(&["nosuid", "noexec"]),
            "/dev/mqueue" => entry.kind == "mqueue" && all(&["nosuid", "nodev", "noexec"]),
            path if path == profile.storage_path => {
                tmpfs && all(&["rw", "nosuid", "nodev", "noexec"])
            }
            path if PRIVATE_TMPFS.contains(&path) => tmpfs && all(&["nosuid", "nodev", "noexec"]),
            path if RUNTIME_FILES.contains(&path) => {
                // The file the runtime wrote, wherever it keeps it: the row's
                // root ends in the same name. Its contents are #630's scope.
                has("ro") && ordinary && entry.root.rsplit('/').next() == path.rsplit('/').next()
            }
            path if READONLY_PROC.contains(&path) => {
                entry.kind == "proc"
                    && entry.device == proc_device
                    && Some(entry.root.as_str()) == path.strip_prefix("/proc")
                    && has("ro")
            }
            path if MASKED_PROC.contains(&path) => {
                // The empty-tmpfs mask must be read-only — both runtimes lay it
                // out `ro`, and a writable one would be executable private
                // storage. The `/null` mask is a device (Docker binds the
                // container's own `/dev/null`, Podman the host devtmpfs), not
                // storage, so it needs no `ro`.
                (tmpfs && has("ro"))
                    || (entry.root == "/null"
                        && (entry.kind == "devtmpfs"
                            || (entry.kind == "tmpfs" && entry.device == dev_device)))
            }
            path if MASKED_SYS.contains(&path) => tmpfs && has("ro"),
            path if DEVICE_NODES.contains(&path) => {
                entry.kind == "devtmpfs" && Some(entry.root.as_str()) == path.strip_prefix("/dev")
            }
            _ => false,
        };
        if !permitted {
            return Err(format!(
                "{} ({} from {} at {})",
                entry.target, entry.kind, entry.device, entry.root
            ));
        }
    }
    for required in ["/", "/proc", "/sys", "/dev", profile.storage_path] {
        if !seen.contains(required) {
            return Err(format!("{required} is not mounted"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn postgres() -> &'static ServerProfile {
        supported("linux-dedicated-v1", Driver::Postgres).unwrap()
    }

    #[test]
    fn an_unimplemented_layout_is_named_rather_than_assumed_to_work() {
        assert!(supported("linux-dedicated-v1", Driver::Postgres).is_some());
        assert!(supported("linux-dedicated-v1", Driver::Mssql).is_some());
        assert!(supported("linux-dedicated-v2", Driver::Postgres).is_none());
        assert!(supported("", Driver::Mssql).is_none());
        assert_eq!(implemented(Driver::Postgres), ["linux-dedicated-v1"]);
    }

    #[test]
    fn engine_profiles_keep_their_own_executable_and_port() {
        let postgres = postgres();
        let mssql = supported("linux-dedicated-v1", Driver::Mssql).unwrap();
        assert_eq!((postgres.executable, postgres.port), ("postgres", 5432));
        assert_eq!((mssql.executable, mssql.port), ("sqlservr", 1433));
        assert_ne!(postgres.storage_path, mssql.storage_path);
    }

    /// The record Podman 4.9.3 writes for the recipe, verbatim in the fields
    /// this profile reads.
    fn podman_record() -> Value {
        serde_json::json!({
            "State": {"Running": true, "Restarting": false, "Paused": false, "Pid": 3836286},
            "HostConfig": {
                "Privileged": false, "PublishAllPorts": false, "NetworkMode": "none",
                "ReadonlyRootfs": true, "PidMode": "private", "IpcMode": "private",
                "UTSMode": "private", "UsernsMode": "", "CgroupnsMode": null,
                "Binds": [], "Devices": [], "PortBindings": {},
                "RestartPolicy": {"Name": "", "MaximumRetryCount": 0},
                "Memory": 536870912, "MemorySwap": 536870912, "NanoCpus": 1000000000,
                "PidsLimit": 64,
                "Tmpfs": {"/tmp": "rw,nosuid,nodev,noexec,size=67108864,mode=1777,rprivate,tmpcopyup"}
            },
            "Mounts": []
        })
    }

    #[test]
    fn the_measured_podman_record_qualifies_and_each_weakening_is_named() {
        assert_eq!(configuration(&podman_record()), Ok(()));
        let weakened = |path: &[&str], value: Value| {
            let mut record = podman_record();
            let mut cursor = &mut record;
            for key in path {
                cursor = &mut cursor[*key];
            }
            *cursor = value;
            configuration(&record)
        };
        assert_eq!(
            weakened(&["HostConfig", "Privileged"], Value::Bool(true)),
            Err("Privileged")
        );
        assert_eq!(
            weakened(&["HostConfig", "NetworkMode"], "bridge".into()),
            Err("NetworkMode")
        );
        assert_eq!(
            weakened(&["HostConfig", "NetworkMode"], Value::Null),
            Err("NetworkMode")
        );
        assert_eq!(
            weakened(&["HostConfig", "ReadonlyRootfs"], Value::Bool(false)),
            Err("ReadonlyRootfs")
        );
        assert_eq!(
            weakened(&["HostConfig", "PidMode"], "host".into()),
            Err("PidMode")
        );
        assert_eq!(
            weakened(&["HostConfig", "IpcMode"], "container:abc".into()),
            Err("IpcMode")
        );
        assert_eq!(
            weakened(&["HostConfig", "IpcMode"], "shareable".into()),
            Err("IpcMode")
        );
        // A missing IpcMode is no evidence of a private namespace.
        assert_eq!(
            weakened(&["HostConfig", "IpcMode"], Value::Null),
            Err("IpcMode")
        );
        assert_eq!(
            weakened(&["HostConfig", "Binds"], serde_json::json!(["/data:/x"])),
            Err("Binds")
        );
        assert_eq!(
            weakened(
                &["HostConfig", "PortBindings"],
                serde_json::json!({"5432/tcp": []})
            ),
            Err("PortBindings")
        );
        assert_eq!(
            weakened(&["HostConfig", "Memory"], Value::from(0)),
            Err("Memory")
        );
        assert_eq!(
            weakened(&["HostConfig", "PidsLimit"], Value::Null),
            Err("PidsLimit")
        );
        assert_eq!(
            weakened(&["HostConfig", "RestartPolicy", "Name"], "always".into()),
            Err("RestartPolicy")
        );
        assert_eq!(
            weakened(
                &["Mounts"],
                serde_json::json!([{"Type": "volume", "Destination": "/var/lib/postgresql"}])
            ),
            Err("Mounts")
        );
        assert_eq!(
            weakened(
                &["Mounts"],
                serde_json::json!([{"Type": "tmpfs", "Destination": "/var/lib/postgresql"}])
            ),
            Ok(())
        );
        assert_eq!(
            weakened(&["State", "Running"], Value::Bool(false)),
            Err("State")
        );
    }

    fn entry(line: &str) -> MountEntry {
        let fields: Vec<_> = line.split_whitespace().collect();
        let split = fields.iter().position(|field| *field == "-").unwrap();
        MountEntry {
            target: fields[4].to_owned(),
            kind: fields[split + 1].to_owned(),
            device: fields[2].to_owned(),
            root: fields[3].to_owned(),
            options: fields[5].split(',').map(str::to_owned).collect(),
        }
    }

    /// `/proc/1/mountinfo` of the recipe under Podman 4.9.3, rootless, with
    /// the overlay's `lowerdir` list shortened. Every row is what the runtime
    /// laid out or the recipe asked for.
    const PODMAN: &str = "\
1373 1219 0:158 / / ro,relatime - overlay overlay rw,lowerdir=/l/a:/l/b,upperdir=/u/diff,workdir=/u/work
1374 1373 0:161 / /dev rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755,uid=1000,gid=1000
1375 1373 0:162 / /tmp rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=65536k,uid=1000,gid=1000
1376 1373 0:163 / /sys ro,nosuid,nodev,noexec,relatime - sysfs sysfs rw
1377 1373 0:164 / /run rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,mode=755,uid=1000,gid=1000
1378 1373 0:165 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw
1379 1374 0:166 / /dev/pts rw,nosuid,noexec,relatime - devpts devpts rw,gid=100004,mode=620,ptmxmode=666
1380 1374 0:160 / /dev/mqueue rw,nosuid,nodev,noexec,relatime - mqueue mqueue rw
1381 1373 0:167 / /var/tmp rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,uid=1000,gid=1000
1369 1373 0:89 /containers/overlay-containers/64bc/userdata/hosts /etc/hosts ro,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=1626324k,mode=700,uid=1000,gid=1000
1370 1374 0:157 / /dev/shm rw,nosuid,nodev,noexec,relatime - tmpfs shm rw,size=64000k,uid=1000,gid=1000
1371 1377 0:89 /containers/overlay-containers/64bc/userdata/.containerenv /run/.containerenv ro,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=1626324k,mode=700,uid=1000,gid=1000
1372 1373 0:89 /containers/overlay-containers/64bc/userdata/hostname /etc/hostname ro,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=1626324k,mode=700,uid=1000,gid=1000
1382 1373 0:168 / /var/lib/postgresql rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=262144k,mode=700,uid=1000,gid=1000
1383 1376 0:23 / /sys/fs/cgroup ro,nosuid,nodev,noexec,relatime - cgroup2 cgroup2 rw
1384 1374 0:5 /null /dev/null rw,nosuid,noexec,relatime - devtmpfs none rw,size=8126596k,mode=755
1385 1374 0:5 /zero /dev/zero rw,nosuid,noexec,relatime - devtmpfs none rw,size=8126596k,mode=755
1386 1374 0:5 /full /dev/full rw,nosuid,noexec,relatime - devtmpfs none rw,size=8126596k,mode=755
1387 1374 0:5 /tty /dev/tty rw,nosuid,noexec,relatime - devtmpfs none rw,size=8126596k,mode=755
1388 1374 0:5 /random /dev/random rw,nosuid,noexec,relatime - devtmpfs none rw,size=8126596k,mode=755
1389 1374 0:5 /urandom /dev/urandom rw,nosuid,noexec,relatime - devtmpfs none rw,size=8126596k,mode=755
1390 1378 0:169 / /proc/acpi ro,relatime - tmpfs tmpfs rw,size=0k,uid=1000,gid=1000
1391 1378 0:5 /null /proc/kcore ro,nosuid,relatime - devtmpfs none rw,size=8126596k,mode=755
1392 1378 0:5 /null /proc/keys ro,nosuid,relatime - devtmpfs none rw,size=8126596k,mode=755
1393 1378 0:5 /null /proc/latency_stats ro,nosuid,relatime - devtmpfs none rw,size=8126596k,mode=755
1394 1378 0:5 /null /proc/timer_list ro,nosuid,relatime - devtmpfs none rw,size=8126596k,mode=755
1395 1378 0:170 / /proc/scsi ro,relatime - tmpfs tmpfs rw,size=0k,uid=1000,gid=1000
1396 1376 0:171 / /sys/firmware ro,relatime - tmpfs tmpfs rw,size=0k,uid=1000,gid=1000
1397 1376 0:172 / /sys/fs/selinux ro,relatime - tmpfs tmpfs rw,size=0k,uid=1000,gid=1000
1398 1376 0:173 / /sys/dev/block ro,relatime - tmpfs tmpfs rw,size=0k,uid=1000,gid=1000
1399 1378 0:165 /bus /proc/bus ro,nosuid,nodev,noexec,relatime - proc proc rw
1400 1378 0:165 /fs /proc/fs ro,nosuid,nodev,noexec,relatime - proc proc rw
1401 1378 0:165 /irq /proc/irq ro,nosuid,nodev,noexec,relatime - proc proc rw
1402 1378 0:165 /sys /proc/sys ro,nosuid,nodev,noexec,relatime - proc proc rw
1403 1378 0:165 /sysrq-trigger /proc/sysrq-trigger ro,nosuid,nodev,noexec,relatime - proc proc rw
";

    /// `/proc/1/mountinfo` of the recipe under Docker 28.0.4 on a GitHub
    /// Actions runner, with the overlay's `lowerdir` list shortened. The
    /// file masks are `/null` bound from the container's own `/dev` tmpfs
    /// (`0:67`), the directory masks are 4k read-only tmpfs, and the `/etc`
    /// files come from the daemon's directory on the root disk.
    const DOCKER: &str = "\
483 454 0:51 / / ro,relatime - overlay overlay rw,lowerdir=/l/a,upperdir=/var/lib/docker/overlay2/55e2/diff,workdir=/var/lib/docker/overlay2/55e2/work,nouserxattr
485 483 0:66 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw
486 483 0:67 / /dev rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755,inode64
487 486 0:68 / /dev/pts rw,nosuid,noexec,relatime - devpts devpts rw,gid=5,mode=620,ptmxmode=666
488 483 0:69 / /sys ro,nosuid,nodev,noexec,relatime - sysfs sysfs ro
489 488 0:30 / /sys/fs/cgroup ro,nosuid,nodev,noexec,relatime - cgroup2 cgroup rw
490 486 0:64 / /dev/mqueue rw,nosuid,nodev,noexec,relatime - mqueue mqueue rw
491 486 0:70 / /dev/shm rw,nosuid,nodev,noexec,relatime - tmpfs shm rw,size=65536k,inode64
492 483 0:71 / /tmp rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=65536k,inode64
493 483 0:72 / /run rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=65536k,mode=755,inode64
494 483 0:73 / /var/tmp rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=65536k,inode64
495 483 8:1 /var/lib/docker/containers/6874f76da82245358ac0d96d215fb338e4082ceeac032b370a732a808db4cabc/resolv.conf /etc/resolv.conf ro,relatime - ext4 /dev/root rw,discard,errors=remount-ro,commit=30
496 483 8:1 /var/lib/docker/containers/6874f76da82245358ac0d96d215fb338e4082ceeac032b370a732a808db4cabc/hostname /etc/hostname ro,relatime - ext4 /dev/root rw,discard,errors=remount-ro,commit=30
497 483 8:1 /var/lib/docker/containers/6874f76da82245358ac0d96d215fb338e4082ceeac032b370a732a808db4cabc/hosts /etc/hosts ro,relatime - ext4 /dev/root rw,discard,errors=remount-ro,commit=30
498 483 0:74 / /var/lib/postgresql rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=262144k,mode=700,uid=999,gid=999,inode64
455 485 0:66 /bus /proc/bus ro,nosuid,nodev,noexec,relatime - proc proc rw
456 485 0:66 /fs /proc/fs ro,nosuid,nodev,noexec,relatime - proc proc rw
457 485 0:66 /irq /proc/irq ro,nosuid,nodev,noexec,relatime - proc proc rw
458 485 0:66 /sys /proc/sys ro,nosuid,nodev,noexec,relatime - proc proc rw
459 485 0:66 /sysrq-trigger /proc/sysrq-trigger ro,nosuid,nodev,noexec,relatime - proc proc rw
460 485 0:75 / /proc/acpi ro,relatime - tmpfs tmpfs ro,size=4k,nr_inodes=1,inode64
461 485 0:67 /null /proc/interrupts rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755,inode64
462 485 0:67 /null /proc/kcore rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755,inode64
463 485 0:67 /null /proc/keys rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755,inode64
464 485 0:67 /null /proc/latency_stats rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755,inode64
465 485 0:67 /null /proc/timer_list rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755,inode64
466 485 0:75 / /proc/scsi ro,relatime - tmpfs tmpfs ro,size=4k,nr_inodes=1,inode64
467 488 0:75 / /sys/firmware ro,relatime - tmpfs tmpfs ro,size=4k,nr_inodes=1,inode64
";

    fn rows(text: &str) -> Vec<MountEntry> {
        text.lines().map(entry).collect()
    }

    #[test]
    fn the_measured_layouts_of_both_container_runtimes_qualify() {
        assert_eq!(contained(&rows(PODMAN), postgres()), Ok(()));
        assert_eq!(contained(&rows(DOCKER), postgres()), Ok(()));
    }

    #[test]
    fn a_row_the_profile_does_not_name_is_refused_as_that_row() {
        let mut table = rows(DOCKER);
        table.push(entry(
            "930 484 259:1 /srv/secret /mnt/secret rw,relatime - ext4 /dev/root rw",
        ));
        assert!(
            contained(&table, postgres())
                .unwrap_err()
                .starts_with("/mnt/secret")
        );
        // Under an otherwise-permitted prefix, or at a runtime's own target.
        let mut table = rows(DOCKER);
        table.push(entry(
            "930 487 259:1 /srv/secret /dev/pbps-secret rw,relatime - ext4 /dev/root rw",
        ));
        assert!(
            contained(&table, postgres())
                .unwrap_err()
                .starts_with("/dev/pbps-secret")
        );
        let mut table = rows(DOCKER);
        table.push(entry(
            "930 484 259:1 /srv/creds /run/credentials ro,relatime - ext4 /dev/root rw",
        ));
        assert!(
            contained(&table, postgres())
                .unwrap_err()
                .starts_with("/run/credentials")
        );
        // A file mask has to come from the container's own /dev or the host's
        // devtmpfs; a /null from any other filesystem is a bind of something.
        let mut table = rows(DOCKER);
        table.retain(|entry| entry.target != "/proc/kcore");
        table.push(entry(
            "463 486 259:1 /null /proc/kcore rw,nosuid - ext4 /dev/root rw",
        ));
        assert!(
            contained(&table, postgres())
                .unwrap_err()
                .starts_with("/proc/kcore")
        );
    }

    #[test]
    fn a_stacked_mount_is_refused_whichever_row_is_visible() {
        let mut table = rows(PODMAN);
        table.push(entry("1404 1373 259:1 /var/lib/postgresql/18/main /var/lib/postgresql rw,relatime - ext4 /dev/root rw"));
        assert!(
            contained(&table, postgres())
                .unwrap_err()
                .contains("more than once")
        );
    }

    /// The recipe forces `noexec` on every private writable tmpfs; a layout
    /// that drops it on `/run` — Podman's default — is executable storage.
    #[test]
    fn a_private_tmpfs_without_noexec_is_refused() {
        let exec_run = DOCKER.replace(
            "493 483 0:72 / /run rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=65536k,mode=755,inode64",
            "493 483 0:72 / /run rw,nosuid,nodev,relatime - tmpfs tmpfs rw,size=65536k,mode=755,inode64",
        );
        assert_ne!(
            exec_run, DOCKER,
            "the anchor row must exist in the pinned table"
        );
        assert!(
            contained(&rows(&exec_run), postgres())
                .unwrap_err()
                .starts_with("/run ")
        );
    }

    #[test]
    fn the_storage_must_be_a_fresh_tmpfs_and_the_root_read_only() {
        const STORAGE: &str = "498 483 0:74 / /var/lib/postgresql rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=262144k,mode=700,uid=999,gid=999,inode64";
        let bound = DOCKER.replace(
            STORAGE,
            "498 483 259:1 /var/lib/postgresql/18/main /var/lib/postgresql rw,relatime - ext4 /dev/root rw",
        );
        assert!(
            contained(&rows(&bound), postgres())
                .unwrap_err()
                .starts_with("/var/lib/postgresql")
        );
        let subtree = DOCKER.replace(
            STORAGE,
            "498 483 0:74 /pgdata /var/lib/postgresql rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=262144k",
        );
        assert!(
            contained(&rows(&subtree), postgres())
                .unwrap_err()
                .starts_with("/var/lib/postgresql")
        );
        let writable_root = DOCKER.replace(
            "483 454 0:51 / / ro,relatime",
            "483 454 0:51 / / rw,relatime",
        );
        assert_eq!(
            contained(&rows(&writable_root), postgres()).unwrap_err(),
            "/ (overlay from 0:51 at /)"
        );
        let missing = DOCKER.replace(&format!("{STORAGE}\n"), "");
        assert_eq!(
            contained(&rows(&missing), postgres()).unwrap_err(),
            "/var/lib/postgresql is not mounted"
        );
        // A masked /sys is not a real one: the anchor reads through it.
        let masked_sys = DOCKER.replace(
            "488 483 0:69 / /sys ro,nosuid,nodev,noexec,relatime - sysfs sysfs ro",
            "488 483 0:69 / /sys ro,nosuid,nodev,noexec,relatime - tmpfs tmpfs ro",
        );
        assert!(
            contained(&rows(&masked_sys), postgres())
                .unwrap_err()
                .starts_with("/sys ")
        );
    }

    #[test]
    fn another_instance_of_a_pseudo_filesystem_is_refused() {
        // A host procfs at a read-only proc target carries a different device.
        let foreign = DOCKER.replace(
            "458 485 0:66 /sys /proc/sys ro,nosuid,nodev,noexec,relatime - proc proc rw",
            "458 485 0:5 /sys /proc/sys ro,nosuid,nodev,noexec,relatime - proc proc rw",
        );
        assert!(
            contained(&rows(&foreign), postgres())
                .unwrap_err()
                .starts_with("/proc/sys")
        );
        // A runtime file bound from a pseudo-filesystem is not the runtime's file.
        let hosts = DOCKER.replace(
            "497 483 8:1 /var/lib/docker/containers/6874f76da82245358ac0d96d215fb338e4082ceeac032b370a732a808db4cabc/hosts /etc/hosts ro,relatime - ext4 /dev/root rw,discard,errors=remount-ro,commit=30",
            "497 483 0:66 /net/hosts /etc/hosts ro,relatime - proc proc rw",
        );
        assert!(
            contained(&rows(&hosts), postgres())
                .unwrap_err()
                .starts_with("/etc/hosts")
        );
        // A block device where Podman binds a character device.
        let block = PODMAN.replace(
            "1384 1374 0:5 /null /dev/null rw,nosuid,noexec,relatime - devtmpfs none rw,size=8126596k,mode=755",
            "1384 1374 0:5 /sda1 /dev/null rw,nosuid,noexec,relatime - devtmpfs none rw,size=8126596k,mode=755",
        );
        assert!(
            contained(&rows(&block), postgres())
                .unwrap_err()
                .starts_with("/dev/null")
        );
        // A writable (non-ro) tmpfs at a mask target is executable storage.
        let mut table = rows(DOCKER);
        table.retain(|entry| entry.target != "/proc/acpi");
        table.push(entry(
            "930 901 0:99 / /proc/acpi rw,relatime - tmpfs tmpfs rw,mode=1777",
        ));
        assert!(
            contained(&table, postgres())
                .unwrap_err()
                .starts_with("/proc/acpi")
        );
    }
}
