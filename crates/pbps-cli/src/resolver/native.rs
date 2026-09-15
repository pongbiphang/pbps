//! Read-only Linux process leases for local runtime/channel qualification.
//!
//! The kernel supplies the process identity. Holding its proc directory keeps
//! a reused numeric PID from substituting a new process. This observes the
//! running main executable; it does not certify native-library compatibility.

use pbps_db::transport::{ConnectionId, PeerVerifiedConn};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read as _;
use std::net::SocketAddr;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

mod execution;
mod private_channel;
mod target;
pub(crate) use execution::{ExecutionLease, ExecutionProfile};
pub(crate) use private_channel::{PrivateChannelLease, PrivateChannelProfile, awaiting_engine};
pub(crate) use target::TargetWitness;
pub use target::{NativeTarget, NativeTargetError};

#[derive(Debug, thiserror::Error)]
#[error("the actual Linux peer process or its protected executable cannot be established")]
pub struct UnqualifiedProcess;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn of(file: &File) -> Result<Self, UnqualifiedProcess> {
        let metadata = file.metadata().map_err(|_| UnqualifiedProcess)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

/// An actual process, never a caller-entered claim about a PID or version.
pub struct ProcessLease {
    directory: File,
    executable: File,
    executable_path: PathBuf,
    start_ticks: u64,
    namespaces: Vec<(&'static str, File, FileIdentity)>,
    pid: u32,
    namespace_pid: u32,
}

impl ProcessLease {
    /// Reads only this process's kernel metadata. The executable must be
    /// installed by root and not writable by a group or other users. A proxy
    /// remains a proxy: the caller must separately match a supported runtime.
    pub fn capture(pid: u32) -> Result<Self, UnqualifiedProcess> {
        if pid == 0 {
            return Err(UnqualifiedProcess);
        }
        let directory = open_process(pid).map_err(|_| UnqualifiedProcess)?;
        let base = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let start_ticks = start_ticks(&base)?;
        let status =
            std::fs::read_to_string(base.join("status")).map_err(|_| UnqualifiedProcess)?;
        let namespace_pid = status
            .lines()
            .find_map(|line| line.strip_prefix("NSpid:"))
            .and_then(|pids| pids.split_whitespace().last())
            .and_then(|pid| pid.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
            .ok_or(UnqualifiedProcess)?;
        let executable_path =
            std::fs::read_link(base.join("exe")).map_err(|_| UnqualifiedProcess)?;
        let executable = File::open(base.join("exe")).map_err(|_| UnqualifiedProcess)?;
        let metadata = executable.metadata().map_err(|_| UnqualifiedProcess)?;
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(UnqualifiedProcess);
        }
        let mut namespaces = Vec::new();
        for name in ["pid", "mnt", "net", "user"] {
            let file = File::open(base.join("ns").join(name)).map_err(|_| UnqualifiedProcess)?;
            let identity = FileIdentity::of(&file)?;
            namespaces.push((name, file, identity));
        }
        let lease = Self {
            directory,
            executable,
            executable_path,
            start_ticks,
            namespaces,
            pid,
            namespace_pid,
        };
        lease.check()?;
        Ok(lease)
    }

