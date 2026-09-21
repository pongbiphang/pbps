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

mod daemon;
pub(crate) mod executables;
mod execution;
mod namespace;
mod private_channel;
mod target;
pub(crate) use daemon::DaemonLease;
pub(crate) use execution::{
    BoundedResourceLease, ExecutionLease, ExecutionProfile, MountEntry, ResourceCeilings,
    cgroup_relative, mount_rows,
};
pub(crate) use namespace::for_each_namespace_task;
pub use namespace::{
    NamespaceError, NamespaceProcfs, NamespaceTaskId, TaskObservation, TaskReading,
};
pub(crate) use private_channel::{
    FORWARDER_PRIVILEGES, PrivateChannelLease, PrivateChannelProfile, WorkloadPrivileges,
    awaiting_engine, guarded_tasks, private_network, security,
};
pub(crate) use target::TargetWitness;
pub use target::{EnvironmentError, NativeTarget, NativeTargetError};

#[derive(Debug, thiserror::Error)]
#[error("the actual Linux peer process or its protected executable cannot be established")]
pub struct UnqualifiedProcess;

/// Which reading of a native binding refused.
///
/// [`UnqualifiedProcess`] is one value for every read below. That is the right
/// answer for a *caller* — nothing it could do differs between them — and no
/// answer at all for anyone holding a CI log from a fixture whose host is
/// already gone. Three occurrences of #674 arrived as one sentence each, on
/// both engines and at three different call sites, and deciding whether the
/// unstable read is a liveness window or a wrong rule needs to know which read
/// moved.
///
/// The same shape as [`crate::resolver::server::Premise`], one layer down.
/// The name is recorded where the refusal is *made* and the caller's type does
/// not change: widening `UnqualifiedProcess` itself is #646, and it would
/// reach every caller in the crate. Propagated errors are never renamed — a
/// refusal is named exactly once, by the read that made it, so a log line
/// names a read and not a call stack.
///
/// [`target`]'s own `stage` names the *step* of `NativeTarget::check` one
/// layer up, and the two compose rather than compete: a failing round now
/// prints the read that refused and then the step it refused in
/// (`reading=owner-count(2)`, `stage=lease-after`). They become one under
/// #646; they are apart today because `target.rs` is being rewritten by
/// another change and a merge conflict there would cost more than a duplicate
/// debug print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reading {
    /// `/proc/<pid>/stat`'s start time: unreadable, or a different process.
    StartTicks,
    /// The held `exe`: unreadable, a different file object, or a moved path.
    Executable,
    /// A held namespace: unreadable, or no longer the one captured.
    Namespace,
    /// The TLS connection this lease was bound to is not the one presented.
    Connection,
    /// The socket behind the connection is no longer the inode captured.
    SocketInode,
    /// The process holding that socket is no longer the one captured.
    SocketOwner,
    /// This verifier's own network namespace, or the service's record of it.
    NetNamespace,
    /// `/proc/self/net/tcp`'s established pair no longer carries the inode
    /// this lease was captured over — read twice, and it moved between them.
    PeerInode,
    /// A pair this cannot answer for at all: not both loopback, or one of
    /// each address family.
    PeerEndpoints,
    /// `/proc/self/net/tcp` itself: unreadable, or a row that will not parse.
    PeerTable,
    /// This verifier's own end of the connection is not in that table.
    PeerClientAbsent,
    /// The engine's end is not in that table at all, in any state.
    PeerServerAbsent,
    /// The engine's end is in that table, but not established — so the socket
    /// this run was bound to is not the one that is there.
    ///
    /// The code is `/proc/net/tcp`'s own `st` column, and it is read **of the
    /// engine's row**, which is where the direction lives. **Measured** on a
    /// loopback pair: when this verifier closed first the engine's row read
    /// `08`, and when the engine closed first it read `04` or `05` depending
    /// on whether the read caught it before our ACK. So on the engine's row
    /// `08` (`CLOSE_WAIT`) means it is holding *our* FIN and has not closed
    /// yet, while `04`/`05` (`FIN_WAIT1`, `FIN_WAIT2`) and `06` (`TIME_WAIT`)
    /// mean the engine closed first. A
    /// refusal that named the wrong end would send the next investigation to
    /// the wrong process, which is worse than not naming one.
    PeerServerState(u8),
    /// The engine's end is in that table more than once, which one
    /// established pair cannot be.
    PeerServerDuplicated,
    /// The number of processes in the service's scope holding the socket,
    /// where exactly one is the only answer a lease can be built on.
    OwnerCount(usize),
    /// Walking the service's descendants: a `task`, `children` or `stat` read
    /// that did not merely say the process is gone.
    Scope,
    /// A process named by `children` whose own `stat` no longer names the
    /// parent that named it — a reparent, or a reused numeric PID.
    ScopeParent,
    /// The descriptor table of a process in the scope.
    ScopeDescriptors,
    /// A process the walk has just seen has no `/proc` entry to open.
    CaptureOpen,
    /// Its `stat` start time, when establishing it rather than re-reading it.
    CaptureStartTicks,
    /// Its `status`, or the `NSpid` line inside it.
    CaptureStatus,
    /// Its `exe` link, the file behind it, or that file's metadata.
    CaptureExecutable,
    /// Its executable is readable and is not a root-owned, group- and
    /// other-unwritable file — the rule a lease is built on rather than a
    /// read that moved. #650's shape lands here.
    CaptureUnprotected,
    /// One of its namespaces, when establishing it.
    CaptureNamespace,
    /// The recaptured process is not the one the walk held a directory for:
    /// its numeric PID now answers for something else.
    CaptureReplaced,
}

