//! Observations through a held procfs view of one PID namespace.
//!
//! Parent-child links are not an inventory: a parent's exit can move a live
//! child behind a walk's cursor (#730). Procfs supplies namespace membership
//! independently of those links. It still supplies sequential observations,
//! not an atomic snapshot, an origin claim, or continuous containment.
//! DECISIONS 528 records the view, coordinate and lifetime contract.

use super::{
    FileIdentity, ProcSource, ProcessLease, UnqualifiedProcess, exited_stat, process_gone,
};
use rustix::fs::{Mode, OFlags, ResolveFlags};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::Arc;

mod sockets;
pub(crate) use sockets::{belongs_to_service, observed_socket_holders};

/// Why an observation cannot be used. This API distinguishes an unsupported
/// view from an unreadable live task and a replaced identity; it does not
/// change the older native qualification error surface (#646).
#[derive(Debug, thiserror::Error)]
pub enum NamespaceError {
    #[error("the selected process is no longer qualified")]
    Anchor,
    #[error("the procfs view has unknown provenance or hides namespace members")]
    View,
    #[error("a pinned procfs view or task identity changed")]
    Replaced,
    #[error("live task or namespace metadata is unreadable")]
    Unreadable,
    #[error("kernel task metadata is malformed")]
    Metadata,
}

impl From<UnqualifiedProcess> for NamespaceError {
    fn from(_: UnqualifiedProcess) -> Self {
        Self::Unreadable
    }
}

impl From<NamespaceError> for UnqualifiedProcess {
    fn from(error: NamespaceError) -> Self {
        #[cfg(test)]
        eprintln!("native namespace observation refused: {error}");
        let _ = error;
        Self
    }
}

/// A task number in a particular PID namespace, never an observer PID.
/// Keeping the namespace handle alive prevents its inode from being reused
/// while an observation carries this coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct NamespaceTaskId {
    namespace: FileIdentity,
    number: u32,
}

impl NamespaceTaskId {
    /// The namespace-local number. It must not be passed to host `/proc`.
    pub fn number(self) -> u32 {
        self.number
    }
}

/// A successful read and a proven exit are different outcomes. Permission,
/// malformed metadata and replacement failures remain errors.
pub enum TaskReading {
    Live(String),
    Exited,
}

/// One held task directory. Its coordinate is not its identity: a later task
/// can reuse the coordinate, but cannot substitute for this open directory.
pub struct TaskObservation {
    directory: File,
    namespace: Arc<File>,
    id: NamespaceTaskId,
    group: NamespaceTaskId,
    start_ticks: u64,
}

impl TaskObservation {
    pub fn id(&self) -> NamespaceTaskId {
        self.id
    }

    /// The group's leader coordinate, even if that leader has exited.
    pub fn group(&self) -> NamespaceTaskId {
        self.group
    }

    /// Whether these held entries identify the same task in the same procfs
    /// instance. Different views require explicit coordinate mapping; their
    /// directory device/inode pairs are not interchangeable.
    pub fn same_task(&self, other: &Self) -> Result<bool, NamespaceError> {
        Ok(self.id == other.id
            && self.start_ticks == other.start_ticks
            && FileIdentity::of(&self.directory)? == FileIdentity::of(&other.directory)?)
    }

    /// Read this task's credentials, not its leader's credentials. The held
    /// inode and matching kernel coordinate bracket the reading. A task can
    /// still change credentials after this returns; launch controls must
    /// enforce whichever restrictions the caller needs between readings.
    pub fn status(&self) -> Result<TaskReading, NamespaceError> {
        if FileIdentity::of(&self.namespace)? != self.id.namespace {
            return Err(NamespaceError::Replaced);
        }
        if !task_alive(&self.directory, self.id.number, self.start_ticks)? {
            return Ok(TaskReading::Exited);
        }
        let status = match read(&self.directory, "status") {
            Ok(status) => status,
            Err(_) if !task_alive(&self.directory, self.id.number, self.start_ticks)? => {
                return Ok(TaskReading::Exited);
            }
            Err(error) => return Err(error),
        };
        if status_id(&status, "Pid:")? != self.id.number
            || status_id(&status, "Tgid:")? != self.group.number
        {
            return Err(NamespaceError::Replaced);
        }
        if !task_alive(&self.directory, self.id.number, self.start_ticks)? {
            return Ok(TaskReading::Exited);
        }
        Ok(TaskReading::Live(status))
    }
}