    pub fn executable_path(&self) -> &Path {
        &self.executable_path
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// PostgreSQL reports getpid() inside its own PID namespace. The kernel's
    /// NSpid mapping binds that value when the verifier observes it from an
    /// ancestor namespace; SQL Server PAL numbers are not used here.
    pub fn namespace_pid(&self) -> u32 {
        self.namespace_pid
    }

    pub fn same_process(&self, other: &Self) -> Result<bool, UnqualifiedProcess> {
        self.check()?;
        other.check()?;
        Ok(self.start_ticks == other.start_ticks
            && FileIdentity::of(&self.directory)? == FileIdentity::of(&other.directory)?
            && self
                .namespaces
                .iter()
                .zip(&other.namespaces)
                .all(|(a, b)| a.0 == b.0 && a.2 == b.2))
    }

    pub(crate) fn same_namespace(
        &self,
        other: &Self,
        name: &str,
    ) -> Result<bool, UnqualifiedProcess> {
        self.check()?;
        other.check()?;
        let identity = |process: &Self| {
            process
                .namespaces
                .iter()
                .find(|(key, _, _)| *key == name)
                .map(|(_, _, identity)| *identity)
                .ok_or(UnqualifiedProcess)
        };
        Ok(identity(self)? == identity(other)?)
    }

    pub fn has_same_executable_parent(&self) -> Result<bool, UnqualifiedProcess> {
        self.check()?;
        let stat = std::fs::read_to_string(proc_base(&self.directory).join("stat"))
            .map_err(|_| UnqualifiedProcess)?;
        let parent = stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or(UnqualifiedProcess)?;
        if parent == 0 {
            return Ok(false);
        }
        let executable =
            File::open(format!("/proc/{parent}/exe")).map_err(|_| UnqualifiedProcess)?;
        let same = FileIdentity::of(&executable)? == FileIdentity::of(&self.executable)?;
        self.check()?;
        Ok(same)
    }

    pub fn check(&self) -> Result<(), UnqualifiedProcess> {
        let base = PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()));
        if start_ticks(&base)? != self.start_ticks {
            return Err(UnqualifiedProcess);
        }
        let current = File::open(base.join("exe")).map_err(|_| UnqualifiedProcess)?;
        if FileIdentity::of(&current)? != FileIdentity::of(&self.executable)?
            || std::fs::read_link(base.join("exe")).map_err(|_| UnqualifiedProcess)?
                != self.executable_path
        {
            return Err(UnqualifiedProcess);
        }
        for (name, held, identity) in &self.namespaces {
            let current = File::open(base.join("ns").join(name)).map_err(|_| UnqualifiedProcess)?;
            if FileIdentity::of(&current)? != *identity || FileIdentity::of(held)? != *identity {
                return Err(UnqualifiedProcess);
            }
        }
        Ok(())
    }
}

/// A socket-owner observation bound to one TLS connection. This contains no
/// engine identity claim; a supported adapter must still recognize the engine
/// and correlate its read-only identity query with this native process.
pub struct SocketOwnerLease {
    connection: ConnectionId,
    local: SocketAddr,
    peer: SocketAddr,
    service: ProcessLease,
    owner: ProcessLease,
    socket_inode: u64,
}

impl SocketOwnerLease {
    pub fn capture(
        connection: &PeerVerifiedConn,
        service_pid: u32,
    ) -> Result<Self, UnqualifiedProcess> {
        let endpoints = connection.tcp_endpoints();
        let service = ProcessLease::capture(service_pid)?;
        let (owner, socket_inode) = socket_owner(&service, endpoints.local(), endpoints.peer())?;
        let lease = Self {
            connection: connection.id(),
            local: endpoints.local(),
            peer: endpoints.peer(),
            service,
            owner,
            socket_inode,
        };
        lease.check(connection)?;
        Ok(lease)
    }

    pub fn owner(&self) -> &ProcessLease {
        &self.owner
    }
    pub fn service(&self) -> &ProcessLease {
        &self.service
    }

    pub fn check(&self, connection: &PeerVerifiedConn) -> Result<(), UnqualifiedProcess> {
        if connection.id() != self.connection
            || connection.tcp_endpoints().local() != self.local
            || connection.tcp_endpoints().peer() != self.peer
        {
            return Err(UnqualifiedProcess);
        }
        self.check_socket()
    }

    fn check_socket(&self) -> Result<(), UnqualifiedProcess> {
        self.owner.check()?;
        let (owner, inode) = socket_owner(&self.service, self.local, self.peer)?;
        if inode != self.socket_inode || !self.owner.same_process(&owner)? {
            return Err(UnqualifiedProcess);
        }
        Ok(())
    }
}

fn proc_base(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

fn encoded(address: SocketAddr) -> String {
    let ip = match address.ip() {
        std::net::IpAddr::V4(ip) => format!("{:08X}", u32::from_ne_bytes(ip.octets())),
        std::net::IpAddr::V6(ip) => ip
            .octets()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|part| format!("{:08X}", u32::from_ne_bytes(*part)))
            .collect(),
    };
    format!("{ip}:{:04X}", address.port())
}

fn peer_inode(local: SocketAddr, peer: SocketAddr) -> Result<u64, UnqualifiedProcess> {
    if !local.ip().is_loopback() || !peer.ip().is_loopback() || local.is_ipv4() != peer.is_ipv4() {
        return Err(UnqualifiedProcess);
    }
    let table = if local.is_ipv4() {
        "/proc/self/net/tcp"
    } else {
        "/proc/self/net/tcp6"
    };
    let text = read_bounded(Path::new(table), 32 * 1024 * 1024)?;
    let mut lines = text.lines();
    lines.next().ok_or(UnqualifiedProcess)?;
    let local = encoded(local);
    let peer = encoded(peer);
    let mut found = None;
    let mut client_present = false;
    for line in lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 {
            return Err(UnqualifiedProcess);
        }
        if fields[3] != "01" {
            continue;
        }
        if fields[1] == local && fields[2] == peer && fields[9] != "0" {
            client_present = true;
        }
        if fields[1] == peer && fields[2] == local {
            let inode = fields[9].parse::<u64>().map_err(|_| UnqualifiedProcess)?;
            if inode == 0 || found.replace(inode).is_some() {
                return Err(UnqualifiedProcess);
            }
        }
    }
    if !client_present {
        return Err(UnqualifiedProcess);
    }
    found.ok_or(UnqualifiedProcess)
}

