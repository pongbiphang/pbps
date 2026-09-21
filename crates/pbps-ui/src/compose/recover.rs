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
        .map(|name| match records.read(&name) {
            Ok(record) => one(git, git_dir, &records, record),
            // Absent, empty and unreadable are three different things, and
            // only one of them is good news (AGENTS.md). A truncated record, a
            // permission denied, or one written by a later version is evidence
            // that *something* was interrupted; dropping it would let this UI
            // advertise composing while an unaccounted compose still owns
            // locks or placed files.
            Err(e) => Recovered::Undecided {
                record: name.to_string_lossy().into_owned(),
                why: format!("this record could not be read ({e}), so nothing was touched"),
            },
        })
        .collect()
}

/// True where any record on disk is either a live compose's or one this UI
/// could not read. Both mean the same thing for the question being asked:
/// something may still own the locks and the placed files.
pub fn something_else_owns_this_checkout(git_dir: &Path) -> bool {
    let Ok(records) = Records::open(git_dir) else {
        return false;
    };
    let Ok(names) = records.list() else {
        return false;
    };
    names.into_iter().any(|name| match records.read(&name) {
        Ok(record) => record.process.alive(),
        Err(_) => true,
    })
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
    // Every restored exchange leaves the replacement beside its path, and a
    // recovered `link()` leaves two names. They are moved under `previous/`
    // and kept — never deleted, since an editor may hold one open — and this
    // happens *before* the record goes, because the record is the only thing
    // that names them.
    let previous_directory = git_dir.join("pbps-ui").join("previous");
    let _ = std::fs::create_dir_all(&previous_directory);
    match Dir::open_root(&previous_directory) {
        Ok(previous) => {
            for placed in &record.paths {
                if !placed.undone {
                    continue;
                }
                if let Err(detail) = retain_leftovers(&root, placed, &previous, &record.id) {
                    reported.push(format!("{}: {detail}", placed.path));
                }
            }
        }
        Err(e) => reported.push(format!("the displaced files could not be kept: {e}")),
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

/// Move whatever an undo left beside a path out of the working tree.
fn retain_leftovers(root: &Dir, placed: &Placed, previous: &Dir, id: &str) -> Result<(), String> {
    let path = RepoPath::new(placed.path.as_bytes()).map_err(|e| e.to_string())?;
    let (parent, _) = root.walk_to_parent(&path).map_err(|e| e.to_string())?;
    for name in [
        OsString::from(placed.temporary.clone()),
        OsString::from(format!("{}.rolled-back", placed.temporary)),
    ] {
        let file = match parent.open_file(&name) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.to_string()),
        };
        let bytes = file.read().map_err(|e| e.to_string())?;
        let kept_as = OsString::from(format!("{}-{id}", name.to_string_lossy()));
        match previous.create_new(&kept_as, 0o600) {
            Ok(kept) => {
                kept.write_all(&bytes).map_err(|e| e.to_string())?;
                kept.flush().map_err(|e| e.to_string())?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.to_string()),
        }
        parent.unlink(&name).map_err(|e| e.to_string())?;
    }
    Ok(())
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
    // Three answers, not two. A name holding a symbolic link, a directory or
    // a file that cannot be read is *not* an absent name, and collapsing them
    // would let recovery read somebody else's object as a completed
    // restoration — or, worse, take it away.
    #[derive(PartialEq, Eq)]
    enum There {
        File(String, u64, u64),
        Nothing,
        Unusable,
    }
    let at = |name: &OsString| -> There {
        match parent.look(name) {
            Ok(None) => There::Nothing,
            Ok(Some(look)) if look.is_link || !look.is_regular => There::Unusable,
            Ok(Some(_)) => {
                match parent
                    .open_file(name)
                    .and_then(|file| Ok((file.read()?, file.identity()?)))
                {
                    Ok((bytes, identity)) => match attributes::hash(git, &bytes) {
                        Ok(blob) => There::File(blob, identity.device, identity.inode),
                        Err(_) => There::Unusable,
                    },
                    Err(_) => There::Unusable,
                }
            }
            Err(_) => There::Unusable,
        }
    };
    let matches = |found: &There, wanted: &super::record::Identity| match found {
        There::File(blob, device, inode) => {
            *blob == wanted.blob && *device == wanted.device && *inode == wanted.inode
        }
        There::Nothing | There::Unusable => false,
    };
    let here = at(&leaf);
    let beside = at(&temporary);

    match &placed.original {
        // An exchange.
        Some(original) => {
            if matches(&here, original) {
                // Already undone, or never done.
                Undo::AlreadyDone
            } else if here == There::Unusable || beside == There::Unusable {
                Undo::Refused("one of the two names holds something this UI cannot read".to_owned())
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
            if here == There::Nothing {
                Undo::AlreadyDone
            } else if here == There::Unusable {
                Undo::Refused("the path holds something this UI cannot read".to_owned())
            } else if matches(&here, &placed.replacement) {
                // Only where the *whole* recorded identity matches. The
                // temporary a `link()` was made from normally still exists,
                // so testing for *it* instead would authorise taking away
                // whatever is at the path — including the file an editor
                // wrote there after the crash.
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
    // Whatever it cannot do is *reported*, and the record is kept for it: a
    // permission error, an unreadable temporary or a full disk leaves a
    // displaced file beside its declaration, and a record removed at that
    // moment is the only thing that could have named it.
    let mut unresolved: Vec<String> = Vec::new();
    let previous_directory = git_dir.join("pbps-ui").join("previous");
    if let Err(e) = std::fs::create_dir_all(&previous_directory) {
        unresolved.push(format!("the retention directory could not be made: {e}"));
    }
    match (
        Dir::open_root(git.root()),
        Dir::open_root(&previous_directory),
    ) {
        (Ok(root), Ok(previous)) => {
            for placed in &record.paths {
                if let Err(detail) = finish_one(&root, placed, &previous, &record.id) {
                    unresolved.push(format!("{}: {detail}", placed.path));
                }
            }
        }
        _ => unresolved.push("the retention directory could not be opened".to_owned()),
    }
    for lock in &record.locks {
        if reclaimable(Path::new(&lock.lock_file), record) {
            let _ = std::fs::remove_file(&lock.lock_file);
        }
    }
    let _ = std::fs::remove_dir_all(git_dir.join("pbps-ui").join("intent").join(&record.id));
    let commit = record.commit.clone().unwrap_or_default();
    if !unresolved.is_empty() {
        return Recovered::Undecided {
            record: record.id.clone(),
            why: format!(
                "the commit {commit} stands and the index is installed, but the files this \
                 compose displaced are still beside their paths: {}. The record has been kept.",
                unresolved.join("; ")
            ),
        };
    }
    let _ = records.remove(record);
    Recovered::Finished {
        record: record.id.clone(),
        commit,
    }
}

/// One path's share of the cleanup step 2 owed.
fn finish_one(root: &Dir, placed: &Placed, previous: &Dir, id: &str) -> Result<(), String> {
    let path = RepoPath::new(placed.path.as_bytes()).map_err(|e| e.to_string())?;
    let (parent, _) = root.walk_to_parent(&path).map_err(|e| e.to_string())?;
    let temporary = OsString::from(&placed.temporary);
    if placed.original.is_none() {
        // The one name this UI ever unlinks: the temporary its own `link()`
        // was made from.
        return match parent.unlink(&temporary) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        };
    }
    let file = match parent.open_file(&temporary) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let bytes = file.read().map_err(|e| e.to_string())?;
    let name = OsString::from(format!("{}-{id}", placed.temporary));
    match previous.create_new(&name, 0o600) {
        Ok(kept) => {
            kept.write_all(&bytes).map_err(|e| e.to_string())?;
            kept.flush().map_err(|e| e.to_string())?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.to_string()),
    }
    parent.unlink(&temporary).map_err(|e| e.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::record::Identity;
    use crate::compose::scratch_repo::Scratch;

    /// The `link()` half of the undo, which a rename cannot reach: an intent
    /// command that rewrites the ids file exchanges it, and a project whose
    /// ids file is absent has no identities for the intent to resolve. The
    /// *record* is this function's input, so building one is building the
    /// input — the files beside it are real, and they are what decides.
    fn a_created_path(blob: &str, device: u64, inode: u64) -> Placed {
        Placed {
            path: "schema/new.yml".to_owned(),
            temporary: "new.yml.pbps-ui-test".to_owned(),
            original: None,
            replacement: Identity {
                blob: blob.to_owned(),
                device,
                inode,
            },
            entry_mode: Some(0o100644),
            entry_blob: Some(blob.to_owned()),
            keep: None,
            retained_as: None,
            undone: false,
        }
    }

    fn identity_of(scratch: &Scratch, relative: &str) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt as _;
        let found = std::fs::metadata(scratch.path(relative)).unwrap();
        (found.dev(), found.ino())
    }

    #[test]
    fn a_created_path_is_taken_away_only_where_its_whole_identity_matches() {
        let scratch = Scratch::new("undo-created");
        scratch.write("schema/keep.yml", b"table: keep\n");
        scratch.commit("one");
        let git = scratch.runner();
        let root = Dir::open_root(&scratch.root).unwrap();

        // What the compose placed, and the source its `link()` was made from,
        // which is why testing for *that* name instead is not enough.
        scratch.write("schema/new.yml", b"table: new\n");
        scratch.write("schema/new.yml.pbps-ui-test", b"table: new\n");
        let blob = attributes::hash(&git, b"table: new\n").unwrap();
        let (device, inode) = identity_of(&scratch, "schema/new.yml");
        let placed = a_created_path(&blob, device, inode);

        // An editor saves its own file over the path after the crash.
        scratch.write("schema/.swap", b"the editor's own\n");
        std::fs::rename(scratch.path("schema/.swap"), scratch.path("schema/new.yml")).unwrap();

        let outcome = undo(&git, &root, &placed);
        assert!(
            matches!(outcome, Undo::Refused(_)),
            "the editor's file must be reported, not renamed away"
        );
        assert_eq!(
            std::fs::read(scratch.path("schema/new.yml")).unwrap(),
            b"the editor's own\n"
        );

        // The control: the file this compose actually placed *is* taken away.
        std::fs::write(scratch.path("schema/new.yml"), b"table: new\n").unwrap();
        let (device, inode) = identity_of(&scratch, "schema/new.yml");
        let placed = a_created_path(&blob, device, inode);
        assert!(matches!(undo(&git, &root, &placed), Undo::Done));
        assert!(!scratch.path("schema/new.yml").exists());
    }

    #[test]
    fn a_name_holding_something_unreadable_is_neither_absent_nor_restored() {
        // Absent, empty and unreadable are three different things, and
        // collapsing the last two would have recovery report a link as a
        // completed restoration.
        let scratch = Scratch::new("undo-unusable");
        scratch.write("schema/keep.yml", b"table: keep\n");
        scratch.commit("one");
        let git = scratch.runner();
        let root = Dir::open_root(&scratch.root).unwrap();
        std::os::unix::fs::symlink("keep.yml", scratch.path("schema/new.yml")).unwrap();
        let placed = a_created_path("whatever", 1, 2);

        assert!(matches!(undo(&git, &root, &placed), Undo::Refused(_)));
        assert!(
            std::fs::symlink_metadata(scratch.path("schema/new.yml")).is_ok(),
            "and it is left alone"
        );
    }
}
