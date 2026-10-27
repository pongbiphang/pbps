//! The private run record and its lock (ADR-0019 section 3, "Evidence of ownership").
//!
//! A run owns two files in the per-user state directory: `<run id>.lock`, held
//! exclusively for the whole run (the kernel drops it when the process dies,
//! SIGKILL included), and `<run id>.json`, replaced atomically at each step of
//! the run so that a kill at any point leaves a record that says how far the
//! run got. The record is ownership metadata: no SQL, no password, nothing a
//! deployment could reuse.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::Error;
use super::owner::Process;

pub(super) const VERSION: u32 = 1;

/// How far a run got. Each step is one atomic replacement of the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    /// Written before `docker run`: the container may or may not exist.
    Pending,
    /// The runtime returned the container's id, which is recorded.
    Created,
    /// Removal was started; the record outlives it until absence is confirmed.
    Removing,
}

/// The runtime the container was asked of, so that a later run only judges it
/// against the same one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Daemon {
    /// `DOCKER_HOST`, or `default`.
    pub endpoint: String,
    pub identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub version: u32,
    pub run_id: String,
    pub purpose: String,
    pub engine: String,
    pub process: Process,
    pub daemon: Daemon,
    /// The name asked for, and the id once the runtime returned it.
    pub container: Container,
    pub volumes: Vec<String>,
    /// What a removal reported as not confirmed gone but did not identify (the
    /// start's own, before the container's id was known): kept, so that a later
    /// pass cannot read an empty listing as "nothing was left".
    pub leftovers: Vec<String>,
    /// The labels the container was asked to carry.
    pub labels: BTreeMap<String, String>,
    pub state: State,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Container {
    pub name: String,
    pub id: Option<String>,
}

/// 128 random bits as 32 lowercase hex digits: unpredictable, and nothing
/// derived from a pid, a time or a name.
pub(super) fn new_run_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

