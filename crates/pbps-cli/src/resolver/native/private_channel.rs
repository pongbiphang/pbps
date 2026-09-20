//! Actual kernel endpoints behind the fixed private forwarder. This proves
//! the backend/process boundary; complete runtime admission must also qualify
//! containment, target separation and the relevant engine compatibility.

use super::{
    File, ProcessLease, UnqualifiedProcess, complete_process_scope, observe_incidental, proc_base,
    socket_owners,
};
use pbps_db::resolver::{BackendProcess, InstanceObservation};
use pbps_db::transport::ConnectionId;
use std::io::Read as _;

pub(crate) struct PrivateChannelProfile {
    pub executable: &'static str,
    pub uid: u32,
    pub capabilities: u64,
    pub port: u16,
}

pub(crate) struct PrivateChannelLease {
    connection: ConnectionId,
    workload: ProcessLease,
    control: ProcessLease,
    backend: ProcessLease,
    forwarders: Vec<ProcessLease>,
    sockets: Sockets,
    profile: PrivateChannelProfile,
}

#[derive(PartialEq, Eq)]
struct Sockets {
    client: u64,
    server: u64,
    local: String,
    peer: String,
}

impl PrivateChannelLease {
    pub(crate) fn capture(
        workload: u32,
        control: u32,
        connection: ConnectionId,
        identity: &InstanceObservation,
        profile: PrivateChannelProfile,
    ) -> Result<Self, UnqualifiedProcess> {
        let workload = ProcessLease::capture(workload)?;
        let control = ProcessLease::capture(control)?;
        if workload.same_namespace(&control, "pid")?
            || workload.same_namespace(&control, "mnt")?
            || !workload.same_namespace(&control, "net")?
            || !workload.same_namespace(&control, "user")?
        {
            return Err(UnqualifiedProcess);
        }
        guard(&workload)?;
        guard(&control)?;
        private_network(&workload)?;
        let sockets = sockets(&workload, profile.port)?;
        let mut backends = socket_owners(&workload, sockets.server)?;
        if backends.len() != 1 {
            return Err(UnqualifiedProcess);
        }
        let backend = backends.remove(0);
        let forwarders = socket_owners(&control, sockets.client)?;
        let lease = Self {
            connection,
            workload,
            control,
            backend,
            forwarders,
            sockets,
            profile,
        };
        lease.check(connection, identity)?;
        Ok(lease)
    }

    pub(crate) fn check(
        &self,
        connection: ConnectionId,
        identity: &InstanceObservation,
    ) -> Result<(), UnqualifiedProcess> {
        if connection != self.connection {
            return Err(UnqualifiedProcess);
        }
        guard(&self.workload)?;
        guard(&self.control)?;
        private_network(&self.workload)?;
        self.backend.check()?;
        if self
            .backend
            .executable_path()
            .file_name()
            .is_none_or(|name| name != self.profile.executable)
        {
            return Err(UnqualifiedProcess);
        }
        if let BackendProcess::NativePid(pid) = identity.process
            && pid.get() != self.backend.namespace_pid()
        {
            return Err(UnqualifiedProcess);
        }
        let current = sockets(&self.workload, self.profile.port)?;
        if current != self.sockets {
            return Err(UnqualifiedProcess);
        }
        let backends = socket_owners(&self.workload, current.server)?;
        if backends.len() != 1 || !backends[0].same_process(&self.backend)? {
            return Err(UnqualifiedProcess);
        }
        let forwarders = socket_owners(&self.control, current.client)?;
        if forwarders.len() != 3 || forwarders.len() != self.forwarders.len() {
            return Err(UnqualifiedProcess);
        }
        let mut shells = 0;
        let mut readers_writers = 0;
        for (current, held) in forwarders.iter().zip(&self.forwarders) {
            if !current.same_process(held)? {
                return Err(UnqualifiedProcess);
            }
            match current
                .executable_path()
                .file_name()
                .and_then(|name| name.to_str())
            {
                Some("bash") => shells += 1,
                Some("cat") => readers_writers += 1,
                _ => return Err(UnqualifiedProcess),
            }
            security(current, 65534, 0)?;
        }
        if shells != 1 || readers_writers != 2 {
            return Err(UnqualifiedProcess);
        }
        // Every occupant, so a walk that could not account for one refuses
        // rather than passing the rest (#730).
        for (pid, directory) in complete_process_scope(&self.workload)? {
            if pid == self.workload.pid() {
                continue;
            }
            observe_incidental(pid, &directory, |process| {
                security(&process, self.profile.uid, self.profile.capabilities)
            })?;
        }
        // Re-read after descriptor inspection so a closed/replaced socket
        // cannot be accepted from an earlier table snapshot.
        if sockets(&self.workload, self.profile.port)? != self.sockets {
            return Err(UnqualifiedProcess);
        }
        self.backend.check()?;
        Ok(())
    }