fn read_bounded(path: &Path, limit: usize) -> Result<String, UnqualifiedProcess> {
    let mut text = String::new();
    File::open(path)
        .map_err(|_| UnqualifiedProcess)?
        .take(limit as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|_| UnqualifiedProcess)?;
    if text.len() > limit {
        return Err(UnqualifiedProcess);
    }
    Ok(text)
}

// A held proc inode can outlive its task. Linux reports ESRCH as well as
// ENOENT in that case. Permission and I/O failures still mean unreadable.
fn process_gone(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(3)
}

fn open_process(pid: u32) -> std::io::Result<File> {
    // The opt-in kernel fixture mounts only its two owned process trees into
    // a private PID namespace. Production always uses the real proc mount.
    #[cfg(test)]
    if std::env::var_os("PBPS_NATIVE_OWNED_PROC_FIXTURE").is_some() {
        return File::open(format!("/pbps-owned-proc/{pid}")).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "process outside the fixture's mounted scope",
                )
            } else {
                error
            }
        });
    }
    File::open(format!("/proc/{pid}"))
}

fn process_scope(service: &ProcessLease) -> Result<Vec<(u32, File)>, UnqualifiedProcess> {
    let mut scope = vec![(
        service.pid,
        service
            .directory
            .try_clone()
            .map_err(|_| UnqualifiedProcess)?,
    )];
    let mut seen = BTreeSet::from([service.pid]);
    let mut cursor = 0;
    while cursor < scope.len() {
        let (parent_pid, directory) = &scope[cursor];
        let parent_pid = *parent_pid;
        let mut children = Vec::new();
        let tasks = match std::fs::read_dir(proc_base(directory).join("task")) {
            Ok(tasks) => tasks,
            // Another session may exit while its already-observed proc
            // directory is held. The selected service and socket owner have
            // their own live leases; a vanished unrelated child is not an
            // unreadable live process or a reason to invalidate that socket.
            Err(error) if process_gone(&error) => {
                cursor += 1;
                continue;
            }
            Err(_) => return Err(UnqualifiedProcess),
        };
        for task in tasks {
            let task = task.map_err(|_| UnqualifiedProcess)?;
            let text = match std::fs::read_to_string(task.path().join("children")) {
                Ok(text) if text.len() <= 65536 => text,
                Err(error) if process_gone(&error) => continue,
                Ok(_) | Err(_) => return Err(UnqualifiedProcess),
            };
            for pid in text.split_whitespace() {
                children.push(pid.parse::<u32>().map_err(|_| UnqualifiedProcess)?);
            }
        }
        for pid in children {
            if !seen.insert(pid) {
                continue;
            }
            if scope.len() >= 1024 {
                return Err(UnqualifiedProcess);
            }
            let directory = match open_process(pid) {
                Ok(file) => file,
                Err(error) if process_gone(&error) => continue,
                Err(_) => return Err(UnqualifiedProcess),
            };
            let stat = match std::fs::read_to_string(proc_base(&directory).join("stat")) {
                Ok(stat) => stat,
                Err(error) if process_gone(&error) => continue,
                Err(_) => return Err(UnqualifiedProcess),
            };
            let ppid = stat
                .rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u32>().ok())
                .ok_or(UnqualifiedProcess)?;
            if ppid != parent_pid {
                return Err(UnqualifiedProcess);
            }
            scope.push((pid, directory));
        }
        cursor += 1;
    }
    Ok(scope)
}

fn socket_owner(
    service: &ProcessLease,
    local: SocketAddr,
    peer: SocketAddr,
) -> Result<(ProcessLease, u64), UnqualifiedProcess> {
    service.check()?;
    let self_net = File::open("/proc/self/ns/net").map_err(|_| UnqualifiedProcess)?;
    let own_net = service
        .namespaces
        .iter()
        .find(|(name, _, _)| *name == "net")
        .ok_or(UnqualifiedProcess)?;
    if own_net.2 != FileIdentity::of(&self_net)? {
        return Err(UnqualifiedProcess);
    }
    let inode = peer_inode(local, peer)?;
    let mut owners = socket_owners(service, inode)?;
    if owners.len() != 1 {
        return Err(UnqualifiedProcess);
    }
    service.check()?;
    if peer_inode(local, peer)? != inode {
        return Err(UnqualifiedProcess);
    }
    Ok((owners.remove(0), inode))
}