pub(super) fn is_run_id(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The per-user directory the records live in.
pub(crate) struct StateDir {
    path: PathBuf,
}

/// `$XDG_STATE_HOME/pbps/test-runs`, or `~/.local/state/pbps/test-runs`.
/// A relative `XDG_STATE_HOME` is ignored, as the specification says.
pub(crate) fn default_dir(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<PathBuf, Error> {
    let base = var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            var("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|home| home.join(".local/state"))
        })
        .ok_or_else(|| Error::State("neither XDG_STATE_HOME nor HOME names a directory".into()))?;
    Ok(base.join("pbps/test-runs"))
}

fn state(path: &Path, what: impl std::fmt::Display) -> Error {
    Error::State(format!("{}: {what}", path.display()))
}

/// `path` is a directory this user owns that nobody else can enter, and not a
/// link to one; created (and its parents) when it does not exist. Every file
/// in it is private for that reason alone. The user is read from `/proc` so
/// that no system-call crate is needed: records are Linux-only (the resolver's
/// Docker profile is), and every other platform refuses here.
#[cfg(target_os = "linux")]
fn private_dir(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| state(parent, e))?;
            }
            // Another invocation may have just made it: what exists is
            // judged below, whoever made it.
            match std::fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(state(path, e)),
            }
        }
        Err(e) => return Err(state(path, e)),
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| state(path, e))?;
    let me = std::fs::metadata("/proc/self")
        .map_err(|e| state(path, e))?
        .uid();
    if !meta.is_dir() {
        Err(state(path, "is not a directory (a link is not followed)"))
    } else if meta.uid() != me {
        Err(state(path, "belongs to another user"))
    } else if meta.permissions().mode() & 0o077 != 0 {
        Err(state(
            path,
            "can be entered by other users; it must be mode 0700",
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn private_dir(_: &Path) -> Result<(), Error> {
    Err(Error::Unsupported)
}

impl StateDir {
    /// Opens `path` as the state directory, creating it private.
    pub(crate) fn open(path: PathBuf) -> Result<Self, Error> {
        private_dir(&path)?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn file(&self, run_id: &str, extension: &str) -> PathBuf {
        self.path.join(format!("{run_id}.{extension}"))
    }

    /// Creates the run's lock and holds it. Fails when the lock cannot be taken:
    /// a run id is never reused. The file is locked **before** it appears under
    /// its name (a hard link is atomic and fails on an existing name), so a
    /// recovery that finds the name always finds a lock that is held, and never
    /// one it could take and delete from under a run that is just starting.
    pub(super) fn lock(&self, run_id: &str) -> Result<File, Error> {
        let path = self.file(run_id, "lock");
        let hidden = self.path.join(format!(".{run_id}.lock"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&hidden)
            .map_err(|e| state(&hidden, e))?;
        let published = file
            .try_lock()
            .map_err(|e| state(&hidden, e))
            .and_then(|()| std::fs::hard_link(&hidden, &path).map_err(|e| state(&path, e)));
        // Either way the hidden name has done its work; a kill between the two
        // leaves one file named by 128 random bits, which nothing reads.
        let _ = std::fs::remove_file(&hidden);
        published.map(|()| file)
    }

    /// Replaces the run's record: written to a temporary file, flushed,
    /// renamed over the old one and the directory flushed, so a reader sees the
    /// old record or the new, never a torn one.
    pub(super) fn write(&self, record: &Record) -> Result<(), Error> {
        let path = self.file(&record.run_id, "json");
        let temporary = self.file(&record.run_id, "json.tmp");
        // A temporary left by a kill mid-write is this run's own.
        let _ = std::fs::remove_file(&temporary);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| state(&temporary, e))?;
        let text = serde_json::to_vec_pretty(record).expect("a record serializes");
        file.write_all(&text)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|e| state(&temporary, e))?;
        std::fs::rename(&temporary, &path).map_err(|e| state(&path, e))?;
        File::open(&self.path)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| state(&self.path, e))
    }

    /// The record of `run_id`, if there is one; a file that is not this user's
    /// private regular file, or does not parse, is an error and never absence.
    pub(super) fn read(&self, run_id: &str) -> Result<Option<Record>, Error> {
        let path = self.file(run_id, "json");
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(state(&path, e)),
        };
        if !meta.is_file() {
            return Err(state(
                &path,
                "is not a regular file (a link is not followed)",
            ));
        }
        let text = std::fs::read(&path).map_err(|e| state(&path, e))?;
        let record: Record = serde_json::from_slice(&text)
            .map_err(|e| state(&path, format!("does not parse: {e}")))?;
        if record.version != VERSION {
            return Err(state(
                &path,
                format!(
                    "is version {}, and this tool reads {VERSION}",
                    record.version
                ),
            ));
        }
        if record.run_id != run_id {
            return Err(state(&path, "names another run"));
        }
        Ok(Some(record))
    }

    /// The run ids with a lock or a record, sorted. Names that are not a run
    /// id's are not ours and are left alone.
    pub(super) fn runs(&self) -> Result<Vec<String>, Error> {
        let mut ids = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(&self.path).map_err(|e| state(&self.path, e))? {
            let name = entry.map_err(|e| state(&self.path, e))?.file_name();
            let name = name.to_string_lossy();
            if let Some((id, extension)) = name.split_once('.')
                && is_run_id(id)
                && matches!(extension, "lock" | "json" | "json.tmp")
            {
                ids.insert(id.to_owned());
            }
        }
        Ok(ids.into_iter().collect())
    }

    /// The lock file of `run_id`, taken only if nobody holds it. `None` means
    /// somebody does: a run that is alive.
    pub(super) fn try_lock_existing(&self, run_id: &str) -> Result<Option<File>, Error> {
        let path = self.file(run_id, "lock");
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| state(&path, e))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(state(&path, e)),
        }
    }
}