    pub(crate) fn separate_from(&self, target: &ProcessLease) -> Result<(), UnqualifiedProcess> {
        for namespace in ["pid", "mnt", "net"] {
            if self.workload.same_namespace(target, namespace)? {
                return Err(UnqualifiedProcess);
            }
        }
        Ok(())
    }
}

pub(crate) fn awaiting_engine(
    pid: u32,
    profile: &PrivateChannelProfile,
) -> Result<ProcessLease, UnqualifiedProcess> {
    let root = ProcessLease::capture(pid)?;
    guard(&root)?;
    private_network(&root)?;
    // `complete` and not `seen`: this asserts the tree is exactly the root
    // and one shell, and a walk missing a process makes a larger tree look
    // like the expected one (#730).
    let scope = complete_process_scope(&root)?;
    if scope.len() != 2 {
        return Err(UnqualifiedProcess);
    }
    let child = ProcessLease::capture(scope[1].0)?;
    if child
        .executable_path()
        .file_name()
        .is_none_or(|name| name != "bash")
    {
        return Err(UnqualifiedProcess);
    }
    security(&child, profile.uid, profile.capabilities)?;
    root.check()?;
    Ok(root)
}

pub(crate) fn guard(process: &ProcessLease) -> Result<(), UnqualifiedProcess> {
    process.check()?;
    if process.namespace_pid() != 1
        || process
            .executable_path()
            .file_name()
            .is_none_or(|name| name != "timeout")
    {
        return Err(UnqualifiedProcess);
    }
    // SETUID/SETGID/SETPCAP/KILL, optionally NET_BIND_SERVICE for the engine.
    let status = status(process)?;
    check_status(&status, 0, 0x5e0)
}

pub(crate) fn private_network(process: &ProcessLease) -> Result<(), UnqualifiedProcess> {
    // TCP filtering alone does not bound UDP/DNS or alternate protocols.
    // Only the private loopback device may exist. The workload cannot create
    // an interface or change its namespace: NET_ADMIN and unshare are absent.
    let devices = super::read_bounded(&proc_base(&process.directory).join("net/dev"), 65536)?;
    let mut names = Vec::new();
    for line in devices.lines().skip(2) {
        let (name, _) = line.split_once(':').ok_or(UnqualifiedProcess)?;
        names.push(name.trim());
    }
    if names != ["lo"] {
        return Err(UnqualifiedProcess);
    }
    let routes = super::read_bounded(&proc_base(&process.directory).join("net/route"), 65536)?;
    if routes.lines().count() != 1 || !routes.starts_with("Iface") {
        return Err(UnqualifiedProcess);
    }
    process.check()
}

fn status(process: &ProcessLease) -> Result<String, UnqualifiedProcess> {
    let mut text = String::new();
    File::open(proc_base(&process.directory).join("status"))
        .map_err(|_| UnqualifiedProcess)?
        .take(65537)
        .read_to_string(&mut text)
        .map_err(|_| UnqualifiedProcess)?;
    if text.len() > 65536 {
        return Err(UnqualifiedProcess);
    }
    Ok(text)
}

