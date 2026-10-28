//! Who owns a run, and whether they are provably gone (ADR-0019 section 3).
//!
//! A pid alone is not an identity: pids are reused, and two processes in
//! different pid namespaces can share one. A process is the same process only
//! if its pid, its start time and the boot it started in all agree.

use serde::{Deserialize, Serialize};

use super::Error;

/// A process as `/proc` identifies it: the pid, the clock tick it started at
/// since boot, and the boot (`/proc/sys/kernel/random/boot_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Process {
    pub pid: u32,
    pub start_ticks: u64,
    pub boot_id: String,
}

/// What the observations say of a record's owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Owner {
    /// Somebody holds the run's lock.
    Alive,
    Dead(&'static str),
    /// Nothing proves either; the reason is shown and nothing is removed.
    Unprovable(String),
}

/// The start tick of `/proc/<pid>/stat`: field 22, counted after the command
/// name, which is parenthesised and may itself hold spaces and parentheses.
pub(super) fn start_ticks(stat: &str) -> Option<u64> {
    let (_, after_name) = stat.rsplit_once(')')?;
    // `after_name` starts at field 3, the state.
    after_name.split_whitespace().nth(22 - 3)?.parse().ok()
}

pub(super) fn boot_id() -> Result<String, Error> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|id| id.trim().to_owned())
        .ok()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::State("the boot id could not be read".into()))
}

/// The start tick of `pid`, or `None` when no such process exists. A process
/// that exists but cannot be read is an error and never absence.
pub(super) fn lookup(pid: u32) -> Result<Option<u64>, Error> {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => start_ticks(&stat)
            .map(Some)
            .ok_or_else(|| Error::State(format!("/proc/{pid}/stat could not be parsed"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::State(format!(
            "process {pid} could not be read: {e}"
        ))),
    }
}

impl Process {
    /// This process.
    pub(super) fn current() -> Result<Self, Error> {
        let pid = std::process::id();
        let start_ticks =
            lookup(pid)?.ok_or_else(|| Error::State("this process is not in /proc".into()))?;
        Ok(Self {
            pid,
            start_ticks,
            boot_id: boot_id()?,
        })
    }
}

/// Whether the owner of a record is provably gone. `lock_free` is whether the
/// run's lock could be taken, `boot_now` the current boot, and `running` what
/// is known of the recorded pid now.
///
/// A held lock is the whole answer for a live run. A free lock does not by
/// itself prove death (a lock a filesystem does not honour looks free), so the
/// recorded process must also be provably another one, or gone.
pub(super) fn judge(
    recorded: &Process,
    lock_free: bool,
    boot_now: &str,
    running: Result<Option<u64>, Error>,
) -> Owner {
    if !lock_free {
        return Owner::Alive;
    }
    if recorded.boot_id != boot_now {
        return Owner::Dead("the machine has been restarted since");
    }
    match running {
        Err(e) => Owner::Unprovable(e.to_string()),
        Ok(None) => Owner::Dead("its process no longer exists"),
        Ok(Some(ticks)) if ticks != recorded.start_ticks => {
            Owner::Dead("its pid now belongs to another process")
        }
        Ok(Some(_)) => Owner::Unprovable(
            "its process is still running although its lock is free; the state directory may be \
             on a filesystem that does not honour locks"
                .into(),
        ),
    }
}
