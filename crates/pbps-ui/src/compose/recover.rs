//! What the UI does at launch with a record a crash left behind.
//!
//! The rule that governs all of it: **act only where the evidence is
//! unambiguous**. A record whose process is still alive is a compose someone
//! is running right now, and nothing of it is reclaimed. A record in
//! `rolling-back` resumes the rollback and never the success path, whatever
//! `HEAD`, the branch tip or the prepared index now say — because `HEAD` being
//! pointed back at the composed commit must not turn a partial undo into
//! permission to install the prepared index (#152). And where neither the
//! pending nor the completed state matches what the record describes, a third
//! party has touched the files: recovery keeps the record, reports what
//! differs, and leaves that path alone.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::attributes;
use super::fsx::Dir;
use super::git::Git;
use super::locks;
use super::record::{Phase, Placed, Record, Records};
use super::refs::{self, Lease};
use super::repo_path::RepoPath;

/// What recovery did with one record, for the page.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Recovered {
    /// The process is still running. Nothing was touched, and this UI will not
    /// compose while it stands.
    StillRunning { record: String },
    /// Nothing had been placed; the locks it named are gone and so is it.
    LocksCleared { record: String },
    /// The rollback was finished. The prepared index was never installed and
    /// the user's index is unchanged.
    RolledBack {
        record: String,
        restored: Vec<String>,
        reported: Vec<String>,
    },
    /// The interrupted success was finished: the prepared index installed (or
    /// found already installed) and the cleanup step 2 owed carried out.
    Finished { record: String, commit: String },
    /// The evidence does not say. The record is kept and the page shows it.
    Undecided { record: String, why: String },
}

/// Read every record and act where the evidence allows.
pub fn at_launch(git: &Git, git_dir: &Path) -> Vec<Recovered> {
    let Ok(records) = Records::open(git_dir) else {
        return Vec::new();
    };
    let Ok(names) = records.list() else {
        return Vec::new();
    };
    names
        .into_iter()
        .filter_map(|name| records.read(&name).ok())
        .map(|record| one(git, git_dir, &records, record))
        .collect()
}

fn one(git: &Git, git_dir: &Path, records: &Records, mut record: Record) -> Recovered {
    if record.process.alive() {
        return Recovered::StillRunning { record: record.id };
    }
    match record.phase {
        Phase::Locking => clear_locks(records, &record),
        // A record in `rolling-back` resumes that undo, never the success
        // path, regardless of the current `HEAD`, branch tip or prepared-index
        // hash. This arm is *before* any check of the refs on purpose: the
        // decision is not recomputed after a crash.
        Phase::RollingBack => roll_back(git, git_dir, records, &mut record),
        Phase::Placing => roll_back_after_publishing(git, git_dir, records, &mut record),
        Phase::Composed | Phase::Installed => match branch_holds_the_commit(git, &record) {
            Some(true) => finish(git, git_dir, records, &mut record),
            Some(false) => roll_back_after_publishing(git, git_dir, records, &mut record),
            None => Recovered::Undecided {
                record: record.id.clone(),
                why: "the branch this compose was on could not be read".to_owned(),
            },
        },
    }
}

/// A record found in `locking` has nothing placed and is finished by removing
/// the locks it names and the record.
fn clear_locks(records: &Records, record: &Record) -> Recovered {
    for lock in &record.locks {
        if reclaimable(Path::new(&lock.lock_file), record) {
            let _ = std::fs::remove_file(&lock.lock_file);
        }
    }
    if let Some(index_lock) = &record.index_lock_file
        && ours_by_hash(Path::new(index_lock), record)
    {
        let _ = std::fs::remove_file(index_lock);
    }
    let _ = records.remove(record);
    Recovered::LocksCleared {
        record: record.id.clone(),
    }
}

/// Publish `rolling-back` first, then undo. A crash before publication has
/// undone nothing; one after it can only resume rollback, even where no path
/// needed restoration.
fn roll_back_after_publishing(
    git: &Git,
    git_dir: &Path,
    records: &Records,
    record: &mut Record,
) -> Recovered {
    if record.advance(Phase::RollingBack).is_err() || records.publish(record).is_err() {
        return Recovered::Undecided {
            record: record.id.clone(),
            why: "the rollback could not be recorded, so nothing was undone".to_owned(),
        };
    }
    roll_back(git, git_dir, records, record)
}