/// A pinned procfs view belonging to the selected process's PID namespace.
///
/// The anchor remains borrowed and checked for the view's entire lifetime.
/// Looking through its root avoids entering its namespaces or starting an
/// observer inside the workload. Local task numbers are never translated by
/// scanning the host. Visible child PID namespaces are included; a consumer
/// requiring exact namespace equality must separately check each task.
pub struct NamespaceProcfs<'a> {
    anchor: &'a ProcessLease,
    directory: Arc<File>,
    namespace: Arc<File>,
    identity: FileIdentity,
    mount: u64,
}

impl<'a> NamespaceProcfs<'a> {
    pub fn capture(anchor: &'a ProcessLease) -> Result<Self, NamespaceError> {
        anchor.check().map_err(|_| NamespaceError::Anchor)?;
        let directory = anchor.open_in_root("proc")?;
        if rustix::fs::fstatfs(&directory)
            .map_err(|_| NamespaceError::Unreadable)?
            .f_type
            != rustix::fs::PROC_SUPER_MAGIC
        {
            return Err(NamespaceError::View);
        }
        let namespace = namespace_init(&directory)?;
        if !anchor.owns_namespace("pid", &namespace)? {
            return Err(NamespaceError::View);
        }
        let view = Self {
            identity: FileIdentity::of(&directory)?,
            mount: mount_id(&directory)?,
            directory: Arc::new(directory),
            namespace: Arc::new(namespace),
            anchor,
        };
        view.check()?;
        Ok(view)
    }

    /// Verify the selected view is still mounted at the qualified location.
    /// A hidden process is not an absent process: hidepid views are refused,
    /// even when this observer happens to have sufficient access today.
    pub fn check(&self) -> Result<(), NamespaceError> {
        self.anchor.check().map_err(|_| NamespaceError::Anchor)?;
        let current = self.anchor.open_in_root("proc")?;
        if FileIdentity::of(&current)? != self.identity
            || mount_id(&current)? != self.mount
            || !self
                .anchor
                .owns_namespace("pid", &namespace_init(&self.directory)?)?
        {
            return Err(NamespaceError::Replaced);
        }
        visible_mount(
            &self.anchor.read_proc("mountinfo", 1024 * 1024)?,
            self.mount,
        )?;
        self.anchor.check().map_err(|_| NamespaceError::Anchor)
    }

    /// Observe tasks visible in this view without following `children`.
    ///
    /// Entries which disappear before they can be opened are reported by
    /// the kernel as absent at that lookup. Held entries are discarded only
    /// after proving exit through their held inode. A live group's unreadable
    /// task directory refuses the observation. Success does not assert that
    /// no task could have appeared after its directory position was passed.
    pub fn observe(&self) -> Result<Vec<TaskObservation>, NamespaceError> {
        let mut observations = Vec::new();
        self.visit::<NamespaceError>(|task| {
            observations.push(task);
            Ok(())
        })?;
        Ok(observations)
    }

    // Production consumes each entry before opening the next. Collecting
    // every held directory would make an admitted container's task ceiling
    // depend on the observer's unrelated RLIMIT_NOFILE. The generic error
    // preserves callback policy refusals separately from procfs diagnostics.
    fn visit<E: From<NamespaceError>>(
        &self,
        mut inspect: impl FnMut(TaskObservation) -> Result<(), E>,
    ) -> Result<(), E> {
        self.check()?;
        let namespace = FileIdentity::of(&self.namespace).map_err(NamespaceError::from)?;
        let mut observed = false;
        for pid in ids(&self.directory)? {
            let group = match open(&self.directory, &pid.to_string(), OFlags::DIRECTORY) {
                Ok(group) => group,
                Err(error) if process_gone(&error) => continue,
                Err(_) => return Err(NamespaceError::Unreadable.into()),
            };
            let tasks = match open(&group, "task", OFlags::DIRECTORY) {
                Ok(tasks) => tasks,
                Err(_) if group_exited(&group)? => continue,
                Err(_) => return Err(NamespaceError::Unreadable.into()),
            };
            let mut found = false;
            for tid in task_ids(&group, &tasks)? {
                let directory = match open(&tasks, &tid.to_string(), OFlags::DIRECTORY) {
                    Ok(directory) => directory,
                    Err(error) if process_gone(&error) => continue,
                    Err(_) => return Err(NamespaceError::Unreadable.into()),
                };
                let stat = match read_stat(&directory)? {
                    Some(stat) => stat,
                    None => continue,
                };
                let (number, state, start_ticks) = task_stat(&stat)?;
                if number != tid {
                    return Err(NamespaceError::Unreadable.into());
                }
                if matches!(state, "X" | "Z") {
                    continue;
                }
                let task = TaskObservation {
                    directory,
                    namespace: Arc::clone(&self.namespace),
                    id: NamespaceTaskId {
                        namespace,
                        number: tid,
                    },
                    group: NamespaceTaskId {
                        namespace,
                        number: pid,
                    },
                    start_ticks,
                };
                if let TaskReading::Live(_) = task.status()? {
                    found = true;
                    observed = true;
                    inspect(task)?;
                }
            }
            if !found && !group_exited(&group)? {
                return Err(NamespaceError::Unreadable.into());
            }
        }
        self.check()?;
        if !observed {
            return Err(NamespaceError::Unreadable.into());
        }
        Ok(())
    }
}