#[cfg(test)]
thread_local! {
    /// The last reading to refuse on this thread, for tests that need to
    /// assert *which* read answered rather than only that one did.
    static LAST_READING: std::cell::Cell<Option<Reading>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn last_reading() -> Option<Reading> {
    LAST_READING.with(std::cell::Cell::get)
}

impl Reading {
    /// The caller's answer, with the name recorded first.
    ///
    /// Under `cfg(test)` — which is every context that runs the resolver
    /// fixtures, since they are `cargo test` binaries — the name goes to
    /// stderr the way `docker/session.rs` reports its startup stages, and to
    /// a thread-local a unit test can read back.
    fn refuse(self) -> UnqualifiedProcess {
        #[cfg(test)]
        {
            LAST_READING.with(|cell| cell.set(Some(self)));
            eprintln!("native binding reading={self}");
        }
        UnqualifiedProcess
    }

    /// For `map_err`, where the read's own error carries nothing this names.
    fn named<E>(self) -> impl FnOnce(E) -> UnqualifiedProcess {
        move |_| self.refuse()
    }
}

impl std::fmt::Display for Reading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StartTicks => f.write_str("start-ticks"),
            Self::Executable => f.write_str("executable"),
            Self::Namespace => f.write_str("namespace"),
            Self::Connection => f.write_str("connection"),
            Self::SocketInode => f.write_str("socket-inode"),
            Self::SocketOwner => f.write_str("socket-owner"),
            Self::NetNamespace => f.write_str("net-namespace"),
            Self::PeerInode => f.write_str("peer-inode"),
            Self::PeerEndpoints => f.write_str("peer-endpoints"),
            Self::PeerTable => f.write_str("peer-table"),
            Self::PeerClientAbsent => f.write_str("peer-client-absent"),
            Self::PeerServerAbsent => f.write_str("peer-server-absent"),
            Self::PeerServerState(state) => write!(f, "peer-server-state({state:02x})"),
            Self::PeerServerDuplicated => f.write_str("peer-server-duplicated"),
            Self::OwnerCount(found) => write!(f, "owner-count({found})"),
            Self::Scope => f.write_str("scope"),
            Self::ScopeParent => f.write_str("scope-parent"),
            Self::ScopeDescriptors => f.write_str("scope-descriptors"),
            Self::CaptureOpen => f.write_str("capture-open"),
            Self::CaptureStartTicks => f.write_str("capture-start-ticks"),
            Self::CaptureStatus => f.write_str("capture-status"),
            Self::CaptureExecutable => f.write_str("capture-executable"),
            Self::CaptureUnprotected => f.write_str("capture-unprotected"),
            Self::CaptureNamespace => f.write_str("capture-namespace"),
            Self::CaptureReplaced => f.write_str("capture-replaced"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    pub(crate) fn of(file: &File) -> Result<Self, UnqualifiedProcess> {
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
    source: ProcSource,
    namespace_pid: u32,
}

// A local PID must never be reopened in the observer's procfs. Holding the
// selected view also keeps its PID namespace alive for relative parent reads.
enum ProcSource {
    Observer(u32),
    Namespace(std::sync::Arc<File>),
}

impl ProcessLease {
    /// Reads only this process's kernel metadata. The executable must be
    /// installed by root and not writable by a group or other users. A proxy
    /// remains a proxy: the caller must separately match a supported runtime.
    pub fn capture(pid: u32) -> Result<Self, UnqualifiedProcess> {
        if pid == 0 {
            return Err(Reading::CaptureOpen.refuse());
        }
        let directory = open_process(pid).map_err(Reading::CaptureOpen.named())?;
        Self::capture_held(directory, ProcSource::Observer(pid))
    }

    fn capture_held(directory: File, source: ProcSource) -> Result<Self, UnqualifiedProcess> {
        let base = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let start_ticks = start_ticks(&base).map_err(Reading::CaptureStartTicks.named())?;
        let status =
            std::fs::read_to_string(base.join("status")).map_err(Reading::CaptureStatus.named())?;
        let namespace_pid = status
            .lines()
            .find_map(|line| line.strip_prefix("NSpid:"))
            .and_then(|pids| pids.split_whitespace().last())
            .and_then(|pid| pid.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| Reading::CaptureStatus.refuse())?;
        let executable_path =
            std::fs::read_link(base.join("exe")).map_err(Reading::CaptureExecutable.named())?;
        let executable =
            File::open(base.join("exe")).map_err(Reading::CaptureExecutable.named())?;
        let metadata = executable
            .metadata()
            .map_err(Reading::CaptureExecutable.named())?;
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(Reading::CaptureUnprotected.refuse());
        }
        let mut namespaces = Vec::new();
        // `ipc` is here for the dedicated-server profile's occupant accounting:
        // a container sharing the engine's IPC namespace reaches its shared
        // memory. The Docker profile only ever asks about pid/mnt/net/user, so
        // this is additive — `same_process` and `check` iterate whatever was
        // captured, and no caller assumes the set's size.
        for name in ["pid", "mnt", "net", "user", "ipc"] {
            let file = File::open(base.join("ns").join(name))
                .map_err(Reading::CaptureNamespace.named())?;
            let identity = FileIdentity::of(&file).map_err(Reading::CaptureNamespace.named())?;
            namespaces.push((name, file, identity));
        }
        let lease = Self {
            directory,
            executable,
            executable_path,
            start_ticks,
            namespaces,
            source,
            namespace_pid,
        };
        lease.check()?;
        Ok(lease)
    }

    pub fn executable_path(&self) -> &Path {
        &self.executable_path
    }

    /// Opens a path inside this process's own mount namespace, through the
    /// held proc directory, so a reused numeric PID cannot answer for it,
    /// and resolved the way the process itself would resolve it (see
    /// [`open_within`]).
    pub(crate) fn open_in_root(&self, relative: &str) -> Result<File, UnqualifiedProcess> {
        self.check()?;
        let file = open_within(&self.root()?, relative, rustix::fs::OFlags::empty())
            .map_err(|_| UnqualifiedProcess)?;
        self.check()?;
        Ok(file)
    }

    /// One bounded file inside this process's own mount namespace.
    pub(crate) fn read_root_file(
        &self,
        relative: &str,
        limit: usize,
    ) -> Result<String, UnqualifiedProcess> {
        self.check()?;
        let file = open_within(&self.root()?, relative, rustix::fs::OFlags::empty())
            .map_err(|_| UnqualifiedProcess)?;
        let text = read_bounded_from(file, limit)?;
        self.check()?;
        Ok(text)
    }

    /// The process's working directory, as a path inside its own mount
    /// namespace, through the held proc directory. For a backend that is the
    /// data directory, which is where the engine's loader resolves a library
    /// named by a relative path.
    pub(crate) fn working_directory(&self) -> Result<PathBuf, UnqualifiedProcess> {
        self.check()?;
        std::fs::read_link(proc_base(&self.directory).join("cwd")).map_err(|_| UnqualifiedProcess)
    }

    /// The process's root directory, through the held proc directory.
    fn root(&self) -> Result<File, UnqualifiedProcess> {
        File::open(proc_base(&self.directory).join("root")).map_err(|_| UnqualifiedProcess)
    }

    /// The entries of a directory inside this process's mount namespace.
    pub(crate) fn read_root_dir(
        &self,
        relative: &str,
    ) -> Result<BTreeSet<String>, UnqualifiedProcess> {
        self.check()?;
        let mut names = BTreeSet::new();
        let dir = open_within(&self.root()?, relative, rustix::fs::OFlags::DIRECTORY)
            .map_err(|_| UnqualifiedProcess)?;
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))
            .map_err(|_| UnqualifiedProcess)?
        {
            let entry = entry.map_err(|_| UnqualifiedProcess)?;
            names.insert(
                entry
                    .file_name()
                    .into_string()
                    .map_err(|_| UnqualifiedProcess)?,
            );
        }
        self.check()?;
        Ok(names)
    }

    /// Whether a namespace handle opened elsewhere is this process's own.
    ///
    /// A `/proc` or `/sys` mount cannot be told apart from the host's by its
    /// filesystem type; what distinguishes them is the namespace the instance
    /// belongs to, and this is how that comparison is made.
    pub(crate) fn owns_namespace(
        &self,
        name: &str,
        other: &File,
    ) -> Result<bool, UnqualifiedProcess> {
        self.check()?;
        let held = self
            .namespaces
            .iter()
            .find(|(key, _, _)| *key == name)
            .map(|(_, _, identity)| *identity)
            .ok_or(UnqualifiedProcess)?;
        let same = FileIdentity::of(other)? == held;
        self.check()?;
        Ok(same)
    }

    /// Reads one bounded file through the *held* proc directory, so a reused
    /// numeric PID cannot answer for the process this lease captured.
    /// The held handle on `/proc/<pid>/exe`: the executed file object
    /// itself, so reading it reads what runs even after the path changed.
    pub(crate) fn executable_file(&self) -> &File {
        &self.executable
    }

    /// Opens a file under this process's own `/proc/<pid>/` entry, such as a
    /// `map_files/<range>` mapped file object.
    pub(crate) fn open_proc(&self, relative: &str) -> Result<File, UnqualifiedProcess> {
        File::open(proc_base(&self.directory).join(relative)).map_err(|_| UnqualifiedProcess)
    }

    pub(crate) fn read_proc(
        &self,
        relative: &str,
        limit: usize,
    ) -> Result<String, UnqualifiedProcess> {
        read_bounded(&proc_base(&self.directory).join(relative), limit)
    }

    pub fn observer_pid(&self) -> Result<u32, UnqualifiedProcess> {
        match self.source {
            ProcSource::Observer(pid) => Ok(pid),
            ProcSource::Namespace(_) => Err(UnqualifiedProcess),
        }
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
        // Different procfs instances assign different directory inodes to
        // the same task. Both held entries must still be live: within a held
        // PID namespace, two live tasks cannot share the innermost task ID.
        let same = self.start_ticks == other.start_ticks
            && self.namespace_pid == other.namespace_pid
            && self
                .namespaces
                .iter()
                .zip(&other.namespaces)
                .all(|(a, b)| a.0 == b.0 && a.2 == b.2);
        // In particular, the first entry must not have exited before the
        // second check and allowed same-tick PID reuse in another proc view.
        self.check()?;
        other.check()?;
        Ok(same)
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
        let parent_directory = match &self.source {
            ProcSource::Observer(_) => File::open(format!("/proc/{parent}")),
            ProcSource::Namespace(directory) => namespace::open(
                directory,
                &parent.to_string(),
                rustix::fs::OFlags::DIRECTORY,
            ),
        };
        let parent_directory = parent_directory.map_err(|_| UnqualifiedProcess)?;
        let executable =
            File::open(proc_base(&parent_directory).join("exe")).map_err(|_| UnqualifiedProcess)?;
        let same = FileIdentity::of(&executable)? == FileIdentity::of(&self.executable)?;
        self.check()?;
        Ok(same)
    }

    pub fn check(&self) -> Result<(), UnqualifiedProcess> {
        let base = PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()));
        if start_ticks(&base).map_err(|_| Reading::StartTicks.refuse())? != self.start_ticks {
            return Err(Reading::StartTicks.refuse());
        }
        let current = File::open(base.join("exe")).map_err(Reading::Executable.named())?;
        if FileIdentity::of(&current).map_err(Reading::Executable.named())?
            != FileIdentity::of(&self.executable).map_err(Reading::Executable.named())?
            || std::fs::read_link(base.join("exe")).map_err(Reading::Executable.named())?
                != self.executable_path
        {
            return Err(Reading::Executable.refuse());
        }
        for (name, held, identity) in &self.namespaces {
            let current =
                File::open(base.join("ns").join(name)).map_err(Reading::Namespace.named())?;
            if FileIdentity::of(&current).map_err(Reading::Namespace.named())? != *identity
                || FileIdentity::of(held).map_err(Reading::Namespace.named())? != *identity
            {
                return Err(Reading::Namespace.refuse());
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
            return Err(Reading::Connection.refuse());
        }
        self.check_socket()
    }

    fn check_socket(&self) -> Result<(), UnqualifiedProcess> {
        // Propagated, not renamed: the owner's own reads name themselves.
        self.owner.check()?;
        let (owner, inode) = socket_owner(&self.service, self.local, self.peer)?;
        if inode != self.socket_inode {
            return Err(Reading::SocketInode.refuse());
        }
        if !self.owner.same_process(&owner)? {
            return Err(Reading::SocketOwner.refuse());
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
        return Err(Reading::PeerEndpoints.refuse());
    }
    let table = if local.is_ipv4() {
        "/proc/self/net/tcp"
    } else {
        "/proc/self/net/tcp6"
    };
    let text =
        read_bounded(Path::new(table), 32 * 1024 * 1024).map_err(Reading::PeerTable.named())?;
    server_inode(&text, &encoded(local), &encoded(peer)).map_err(Reading::refuse)
}