fn socket_owners(
    service: &ProcessLease,
    inode: u64,
) -> Result<Vec<ProcessLease>, UnqualifiedProcess> {
    let expected = PathBuf::from(format!("socket:[{inode}]"));
    let mut owners = Vec::new();
    for (pid, directory) in process_scope(service)? {
        let entries = match std::fs::read_dir(proc_base(&directory).join("fd")) {
            Ok(entries) => entries,
            Err(error) if process_gone(&error) => continue,
            Err(_) => return Err(UnqualifiedProcess),
        };
        let mut owns_socket = false;
        for entry in entries {
            let entry = entry.map_err(|_| UnqualifiedProcess)?;
            match std::fs::read_link(entry.path()) {
                Ok(path) => owns_socket |= path == expected,
                Err(error) if process_gone(&error) => (),
                Err(_) => return Err(UnqualifiedProcess),
            }
        }
        if owns_socket {
            let lease = ProcessLease::capture(pid)?;
            if FileIdentity::of(&directory)? != FileIdentity::of(&lease.directory)? {
                return Err(UnqualifiedProcess);
            }
            owners.push(lease);
        }
    }
    service.check()?;
    Ok(owners)
}

fn start_ticks(base: &Path) -> Result<u64, UnqualifiedProcess> {
    let mut stat = String::new();
    File::open(base.join("stat"))
        .map_err(|_| UnqualifiedProcess)?
        .take(8193)
        .read_to_string(&mut stat)
        .map_err(|_| UnqualifiedProcess)?;
    if stat.len() > 8192 {
        return Err(UnqualifiedProcess);
    }
    // comm is parenthesized and can itself contain spaces and ')'. The final
    // ')' terminates it; fields from state onward cannot contain that byte.
    let (_, fields) = stat.rsplit_once(')').ok_or(UnqualifiedProcess)?;
    fields
        .split_whitespace()
        .nth(19)
        .ok_or(UnqualifiedProcess)?
        .parse()
        .map_err(|_| UnqualifiedProcess)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Command, Stdio};

    #[test]
    fn a_process_lease_expires_even_while_its_proc_directory_is_held() {
        let mut child = Command::new("/usr/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let lease = ProcessLease::capture(child.id());
        let cleanup = child.kill();
        child.wait().unwrap();
        cleanup.unwrap();
        let lease = lease.unwrap();
        assert!(lease.executable_path().ends_with("sleep"));
        let vanished =
            std::fs::read_to_string(proc_base(&lease.directory).join("stat")).unwrap_err();
        assert!(
            process_gone(&vanished),
            "a held dead proc inode is absence, not unreadable live state"
        );
        assert!(
            lease.check().is_err(),
            "an open proc directory must not preserve a dead peer's qualification"
        );
        assert!(ProcessLease::capture(0).is_err());
        assert!(ProcessLease::capture(u32::MAX).is_err());
    }

    #[test]
    fn writable_executable_content_cannot_identify_a_trusted_peer() {
        let path = std::env::temp_dir().join(format!(
            "pbps-untrusted-executable-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::copy("/usr/bin/sleep", &path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        // Another parallel test can fork while copy holds its writable file
        // descriptor. CLOEXEC closes that inherited descriptor at exec, not
        // at fork; retry only this transient fixture error, with a deadline.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut child = loop {
            match Command::new(&path).arg("30").spawn() {
                Ok(child) => break child,
                Err(error)
                    if error.raw_os_error() == Some(26) && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => {
                    std::fs::remove_file(&path).unwrap();
                    panic!("cannot start the writable-executable fixture: {error}");
                }
            }
        };
        let lease = ProcessLease::capture(child.id());
        let cleanup = child.kill();
        child.wait().unwrap();
        std::fs::remove_file(path).unwrap();
        cleanup.unwrap();
        assert!(lease.is_err());
    }

    #[test]
    fn exec_in_the_same_process_invalidates_its_previous_lease() {
        use std::io::Write as _;
        let mut child = Command::new("/bin/bash")
            .args(["-c", "read -r line; exec /usr/bin/sleep 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let lease = ProcessLease::capture(child.id());
        let invalidated = match &lease {
            Ok(lease) => {
                child.stdin.take().unwrap().write_all(b"go\n").unwrap();
                let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while lease.check().is_ok() && std::time::Instant::now() < until {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                lease.check().is_err()
            }
            Err(_) => false,
        };
        let cleanup = child.kill();
        child.wait().unwrap();
        cleanup.unwrap();
        assert!(
            lease.is_ok(),
            "the original root-installed executable must be observable"
        );
        assert!(
            invalidated,
            "a stable numeric PID cannot preserve an exec-replaced peer's lease"
        );
    }
}
