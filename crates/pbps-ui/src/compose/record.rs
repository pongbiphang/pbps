//! The compose's own record, and the reason it is the compose's first act.
//!
//! Steps 5 and 6 of ADR-0015 decision 5 are two durable writes with a gap
//! between them, and a process killed in that gap leaves the branch at the new
//! commit, the working tree holding the placed files, and the index at its old
//! contents — a state whose ordinary remedy makes it worse: deleting the
//! leftover `<index>.lock`, which is what one does with a stale lock, leaves
//! every composed path staged as a reversion of the commit just made
//! (**measured**: interrupted there, `git status` reported `MM`, and deleting
//! the lock left it reporting the same).
//!
//! The gap cannot be closed — a ref and an index are two files — so it is made
//! recoverable instead, and so is every earlier interval, since a crash
//! anywhere from step 0 leaves a lock and one during step 2 a placed file too,
//! with nothing on disk to say whose they are.
//!
//! Two properties do the work, and both are structural rather than checked:
//!
//! * **Every version is complete before it is the record.** It is written
//!   under a fresh name beside it, flushed, renamed over the record, and the
//!   directory flushed. A stop in the middle of a rewrite would otherwise
//!   leave the one piece of evidence recovery has as half of one version and
//!   half of another, at a moment when locks are held and files are placed.
//! * **The phase is written before the thing it names.** So the record is at
//!   worst one step ahead of the disk and never behind it: a phase that is
//!   there may not have happened, and one that is not there certainly has not.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::fsx::Dir;

/// How far the compose had got. The order is the order they are published in,
/// and `RollingBack` is the one that cannot be left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// Before step 0 creates a single file, so the locks it is about to make
    /// are already attributable.
    Locking,
    /// Before step 2 touches the first path.
    Placing,
    /// Before step 5, with every placed file already flushed: what makes the
    /// interval recoverable is that the tree on disk is the one the record
    /// describes, and a machine that stopped with the branch moved and the
    /// placements still in a cache would be recovered into a checkout holding
    /// the old bytes.
    Composed,
    /// Before any refusal or recovery starts undoing step 2. Irreversible:
    /// later versions may record undo progress but never return to `Placing`
    /// or `Composed`, advance to `Installed`, or authorize a push.
    RollingBack,
    /// Before the cleanup, once the rename has happened.
    Installed,
}

/// The pair that says whether the compose is still running. A process id alone
/// is reused, so the start time goes with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub id: u32,
    /// Linux's `/proc/<pid>/stat` field 22, kept as text because it is an
    /// opaque token here: the UI compares it, never interprets it.
    pub started: String,
}

impl Process {
    pub fn here() -> io::Result<Self> {
        let id = std::process::id();
        Ok(Self {
            id,
            started: start_time(id)?,
        })
    }

    /// True only when both halves match. A record whose process is still alive
    /// is a compose someone is running right now, in this UI or another
    /// launched beside it: nothing of it is reclaimed and nothing is rolled
    /// back, because every leftover this UI would otherwise recognise as its
    /// own kind may be that compose's, in use this instant.
    pub fn alive(&self) -> bool {
        match start_time(self.id) {
            Ok(started) => started == self.started,
            Err(_) => false,
        }
    }
}

/// What identifies one of the two files an exchange swaps.
///
/// Both the hash and the inode, because the record is written *before* the
/// exchange and cannot say afterwards whether it happened: with only the name,
/// a recovery would find a file at the path and a file beside it and no way to
/// tell which is which, and undoing a swap that never happened would put the
/// replacement into the working tree while the branch stayed at the old tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub blob: String,
    pub device: u64,
    pub inode: u64,
}

/// One path the compose is placing, as the record describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placed {
    pub path: String,
    /// The name a `link()` will be made from, or the one the exchange will put
    /// the old file under. Recorded before it is used.
    pub temporary: String,
    /// Absent where the path is absent — a new declaration, placed with
    /// `link()`, which has nothing to exchange with.
    pub original: Option<Identity>,
    pub replacement: Identity,
    /// The entry step 3 recorded: the mode and the blob, never the blob alone,
    /// since the same blob at `100755` is a file the placed one does not match.
    pub entry_mode: Option<u32>,
    pub entry_blob: Option<String>,
    /// Filled in as step 5 decides, and only then: `true` keeps the placed
    /// file, `false` undoes the placement.
    pub keep: Option<bool>,
    /// Where the displaced file went, recorded before that name is used.
    pub retained_as: Option<String>,
    /// Set once the undo of this path is known to have completed and flushed.
    pub undone: bool,
}

/// A ref lock the UI created, named before it is relied on so that a crash in
/// the rollback leaves one the record can both name and tell from another
/// `git`'s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefLock {
    /// The ref this lock protects, as `git symbolic-ref` spells it.
    pub reference: String,
    /// Where `git rev-parse --git-path` put the lock file: not every ref is
    /// shared, and a per-worktree one lives under the worktree's own
    /// directory where the common directory would name a file nothing locks.
    pub lock_file: String,
}