/// The inode of the engine's end of one established loopback pair, from the
/// text of `/proc/self/net/tcp`.
///
/// Separated from the read so the table can be handed in: every refusal below
/// is a shape a live table produces, and none of them can be staged against
/// the real one.
///
/// **One pair can appear more than once, and that is not two sockets.**
/// Iterating `/proc/net/tcp` is a `seq_file` walk over hash buckets and not a
/// snapshot, so a row can be emitted twice when the table changes between
/// chunks. Measured against a loopback pair held open while four threads
/// churned 7.3 million connections: **1,413 of 43,311 reads** returned the
/// same pair twice, and **every one of them carried the same inode** — two
/// different inodes for one established four-tuple was never seen, and cannot
/// be, because a four-tuple is one socket. Refusing the repetition is
/// therefore refusing a read of the table rather than a change to the socket,
/// and it was 14% of reads under that load (#674, DECISIONS 522).
///
/// **It does not drop one, which was worth finding out.** `peer-server-absent`
/// is how CI named this read, and a walk that skips rows was the obvious
/// explanation. Measured, it is not: a pool of 3,000 connections filled and
/// emptied repeatedly with `RST` closes, so that a close is an immediate
/// removal, produced no missed row in 2,451 reads; and 200 established pairs
/// held open while four threads churned 8,664,700 connections produced no
/// missed row in **2,202,800 row observations**, while a third of those reads
/// carried a duplicate. The walk re-emits on insertion and does not skip on
/// removal.
///
/// So an absent row means the engine's end was not established, and that is
/// the check being right rather than a read to be hardened. What it could not
/// say was *which*: no row at all, or a row in another state. Those are
/// different facts about who closed the socket, and they are told apart now
/// (#674).
fn server_inode(text: &str, local: &str, peer: &str) -> Result<u64, Reading> {
    let mut lines = text.lines();
    lines.next().ok_or(Reading::PeerTable)?;
    let mut found: Option<u64> = None;
    let mut client_present = false;
    let mut server_state = None;
    for line in lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 {
            return Err(Reading::PeerTable);
        }
        let established = fields[3] == "01";
        if established && fields[1] == local && fields[2] == peer && fields[9] != "0" {
            client_present = true;
        }
        if fields[1] == peer && fields[2] == local {
            // Kept even though only an established pair is this connection:
            // it is the difference between no row for the pair at all and a
            // row that says how the connection ended and which end began it,
            // and one bare name said neither.
            if !established {
                // Parsed, never `.ok()`: a state this cannot read is the
                // table being unreadable, and dropping the failure would let
                // it arrive as `PeerServerAbsent` — a fact about the
                // connection — or leave an earlier row's state standing in
                // for it. Absent, unreadable and "in another state" are three
                // things and this function answers all three.
                server_state =
                    Some(u8::from_str_radix(fields[3], 16).map_err(|_| Reading::PeerTable)?);
                continue;
            }
            let inode = fields[9].parse::<u64>().map_err(|_| Reading::PeerTable)?;
            if inode == 0 {
                return Err(Reading::PeerServerAbsent);
            }
            // Repeated is one socket seen twice; a *different* inode for the
            // same four-tuple is the table contradicting itself.
            if found
                .replace(inode)
                .is_some_and(|previous| previous != inode)
            {
                return Err(Reading::PeerServerDuplicated);
            }
        }
    }
    if !client_present {
        return Err(Reading::PeerClientAbsent);
    }
    found.ok_or(server_state.map_or(Reading::PeerServerAbsent, Reading::PeerServerState))
}

pub(crate) fn read_bounded(path: &Path, limit: usize) -> Result<String, UnqualifiedProcess> {
    read_bounded_from(File::open(path).map_err(|_| UnqualifiedProcess)?, limit)
}

fn read_bounded_from(file: File, limit: usize) -> Result<String, UnqualifiedProcess> {
    let mut text = String::new();
    file.take(limit as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|_| UnqualifiedProcess)?;
    if text.len() > limit {
        return Err(UnqualifiedProcess);
    }
    Ok(text)
}

