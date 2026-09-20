//! The kernel's reading of a supplied container.
//!
//! The daemon's record says what the operator asked for; this is what the
//! kernel is actually doing. Every premise the profile names is read from the
//! container's init process and from every task sharing its namespaces, and
//! read again on every check: an admission is a measurement that held once.

use super::profile::{self, ServerProfile};
use super::{Error, Premise};
use crate::resolver::native::{
    BoundedResourceLease, ProcessLease, UnqualifiedProcess, cgroup_relative,
    complete_process_scope, for_each_occupant, foreign_network_tasks, groups, mount_rows,
    private_network, security,
};

/// The processes the daemon's record names, before anything is measured of
/// their runtime.
///
/// Separate from `ServerRuntime` so the order the profile promises is the
/// order the code takes: instance separation is answered on the actual
/// processes first, so a resolver pointed at the target is refused as the
/// target rather than as a runtime that happens to be bounded differently.
pub(crate) struct ServerProcesses {
    init: ProcessLease,
    engine: ProcessLease,
}

impl ServerProcesses {
    /// `init_pid` is the container's init as the daemon reports it. The
    /// engine service is found beneath it rather than named by the operator:
    /// SQL Server's image starts the engine under a launcher, and a backend
    /// runs the same executable as its service root.
    pub(crate) fn identify(init_pid: u32, profile: &'static ServerProfile) -> Result<Self, Error> {
        let init = ProcessLease::capture(init_pid).map_err(|_| {
            Error::Unqualified("the supplied container's init process is unreadable")
        })?;
        if init.namespace_pid() != 1 {
            return Err(Error::Unqualified(
                "the supplied container's init is not PID 1 of its own namespace",
            ));
        }
        let mut engines = Vec::new();
        // Every process in the container, so a walk that could not account
        // for one refuses rather than reporting the engines it did see
        // (#730).
        for (pid, _) in complete_process_scope(&init)
            .map_err(|_| Error::Unqualified("the supplied container's processes are unreadable"))?
        {
            let Ok(process) = ProcessLease::capture(pid) else {
                continue;
            };
            if process
                .executable_path()
                .file_name()
                .is_some_and(|name| name == profile.executable)
                && !process.has_same_executable_parent().map_err(|_| {
                    Error::Unqualified("the supplied container's processes are unreadable")
                })?
            {
                engines.push(process);
            }
        }
        if engines.len() != 1 {
            return Err(Error::EngineExecutable);
        }
        init.check()
            .map_err(|_| Error::Unqualified("the supplied container's init process changed"))?;
        Ok(Self {
            init,
            engine: engines.remove(0),
        })
    }

    /// The resolver must be outside the target's instance. Different database
    /// names, logins and aliases do not change the service process, and a
    /// shared namespace means one runtime however many endpoints it has.
    pub(crate) fn separate_from(&self, target: &ProcessLease) -> Result<(), Error> {
        let unreadable =
            |_| Error::Unqualified("a process this runtime was bound to is unreadable");
        for process in [&self.init, &self.engine] {
            if process.same_process(target).map_err(unreadable)? {
                return Err(Error::TargetInstance);
            }
            for namespace in ["pid", "mnt", "net"] {
                if process
                    .same_namespace(target, namespace)
                    .map_err(unreadable)?
                {
                    return Err(Error::TargetInstance);
                }
            }
        }
        Ok(())
    }
}

pub(crate) struct ServerRuntime {
    /// The container's init, holding its cgroup: every occupant must sit in
    /// that cgroup or below it.
    init: BoundedResourceLease,
    engine: ProcessLease,
    profile: &'static ServerProfile,
}

impl ServerRuntime {
    pub(crate) fn bind(
        processes: ServerProcesses,
        profile: &'static ServerProfile,
    ) -> Result<Self, Error> {
        Ok(Self {
            init: BoundedResourceLease::capture(processes.init, profile.resources)
                .map_err(Premise::Resources.named())?,
            engine: processes.engine,
            profile,
        })
    }

    pub(crate) fn init(&self) -> &ProcessLease {
        self.init.process()
    }

    pub(crate) fn engine(&self) -> &ProcessLease {
        &self.engine
    }

