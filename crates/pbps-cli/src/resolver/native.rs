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
mod private_channel;
mod target;
pub(crate) use daemon::DaemonLease;
pub(crate) use execution::{
    BoundedResourceLease, ExecutionLease, ExecutionProfile, MountEntry, ResourceCeilings,
    cgroup_relative, mount_rows,
};
pub(crate) use private_channel::{
    PrivateChannelLease, PrivateChannelProfile, awaiting_engine, guard, private_network, security,
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
    /// `/proc/self/net/tcp`: unreadable, malformed, or no longer carrying the
    /// established pair this lease was captured over.
    PeerInode,
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
            Self::OwnerCount(found) => write!(f, "owner-count({found})"),
            Self::Scope => f.write_str("scope"),
            Self::ScopeParent => f.write_str("scope-parent"),
            Self::ScopeDescriptors => f.write_str("scope-descriptors"),
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
        // `ipc` is here for the dedicated-server profile's occupant accounting:
        // a container sharing the engine's IPC namespace reaches its shared
        // memory. The Docker profile only ever asks about pid/mnt/net/user, so
        // this is additive — `same_process` and `check` iterate whatever was
        // captured, and no caller assumes the set's size.
        for name in ["pid", "mnt", "net", "user", "ipc"] {
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
        return Err(Reading::PeerInode.refuse());
    }
    let table = if local.is_ipv4() {
        "/proc/self/net/tcp"
    } else {
        "/proc/self/net/tcp6"
    };
    let text =
        read_bounded(Path::new(table), 32 * 1024 * 1024).map_err(Reading::PeerInode.named())?;
    let mut lines = text.lines();
    lines.next().ok_or_else(|| Reading::PeerInode.refuse())?;
    let local = encoded(local);
    let peer = encoded(peer);
    let mut found = None;
    let mut client_present = false;
    for line in lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 {
            return Err(Reading::PeerInode.refuse());
        }
        if fields[3] != "01" {
            continue;
        }
        if fields[1] == local && fields[2] == peer && fields[9] != "0" {
            client_present = true;
        }
        if fields[1] == peer && fields[2] == local {
            let inode = fields[9]
                .parse::<u64>()
                .map_err(Reading::PeerInode.named())?;
            if inode == 0 || found.replace(inode).is_some() {
                return Err(Reading::PeerInode.refuse());
            }
        }
    }
    if !client_present {
        return Err(Reading::PeerInode.refuse());
    }
    found.ok_or_else(|| Reading::PeerInode.refuse())
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
    if threads == 0 {
        return Err(UnqualifiedProcess);
    }
    // A dead leader can retain live threads, and /proc/PID/task can become
    // unavailable after pthread_exit. The same kernel stat record must show
    // that only the dead leader remains; absence of its task directory is not
    // proof of group exit (proc_pid_task(5), proc_pid_stat(5) field 20).
    Ok(matches!(*state, "Z" | "X") && threads == 1)
}

pub(crate) fn observe_incidental<T>(
    pid: u32,
    directory: &File,
    inspect: impl FnOnce(ProcessLease) -> Result<T, UnqualifiedProcess>,
) -> Result<Option<T>, UnqualifiedProcess> {
    let result = (|| {
        let process = ProcessLease::capture(pid)?;
        if FileIdentity::of(directory)? != FileIdentity::of(&process.directory)? {
            return Err(UnqualifiedProcess);
        }
        inspect(process)
    })();
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            if !process_exited(directory)? {
                return Err(error);
            }
            // Prove exit through the held proc inode, then exclude replacement
            // of its numeric PID. Never turn a live permission failure into
            // absence. Essential service/backend leases still must stay live.
            match open_process(pid) {
                Ok(current) if FileIdentity::of(&current)? == FileIdentity::of(directory)? => (),
                Err(error) if process_gone(&error) => (),
                Ok(_) | Err(_) => return Err(UnqualifiedProcess),
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
                .and_then(|s| s.parse::<u32>().ok())
                .ok_or_else(|| Reading::Scope.refuse())?;
            // Named apart from the reads around it: this is the one refusal
            // in the walk that a process merely coming and going can produce
            // without any read failing — the pid was in a `children` list a
            // moment ago and its own `stat` now names a different parent, by
            // reparenting or by numeric reuse (#674).
            if ppid != parent_pid {
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
        assert_eq!(last_reading(), Some(Reading::PeerInode));

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
        assert!(exited_stat(&stat("Z", "unknown")).is_err());
        assert!(exited_stat(&stat("Z", "0")).is_err());
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
        // The same exec window as everywhere else, and here it would let the
        // test pass for the wrong reason: a pre-exec capture is refused
        // because this test binary is not root-installed, which is not the
        // refusal this test is about.
        wait_for_exec(child.id(), path.file_name().unwrap().to_str().unwrap());
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