/// Apply a container check through held task entries, never by translating
/// a namespace-local number into an observer PID. Each task is qualified
/// separately: threads may have different credentials. The callback may
/// retain an engine lease, but no detached local coordinate is used later.
pub(crate) fn for_each_namespace_task(
    anchor: &ProcessLease,
    mut inspect: impl FnMut(&TaskObservation, ProcessLease) -> Result<(), UnqualifiedProcess>,
) -> Result<(), UnqualifiedProcess> {
    let view = NamespaceProcfs::capture(anchor)?;
    view.visit(|task| {
        let result = (|| {
            if matches!(task.status()?, TaskReading::Exited) {
                return Ok(());
            }
            let process = ProcessLease::capture_held(
                task.directory.try_clone().map_err(|_| UnqualifiedProcess)?,
                ProcSource::Namespace(Arc::clone(&view.directory)),
            )?;
            if process.start_ticks != task.start_ticks || !anchor.same_namespace(&process, "pid")? {
                return Err(UnqualifiedProcess);
            }
            inspect(&task, process)
        })();
        if let Err(error) = result {
            // A held incidental task may exit during any read, including
            // the callback. Unknown live state never becomes absence.
            if !matches!(task.status()?, TaskReading::Exited) {
                return Err(error);
            }
        }
        Ok(())
    })
}

// Every ordinary component stays on the held procfs mount. Bind-mounting a
// different proc entry over one PID keeps f_type=procfs; NO_XDEV catches the
// substituted mount, including a bind from this same procfs superblock.
pub(super) fn open(directory: &File, path: &str, flags: OFlags) -> std::io::Result<File> {
    rustix::fs::openat2(
        directory,
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | flags,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )
    .map(File::from)
    .map_err(Into::into)
}

fn read(directory: &File, path: &str) -> Result<String, NamespaceError> {
    super::read_bounded_from(
        open(directory, path, OFlags::empty()).map_err(|_| NamespaceError::Unreadable)?,
        65536,
    )
    .map_err(Into::into)
}

fn read_stat(directory: &File) -> Result<Option<String>, NamespaceError> {
    use std::io::Read;
    let file = match open(directory, "stat", OFlags::empty()) {
        Ok(file) => file,
        Err(error) if process_gone(&error) => return Ok(None),
        Err(_) => return Err(NamespaceError::Unreadable),
    };
    let mut stat = String::new();
    match file.take(65537).read_to_string(&mut stat) {
        Ok(_) if stat.len() <= 65536 => Ok(Some(stat)),
        Ok(_) => Err(NamespaceError::Metadata),
        Err(error) if process_gone(&error) => Ok(None),
        Err(_) => Err(NamespaceError::Unreadable),
    }
}

fn group_exited(directory: &File) -> Result<bool, NamespaceError> {
    match read_stat(directory)? {
        Some(stat) => exited_stat(&stat).map_err(|_| NamespaceError::Metadata),
        None => Ok(true),
    }
}