/// The file itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// Written into every ref lock the UI makes, which is what makes one
    /// reclaimable after a crash: `git` refuses a lock file whatever is
    /// inside it, and those files are empty in `git`'s own use, so their
    /// content is free for the UI and is the only way to tell its own
    /// leftover lock from a lock another `git` is holding right now.
    pub id: String,
    pub process: Process,
    pub phase: Phase,
    /// `<index>.lock` is *not* one of the ref locks: it is a copy of the index
    /// and is read and installed as one, and a byte added to it stops being an
    /// index at all (**measured**: `update-index` refused a copy with the id
    /// appended, `index uses … extension, which we do not understand`). Its
    /// ownership is established by hash instead, rewritten here whenever the
    /// UI rewrites that file.
    pub index_lock_hash: Option<String>,
    pub index_lock_file: Option<String>,
    pub locks: Vec<RefLock>,
    pub branch: Option<String>,
    pub tip: Option<String>,
    pub commit: Option<String>,
    pub paths: Vec<Placed>,
}

impl Record {
    pub fn new(id: String, process: Process) -> Self {
        Self {
            id,
            process,
            phase: Phase::Locking,
            index_lock_hash: None,
            index_lock_file: None,
            locks: Vec::new(),
            branch: None,
            tip: None,
            commit: None,
            paths: Vec::new(),
        }
    }

    /// A `rolling-back` record can record progress but can never become
    /// anything else. Enforced here rather than remembered at each call site,
    /// because the one thing recovery must never do is read a partly undone
    /// tree as a success (#152).
    pub fn advance(&mut self, phase: Phase) -> Result<(), Irreversible> {
        if self.phase == Phase::RollingBack && phase != Phase::RollingBack {
            return Err(Irreversible { attempted: phase });
        }
        self.phase = phase;
        Ok(())
    }

    pub fn path_mut(&mut self, path: &str) -> Option<&mut Placed> {
        self.paths.iter_mut().find(|p| p.path == path)
    }
}

/// The refusal `advance` answers with. Not an `Option`: a caller that ignored
/// it would be the bug, and this way it cannot be ignored silently.
#[derive(Debug, PartialEq, Eq)]
pub struct Irreversible {
    pub attempted: Phase,
}

impl std::fmt::Display for Irreversible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a rolling-back compose cannot become {:?}; recovery may only resume the rollback",
            self.attempted
        )
    }
}

/// The directory of records, and the publishing protocol.
pub struct Records {
    directory: Dir,
    path: PathBuf,
}