    /// Measures every premise of the named profile.
    ///
    /// `forwarders` are this run's own control containers: they share the
    /// engine's network namespace by design and are the only processes
    /// outside its PID namespace allowed to.
    pub(crate) fn check(&self, forwarders: &[&ProcessLease]) -> Result<(), Error> {
        self.init.check().map_err(Premise::Resources.named())?;
        self.engine.check().map_err(Premise::Lease.named())?;
        let init = self.init.process();
        private_endpoint(init).map_err(Premise::Network.named())?;
        anchors(init).map_err(Premise::Anchors.named())?;
        let rows = mount_rows(init).map_err(Premise::Mounts.named())?;
        profile::contained(&rows, self.profile).map_err(Error::Mount)?;
        device_not_engine_writable(init).map_err(Premise::Device.named())?;
        occupants(init, self.profile)?;
        accounted(init, forwarders)?;
        self.init.check().map_err(Premise::Resources.named())
    }
}

/// Only a real loopback device, by link type and flag rather than by name,
/// with no IPv4 or IPv6 route out and no address but `::1`.
fn private_endpoint(process: &ProcessLease) -> Result<(), UnqualifiedProcess> {
    private_network(process)?;
    let kind = process.read_root_file("sys/class/net/lo/type", 64)?;
    let flags = process.read_root_file("sys/class/net/lo/flags", 64)?;
    let flags = u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16)
        .map_err(|_| UnqualifiedProcess)?;
    // IFF_LOOPBACK, alongside the link type: a renamed veth has neither.
    if kind.trim() != "772" || flags & 0x8 == 0 {
        return Err(UnqualifiedProcess);
    }
    for table in ["net/ipv6_route", "net/if_inet6"] {
        for line in process.read_proc(table, 1024 * 1024)?.lines() {
            if line.split_whitespace().last() != Some("lo") {
                return Err(UnqualifiedProcess);
            }
        }
    }
    process.check()
}

/// A filesystem type proves nothing on its own: the host has a procfs and a
/// sysfs too, and binding one in keeps the expected type. So the container's
/// `/proc` must show its own init as PID 1, and its `/sys` must show only the
/// private loopback device the rest of the profile establishes.
fn anchors(init: &ProcessLease) -> Result<(), UnqualifiedProcess> {
    let pid_namespace = init.open_in_root("proc/1/ns/pid")?;
    if !init.owns_namespace("pid", &pid_namespace)? {
        return Err(UnqualifiedProcess);
    }
    if init.read_root_dir("sys/class/net")? != std::collections::BTreeSet::from(["lo".to_owned()]) {
        return Err(UnqualifiedProcess);
    }
    init.check()
}

/// `/dev` is a tmpfs the runtime manages and cannot portably be mounted
/// `noexec`, unlike the other private tmpfs the mount allowlist requires it
/// of. What keeps a task from placing an executable there instead is that it
/// is root-owned and writable by nobody else, and every task in the container
/// is unprivileged (`occupants`). `open_in_root` follows the container's
/// mount namespace, but the owner is read as this process sees it, in the
/// initial user namespace. That is the right view for the root-run
/// containers this profile admits, where the runtime creates `/dev` as uid 0;
/// a rootless container maps it to the invoking user and is not admittable
/// anyway, since the executable and the daemon must be root-owned (#686).
fn device_not_engine_writable(init: &ProcessLease) -> Result<(), UnqualifiedProcess> {
    use std::os::unix::fs::MetadataExt as _;
    let dev = init.open_in_root("dev")?;
    let metadata = dev.metadata().map_err(|_| UnqualifiedProcess)?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(UnqualifiedProcess);
    }
    init.check()
}