/// Opens `relative` beneath `root` as a process whose root directory is
/// `root` would: `openat2` with `RESOLVE_IN_ROOT`, so an absolute symbolic
/// link inside it and a `..` at its top resolve inside it. A plain open of
/// `/proc/<pid>/root/<relative>` follows an absolute link from the
/// inspector's own root instead — a container's `/lib/foo.so -> /opt/foo.so`
/// read the host's `/opt/foo.so`, absent or another file, so a library the
/// engine loads fine was unreadable or the wrong content was hashed for
/// both sides (finding on #688).
pub(crate) fn open_within(
    root: &File,
    relative: &str,
    flags: rustix::fs::OFlags,
) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags, ResolveFlags};
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | flags;
    match rustix::fs::openat2(root, relative, flags, Mode::empty(), ResolveFlags::IN_ROOT) {
        Ok(fd) => Ok(File::from(fd)),
        // `RESOLVE_IN_ROOT` also refuses magic links (documented, as
        // `RESOLVE_NO_MAGICLINKS` does; measured as `EXDEV`, the escape
        // error, on 6.6), and a namespace handle such as `proc/1/ns/pid` is
        // one: in CI the containment check read "the container's /proc is
        // not its own instance" through it. A magic link names a kernel
        // object, not a path — its target reads `pid:[4026531836]` — so
        // following it cannot leave the root. The parent is resolved inside
        // the root, the last component is checked to be such a link and not
        // a path-shaped one, and it is opened through the parent's handle.
        Err(rustix::io::Errno::LOOP | rustix::io::Errno::XDEV) => {
            let (parent, name) = relative.rsplit_once('/').unwrap_or((".", relative));
            let dir = rustix::fs::openat2(
                root,
                parent,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::IN_ROOT,
            )?;
            let target = rustix::fs::readlinkat(&dir, name, Vec::new())?;
            let target = target.to_bytes();
            if target.contains(&b'/') || !target.contains(&b':') {
                return Err(rustix::io::Errno::LOOP.into());
            }
            let fd = rustix::fs::openat(&dir, name, flags, Mode::empty())?;
            Ok(File::from(fd))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod confined_open_tests {
    use super::open_within;
    use std::io::Read as _;

    /// A root of its own, with an absolute link pointing at a path that
    /// exists both inside it and on the host, and one that exists on the
    /// host alone: the first reads the inside file, the second is absent.
    #[test]
    fn an_absolute_link_inside_a_root_resolves_inside_that_root() {
        let root = std::env::temp_dir().join(format!("pbps-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/hostname"), "inside\n").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", root.join("host_link")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("host_only")).unwrap();
        std::os::unix::fs::symlink("../../etc/hostname", root.join("etc/up_and_over")).unwrap();
        assert!(
            std::path::Path::new("/etc/passwd").exists(),
            "the host has the file"
        );
        let dir = std::fs::File::open(&root).unwrap();
        let mut text = String::new();
        open_within(&dir, "host_link", rustix::fs::OFlags::empty())
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "inside\n");
        // `..` above the root stays at the root, as it would for the process.
        text.clear();
        open_within(&dir, "etc/up_and_over", rustix::fs::OFlags::empty())
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "inside\n");
        let error = open_within(&dir, "host_only", rustix::fs::OFlags::empty()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
        // A directory is listed through its confined handle.
        let listed = open_within(&dir, "etc", rustix::fs::OFlags::DIRECTORY).unwrap();
        assert!(listed.metadata().unwrap().is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A namespace handle is a magic link, which `RESOLVE_IN_ROOT` refuses;
    /// it is still opened, and is the same object a plain open reaches,
    /// while a path-shaped link in the last component stays confined.
    #[test]
    fn a_magic_link_under_the_root_opens_and_a_path_link_stays_confined() {
        use std::os::unix::fs::MetadataExt as _;
        let root = std::fs::File::open("/").unwrap();
        let relative = format!("proc/{}/ns/pid", std::process::id());
        let through_root = open_within(&root, &relative, rustix::fs::OFlags::empty()).unwrap();
        let plain = std::fs::File::open(format!("/{relative}")).unwrap();
        assert_eq!(
            through_root.metadata().unwrap().ino(),
            plain.metadata().unwrap().ino()
        );
        // The confinement test above covers a path link that escapes; here
        // the same shape sits in the last component of a deeper path.
        let dir = std::env::temp_dir().join(format!("pbps-magic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.join("a/out")).unwrap();
        let confined = std::fs::File::open(&dir).unwrap();
        let error = open_within(&confined, "a/out", rustix::fs::OFlags::empty()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

// A held proc inode can outlive its task. Linux reports ESRCH as well as
// ENOENT in that case. Permission and I/O failures still mean unreadable.
fn process_gone(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(3)
}

fn process_exited(directory: &File) -> Result<bool, UnqualifiedProcess> {
    let stat = match std::fs::read_to_string(proc_base(directory).join("stat")) {
        Ok(stat) => stat,
        Err(error) if process_gone(&error) => return Ok(true),
        Err(_) => return Err(UnqualifiedProcess),
    };
    exited_stat(&stat)
}

fn exited_stat(stat: &str) -> Result<bool, UnqualifiedProcess> {
    let (_, fields) = stat.rsplit_once(')').ok_or(UnqualifiedProcess)?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    let state = fields.first().ok_or(UnqualifiedProcess)?;
    let threads: u32 = fields
        .get(17)
        .ok_or(UnqualifiedProcess)?
        .parse()
        .map_err(|_| UnqualifiedProcess)?;
    // A dead leader can retain live threads, and /proc/PID/task can become
    // unavailable after pthread_exit. The same kernel stat record must show
    // that no thread of the group remains; absence of its task directory is
    // not proof of group exit (proc_pid_task(5), proc_pid_stat(5) field 20).
    //
    // **Zero is such a record, not a malformed one.** `aa7315d2` refused it,
    // and its own message says what it meant to admit — "a dead leader with
    // no surviving threads". Measured while fixing #674, walking a shell that
    // spawns and reaps children: a child caught mid-exit reports a complete
    // fifty-field `stat` with `state` `X` or `Z` and `num_threads` **0**,
    // 1,409 times. Refusing that is refusing the very case this admits, one
    // value further on, and it is what made `process_scope` refuse a whole
    // walk for a child that was simply finishing (DECISIONS 522). An
    // unreadable or unparseable count is still an error, because that is a
    // reading nobody made.
    Ok(matches!(*state, "Z" | "X") && threads <= 1)
}

pub(crate) fn observe_incidental<T>(
    pid: u32,
    directory: &File,
    inspect: impl FnOnce(ProcessLease) -> Result<T, UnqualifiedProcess>,
) -> Result<Option<T>, UnqualifiedProcess> {
    let result = (|| {
        let process = ProcessLease::capture(pid)?;
        if FileIdentity::of(directory).map_err(Reading::CaptureReplaced.named())?
            != FileIdentity::of(&process.directory).map_err(Reading::CaptureReplaced.named())?
        {
            return Err(Reading::CaptureReplaced.refuse());
        }
        inspect(process)
    })();
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            if !process_exited(directory).map_err(Reading::CaptureReplaced.named())? {
                return Err(error);
            }
            // Prove exit through the held proc inode, then exclude replacement
            // of its numeric PID. Never turn a live permission failure into
            // absence. Essential service/backend leases still must stay live.
            match open_process(pid) {
                Ok(current)
                    if FileIdentity::of(&current).map_err(Reading::CaptureReplaced.named())?
                        == FileIdentity::of(directory)
                            .map_err(Reading::CaptureReplaced.named())? => {}
                Err(error) if process_gone(&error) => (),
                Ok(_) | Err(_) => return Err(Reading::CaptureReplaced.refuse()),
            }
            Ok(None)
        }
    }
}

pub(crate) fn open_process(pid: u32) -> std::io::Result<File> {
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

pub(crate) fn process_scope(
    service: &ProcessLease,
) -> Result<Vec<(u32, File)>, UnqualifiedProcess> {
    // Socket qualification still uses observer coordinates until #743.
    let pid = service.observer_pid()?;
    let mut scope = vec![(
        pid,
        service
            .directory
            .try_clone()
            .map_err(Reading::Scope.named())?,
    )];
    let mut seen = BTreeSet::from([pid]);
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
            Err(_) => return Err(Reading::Scope.refuse()),
        };
        for task in tasks {
            let task = task.map_err(Reading::Scope.named())?;
            let text = match std::fs::read_to_string(task.path().join("children")) {
                Ok(text) if text.len() <= 65536 => text,
                Err(error) if process_gone(&error) => continue,
                Ok(_) | Err(_) => return Err(Reading::Scope.refuse()),
            };
            for pid in text.split_whitespace() {
                children.push(pid.parse::<u32>().map_err(Reading::Scope.named())?);
            }
        }
        for pid in children {
            if !seen.insert(pid) {
                continue;
            }
            // A bound against a pathological /proc, not a profile limit: it
            // must exceed the largest `pids.max` any profile admits (the
            // dedicated-server profile allows 2048) so that a runtime meeting
            // its named ceiling is never refused for reaching this instead —
            // `process_scope` counts processes, and threads do not add entries
            // here, so real engines stay far below it (finding on #640).
            if scope.len() >= 4096 {
                return Err(Reading::Scope.refuse());
            }
            let directory = match open_process(pid) {
                Ok(file) => file,
                Err(error) if process_gone(&error) => continue,
                Err(_) => return Err(Reading::Scope.refuse()),
            };
            let stat = match std::fs::read_to_string(proc_base(&directory).join("stat")) {
                Ok(stat) => stat,
                Err(error) if process_gone(&error) => continue,
                Err(_) => return Err(Reading::Scope.refuse()),
            };
            let ppid = stat
                .rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| Reading::Scope.refuse())?;
            // A pid was in a `children` list a moment ago and its own `stat`
            // now names a different parent. Two very different things look
            // like this, and refusing both is what made a valid deployment
            // intermittently refused (#674, DECISIONS 522).
            //
            // **Measured on this machine**, walking a shell that spawns and
            // reaps two children in a loop: 1,155,160 walks produced 2,771 of
            // these, and 2,769 of them were a process in state `X` or `Z` —
            // the child caught mid-exit, its `stat` already reparented to the
            // reaper while `/proc/<pid>` still answers.
            //
            // A process that is over is the one case that can be passed over
            // without asking anything else: it holds no descriptor, runs no
            // code and owns no socket, so no caller of this walk has a
            // question it could answer.
            //
            // **`exited_stat` and not a state test written here.** This file
            // already answers "is this process over", and it requires the
            // thread count as well as the state, because a dead leader can
            // retain live threads and a surviving thread can hold the socket
            // or have descendants of its own —
            // `a_zombie_leader_does_not_prove_that_its_other_threads_exited`
            // pins exactly that. A second, weaker answer to one question was
            // the first shape of this branch, and review caught it.
            //
            // **Anything not proved over refuses**, as it did before. Absence
            // from a parent's list is not proof either: a live descendant
            // reparented to a subreaper leaves the list while remaining in
            // the scope, and passing it over would hand
            // `PrivateChannelLease::check` and `check_kernel_parts` an
            // incomplete scan that reads as a complete one.
            if ppid != parent_pid {
                if exited_stat(&stat).map_err(Reading::Scope.named())? {
                    continue;
                }
                return Err(Reading::ScopeParent.refuse());
            }
            scope.push((pid, directory));
        }
        cursor += 1;
    }
    Ok(scope)
}

/// The tasks sharing one of a lease's namespaces, by kernel id.
///
/// Tasks, not processes. Linux keeps credentials, the seccomp and
/// no-new-privileges state, the namespaces and the descriptor table per task,
/// so a thread can differ from its group's leader — measured: SQL Server's
/// engine runs 116 of them. Scanning only `/proc`'s top level would observe
/// the leader and qualify the rest by association.
///
/// Ids rather than leases: a lease holds six descriptors, and holding one per
/// task would exhaust a process's file-descriptor limit on an engine like
/// that. Callers capture them one at a time.
pub(crate) fn namespace_task_ids(
    anchor: &ProcessLease,
    namespace: &str,
) -> Result<Vec<u32>, UnqualifiedProcess> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir("/proc").map_err(|_| UnqualifiedProcess)? {
        let entry = entry.map_err(|_| UnqualifiedProcess)?;
        let Some(group) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let directory = match open_process(group) {
            Ok(directory) => directory,
            Err(error) if process_gone(&error) => continue,
            Err(_) => return Err(UnqualifiedProcess),
        };
        let tasks = match std::fs::read_dir(proc_base(&directory).join("task")) {
            Ok(tasks) => tasks,
            Err(error) if process_gone(&error) => {
                // A leader can exit while its threads live, and the task
                // directory goes with it; the surviving tasks are then in no
                // listing at all (DECISIONS 497 measured the same shape).
                if process_exited(&directory)? {
                    continue;
                }
                // They cannot be enumerated, so the only sound answers are
                // "demonstrably not in this namespace" or a refusal. A thread
                // could in principle have entered it alone, which needs
                // privilege the provisioning boundary already assumes.
                match File::open(proc_base(&directory).join("ns").join(namespace)) {
                    Ok(handle) if !anchor.owns_namespace(namespace, &handle)? => continue,
                    Ok(_) | Err(_) => return Err(UnqualifiedProcess),
                }
            }
            Err(_) => return Err(UnqualifiedProcess),
        };
        for task in tasks {
            let task = task.map_err(|_| UnqualifiedProcess)?;
            let Some(id) = task
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            // Through the leader's held directory, so a reused group id
            // cannot answer for the tasks of another process.
            let handle = match File::open(task.path().join("ns").join(namespace)) {
                Ok(handle) => handle,
                Err(error) if process_gone(&error) => continue,
                Err(_) => return Err(UnqualifiedProcess),
            };
            if anchor.owns_namespace(namespace, &handle)? {
                ids.push(id);
            }
        }
    }
    if ids.is_empty() {
        // The anchor is in its own namespace, so an empty answer means the
        // scan saw nothing at all rather than that nothing is there.
        return Err(UnqualifiedProcess);
    }
    anchor.check()?;
    Ok(ids)
}

/// The tasks sharing a lease's network namespace whose PID namespace is none
/// of the given anchors'.
///
/// A capture-free census: it compares namespace identity rather than
/// qualifying each task, so a short-lived occupant — a forwarder's `cat` pipe
/// being reaped and respawned while it forwards — is skipped as it vanishes
/// rather than refused for having, at the instant of capture, no executable to
/// read. `for_each_occupant` cannot serve here for exactly that reason: it
/// captures a `ProcessLease` for every occupant, and a zombie between reap and
/// wait has none. The caller decides what an unaccounted task means.
pub(crate) fn foreign_network_tasks(
    net_anchor: &ProcessLease,
    pid_anchors: &[&ProcessLease],
) -> Result<Vec<u32>, UnqualifiedProcess> {
    let mut foreign = Vec::new();
    for id in namespace_task_ids(net_anchor, "net")? {
        let directory = match open_process(id) {
            Ok(directory) => directory,
            Err(error) if process_gone(&error) => continue,
            Err(_) => return Err(UnqualifiedProcess),
        };
        let handle = match File::open(proc_base(&directory).join("ns").join("pid")) {
            Ok(handle) => handle,
            Err(error) if process_gone(&error) => continue,
            Err(_) => return Err(UnqualifiedProcess),
        };
        let mut owned = false;
        for anchor in pid_anchors {
            if anchor.owns_namespace("pid", &handle)? {
                owned = true;
                break;
            }
        }
        if !owned {
            foreign.push(id);
        }
    }
    net_anchor.check()?;
    Ok(foreign)
}

/// Applies one check to every task sharing a lease's namespace, capturing
/// each in turn and letting it go before the next.
pub(crate) fn for_each_occupant(
    anchor: &ProcessLease,
    namespace: &str,
    mut inspect: impl FnMut(&ProcessLease) -> Result<(), UnqualifiedProcess>,
) -> Result<(), UnqualifiedProcess> {
    for id in namespace_task_ids(anchor, namespace)? {
        let directory = match open_process(id) {
            Ok(directory) => directory,
            Err(error) if process_gone(&error) => continue,
            Err(_) => return Err(UnqualifiedProcess),
        };
        if let Some(()) = observe_incidental(id, &directory, |lease| inspect(&lease))? {
            continue;
        }
    }
    anchor.check()
}

pub(crate) fn groups(process: &ProcessLease) -> Result<(Vec<u32>, Vec<u32>), UnqualifiedProcess> {
    let status = process.read_proc("status", 65536)?;
    let field = |name: &str| -> Result<Vec<u32>, UnqualifiedProcess> {
        let mut values = status.lines().filter_map(|line| line.strip_prefix(name));
        let value = values.next().ok_or(UnqualifiedProcess)?;
        if values.next().is_some() {
            return Err(UnqualifiedProcess);
        }
        value
            .split_whitespace()
            .map(|entry| entry.parse::<u32>().map_err(|_| UnqualifiedProcess))
            .collect()
    };
    let gids = field("Gid:")?;
    if gids.len() != 4 {
        return Err(UnqualifiedProcess);
    }
    let supplementary = field("Groups:")?;
    process.check()?;
    Ok((gids, supplementary))
}

fn socket_owner(
    service: &ProcessLease,
    local: SocketAddr,
    peer: SocketAddr,
) -> Result<(ProcessLease, u64), UnqualifiedProcess> {
    service.check()?;
    let self_net = File::open("/proc/self/ns/net").map_err(Reading::NetNamespace.named())?;
    let own_net = service
        .namespaces
        .iter()
        .find(|(name, _, _)| *name == "net")
        .ok_or_else(|| Reading::NetNamespace.refuse())?;
    if own_net.2 != FileIdentity::of(&self_net).map_err(Reading::NetNamespace.named())? {
        return Err(Reading::NetNamespace.refuse());
    }
    let inode = peer_inode(local, peer)?;
    let mut owners = socket_owners(service, inode)?;
    // The count itself is the diagnosis, so it is what gets reported: a
    // second holder and none at all are different accidents, and a walk that
    // saw a task appear or exit produces one or the other (#674).
    if owners.len() != 1 {
        return Err(Reading::OwnerCount(owners.len()).refuse());
    }
    service.check()?;
    if peer_inode(local, peer)? != inode {
        return Err(Reading::PeerInode.refuse());
    }
    Ok((owners.remove(0), inode))
}

pub(crate) fn socket_owners(
    service: &ProcessLease,
    inode: u64,
) -> Result<Vec<ProcessLease>, UnqualifiedProcess> {
    let expected = PathBuf::from(format!("socket:[{inode}]"));
    let mut owners = Vec::new();
    for (pid, directory) in process_scope(service)? {
        let entries = match std::fs::read_dir(proc_base(&directory).join("fd")) {
            Ok(entries) => entries,
            Err(error) if process_gone(&error) => continue,
            Err(_) => return Err(Reading::ScopeDescriptors.refuse()),
        };
        let mut owns_socket = false;
        for entry in entries {
            let entry = entry.map_err(Reading::ScopeDescriptors.named())?;
            match std::fs::read_link(entry.path()) {
                Ok(path) => owns_socket |= path == expected,
                Err(error) if process_gone(&error) => (),
                Err(_) => return Err(Reading::ScopeDescriptors.refuse()),
            }
        }
        if owns_socket && let Some(lease) = observe_incidental(pid, &directory, Ok)? {
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

/// Spawns a child and waits until it has actually `exec`ed the program named.
///
/// Between `fork` and `execve` the child still wears its **parent's**
/// executable, and this parent is a `cargo test` binary under
/// `target/debug/deps` — owned by the build user, not by root.
/// [`ProcessLease::capture`] refuses exactly that, and rightly: a
/// root-installed executable is the whole point of the check. So a test that
/// captures a child it has just spawned is asserting a state it has not yet
/// established, and is told `UnqualifiedProcess` about a process that is
/// perfectly qualified a microsecond later.
///
/// **Measured** on a 32-core machine, with the refusal instrumented to say
/// which of `capture`'s six legs fired: 2 refusals in 82,028 spawns, both
/// `exe rejected: path=…/target/debug/deps/pbps_cli-… uid=1000 mode=100755`.
/// One in forty thousand passes locally forever — 240 whole-module runs
/// pinned to one core against two CPU hogs never showed it — and is common
/// enough on a two-core CI runner running the whole workspace's tests to have
/// turned `master` red on four different tests (issue #638 collects the runs).
///
/// Waiting on `/proc/PID/exe` rather than on a sleep: the window is the exec
/// itself, so the thing to wait for is the exec, and a duration long enough to
/// be safe on the slowest runner would be paid by every run on every machine.
///
/// **Production never meets this window.** Every non-test caller of `capture`
/// takes its pid from a connection, a daemon handshake or the catalog — never
/// from a process it has just spawned — so this is a defect in what the tests
/// establish, not in what the resolver checks.
#[cfg(test)]
pub(crate) fn spawned_and_execed(
    command: &mut std::process::Command,
    program: &str,
) -> std::process::Child {
    let child = command
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {program}: {error}"));
    wait_for_exec(child.id(), program);
    child
}

/// The waiting half of [`spawned_and_execed`], for the one fixture that has to
/// build its own child with a retry loop of its own.
#[cfg(test)]
pub(crate) fn wait_for_exec(pid: u32, program: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    // A `read_link` that fails means the child is already gone, which is a
    // different problem and the caller's to report: stop waiting and let its
    // own assertions say so.
    while std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|path| !path.ends_with(program))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "pid {pid} never replaced this test binary with {program} in /proc/PID/exe"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Command, Stdio};

    /// [`spawned_and_execed`] returns only once `/proc/PID/exe` names the
    /// program asked for, which is what every capture below depends on.
    ///
    /// The fork/exec window it exists to close is a microsecond wide and fires
    /// about once in forty thousand spawns, so a test that merely spawned and
    /// captured in a loop would pass without the fix. This makes the same
    /// transition wide enough to assert on instead: `bash` execs `sleep`, so
    /// `/proc/PID/exe` is observably `bash` first and `sleep` after, and a
    /// helper that did not wait would be handing back a child wearing the
    /// wrong executable almost every run.
    #[test]
    fn waiting_for_an_exec_returns_only_once_the_named_program_is_running() {
        let mut child = spawned_and_execed(
            Command::new("/bin/bash")
                .args(["-c", "exec /usr/bin/sleep 30"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            "sleep",
        );
        let executable = std::fs::read_link(format!("/proc/{}/exe", child.id())).unwrap();
        assert!(
            executable.ends_with("sleep"),
            "returned while the child was still {}",
            executable.display()
        );
        // And the state the other tests in this module go on to assert: a
        // child handed back by the helper is one `capture` qualifies.
        ProcessLease::capture(child.id())
            .expect("a root-installed executable is observable once the exec has happened");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// A refusal names the read that made it, and the name survives the
    /// collapse to `UnqualifiedProcess`.
    ///
    /// Three occurrences of #674 arrived as one sentence each and none of them
    /// said which read moved, which is why a third round of the CI matrix was
    /// needed to learn what a line of log should have said. The caller's type
    /// is deliberately unchanged — widening it is #646 — so what this asserts
    /// is that the name is recorded before the value is thrown away.
    #[test]
    fn a_refused_reading_names_itself_before_it_becomes_one_value() {
        // A read whose refusal needs no process at all: this pair is not
        // loopback, so the socket table is never consulted.
        LAST_READING.with(|cell| cell.set(None));
        assert!(
            peer_inode(
                "93.184.216.34:5432".parse().unwrap(),
                "93.184.216.34:6000".parse().unwrap(),
            )
            .is_err()
        );
        assert_eq!(last_reading(), Some(Reading::PeerEndpoints));

        // And a lease whose process is gone: the held proc directory outlives
        // the process, so the read that refuses is the one that asks what it
        // is — not a later one that would have asked what it runs.
        let mut child = spawned_and_execed(Command::new("/usr/bin/sleep").arg("30"), "sleep");
        let lease = ProcessLease::capture(child.id()).unwrap();
        lease.check().expect("a live child is qualified");
        child.kill().unwrap();
        child.wait().unwrap();
        LAST_READING.with(|cell| cell.set(None));
        assert!(lease.check().is_err(), "a reaped process is not this lease");
        assert_eq!(last_reading(), Some(Reading::StartTicks));
    }

    /// Establishing a process is named apart from re-reading one.
    ///
    /// `socket_owners` recaptures every holder it finds through
    /// `observe_incidental`, so a `capture` that refuses on a **live** process
    /// propagates straight out of the socket-owner walk. Left unnamed, that is
    /// the one way a refusal on the path #674 lives on could still reach a CI
    /// log carrying nothing but the outer step — found in review of this
    /// change, which had named only the re-reads.
    ///
    /// `CaptureUnprotected` earns its own name rather than sharing
    /// `CaptureExecutable`: it is the rule a lease is built on and not a read
    /// that moved, and it is the answer #650 is looking for.
    #[test]
    fn establishing_a_process_is_named_apart_from_re_reading_one() {
        // An executable a lease may not be built on. World-writable rather
        // than non-root-owned, because the uid half of that rule is the build
        // user's and a suite built as root would find its own binary
        // acceptable — the mode half refuses for either of them.
        let (mut writable, path) = spawned_writable_executable();
        LAST_READING.with(|cell| cell.set(None));
        let refused = ProcessLease::capture(writable.id());
        let reading = last_reading();
        let cleanup = writable.kill();
        writable.wait().unwrap();
        std::fs::remove_file(path).unwrap();
        cleanup.unwrap();
        assert!(refused.is_err());
        assert_eq!(reading, Some(Reading::CaptureUnprotected));

        // And no process at all to open.
        LAST_READING.with(|cell| cell.set(None));
        assert!(ProcessLease::capture(0).is_err());
        assert_eq!(last_reading(), Some(Reading::CaptureOpen));

        // The recapture inside the walk carries the name out rather than
        // flattening it: a live process whose capture refuses is an error
        // `observe_incidental` propagates, not an absence it reports.
        let mut child = spawned_and_execed(Command::new("/usr/bin/sleep").arg("30"), "sleep");
        let directory = open_process(child.id()).unwrap();
        LAST_READING.with(|cell| cell.set(None));
        let walked = observe_incidental(child.id(), &directory, Ok);
        assert!(walked.is_ok(), "a live, root-installed child is capturable");
        assert_eq!(last_reading(), None, "nothing refused");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// A child leaving the tree while the tree is being walked is not a
    /// reason to refuse the walk.
    ///
    /// This is #674's liveness window, and it is the one the walk meets in
    /// production: PostgreSQL forks a backend per connection and SQL Server's
    /// engine runs 116 tasks, so the service's own process tree churns
    /// exactly like this fixture. `socket_owners` walks that tree for every
    /// `SocketOwnerLease::check`, so a refusal here reached the operator as
    /// "the target this run was aimed at changed" — a valid deployment
    /// intermittently refused.
    ///
    /// **Measured before the fix**: 785,426 walks of this fixture produced
    /// 1,055 refusals, every one of them `scope-parent`. Classified over
    /// 1,155,160 walks, all 2,771 occurrences had the pid **no longer listed**
    /// as a child by the time it was asked again, and 2,761 of those were in
    /// state `X`, the child caught mid-exit with its `stat` already
    /// reparented. None was still listed and claiming another parent, which
    /// is the inconsistency the check exists for and the one that still
    /// refuses.
    ///
    /// The budget is time rather than iterations because the rate is what
    /// matters: unfixed, this window opened roughly once per 750 walks, and
    /// this machine walks it tens of thousands of times a second.
    ///
    /// **Zero is deliberately not the bar, and the residual is not a bug.**
    /// Measured across five runs after the fix: 1 or 2 refusals per ~200,000
    /// walks, against 267 before it. Those are a pid **reused** between the
    /// `children` read and the `stat` read by a process outside the tree —
    /// indistinguishable from a live descendant that reparented, without
    /// comparing the opened process's `starttime` against the moment the list
    /// was read, which needs a clock-tick conversion this crate's `rustix`
    /// features do not carry. Refusing is the fail-closed answer to that
    /// ambiguity, and at one walk in a hundred thousand — on a fixture that
    /// spawns forty thousand processes a second, which no engine does — it is
    /// not what made CI intermittent. Closing it is #729.
    #[test]
    fn a_child_leaving_the_tree_does_not_refuse_the_walk() {
        let mut tree = spawned_and_execed(
            Command::new("/bin/bash")
                .args([
                    "-c",
                    "exec /bin/bash -c 'while :; do /usr/bin/true & /usr/bin/true & wait; done'",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            "bash",
        );
        // `/bin/bash` is root-installed, so a lease can be built on it without
        // the root fixture the resolver matrix needs.
        let lease = ProcessLease::capture(tree.id()).expect("a root-installed shell");

        let mut walks = 0usize;
        let mut refused: std::collections::BTreeMap<String, usize> = Default::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            walks += 1;
            LAST_READING.with(|cell| cell.set(None));
            if process_scope(&lease).is_err() {
                let name = last_reading()
                    .map(|reading| reading.to_string())
                    .unwrap_or_else(|| "unnamed".to_owned());
                *refused.entry(name).or_default() += 1;
            }
        }
        tree.kill().unwrap();
        tree.wait().unwrap();

        assert!(
            walks > 10_000,
            "too few walks to say anything: {walks} in five seconds"
        );
        let refusals: usize = refused.values().sum();
        assert!(
            refusals * 20_000 < walks,
            "{refusals} refusals in {walks} walks is the rate this fixture had before the \
             departing child was passed over (267 in 206,771), not after it (1 or 2 in \
             200,000): {refused:?}"
        );
    }

    /// One established pair read twice is one socket, and the negative cases
    /// around that.
    ///
    /// Iterating `/proc/net/tcp` is not a snapshot. Measured against a
    /// loopback pair held open while four threads churned 7.3 million
    /// connections, **1,413 of 43,311 reads returned the same pair twice**,
    /// every one carrying the same inode, and refusing them was 14% of reads
    /// under that load. Two *different* inodes for one four-tuple was never
    /// seen, and is the one shape that is the table contradicting itself
    /// (#674).
    #[test]
    fn one_established_pair_read_twice_is_still_one_socket() {
        // `local` and `peer` as the table spells them; the columns are
        // `sl local rem st ... inode`.
        let row = |l: &str, r: &str, state: &str, inode: &str| {
            format!("   0: {l} {r} {state} 00000000:00000000 00:00000000 00000000 1000 0 {inode} 1")
        };
        let table = |rows: &[String]| {
            let mut text = String::from(
                "  sl  local_address rem_address   st ...
",
            );
            for row in rows {
                text.push_str(row);
                text.push('\n');
            }
            text
        };
        let client = row("0100007F:1538", "0100007F:81E4", "01", "4242");
        let server = row("0100007F:81E4", "0100007F:1538", "01", "9001");

        // The ordinary read.
        assert_eq!(
            server_inode(
                &table(&[client.clone(), server.clone()]),
                "0100007F:1538",
                "0100007F:81E4"
            ),
            Ok(9001)
        );
        // The same pair emitted twice by one walk of the hash buckets.
        assert_eq!(
            server_inode(
                &table(&[client.clone(), server.clone(), server.clone()]),
                "0100007F:1538",
                "0100007F:81E4"
            ),
            Ok(9001),
            "a repeated row is one socket seen twice"
        );
        // Two inodes for one four-tuple is the table contradicting itself.
        let other = row("0100007F:81E4", "0100007F:1538", "01", "9002");
        assert_eq!(
            server_inode(
                &table(&[client.clone(), server.clone(), other]),
                "0100007F:1538",
                "0100007F:81E4"
            ),
            Err(Reading::PeerServerDuplicated)
        );
        // The engine's end missing, which is how CI named this read.
        assert_eq!(
            server_inode(
                &table(std::slice::from_ref(&client)),
                "0100007F:1538",
                "0100007F:81E4"
            ),
            Err(Reading::PeerServerAbsent)
        );
        // Our own end missing.
        assert_eq!(
            server_inode(
                &table(std::slice::from_ref(&server)),
                "0100007F:1538",
                "0100007F:81E4"
            ),
            Err(Reading::PeerClientAbsent)
        );
        // A pair that is there but not established is not this connection,
        // and it says so with the state rather than reading as no row at all.
        // The state is the **engine's**, so `08` (`CLOSE_WAIT`) is the engine
        // holding our FIN — this end closed first — and `06` (`TIME_WAIT`) is
        // the engine having closed. Measured: closing this end put `05` on it
        // and `08` on the engine's, and closing the engine's swapped them, so
        // the two are not interchangeable and naming the wrong end would send
        // the next investigation to the wrong process (issue 674).
        let closing = row("0100007F:81E4", "0100007F:1538", "08", "9001");
        assert_eq!(
            server_inode(
                &table(&[client.clone(), closing]),
                "0100007F:1538",
                "0100007F:81E4"
            ),
            Err(Reading::PeerServerState(8))
        );
        let waiting = row("0100007F:81E4", "0100007F:1538", "06", "9001");
        assert_eq!(
            server_inode(
                &table(&[client.clone(), waiting]),
                "0100007F:1538",
                "0100007F:81E4"
            ),
            Err(Reading::PeerServerState(6))
        );
        assert_eq!(
            Reading::PeerServerState(6).to_string(),
            "peer-server-state(06)"
        );
        // An inode of zero is no socket at all, never a silent skip.
        let zero = row("0100007F:81E4", "0100007F:1538", "01", "0");
        assert_eq!(
            server_inode(
                &table(&[client.clone(), zero]),
                "0100007F:1538",
                "0100007F:81E4"
            ),
            Err(Reading::PeerServerAbsent)
        );
        // A state nobody could read is the table, not the connection: it
        // must not arrive as "no row for the pair", and it must not leave an
        // earlier row's state standing in for it.
        let unreadable = row("0100007F:81E4", "0100007F:1538", "zz", "9001");
        assert_eq!(
            server_inode(
                &table(&[client.clone(), unreadable.clone()]),
                "0100007F:1538",
                "0100007F:81E4"
            ),
            Err(Reading::PeerTable)
        );
        let waiting_then_unreadable = row("0100007F:81E4", "0100007F:1538", "06", "9001");
        assert_eq!(
            server_inode(
                &table(&[client.clone(), waiting_then_unreadable, unreadable]),
                "0100007F:1538",
                "0100007F:81E4"
            ),
            Err(Reading::PeerTable),
            "an earlier row's state does not answer for one that cannot be read"
        );
        // A row that is not a row, and a table with no header.
        assert_eq!(
            server_inode("header\nshort row\n", "a", "b"),
            Err(Reading::PeerTable)
        );
        assert_eq!(server_inode("", "a", "b"), Err(Reading::PeerTable));
    }

    /// Which end closed first, as the **engine's** row spells it.
    ///
    /// `PeerServerState` carries that row's `st` column so a refusal can say
    /// more than "not established", and the whole value of it is the
    /// direction — which is the easy thing to get backwards. The first draft
    /// of this change did, calling `08` on the engine's row "the engine
    /// having closed its end", which would send the next investigation to the
    /// wrong process.
    ///
    /// So it is measured here rather than recited: a loopback pair, closed
    /// from one end and then from the other, read out of the same table the
    /// resolver reads. No root, no container — this is `127.0.0.1` and
    /// `/proc/self/net/tcp`.
    #[test]
    fn the_engines_row_says_which_end_closed_first() {
        use std::net::{TcpListener, TcpStream};

        // The engine's row is the one whose local address is our peer.
        let engine_row = |local: &str, peer: &str| -> Option<String> {
            let text = read_bounded(Path::new("/proc/self/net/tcp"), 32 * 1024 * 1024).ok()?;
            text.lines().skip(1).find_map(|line| {
                let f: Vec<_> = line.split_whitespace().collect();
                (f.len() >= 10 && f[1] == peer && f[2] == local).then(|| f[3].to_owned())
            })
        };
        // A close is not instant on the wire, so wait for the row to leave
        // `01` rather than sleeping a guessed interval.
        let settled = |local: &str, peer: &str| -> String {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match engine_row(local, peer) {
                    Some(state) if state != "01" => return state,
                    _ if std::time::Instant::now() >= deadline => {
                        return engine_row(local, peer).unwrap_or_else(|| "absent".to_owned());
                    }
                    _ => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // This verifier is the client. Close its end first.
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        let local = encoded(client.local_addr().unwrap());
        let peer = encoded(client.peer_addr().unwrap());
        drop(client);
        let ours_first = settled(&local, &peer);
        drop(server);

        // And the other way round: the engine's end closes first.
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        let local = encoded(client.local_addr().unwrap());
        let peer = encoded(client.peer_addr().unwrap());
        drop(server);
        let theirs_first = settled(&local, &peer);
        drop(client);

        assert_eq!(
            ours_first, "08",
            "this end closing first leaves the engine's row holding our FIN, \
             in CLOSE_WAIT"
        );
        // `04` or `05`: the engine has sent its FIN, and whether this read
        // catches it before or after our ACK is a race no assertion should
        // pin. Both say the same thing — that end closed first.
        assert!(
            theirs_first == "04" || theirs_first == "05",
            "the engine closing first leaves its own row in FIN_WAIT1 or \
             FIN_WAIT2, not `{theirs_first}`"
        );
        assert_ne!(
            ours_first, theirs_first,
            "the two directions must not render the same, or the state says \
             nothing about who closed"
        );
    }

    /// The count is part of the name where the refusal *is* a count.
    ///
    /// `owner-count(0)` and `owner-count(2)` are different accidents — a walk
    /// that saw the holder exit, and one that saw a second thread group hold
    /// the same descriptor — and a bare "the owner could not be established"
    /// distinguishes neither (#674).
    #[test]
    fn a_counted_refusal_reports_the_count_it_counted() {
        assert_eq!(Reading::OwnerCount(0).to_string(), "owner-count(0)");
        assert_eq!(Reading::OwnerCount(2).to_string(), "owner-count(2)");
        assert_ne!(Reading::OwnerCount(0), Reading::OwnerCount(2));
        // Every other reading is a plain name, and no two share one.
        let names = [
            Reading::StartTicks,
            Reading::Executable,
            Reading::Namespace,
            Reading::Connection,
            Reading::SocketInode,
            Reading::SocketOwner,
            Reading::NetNamespace,
            Reading::PeerInode,
            Reading::Scope,
            Reading::ScopeParent,
            Reading::ScopeDescriptors,
            Reading::CaptureOpen,
            Reading::CaptureStartTicks,
            Reading::CaptureStatus,
            Reading::CaptureExecutable,
            Reading::CaptureUnprotected,
            Reading::CaptureNamespace,
            Reading::CaptureReplaced,
            Reading::PeerEndpoints,
            Reading::PeerTable,
            Reading::PeerClientAbsent,
            Reading::PeerServerAbsent,
            Reading::PeerServerDuplicated,
            Reading::PeerServerState(6),
        ]
        .map(|reading| reading.to_string());
        let distinct: BTreeSet<&String> = names.iter().collect();
        assert_eq!(distinct.len(), names.len(), "{names:?}");
    }

    #[test]
    fn incidental_exit_during_inspection_does_not_hide_live_failures_or_pid_replacement() {
        let mut owner = spawned_and_execed(Command::new("/usr/bin/sleep").arg("30"), "sleep");
        let mut child = spawned_and_execed(Command::new("/usr/bin/sleep").arg("30"), "sleep");
        let owner_directory = open_process(owner.id()).unwrap();
        let directory = open_process(child.id()).unwrap();
        let owner_lease = ProcessLease::capture(owner.id()).unwrap();
        let live = observe_incidental(child.id(), &directory, |lease| lease.check());
        let unreadable = observe_incidental(owner.id(), &owner_directory, |_| {
            Err::<(), _>(UnqualifiedProcess)
        });
        let exited = observe_incidental(child.id(), &directory, |lease| {
            child.kill().unwrap();
            child.wait().unwrap();
            lease.check()
        });
        let reaped = observe_incidental(child.id(), &directory, |lease| lease.check());
        // Pairing an old held inode with another live numeric PID simulates
        // the identity mismatch that reuse produces, without exhausting PIDs.
        let replaced = observe_incidental(owner.id(), &directory, |lease| lease.check());
        let owner_still_live = owner_lease.check();
        owner.kill().unwrap();
        owner.wait().unwrap();
        assert!(matches!(live, Ok(Some(()))));
        assert!(unreadable.is_err(), "unreadable live state is not absence");
        assert!(
            matches!(exited, Ok(None)),
            "exit during inspection is benign"
        );
        assert!(
            matches!(reaped, Ok(None)),
            "an already reaped child is benign"
        );
        assert!(
            replaced.is_err(),
            "PID substitution must still refuse the scan"
        );
        assert!(owner_still_live.is_ok());
    }

    #[test]
    fn an_unreaped_incidental_child_is_skipped_only_after_all_tasks_exit() {
        let mut child = spawned_and_execed(Command::new("/usr/bin/sleep").arg("30"), "sleep");
        let directory = open_process(child.id()).unwrap();
        child.kill().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !process_exited(&directory).unwrap() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let exited = observe_incidental(child.id(), &directory, |lease| lease.check());
        child.wait().unwrap();
        assert!(matches!(exited, Ok(None)));
    }

    #[test]
    fn a_zombie_leader_does_not_prove_that_its_other_threads_exited() {
        // Linux reports Z with num_threads > 1 after a leader calls
        // pthread_exit while a worker remains alive. comm may contain ')'.
        let stat = |state: &str, threads: &str| {
            format!(
                "1 (fixture) name) {state} {} {threads}",
                ["0"; 16].join(" ")
            )
        };
        assert!(!exited_stat(&stat("Z", "2")).unwrap());
        assert!(!exited_stat(&stat("S", "1")).unwrap());
        assert!(exited_stat(&stat("Z", "1")).unwrap());
        assert!(exited_stat(&stat("X", "1")).unwrap());
        // And none at all, which is what a child caught mid-exit actually
        // reports — measured 1,409 times while fixing #674. This assertion
        // read `is_err()` until then, which refused the very case this
        // function's own commit message set out to admit, "a dead leader with
        // no surviving threads".
        assert!(exited_stat(&stat("Z", "0")).unwrap());
        assert!(exited_stat(&stat("X", "0")).unwrap());
        // A live state is still live at any count.
        assert!(!exited_stat(&stat("S", "0")).unwrap());
        // A count nobody could read stays an error, which is a different
        // thing from a count of none.
        assert!(exited_stat(&stat("Z", "unknown")).is_err());
        assert!(exited_stat("unreadable").is_err());
    }

    #[test]
    fn a_process_lease_expires_even_while_its_proc_directory_is_held() {
        let mut child = spawned_and_execed(
            Command::new("/usr/bin/sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            "sleep",
        );
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
        let (mut child, path) = spawned_writable_executable();
        let lease = ProcessLease::capture(child.id());
        let cleanup = child.kill();
        child.wait().unwrap();
        std::fs::remove_file(path).unwrap();
        cleanup.unwrap();
        assert!(lease.is_err());
    }

    /// A running process whose executable is world-writable, and therefore one
    /// no lease may be built on whoever owns it.
    ///
    /// The **mode** is what makes this fixture portable, and the uid is not: a
    /// suite built and run as root has a root-owned test binary that
    /// `capture` accepts, so a test that reached for the test binary's own pid
    /// to provoke this refusal would pass for the build user and fail for a
    /// root builder (found in review of #674). `0o777` is refused by
    /// `mode & 0o022` for either of them.
    fn spawned_writable_executable() -> (std::process::Child, PathBuf) {
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
        let child = loop {
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
        // The same exec window as everywhere else, and here it would let a
        // caller pass for the wrong reason: a pre-exec capture is refused
        // because the test binary is not root-installed, which is a different
        // refusal — and on a root builder it would not be refused at all.
        wait_for_exec(child.id(), path.file_name().unwrap().to_str().unwrap());
        (child, path)
    }

    #[test]
    fn exec_in_the_same_process_invalidates_its_previous_lease() {
        use std::io::Write as _;
        let mut child = spawned_and_execed(
            Command::new("/bin/bash")
                .args(["-c", "read -r line; exec /usr/bin/sleep 30"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            "bash",
        );
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