fn roll_back(git: &Git, git_dir: &Path, records: &Records, record: &mut Record) -> Recovered {
    let Ok(root) = Dir::open_root(git.root()) else {
        return Recovered::Undecided {
            record: record.id.clone(),
            why: "the worktree could not be opened".to_owned(),
        };
    };
    let mut restored = Vec::new();
    let mut reported = Vec::new();
    // In undo order, which is the reverse of the order they were placed.
    for index in (0..record.paths.len()).rev() {
        let placed = record.paths[index].clone();
        if placed.undone || placed.keep == Some(true) {
            continue;
        }
        match undo(git, &root, &placed) {
            Undo::Done => {
                restored.push(placed.path.clone());
                record.paths[index].undone = true;
                // Every undo is flushed before its completion is published;
                // a crash before that progress update is answered by the
                // recorded identities at the two names.
                let _ = root.sync();
                let _ = records.publish(record);
            }
            Undo::AlreadyDone => {
                record.paths[index].undone = true;
                let _ = records.publish(record);
            }
            Undo::Refused(why) => reported.push(format!("{}: {why}", placed.path)),
        }
    }
    // The prepared index is discarded without being installed, and the user's
    // index and refs are not changed by a rollback.
    if let Some(index_lock) = &record.index_lock_file
        && ours_by_hash(Path::new(index_lock), record)
    {
        let _ = std::fs::remove_file(index_lock);
    }
    for lock in &record.locks {
        if reclaimable(Path::new(&lock.lock_file), record) {
            let _ = std::fs::remove_file(&lock.lock_file);
        }
    }
    let _ = std::fs::remove_dir_all(git_dir.join("pbps-ui").join("intent").join(&record.id));
    if reported.is_empty() {
        let _ = records.remove(record);
    }
    Recovered::RolledBack {
        record: record.id.clone(),
        restored,
        reported,
    }
}

enum Undo {
    Done,
    AlreadyDone,
    Refused(String),
}

/// Undo one placement from the record's own evidence.
///
/// The record holds what identifies each of the two files, so recovery reads
/// what is at each of the two names and knows which side of the exchange it is
/// on. A name already moved is skipped only when that evidence proves the
/// recorded operation completed, not merely because a lookup failed.
fn undo(git: &Git, root: &Dir, placed: &Placed) -> Undo {
    let Ok(path) = RepoPath::new(placed.path.as_bytes()) else {
        return Undo::Refused("the recorded path is not one this UI would have placed".to_owned());
    };
    let Ok((parent, leaf)) = root.walk_to_parent(&path) else {
        return Undo::Refused("the directory holding it is no longer reachable".to_owned());
    };
    let temporary = OsString::from(&placed.temporary);
    let at = |name: &OsString| -> Option<(String, u64, u64)> {
        let look = parent.look(name).ok()??;
        if look.is_link || !look.is_regular {
            return None;
        }
        let file = parent.open_file(name).ok()?;
        let bytes = file.read().ok()?;
        let identity = file.identity().ok()?;
        Some((
            attributes::hash(git, &bytes).ok()?,
            identity.device,
            identity.inode,
        ))
    };
    let matches = |found: &Option<(String, u64, u64)>, wanted: &super::record::Identity| {
        found.as_ref().is_some_and(|(blob, device, inode)| {
            *blob == wanted.blob && *device == wanted.device && *inode == wanted.inode
        })
    };
    let here = at(&leaf);
    let beside = at(&temporary);

    match &placed.original {
        // An exchange.
        Some(original) => {
            if matches(&here, original) {
                // Already undone, or never done.
                Undo::AlreadyDone
            } else if matches(&here, &placed.replacement) && matches(&beside, original) {
                match parent.exchange(&leaf, &temporary) {
                    Ok(()) => Undo::Done,
                    Err(e) => Undo::Refused(e.to_string()),
                }
            } else {
                Undo::Refused(
                    "neither name holds what this compose put there; \
                     something else has touched them"
                        .to_owned(),
                )
            }
        }
        // A `link()`. The path did not exist before, so restoring it means
        // taking the name away — by rename, never by unlink.
        None => {
            if here.is_none() {
                Undo::AlreadyDone
            } else if matches(&here, &placed.replacement) || beside.is_some() {
                let away = OsString::from(format!("{}.rolled-back", placed.temporary));
                match parent.rename(&leaf, &away) {
                    Ok(()) => Undo::Done,
                    Err(e) => Undo::Refused(e.to_string()),
                }
            } else {
                Undo::Refused("the path holds something this compose did not put there".to_owned())
            }
        }
    }
}

/// The success path's three questions, all of which step 5 asked after its own
/// `update-ref`. A chain that only resolves to the branch, or any read error,
/// cannot authorize index installation.
fn branch_holds_the_commit(git: &Git, record: &Record) -> Option<bool> {
    let (Some(branch), Some(commit)) = (&record.branch, &record.commit) else {
        return Some(false);
    };
    let lease = Lease {
        reference: branch.clone(),
        tip: commit.clone(),
    };
    match refs::post_write(git, &lease, commit) {
        Ok(()) => Some(true),
        Err(refs::RefRefusal::Unexpected(_)) => None,
        Err(_) => Some(false),
    }
}