/// Every task in the container's PID namespace — not only what the service
/// started — at the profile's uid and group, with no-new-privileges, a
/// seccomp filter and the capability ceiling, still in the container's
/// network and mount namespaces, and inside its cgroup.
///
/// Judged as found, not against an earlier listing: a backend PostgreSQL
/// forks or a thread SQL Server pools between two readings is a task the
/// engine started, and refusing it for being new would end every run on a
/// busy engine (finding on #640).
fn occupants(init: &ProcessLease, profile: &ServerProfile) -> Result<(), Error> {
    let bounded = cgroup_relative(init).map_err(Premise::Occupants.named())?;
    for_each_occupant(init, "pid", |occupant| {
        // Membership has to hold both ways. A qualified task that left the
        // namespaces — into an externally connected network, say, or an
        // external IPC namespace to receive through foreign shared memory —
        // would bridge the run straight out of its containment while keeping
        // its access to the engine.
        for namespace in ["net", "mnt", "ipc"] {
            if !init.same_namespace(occupant, namespace)? {
                return Err(UnqualifiedProcess);
            }
        }
        // `security` confirms no-new-privileges, that a seccomp filter is
        // loaded, and the capability ceiling. A filter's *presence* is
        // measured; its BPF contents cannot be read from `/proc`, so a
        // permissive operator policy allowing a non-IP channel such as
        // `AF_VSOCK` — which the loopback network checks do not contain — is
        // the operator's provisioning responsibility, tracked in #684.
        security(occupant, profile.uid, profile.capabilities)?;
        let (gids, supplementary) = groups(occupant)?;
        if gids.iter().any(|value| *value != profile.gid)
            || supplementary.iter().any(|value| *value != profile.gid)
        {
            return Err(UnqualifiedProcess);
        }
        let cgroup = cgroup_relative(occupant)?;
        if cgroup != bounded && !cgroup.starts_with(&format!("{bounded}/")) {
            return Err(UnqualifiedProcess);
        }
        Ok(())
    })
    .map_err(Premise::Occupants.named())
}

/// Nothing may share the container's network, mount or IPC namespace that
/// is not in its PID namespace, except this run's own forwarders in the
/// network namespace. A container joined with `--network container:` would otherwise
/// be invisible to every other check.
fn accounted(init: &ProcessLease, forwarders: &[&ProcessLease]) -> Result<(), Error> {
    // The network namespace is the engine's tasks plus this run's forwarders.
    // A forwarder shares only that namespace, so its own tasks are the one
    // exception, matched by PID namespace: a forwarder's `bash` reaps and
    // respawns its `cat` pipes, so a task list captured a moment earlier
    // would race a legitimate child (finding on #640, where matching each
    // task against a captured process tree refused the forwarder itself).
    //
    // Membership in a forwarder's PID namespace is accepted because joining
    // it needs `--pid container:<id>` on the same root daemon — and the id is
    // listable through that socket, so the name's randomness is no defence.
    // What excludes it is that root access to the daemon socket is
    // provisioning-administrator access, the trust boundary this profile
    // does not claim to hold against. A foreign process joining the engine's
    // network namespace directly is what this refuses, and is the reachable
    // case. Narrowing the exception to the forwarder's exact task set is
    // #681.
    //
    // Censused capture-free, by PID-namespace identity: a forwarder shares
    // the engine's network namespace and reaps and respawns its `cat` pipes
    // as it forwards, so qualifying each occupant would race a legitimate
    // child that is momentarily a zombie with no executable to read (finding
    // on #640).
    let mut pid_anchors = vec![init];
    pid_anchors.extend(forwarders.iter().copied());
    let foreign = foreign_network_tasks(init, &pid_anchors).map_err(Premise::Accounting.named())?;
    if !foreign.is_empty() {
        #[cfg(test)]
        eprintln!(
            "accounting refused net occupants outside every known pid namespace: {foreign:?}"
        );
        return Err(Error::Containment(Premise::Accounting));
    }
    // Mount and IPC alike: a container joined to either reaches the
    // engine's files or its shared memory without being in any listing the
    // engine's PID namespace produces. No forwarder shares these.
    for namespace in ["mnt", "ipc"] {
        for_each_occupant(init, namespace, |occupant| {
            if init.same_namespace(occupant, "pid")? {
                Ok(())
            } else {
                refused(occupant, namespace);
                Err(UnqualifiedProcess)
            }
        })
        .map_err(Premise::Accounting.named())?;
    }
    Ok(())
}

/// Names an occupant the accounting refused, so a fixture whose containers
/// are gone by the time anyone looks still says which process and namespace.
fn refused(occupant: &ProcessLease, namespace: &str) {
    #[cfg(test)]
    eprintln!(
        "accounting refused {} occupant pid={} exe={:?}",
        namespace,
        occupant.pid(),
        occupant.executable_path()
    );
    let _ = (occupant, namespace);
}