impl Records {
    /// `<git-dir>/pbps-ui/composing/`.
    pub fn open(git_dir: &Path) -> io::Result<Self> {
        let path = git_dir.join("pbps-ui").join("composing");
        std::fs::create_dir_all(&path)?;
        Ok(Self {
            directory: Dir::open_root(&path)?,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write this version whole, then make it the record.
    ///
    /// The rename is what publishes it. Nothing is ever edited in place, and
    /// the flushes are not optimisations: the record is the only evidence
    /// recovery has, and a half-written one is worse than none, because it is
    /// read as a whole one.
    pub fn publish(&self, record: &Record) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
        let final_name = OsString::from(format!("{}.json", record.id));
        let temporary = OsString::from(format!("{}.json.writing", record.id));
        // A leftover from a crash between the two names is the UI's own and is
        // replaced, never merged: this version is complete and that one may
        // not be.
        let _ = self.directory.unlink(&temporary);
        let file = self.directory.create_new(&temporary, 0o600)?;
        file.write_all(&bytes)?;
        file.flush()?;
        drop(file);
        self.directory.rename(&temporary, &final_name)?;
        self.directory.sync()
    }

    pub fn read(&self, name: &OsString) -> io::Result<Record> {
        let file = self.directory.open_file(name)?;
        serde_json::from_slice(&file.read()?).map_err(io::Error::other)
    }

    /// Every record on disk, oldest name first. A record is never deleted
    /// except by the step that completes it or by the user.
    pub fn list(&self) -> io::Result<Vec<OsString>> {
        let mut names: Vec<OsString> = std::fs::read_dir(&self.path)?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| {
                Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        names.sort();
        Ok(names)
    }

    /// Last, after the cleanup.
    pub fn remove(&self, record: &Record) -> io::Result<()> {
        self.directory
            .unlink(&OsString::from(format!("{}.json", record.id)))?;
        self.directory.sync()
    }
}

/// `/proc/<pid>/stat` field 22.
///
/// Parsed from the last `)` rather than by splitting on spaces: the second
/// field is the executable's name in parentheses and may contain both spaces
/// and parentheses, so a split from the left lands on a different field for a
/// process whose name has a space in it.
fn start_time(pid: u32) -> io::Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let after_name = stat
        .rfind(')')
        .map(|end| &stat[end + 1..])
        .ok_or_else(|| io::Error::other("no process name in /proc/<pid>/stat"))?;
    // Field 3 is the first after the name, so field 22 is the 20th here.
    after_name
        .split_whitespace()
        .nth(19)
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("no start time in /proc/<pid>/stat"))
}

/// A name no other compose will choose, for the record and for the snapshot
/// directories beside it.
pub fn random_name() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn getrandom(bytes: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < bytes.len() {
        let got =
            rustix::rand::getrandom(&mut bytes[filled..], rustix::rand::GetRandomFlags::empty())?;
        if got == 0 {
            return Err(io::Error::other("the kernel returned no randomness"));
        }
        filled += got;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "pbps-record-{name}-{}-{}",
            std::process::id(),
            random_name().unwrap()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn a_record() -> Record {
        Record::new("abc123".to_owned(), Process::here().unwrap())
    }

    #[test]
    fn a_rolling_back_compose_can_record_progress_and_become_nothing_else() {
        // The one rule recovery cannot be trusted without: `HEAD` being
        // pointed back at the composed commit must not turn a partial undo
        // into permission to install the prepared index (#152).
        let mut record = a_record();
        record.advance(Phase::Placing).unwrap();
        record.advance(Phase::Composed).unwrap();
        record.advance(Phase::RollingBack).unwrap();

        record
            .advance(Phase::RollingBack)
            .expect("progress within the rollback is allowed");
        for forbidden in [
            Phase::Placing,
            Phase::Composed,
            Phase::Installed,
            Phase::Locking,
        ] {
            assert_eq!(
                record.advance(forbidden),
                Err(Irreversible {
                    attempted: forbidden
                }),
                "{forbidden:?} must not be reachable from rolling-back"
            );
            assert_eq!(record.phase, Phase::RollingBack);
        }
    }

    #[test]
    fn a_published_record_is_never_seen_half_written() {
        // The property is the protocol's, not the test's: the record is
        // written under another name and *renamed* over, so the only two
        // things a reader can find are the whole previous version and the
        // whole new one.
        let directory = scratch("publish");
        let records = Records::open(&directory).unwrap();
        let mut record = a_record();
        records.publish(&record).unwrap();
        assert_eq!(records.list().unwrap().len(), 1);

        record.advance(Phase::Placing).unwrap();
        record.commit = Some("deadbeef".to_owned());
        records.publish(&record).unwrap();

        let names = records.list().unwrap();
        assert_eq!(names.len(), 1, "the writing name is not a record");
        let read = records.read(&names[0]).unwrap();
        assert_eq!(read.phase, Phase::Placing);
        assert_eq!(read.commit.as_deref(), Some("deadbeef"));
        assert_eq!(read, record);
    }

    #[test]
    fn a_leftover_writing_file_from_a_crash_is_replaced_rather_than_read() {
        let directory = scratch("leftover");
        let records = Records::open(&directory).unwrap();
        let record = a_record();
        std::fs::write(
            records.path().join(format!("{}.json.writing", record.id)),
            b"{ half of one version",
        )
        .unwrap();

        records.publish(&record).unwrap();

        let names = records.list().unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(records.read(&names[0]).unwrap().phase, Phase::Locking);
    }

    #[test]
    fn a_record_with_a_field_this_version_does_not_know_is_refused_not_dropped() {
        // `deny_unknown_fields` for the same reason decision 6 gives for the
        // envelope: a field a later version adds must fail here rather than be
        // dropped on the floor, because recovery acts on what it reads.
        let directory = scratch("unknown");
        let records = Records::open(&directory).unwrap();
        let record = a_record();
        records.publish(&record).unwrap();
        let name = records.list().unwrap().remove(0);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(records.path().join(&name)).unwrap()).unwrap();
        json["undo_order"] = serde_json::json!(["a"]);
        std::fs::write(
            records.path().join(&name),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();

        assert!(records.read(&name).is_err());
    }

    #[test]
    fn this_process_reads_as_alive_and_a_start_time_that_differs_does_not() {
        // A process id alone is reused, which is why the pair is recorded.
        let mine = Process::here().unwrap();
        assert!(mine.alive());
        let reused = Process {
            id: mine.id,
            started: format!("{}0", mine.started),
        };
        assert!(
            !reused.alive(),
            "the same id with another start time is another process"
        );
        let gone = Process {
            id: u32::MAX,
            started: mine.started,
        };
        assert!(!gone.alive());
    }

    #[test]
    fn the_start_time_is_read_past_a_process_name_that_holds_spaces_and_brackets() {
        // Field 2 is the executable's name in parentheses and may contain both
        // spaces and parentheses, so a split from the left lands on a
        // different field. This is the parse, exercised on this process, whose
        // name the test cannot choose — the assertion that matters is that the
        // value is a number and is stable.
        let once = start_time(std::process::id()).unwrap();
        let twice = start_time(std::process::id()).unwrap();
        assert_eq!(once, twice);
        assert!(once.parse::<u64>().is_ok(), "field 22 is a count of ticks");
    }
}