pub(crate) fn security(
    process: &ProcessLease,
    uid: u32,
    capabilities: u64,
) -> Result<(), UnqualifiedProcess> {
    process.check()?;
    check_status(&status(process)?, uid, capabilities)?;
    process.check()
}

fn check_status(text: &str, uid: u32, capabilities: u64) -> Result<(), UnqualifiedProcess> {
    let field = |name: &str| -> Result<&str, UnqualifiedProcess> {
        let mut values = text.lines().filter_map(|line| line.strip_prefix(name));
        let value = values.next().ok_or(UnqualifiedProcess)?;
        if values.next().is_some() {
            return Err(UnqualifiedProcess);
        }
        Ok(value.trim())
    };
    if field("NoNewPrivs:")? != "1" || field("Seccomp:")? != "2" {
        return Err(UnqualifiedProcess);
    }
    let uids: Vec<_> = field("Uid:")?
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect();
    if uids.len() != 4 || uids.iter().any(|value| value.as_ref().ok() != Some(&uid)) {
        return Err(UnqualifiedProcess);
    }
    for name in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        let actual = u64::from_str_radix(field(name)?, 16).map_err(|_| UnqualifiedProcess)?;
        if actual & !capabilities != 0 {
            return Err(UnqualifiedProcess);
        }
    }
    Ok(())
}

fn sockets(workload: &ProcessLease, port: u16) -> Result<Sockets, UnqualifiedProcess> {
    let base = proc_base(&workload.directory);
    let ipv6 = super::read_bounded(&base.join("net/tcp6"), 65536)?;
    for line in ipv6.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 || fields[3] == "01" {
            return Err(UnqualifiedProcess);
        }
    }
    let text = super::read_bounded(&base.join("net/tcp"), 65536)?;
    let mut lines = text.lines();
    lines.next().ok_or(UnqualifiedProcess)?;
    let server_address = format!("0100007F:{port:04X}");
    let mut established = Vec::new();
    for line in lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 {
            return Err(UnqualifiedProcess);
        }
        if fields[3] != "01" {
            continue;
        }
        let inode: u64 = fields[9].parse().map_err(|_| UnqualifiedProcess)?;
        if inode == 0 {
            return Err(UnqualifiedProcess);
        }
        established.push((fields[1].to_owned(), fields[2].to_owned(), inode));
    }
    if established.len() != 2 {
        return Err(UnqualifiedProcess);
    }
    let client = established
        .iter()
        .find(|(local, peer, _)| local.starts_with("0100007F:") && *peer == server_address)
        .ok_or(UnqualifiedProcess)?;
    let server = established
        .iter()
        .find(|(local, peer, _)| *local == server_address && *peer == client.0)
        .ok_or(UnqualifiedProcess)?;
    if client.2 == server.2 {
        return Err(UnqualifiedProcess);
    }
    Ok(Sockets {
        client: client.2,
        server: server.2,
        local: client.0.clone(),
        peer: client.1.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_or_weakened_kernel_controls_cannot_qualify() {
        let status = "Uid:\t999 999 999 999\nNoNewPrivs:\t1\nSeccomp:\t2\nCapInh:\t0\nCapPrm:\t0\nCapEff:\t0\nCapBnd:\t0\nCapAmb:\t0\n";
        check_status(status, 999, 0).unwrap();
        for bad in [
            status.replace("NoNewPrivs:\t1", "NoNewPrivs:\t0"),
            status.replace("Seccomp:\t2", "Seccomp:\t0"),
            status.replace("CapEff:\t0", "CapEff:\t400"),
            status.replace("CapBnd:\t0", "CapBnd:\t400"),
            status.replace("999 999 999 999", "999 0 999 999"),
            status.replace("CapAmb:\t0\n", ""),
            format!("{status}Seccomp:\t2\n"),
        ] {
            assert!(check_status(&bad, 999, 0).is_err());
        }
    }
}
