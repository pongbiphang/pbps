//! The kernel's reading of a supplied container.
//!
//! The daemon's record says what the operator asked for; this is what the
//! kernel is actually doing. Every premise the profile names is read from the
//! container's init process and from every task sharing its namespaces, and
//! read again on every check: an admission is a measurement that held once.

use super::profile::{self, ServerProfile};
use super::{Error, Premise};
use crate::resolver::native::{
    BoundedResourceLease, ProcessLease, UnqualifiedProcess, cgroup_relative, for_each_occupant,
    groups, mount_rows, private_network, process_scope, security,
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
        for (pid, _) in process_scope(&init)
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
        // namespaces — into an externally connected network, say — would
        // bridge the run straight out of its containment.
        for namespace in ["net", "mnt"] {
            if !init.same_namespace(occupant, namespace)? {
                return Err(UnqualifiedProcess);
            }
        }
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

/// Nothing may share the container's network or mount namespace that is not
/// in its PID namespace, except this run's own forwarders in the network
/// namespace. A container joined with `--network container:` would otherwise
/// be invisible to every other check.
fn accounted(init: &ProcessLease, forwarders: &[&ProcessLease]) -> Result<(), Error> {
    for_each_occupant(init, "net", |occupant| {
        if init.same_namespace(occupant, "pid")? {
            return Ok(());
        }
        for forwarder in forwarders {
            if forwarder.same_namespace(occupant, "pid")? {
                return Ok(());
            }
        }
        Err(UnqualifiedProcess)
    })
    .map_err(Premise::Accounting.named())?;
    for_each_occupant(init, "mnt", |occupant| {
        if init.same_namespace(occupant, "pid")? {
            Ok(())
        } else {
            Err(UnqualifiedProcess)
        }
    })
    .map_err(Premise::Accounting.named())
}