fn task_ids(group: &File, tasks: &File) -> Result<Vec<u32>, NamespaceError> {
    match ids(tasks) {
        Ok(ids) => Ok(ids),
        // Opening `task` does not make its iterator immortal: read_from
        // reopens ".", which can fail after the group exits (#754). The
        // held group's stat must prove exit; a live worker still refuses.
        Err(_) if group_exited(group)? => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn task_alive(directory: &File, id: u32, start: u64) -> Result<bool, NamespaceError> {
    let Some(stat) = read_stat(directory)? else {
        return Ok(false);
    };
    let (current_id, state, current_start) = task_stat(&stat)?;
    if current_id != id || current_start != start {
        return Err(NamespaceError::Replaced);
    }
    Ok(!matches!(state, "X" | "Z"))
}

fn task_stat(stat: &str) -> Result<(u32, &str, u64), NamespaceError> {
    let (prefix, fields) = stat.rsplit_once(')').ok_or(NamespaceError::Metadata)?;
    let number = prefix
        .split_once(' ')
        .ok_or(NamespaceError::Metadata)?
        .0
        .parse()
        .map_err(|_| NamespaceError::Metadata)?;
    let mut fields = fields.split_whitespace();
    let state = fields.next().ok_or(NamespaceError::Metadata)?;
    let start = fields
        .nth(18)
        .ok_or(NamespaceError::Metadata)?
        .parse()
        .map_err(|_| NamespaceError::Metadata)?;
    Ok((number, state, start))
}

fn status_id(status: &str, key: &str) -> Result<u32, NamespaceError> {
    let mut values = status.lines().filter_map(|line| line.strip_prefix(key));
    let id = values
        .next()
        .ok_or(NamespaceError::Metadata)?
        .trim()
        .parse()
        .map_err(|_| NamespaceError::Metadata)?;
    if id == 0 || values.next().is_some() {
        return Err(NamespaceError::Metadata);
    }
    Ok(id)
}

fn ids(directory: &File) -> Result<Vec<u32>, NamespaceError> {
    let mut ids = Vec::new();
    for entry in rustix::fs::Dir::read_from(directory).map_err(|_| NamespaceError::Unreadable)? {
        let entry = entry.map_err(|_| NamespaceError::Unreadable)?;
        let name = entry.file_name().to_bytes();
        if !name.is_empty() && name.iter().all(u8::is_ascii_digit) {
            let id = std::str::from_utf8(name)
                .map_err(|_| NamespaceError::Unreadable)?
                .parse()
                .map_err(|_| NamespaceError::Unreadable)?;
            if id == 0 {
                return Err(NamespaceError::Unreadable);
            }
            ids.push(id);
        }
    }
    Ok(ids)
}

fn namespace_init(directory: &File) -> Result<File, NamespaceError> {
    let ns = open(directory, "1/ns", OFlags::DIRECTORY).map_err(|_| NamespaceError::Unreadable)?;
    // Qualify the link itself without following it across nsfs. Following
    // this one final kernel link is intentional; no path prefix may cross a
    // mount or symlink. The resulting namespace must match the held anchor.
    open(&ns, "pid", OFlags::PATH | OFlags::NOFOLLOW).map_err(|_| NamespaceError::Unreadable)?;
    rustix::fs::openat(&ns, "pid", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map(File::from)
        .map_err(|_| NamespaceError::Unreadable)
}

fn mount_id(directory: &File) -> Result<u64, NamespaceError> {
    let text = super::read_bounded(
        &std::path::PathBuf::from(format!("/proc/self/fdinfo/{}", directory.as_raw_fd())),
        4096,
    )?;
    text.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .ok_or(NamespaceError::Metadata)?
        .trim()
        .parse()
        .map_err(|_| NamespaceError::Unreadable)
}

fn visible_mount(text: &str, id: u64) -> Result<(), NamespaceError> {
    let mut matched = false;
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.first().and_then(|s| s.parse::<u64>().ok()) != Some(id) {
            continue;
        }
        let split = fields
            .iter()
            .position(|s| *s == "-")
            .ok_or(NamespaceError::Metadata)?;
        if matched
            || split < 6
            || fields.len() != split + 4
            || fields[3] != "/"
            || fields[4] != "/proc"
            || fields[split + 1] != "proc"
        {
            return Err(NamespaceError::View);
        }
        for option in fields[split + 3].split(',') {
            if !matches!(
                option,
                "rw" | "ro" | "hidepid=0" | "hidepid=off" | "subset=pid"
            ) {
                return Err(NamespaceError::View);
            }
        }
        matched = true;
    }
    if matched {
        Ok(())
    } else {
        Err(NamespaceError::View)
    }
}

#[cfg(test)]
mod tests;