/// Finish the interrupted success, from whichever side of the rename the crash
/// left it on.
///
/// The lock's absence is not a refusal but the evidence that the rename
/// already happened, which is the one thing the record cannot say about
/// itself, being written before the rename and removed after the cleanup
/// (**measured**: the file's hash before and after the rename was the same).
fn finish(git: &Git, git_dir: &Path, records: &Records, record: &mut Record) -> Recovered {
    let Some(index_lock) = record.index_lock_file.clone() else {
        return Recovered::Undecided {
            record: record.id.clone(),
            why: "the record does not name a prepared index".to_owned(),
        };
    };
    let index_lock = PathBuf::from(index_lock);
    let Ok(index_file) = locks::index_file(git) else {
        return Recovered::Undecided {
            record: record.id.clone(),
            why: "git could not say where the index is".to_owned(),
        };
    };
    if index_lock.exists() {
        if !ours_by_hash(&index_lock, record) {
            return Recovered::Undecided {
                record: record.id.clone(),
                why: "the index lock is another git's work".to_owned(),
            };
        }
        if std::fs::rename(&index_lock, &index_file).is_err() {
            return Recovered::Undecided {
                record: record.id.clone(),
                why: "the prepared index could not be installed".to_owned(),
            };
        }
    } else if !ours_by_hash(&index_file, record) {
        return Recovered::Undecided {
            record: record.id.clone(),
            why: "the installed index is neither the prepared one nor a lock this compose made"
                .to_owned(),
        };
    }
    let _ = record.advance(Phase::Installed);
    let _ = records.publish(record);

    // The cleanup step 2 owed, done by name from the record and skipped where
    // the name is already gone, so that running it twice is running it once.
    let previous_directory = git_dir.join("pbps-ui").join("previous");
    let _ = std::fs::create_dir_all(&previous_directory);
    if let (Ok(root), Ok(previous)) = (
        Dir::open_root(git.root()),
        Dir::open_root(&previous_directory),
    ) {
        for placed in &record.paths {
            let Ok(path) = RepoPath::new(placed.path.as_bytes()) else {
                continue;
            };
            let Ok((parent, _)) = root.walk_to_parent(&path) else {
                continue;
            };
            let temporary = OsString::from(&placed.temporary);
            if placed.original.is_none() {
                let _ = parent.unlink(&temporary);
            } else if let Ok(file) = parent.open_file(&temporary)
                && let Ok(bytes) = file.read()
            {
                let name = OsString::from(format!("{}-{}", placed.temporary, record.id));
                if let Ok(kept) = previous.create_new(&name, 0o600)
                    && kept.write_all(&bytes).is_ok()
                    && kept.flush().is_ok()
                {
                    let _ = parent.unlink(&temporary);
                }
            }
        }
    }
    for lock in &record.locks {
        if reclaimable(Path::new(&lock.lock_file), record) {
            let _ = std::fs::remove_file(&lock.lock_file);
        }
    }
    let _ = std::fs::remove_dir_all(git_dir.join("pbps-ui").join("intent").join(&record.id));
    let commit = record.commit.clone().unwrap_or_default();
    let _ = records.remove(record);
    Recovered::Finished {
        record: record.id.clone(),
        commit,
    }
}

/// A ref lock this record can prove its own.
///
/// Either it holds this record's id, or — in the `composed` phase — it is what
/// an interrupted `update-ref` left: `git` writes the new value into the
/// branch's lock and leaves `HEAD.lock` empty (**measured** from a
/// `reference-transaction` hook in the `prepared` phase), so a branch lock
/// holding exactly the record's commit id is that transaction's, and an empty
/// `HEAD.lock` beside it is the same transaction's too.
fn reclaimable(lock: &Path, record: &Record) -> bool {
    let Ok(contents) = std::fs::read(lock) else {
        return false;
    };
    let text = String::from_utf8_lossy(&contents);
    let text = text.trim();
    if text == record.id {
        return true;
    }
    match (&record.commit, record.phase) {
        (Some(commit), Phase::Composed | Phase::RollingBack | Phase::Installed) => {
            text == commit || text.is_empty()
        }
        _ => false,
    }
}

/// `<index>.lock` carries no id, being a copy of the index, so its ownership
/// is established by hash.
fn ours_by_hash(path: &Path, record: &Record) -> bool {
    let Some(wanted) = &record.index_lock_hash else {
        return false;
    };
    super::run::hash_file(path).is_ok_and(|found| found == *wanted)
}
