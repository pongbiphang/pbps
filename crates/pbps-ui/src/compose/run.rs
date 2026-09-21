//! The compose, in the order ADR-0015 decision 5 puts it.
//!
//! Every refusal below undoes what came before it and leaves the checkout as
//! it was, with anything displaced kept beside its path. The order is not a
//! convenience: the record is published before each thing it names, so a crash
//! anywhere leaves evidence that is at worst one step ahead of the disk and
//! never behind it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::cli::{Cli, CliRefusal, Intent};
use super::fsx::Dir;
use super::git::Git;
use super::listing::{self, Inputs, ListingRefusal, State};
use super::locks::{self, Held, LockRefusal};
use super::place::{PlaceRefusal, Placement, Wanted};
use super::push::{self, PushRefusal};
use super::record::{Phase, Placed, Process, Record, Records, RefLock, random_name};
use super::refs::{self, Lease, RefRefusal};
use super::repo_path::RepoPath;
use super::snapshot::{self, SnapshotRefusal};
use super::tree::{self, CacheEntry, TreeRefusal};

/// What the page asks for.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub intent: Intent,
    pub message: String,
    pub remote: String,
    /// What the page was shown for every path the preview represented:
    /// `Some(blob)` where the working tree held the file, and **`None` where
    /// it did not**.
    ///
    /// Absence has to be pinned as deliberately as content. A declaration the
    /// preview saw deleted, or one that did not exist yet, has no blob to
    /// compare — and without an entry saying so, an editor recreating it
    /// between the diff and the confirmation would be committed unread. The
    /// locks stop `git`, not an editor.
    #[serde(default)]
    pub shown: BTreeMap<String, Option<String>>,
}

/// How far a run goes (DECISIONS 524).
///
/// The page shows the diff *before* the commit, so the preview is a compose
/// that stops once the tree exists and then undoes itself under the ordinary
/// rollback protocol. It costs the placement twice, and buys the thing the
/// protocol otherwise cannot give: a human looking at the change while no
/// lock is held. Pausing mid-compose instead would hold the index lock — and
/// with it every `git` in the checkout — for as long as a person takes to
/// read a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Preview,
    Commit,
}

/// What the page is shown before it asks for the commit.
#[derive(Debug, serde::Serialize)]
pub struct Preview {
    pub branch: String,
    pub tip: String,
    /// `git diff <tip> <tree>`: the recorded tip against the tree the UI
    /// built, exact by construction and rendered without presentation
    /// filters.
    pub diff: String,
    pub paths: Vec<String>,
    /// Prefilled from the intent, spelled the way the CLI spells it.
    pub message: String,
    /// What the working tree held for every path this preview represents:
    /// `Some(blob)` where the file was there, `None` where it was not.
    ///
    /// Sent back with the confirmation, where it is compared against the
    /// working tree as it is *then*. Without it the editor-race guard has
    /// nothing to compare with and never fires; with only the paths that had
    /// content, a file recreated or deleted in between is still committed
    /// unread.
    pub shown: BTreeMap<String, Option<String>>,
}

/// Either answer.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "stage", rename_all = "kebab-case")]
pub enum Done {
    Previewed(Preview),
    Composed(Composed),
}

/// What a compose that finished produced.
#[derive(Debug, serde::Serialize)]
pub struct Composed {
    pub commit: String,
    pub branch: String,
    /// `git log -1 --format=%G?`: the organization's signing policy as the
    /// user's configuration applied it, not a guarantee this UI adds.
    pub signature: String,
    pub destination: Option<String>,
    /// A push that failed *after* the commit was made.
    ///
    /// A late transport failure is not "nothing happened": the branch has
    /// moved, the index is installed, and the working tree matches the
    /// commit. Reporting it as a refusal would tell the user their checkout
    /// was untouched while a commit sat on their branch, and the next thing
    /// they would do is make it again.
    pub push_refused: Option<String>,
    /// What a compose left beside a path, named so the user can find it. The
    /// UI never deletes one of these: a descriptor can outlive any number of
    /// composes.
    pub retained: Vec<String>,
    pub paths: Vec<String>,
    /// Said beside the commit, every time: a hook is the shell's policy at
    /// commit time and this commit did not run one.
    pub hooks_did_not_run: bool,
}

/// Why a compose did not happen, or did not finish.
#[derive(Debug)]
pub enum Refusal {
    /// macOS and Windows: decision 5's own answer for a platform whose calls
    /// are not measured is to refuse and give the commands to run by hand.
    Platform {
        commands: Vec<String>,
    },
    Ref(RefRefusal),
    Lock(LockRefusal),
    Listing(ListingRefusal),
    Snapshot(SnapshotRefusal),
    Place(PlaceRefusal),
    Tree(TreeRefusal),
    Push(PushRefusal),
    Cli(CliRefusal),
    /// The working tree's `pbps.yml` is not the recorded tip's. The working
    /// tree's configuration would answer the next question differently, and
    /// the user commits the configuration first, as they would before `pbps
    /// rename`.
    ConfigurationUncommitted {
        path: String,
    },
    /// A project whose declarations or ids file resolve outside its directory.
    InputsOutside {
        declarations: String,
        ids: String,
    },
    /// Something the record could not be told, which is the one failure that
    /// changes nothing rather than rolling back: an undo whose direction
    /// recovery cannot know must not be started.
    Unrecordable(String),
    /// A refusal whose rollback could not finish. The record is **kept**, so
    /// that the next launch can resume the undo from the same per-path
    /// evidence; removing it would leave a replacement or a displaced file in
    /// the checkout with nothing on disk to explain it.
    RollbackIncomplete {
        why: Box<Refusal>,
        unresolved: Vec<String>,
        record: String,
    },
    /// Another compose is running in this checkout right now.
    AlreadyComposing,
    /// The branch moved and then something moved out from under it: `HEAD`
    /// was redirected, the branch was moved again, or a lock could not be
    /// retaken. The commit exists and is where `update-ref` put it, and the
    /// checkout is *not* at it, so step 6 does not happen and nothing is
    /// pushed — but this is emphatically not "nothing changed".
    CommitStandsButCheckoutMoved {
        commit: String,
        branch: String,
        deciding: String,
        kept: Vec<String>,
        undone: Vec<String>,
    },
    Io(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Platform { .. } => write!(
                f,
                "composing is not supported on this platform yet; run the commands shown by hand"
            ),
            Self::Ref(r) => write!(f, "{r}"),
            Self::Lock(r) => write!(f, "{r}"),
            Self::Listing(r) => write!(f, "{r}"),
            Self::Snapshot(r) => write!(f, "{r}"),
            Self::Place(r) => write!(f, "{r}"),
            Self::Tree(r) => write!(f, "{r}"),
            Self::Push(r) => write!(f, "{r}"),
            Self::Cli(r) => write!(f, "{r}"),
            Self::ConfigurationUncommitted { path } => write!(
                f,
                "`{path}` differs from the commit this checkout is on; commit it first, \
                 as you would before running the intent in a shell"
            ),
            Self::InputsOutside { declarations, ids } => write!(
                f,
                "this project's inputs lie outside its directory ({declarations}, {ids}); \
                 composing for that layout is not supported and the shell's commands work as before"
            ),
            Self::Unrecordable(detail) => write!(
                f,
                "the compose could not write its own recovery record ({detail}), \
                 so nothing was changed"
            ),
            Self::AlreadyComposing => write!(
                f,
                "another compose is running in this checkout; wait for it to finish"
            ),
            Self::CommitStandsButCheckoutMoved {
                commit,
                branch,
                deciding,
                kept,
                undone,
            } => write!(
                f,
                "the commit {commit} was made on {branch}, but this checkout moved while it was \
                 being made and is now at {deciding}. Your index was not changed and nothing was \
                 pushed. Files kept: {}. Files put back: {}. Only you can say which of the two \
                 states you want.",
                if kept.is_empty() {
                    "none".to_owned()
                } else {
                    kept.join(", ")
                },
                if undone.is_empty() {
                    "none".to_owned()
                } else {
                    undone.join(", ")
                }
            ),
            Self::RollbackIncomplete {
                why,
                unresolved,
                record,
            } => write!(
                f,
                "{why} — and putting the checkout back did not finish: {}. \
                 The record `{record}` has been kept; restart the viewer and it will resume \
                 from where it stopped.",
                unresolved.join("; ")
            ),
            Self::Io(detail) => write!(f, "{detail}"),
        }
    }
}

impl Refusal {
    /// Whether this refusal left the repository different from how it found
    /// it.
    ///
    /// The page has to say which. "Nothing was changed; your checkout is as it
    /// was" is true of almost every refusal here and *false* of exactly two —
    /// a commit that stands on a branch whose checkout moved, and a rollback
    /// that could not finish — and a user told the wrong one of those will
    /// retry against a repository that has already moved.
    pub fn left_changes(&self) -> bool {
        matches!(
            self,
            Self::CommitStandsButCheckoutMoved { .. } | Self::RollbackIncomplete { .. }
        )
    }
}

macro_rules! from_refusal {
    ($($variant:ident => $type:ty),* $(,)?) => {
        $(impl From<$type> for Refusal {
            fn from(refusal: $type) -> Self {
                Self::$variant(refusal)
            }
        })*
    };
}
from_refusal! {
    Ref => RefRefusal,
    Lock => LockRefusal,
    Listing => ListingRefusal,
    Snapshot => SnapshotRefusal,
    Place => PlaceRefusal,
    Tree => TreeRefusal,
    Push => PushRefusal,
    Cli => CliRefusal,
}

/// Everything a compose needs that the UI knew at launch.
pub struct Compose<'a> {
    pub git: &'a Git,
    pub cli: &'a Cli,
    /// The project's directory relative to the worktree root, or `None` where
    /// the project *is* the worktree root — which is the ordinary case, and
    /// the one `RepoPath` cannot spell, since `.` is a component it refuses.
    pub project: Option<&'a RepoPath>,
    /// `pbps.yml` within it.
    pub project_file: &'a RepoPath,
    pub git_dir: PathBuf,
    /// `pbps-ui-<random>`, drawn once per launch.
    pub remote_name: String,
    /// Called with each phase after it has been published and flushed.
    ///
    /// The page uses it to say where a compose has got to — a compose spends
    /// most of its time in a subprocess, and a browser waiting on a request
    /// with nothing to show is a compose the user cannot tell from a hung one.
    /// The interruption tests use it for the other thing a published phase
    /// means: that the compose can be stopped here and recovered from.
    pub watching: Option<&'a (dyn Fn(Phase) + Sync)>,
}

impl Compose<'_> {
    /// The pathspec that names the project's subtree. `.` at the worktree
    /// root, where `git` reads it as "everything here" and a `RepoPath` has
    /// nothing to say.
    fn pathspec(&self) -> Result<&str, Refusal> {
        match self.project {
            None => Ok("."),
            Some(project) => project.to_text().ok_or_else(|| {
                Refusal::Io("this project's path is not text git can be given".to_owned())
            }),
        }
    }

    /// Where the project sits inside a snapshot written from a tree.
    fn within(&self, snapshot: &Path) -> PathBuf {
        match self.project.and_then(RepoPath::to_text) {
            None => snapshot.to_path_buf(),
            Some(relative) => snapshot.join(relative),
        }
    }

    /// The scratch a single run needs and nothing keeps.
    fn sweep(&self, id: &str) {
        let under = self.git_dir.join("pbps-ui");
        let _ = std::fs::remove_dir_all(under.join("intent").join(id));
        let _ = std::fs::remove_dir_all(under.join("validate").join(id));
        let _ = std::fs::remove_file(under.join(format!("{id}.index")));
    }

    fn reached(&self, phase: Phase) {
        if let Some(watching) = self.watching {
            watching(phase);
        }
    }

    /// The whole of it. A `Refusal` here means the checkout is as it was.
    pub fn run(&self, request: &Request) -> Result<Composed, Refusal> {
        match self.attempt(request, Stage::Commit)? {
            Done::Composed(composed) => Ok(composed),
            Done::Previewed(_) => unreachable!("a commit stage answers with a commit"),
        }
    }

    /// The same work, stopped once the tree exists, and undone.
    pub fn preview(&self, request: &Request) -> Result<Preview, Refusal> {
        match self.attempt(request, Stage::Preview)? {
            Done::Previewed(preview) => Ok(preview),
            Done::Composed(_) => unreachable!("a preview stage answers with a preview"),
        }
    }

    fn attempt(&self, request: &Request, stage: Stage) -> Result<Done, Refusal> {
        let records = Records::open(&self.git_dir).map_err(|e| Refusal::Io(e.to_string()))?;
        if self.someone_else_is_composing(&records)? {
            return Err(Refusal::AlreadyComposing);
        }
        let id = random_name().map_err(|e| Refusal::Io(e.to_string()))?;
        let mut record = Record::new(
            id.clone(),
            Process::here().map_err(|e| Refusal::Io(e.to_string()))?,
        );
        // The record is the compose's first act, before any lock exists.
        records
            .publish(&record)
            .map_err(|e| Refusal::Unrecordable(e.to_string()))?;

        let outcome = self.compose(request, stage, &records, &mut record);
        // A preview owns its record until it has put everything back, which
        // `compose` has done by here; a refusal is the same shape.
        // A refused compose still owns its record until everything it made has
        // been put back; `compose` has done that by here, so the record goes
        // last, as it does on the success path. The exception is a rollback
        // that could not finish: its record is the only thing that can tell
        // the next launch what is still out of place.
        let incomplete = matches!(outcome, Err(Refusal::RollbackIncomplete { .. }));
        if !incomplete && (outcome.is_err() || matches!(outcome, Ok(Done::Previewed(_)))) {
            let _ = records.remove(&record);
        }
        // Whatever the outcome. Each run writes the project's subtree twice —
        // once for the intent command, once for `validate` — under a fresh
        // random name, so a user reading three previews before committing
        // would otherwise leave six copies behind, and nothing would ever
        // remove them. The record is the only thing under `pbps-ui` that
        // outlives its run, and only while it has something to say.
        self.sweep(&record.id);
        outcome
    }

    /// A record whose process is still alive is a compose someone is running
    /// right now, in this UI or another launched beside it: nothing of it is
    /// reclaimed and this UI refuses to compose while it stands, because every
    /// leftover it would otherwise recognise as its own kind may be that
    /// compose's, in use this instant.
    fn someone_else_is_composing(&self, records: &Records) -> Result<bool, Refusal> {
        for name in records.list().map_err(|e| Refusal::Io(e.to_string()))? {
            match records.read(&name) {
                Ok(found) if found.process.alive() => return Ok(true),
                Ok(_) => {}
                // A record this UI cannot read is not a record that is not
                // there. It may be a live compose's, and its locks and placed
                // files are then in use this instant.
                Err(_) => return Ok(true),
            }
        }
        Ok(false)
    }

    fn compose(
        &self,
        request: &Request,
        stage: Stage,
        records: &Records,
        record: &mut Record,
    ) -> Result<Done, Refusal> {
        let root = Dir::open_root(self.git.root()).map_err(|e| Refusal::Io(e.to_string()))?;

        // Step 1's ref half, before anything is read or placed.
        let lease = refs::lease(self.git)?;
        record.branch = Some(lease.reference.clone());
        record.tip = Some(lease.tip.clone());

        // The compose is refused first unless the working tree's `pbps.yml` is
        // the tip's: the working tree's configuration would answer the next
        // question differently.
        self.project_file_matches_the_tip(&root, &lease.tip)?;

        // The snapshot the intent command will run in.
        let intent_directory = self.git_dir.join("pbps-ui").join("intent").join(&record.id);
        let recorded =
            snapshot::write_out(self.git, &lease.tip, self.pathspec()?, &intent_directory)?;
        let snapshot_project = self.within(&intent_directory);

        // Where the inputs are, asked of the CLI in the snapshot.
        let inputs = self.cli.where_are_the_inputs(&snapshot_project)?;
        let (declarations, ids_file) = self.inputs_inside(&snapshot_project, &inputs)?;

        // The listing: what the working tree has done to those inputs.
        let listed = listing::inputs(
            self.git,
            &lease.tip,
            &Inputs {
                declarations: declarations.as_ref(),
                ids_file: &ids_file,
            },
            |path| self.look_at(&root, path),
        )?;

        // Every deterministic question about the destination is asked here,
        // before a file is placed: ADR-0015 decision 5 reads the remote's tip
        // "before composing", and a refusal that arrives after the branch has
        // moved is one the user cannot act on.
        let (destination, remote_has_branch) = self.destination_for(request, &lease)?;

        // The overlay, and then the command.
        // What the page was shown, compared against the working tree as it is
        // now — *before* the intent command runs. Asked again under the locks
        // below, which is where the answer is authoritative; asked here so
        // that a file which came back while the diff was being read is named
        // as such, rather than reaching the intent command and being refused
        // as an intent that matches nothing.
        self.still_as_shown(&root, request)?;

        self.overlay(&root, &intent_directory, &listed)?;
        self.cli.record(&snapshot_project, &request.intent)?;

        // What the command changed, found by hashing the snapshot against
        // what was laid into it.
        let changed = self.changed_by_command(&intent_directory, &recorded, &listed)?;

        // Step 0: the locks.
        let index_file = locks::index_file(self.git)?;
        let index_lock = PathBuf::from(format!("{}.lock", index_file.display()));
        self.copy_index(&index_file, &index_lock)?;
        record.index_lock_file = Some(index_lock.display().to_string());
        record.index_lock_hash = Some(hash_file(&index_lock).map_err(Refusal::Io)?);
        let head_lock_file = locks::lock_file_for(self.git, "HEAD")?;
        let head_lock = locks::take(
            self.git.root(),
            &head_lock_file,
            record.id.as_bytes(),
            &record.id,
        )
        .map_err(|refusal| {
            let _ = std::fs::remove_file(&index_lock);
            Refusal::Lock(refusal)
        })?;
        // The guard is built *here*, before the record is republished, and not
        // after: both lock files already exist, and a failure to write or
        // flush that version would otherwise leave them on disk with nothing
        // releasing them and no record for recovery to clear them from. A
        // guard that starts one fallible call later is a guard with a hole in
        // it exactly where the first thing that can fail is.
        let held = Guard {
            index_lock: index_lock.clone(),
            head: Some(head_lock),
            branch: None,
        };
        record.locks.push(RefLock {
            reference: "HEAD".to_owned(),
            lock_file: held
                .head
                .as_ref()
                .expect("just taken")
                .path()
                .display()
                .to_string(),
        });
        records
            .publish(record)
            .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
        self.reached(Phase::Locking);

        // From here a refusal has locks and possibly files to put back, so the
        // work happens in one place and the cleanup is the same either way.
        let mut placement = Placement::new(
            Dir::open_root(self.git.root()).map_err(|e| Refusal::Io(e.to_string()))?,
        );
        let result = self.place_and_commit(
            request,
            stage,
            records,
            record,
            &lease,
            &root,
            &intent_directory,
            &listed,
            &changed,
            &index_file,
            &index_lock,
            held,
            &mut placement,
            destination,
            remote_has_branch,
        );
        match result {
            Ok(done) => Ok(done),
            // The deciding-ref protocol has already made a per-path decision
            // and acted on it; running the generic rollback over that would
            // undo the very paths it decided to keep and leave the checkout
            // disagreeing with the tip it was decided against. A rollback that
            // could not finish has likewise already run.
            Err(
                why @ (Refusal::CommitStandsButCheckoutMoved { .. }
                | Refusal::RollbackIncomplete { .. }),
            ) => Err(why),
            Err(why) => {
                // Every undo publishes `rolling-back` first; the record's
                // per-path evidence is what makes retrying an interrupted undo
                // safe.
                if record.advance(Phase::RollingBack).is_ok() {
                    let _ = records.publish(record);
                    self.reached(Phase::RollingBack);
                }
                let unresolved = self.put_back(&mut placement, record);
                if unresolved.is_empty() {
                    Err(why)
                } else {
                    Err(Refusal::RollbackIncomplete {
                        why: Box::new(why),
                        unresolved,
                        record: record.id.clone(),
                    })
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn place_and_commit(
        &self,
        request: &Request,
        stage: Stage,
        records: &Records,
        record: &mut Record,
        lease: &Lease,
        root: &Dir,
        intent_directory: &Path,
        listed: &[listing::Input],
        changed: &BTreeMap<RepoPath, Vec<u8>>,
        index_file: &Path,
        index_lock: &Path,
        mut held: Guard,
        placement: &mut Placement,
        destination: Option<push::Destination>,
        remote_has_branch: bool,
    ) -> Result<Done, Refusal> {
        // Everything the compose will commit: the listed files, which already
        // hold their bytes in the working tree, and every file the command
        // left different, which has to be placed. The command's output wins
        // for a path in both sets — an ids file the user had already edited is
        // exactly that case.
        let mut to_place: Vec<Wanted> = Vec::new();
        for (path, bytes) in changed {
            to_place.push(Wanted {
                path: path.clone(),
                bytes: bytes.clone(),
                // The outer option is kept: `Some(None)` is "the preview
                // expected nothing here", which is an instruction, and `None`
                // is "the preview said nothing about this path", which is not.
                shown: request.shown.get(&path.to_string()).cloned(),
            });
        }
        let untouched: Vec<&listing::Input> = listed
            .iter()
            .filter(|input| !changed.contains_key(&input.path))
            .filter(|input| input.state != State::Unchanged)
            .collect();

        // Step 1's per-path checks, for everything that will be committed —
        // the listed files *and* the ones the command produced. Sweeping the
        // same shape the attribute check was found missing on: a path this
        // compose is about to edit can be one the listing never named, and its
        // staged entry matters just as much.
        // Again, under the locks, which is where it decides.
        self.still_as_shown(root, request)?;

        let mut checked = BTreeSet::new();
        for path in listed
            .iter()
            .map(|input| &input.path)
            .chain(to_place.iter().map(|wanted| &wanted.path))
        {
            if checked.insert(path.clone()) {
                self.index_entry_is_the_tips(path, &lease.tip)?;
            }
        }

        // The `placing` phase, with both identities of every path, before the
        // first exchange.
        let mut prepared = Vec::new();
        for wanted in &to_place {
            let (intent, ready) = placement.intend(self.git, wanted, &record.id)?;
            record.paths.push(Placed {
                path: intent.path,
                temporary: intent.temporary,
                original: intent
                    .original
                    .map(|(blob, device, inode)| super::record::Identity {
                        blob,
                        device,
                        inode,
                    }),
                replacement: super::record::Identity {
                    blob: intent.replacement.0,
                    device: intent.replacement.1,
                    inode: intent.replacement.2,
                },
                entry_mode: None,
                entry_blob: None,
                keep: None,
                retained_as: None,
                undone: false,
            });
            prepared.push(ready);
        }
        record
            .advance(Phase::Placing)
            .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
        records
            .publish(record)
            .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
        self.reached(Phase::Placing);

        for ready in prepared {
            placement.install(self.git, &lease.tip, ready)?;
        }

        // Step 3: the tree, `validate`, and the prepared index.
        let mut entries = Vec::new();
        let mut removals = Vec::new();
        for input in listed {
            if input.state == State::Deleted && !changed.contains_key(&input.path) {
                removals.push(input.path.clone());
            }
        }
        for wanted in &to_place {
            let blob = super::attributes::store(self.git, &wanted.bytes)
                .map_err(|e| Refusal::Place(PlaceRefusal::Attribute(e)))?;
            let mode = self.mode_for(listed, &wanted.path, placement);
            entries.push(CacheEntry {
                mode,
                blob: blob.clone(),
                path: wanted.path.clone(),
            });
            if let Some(placed) = record.path_mut(&wanted.path.to_string()) {
                placed.entry_mode = Some(mode);
                placed.entry_blob = Some(blob);
            }
        }
        for input in untouched {
            if input.state == State::Deleted {
                continue;
            }
            let bytes = self.read_through_handle(root, &input.path).ok_or_else(|| {
                Refusal::Place(PlaceRefusal::Changed {
                    path: input.path.to_string(),
                })
            })?;
            // The last read of a listed file, and therefore the last chance
            // to notice a save. `still_as_shown` looked at this path twice
            // already, and an editor can write between any two looks; these
            // are the exact bytes about to become a blob, so this is the
            // comparison that decides.
            let now = super::attributes::hash(self.git, &bytes)
                .map_err(|e| Refusal::Place(PlaceRefusal::Attribute(e)))?;
            if let Some(expected) = request.shown.get(&input.path.to_string())
                && *expected != Some(now.clone())
            {
                return Err(Refusal::Place(PlaceRefusal::Changed {
                    path: input.path.to_string(),
                }));
            }
            // The attribute check belongs to every path the commit holds, not
            // only to the ones step 2 placed. A listed file keeps the bytes
            // the user gave it and the UI never writes it — but it does store
            // a blob for it, and a `filter` or an `ident` rule would leave
            // that blob disagreeing with the file, so `git status` reports the
            // path modified the moment it is committed. Found by a test
            // written for something else: the compose it was supposed to
            // refuse succeeded, because the only filtered path was one that
            // nothing placed.
            super::attributes::refuse_transformations(
                self.git,
                input.path.to_text().ok_or_else(|| {
                    Refusal::Io("a listed path is not text git can be given".to_owned())
                })?,
                &lease.tip,
                &bytes,
                &bytes,
            )
            .map_err(|e| Refusal::Place(PlaceRefusal::Attribute(e)))?;
            let blob = super::attributes::store(self.git, &bytes)
                .map_err(|e| Refusal::Place(PlaceRefusal::Attribute(e)))?;
            entries.push(CacheEntry {
                mode: self.mode_for(listed, &input.path, placement),
                blob,
                path: input.path.clone(),
            });
        }

        let private_index = self
            .git_dir
            .join("pbps-ui")
            .join(format!("{}.index", record.id));
        let tree = tree::write_tree(self.git, &private_index, &lease.tip, &entries, &removals)?;
        let validate_directory = self
            .git_dir
            .join("pbps-ui")
            .join("validate")
            .join(&record.id);
        snapshot::write_out(self.git, &tree, self.pathspec()?, &validate_directory)?;
        self.cli.validate(&self.within(&validate_directory))?;
        tree::prepare_index(self.git, index_lock, &entries, &removals)?;
        record.index_lock_hash = Some(hash_file(index_lock).map_err(Refusal::Io)?);

        if stage == Stage::Preview {
            // The preview refuses itself on purpose: the caller gets the diff
            // and the checkout gets everything back, through the same rollback
            // a real refusal here would take — published first, like every
            // other undo, so that a crash in the middle of one resumes as a
            // rollback rather than as a success.
            let diff = tree::preview(self.git, &lease.tip, &tree)?;
            record
                .advance(Phase::RollingBack)
                .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
            records
                .publish(record)
                .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
            self.reached(Phase::RollingBack);
            let unresolved = self.put_back(placement, record);
            held.release();
            if !unresolved.is_empty() {
                // A preview that could not put everything back is not a
                // preview that changed nothing, and its record is the only
                // thing that can tell the next launch what is still out of
                // place.
                return Err(Refusal::RollbackIncomplete {
                    why: Box::new(Refusal::Io("the preview could not be undone".to_owned())),
                    unresolved,
                    record: record.id.clone(),
                });
            }
            return Ok(Done::Previewed(Preview {
                branch: lease.reference.clone(),
                tip: lease.tip.clone(),
                diff: String::from_utf8_lossy(&diff).into_owned(),
                paths: entries
                    .iter()
                    .map(|entry| entry.path.to_string())
                    .chain(removals.iter().map(RepoPath::to_string))
                    .collect(),
                message: request.intent.message(),
                // Every path this preview represents, content or absence,
                // read from the working tree rather than from the record:
                // the record holds only what step 2 placed, and a listed file
                // or a deletion is neither.
                shown: entries
                    .iter()
                    .map(|entry| &entry.path)
                    .chain(removals.iter())
                    .map(|path| (path.to_string(), self.hash_of(root, path)))
                    .collect(),
            }));
        }

        // Step 4: the commit.
        let commit = tree::commit_tree(
            self.git,
            &tree,
            &lease.tip,
            &request.message,
            tree::signing_wanted(self.git),
        )?;
        record.commit = Some(commit.clone());

        // Every file step 2 wrote is flushed before `composed`, because what
        // makes the interval recoverable is that the tree on disk is the one
        // the record describes.
        root.sync().map_err(|e| Refusal::Io(e.to_string()))?;
        record
            .advance(Phase::Composed)
            .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
        records
            .publish(record)
            .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
        self.reached(Phase::Composed);

        // Step 5. `update-ref` takes `HEAD`'s lock itself when `HEAD` names
        // the branch it moves, so the UI gives that lock up across the *whole*
        // transaction and retakes it afterwards.
        if let Some(head) = held.head.take() {
            head.release().map_err(|e| Refusal::Io(e.to_string()))?;
        }
        let prepared = refs::prepare(self.git, lease, &commit)?;
        if let Err(refusal) = prepared.still_direct(self.git, lease) {
            prepared.abort()?;
            return Err(Refusal::Ref(refusal));
        }
        prepared.commit()?;

        // From here the branch has moved, and every failure below is a
        // different kind of failure from the ones above it: the commit exists
        // and is where `update-ref` put it. Treating one of these as a
        // pre-write refusal would restore every placed file, remove the
        // record, and report that nothing had changed — with a commit sitting
        // on the branch. So they all go to the deciding-ref protocol instead.
        let retaken = (|| -> Result<(), Refusal> {
            let head_lock_file = locks::lock_file_for(self.git, "HEAD")?;
            held.head = Some(locks::take(
                self.git.root(),
                &head_lock_file,
                record.id.as_bytes(),
                &record.id,
            )?);
            let branch_lock_file = locks::lock_file_for(self.git, &lease.reference)?;
            held.branch = Some(locks::take(
                self.git.root(),
                &branch_lock_file,
                record.id.as_bytes(),
                &record.id,
            )?);
            record.locks.push(RefLock {
                reference: lease.reference.clone(),
                lock_file: branch_lock_file.display().to_string(),
            });
            refs::post_write(self.git, lease, &commit)?;
            Ok(())
        })();
        if retaken.is_err() {
            return self.decide_by_the_ref_the_checkout_is_on(
                records, record, &commit, lease, held, placement,
            );
        }

        // Step 6: the rename that is exactly the commit step of `git`'s own
        // lock. Only a compose that has never entered `rolling-back` gets
        // here — and if it cannot be done, the branch has already moved, so
        // this goes to the deciding-ref protocol like every other failure on
        // this side of the transaction rather than through the generic arm.
        if std::fs::rename(index_lock, index_file).is_err() {
            return self.decide_by_the_ref_the_checkout_is_on(
                records, record, &commit, lease, held, placement,
            );
        }
        // From here the compose has happened: the branch holds the commit, the
        // index is installed and the working tree matches both. Nothing below
        // may return `Err` — a cleanup that fails is something to *say*, not a
        // reason to undo a commit that stands.
        let mut trouble = Vec::new();
        if let Err(e) = record
            .advance(Phase::Installed)
            .map_err(|e| e.to_string())
            .and_then(|()| records.publish(record).map_err(|e| e.to_string()))
        {
            trouble.push(format!("the record could not be updated: {e}"));
        }
        self.reached(Phase::Installed);

        let previous_directory = self.git_dir.join("pbps-ui").join("previous");
        let reported = match std::fs::create_dir_all(&previous_directory)
            .map_err(|e| e.to_string())
            .and_then(|()| Dir::open_root(&previous_directory).map_err(|e| e.to_string()))
        {
            Ok(previous) => placement.finish(&previous),
            Err(e) => {
                trouble.push(format!(
                    "the displaced files are still beside their paths: {e}"
                ));
                Vec::new()
            }
        };
        let retained: Vec<String> = reported
            .iter()
            .map(|outcome| format!("{outcome:?}"))
            .chain(trouble)
            .collect();

        held.release();
        let _ = records.remove(record);
        let _ = std::fs::remove_dir_all(intent_directory);

        // The push, last, and only once the checkout is the commit's. Every
        // *deterministic* question about the destination — how many URLs it
        // has, whether a rewrite rule would move it, whether the branch is
        // behind the remote — was asked before anything was placed, so what is
        // left here is the transport, and a transport failure is reported
        // beside the commit rather than instead of it.
        let published = match &destination {
            None => Ok(None),
            Some(destination) => {
                let created = if remote_has_branch {
                    Ok(())
                } else {
                    push::publish_branch(self.git, destination, &lease.reference, &lease.tip)
                };
                created.and_then(|()| {
                    push::push(self.git, destination, &lease.reference, &lease.tip, &commit)
                        .map(Some)
                })
            }
        };

        Ok(Done::Composed(Composed {
            commit: commit.clone(),
            branch: lease.reference.clone(),
            signature: tree::signature_state(self.git, &commit),
            destination: published.as_ref().ok().and_then(Clone::clone),
            push_refused: published.err().map(|refusal| refusal.to_string()),
            retained,
            paths: entries
                .iter()
                .map(|entry| entry.path.to_string())
                .chain(removals.iter().map(RepoPath::to_string))
                .collect(),
            hooks_did_not_run: true,
        }))
    }

    /// Undo every placement and take the replacements out of the working
    /// tree, answering whatever could not be done.
    ///
    /// The retention is not tidiness. An exchange that is swapped back leaves
    /// the *replacement* under the temporary name beside the path, and that
    /// file was at the path for a moment — an editor that opened it there
    /// holds its inode and may still save through it — so it is moved under
    /// `previous/` and kept rather than deleted, exactly as a swapped-out
    /// original is. Without this a preview leaves a file beside every
    /// declaration it touched, every time.
    fn put_back(&self, placement: &mut Placement, record: &Record) -> Vec<String> {
        let mut unresolved: Vec<String> = placement
            .undo_all()
            .into_iter()
            .filter_map(|outcome| match outcome {
                super::place::Undone::Reported { path, detail } => {
                    Some(format!("{path}: {detail}"))
                }
                super::place::Undone::Restored { .. }
                | super::place::Undone::RenamedAway { .. } => None,
            })
            .collect();
        let previous_directory = self.git_dir.join("pbps-ui").join("previous");
        match std::fs::create_dir_all(&previous_directory)
            .map_err(|e| e.to_string())
            .and_then(|()| Dir::open_root(&previous_directory).map_err(|e| e.to_string()))
        {
            Ok(previous) => {
                for outcome in placement.retain_undone(&previous, &record.id) {
                    if let super::place::Undone::Reported { path, detail } = outcome {
                        unresolved.push(format!("{path}: {detail}"));
                    }
                }
            }
            Err(e) => unresolved.push(format!("the displaced files could not be kept: {e}")),
        }
        unresolved
    }

    /// Every path the preview pinned still holds what it held — content where
    /// it had content, and *nothing* where it had nothing.
    fn still_as_shown(&self, root: &Dir, request: &Request) -> Result<(), Refusal> {
        for (path, expected) in &request.shown {
            let Ok(named) = RepoPath::new(path.as_bytes()) else {
                return Err(Refusal::Place(PlaceRefusal::Changed { path: path.clone() }));
            };
            let now = match self.look_at(root, &named) {
                listing::Seen::Present { blob, .. } => Some(blob),
                listing::Seen::Absent => None,
                listing::Seen::Unusable(why) => {
                    return Err(Refusal::Listing(ListingRefusal::Unusable {
                        path: path.clone(),
                        why,
                    }));
                }
            };
            if now != *expected {
                return Err(Refusal::Place(PlaceRefusal::Changed { path: path.clone() }));
            }
        }
        Ok(())
    }

    /// What to do when the branch moved and then something else did too.
    ///
    /// ADR-0015 decision 5: what becomes of the placed files is decided by the
    /// tip of the branch the checkout is *on*, held still while it decides,
    /// and never by the assumption that it is the UI's commit. Where `HEAD`
    /// names another ref, the placed files belong to *that* checkout now and
    /// the branch the UI advanced has nothing to say about them.
    ///
    /// A chain is refused rather than chased, for the reason step 1 refuses
    /// one: every link would have to be locked and asked again to hold one tip
    /// still, so it decides nothing and step 2 is undone for every path. The
    /// same answer covers a detached `HEAD` with no branch to ask about, an
    /// unborn target, a target no tree can be read from, and a target whose
    /// lock cannot be created because another `git` is mid-transaction on it.
    fn decide_by_the_ref_the_checkout_is_on(
        &self,
        records: &Records,
        record: &mut Record,
        commit: &str,
        lease: &Lease,
        mut held: Guard,
        placement: &mut Placement,
    ) -> Result<Done, Refusal> {
        let (deciding, deciding_lock) = self.deciding_tip(record, lease);
        // The whole entry set of the deciding tip for the paths in question,
        // read as *entries*: the same blob at `100755` is a file the placed
        // one does not match, and a `120000` or `160000` entry with that id is
        // not a file at all.
        let (kept, selected) = decide_paths(deciding.as_deref(), &record.paths, |tip, path| {
            self.entry_at(tip, path)
        });
        for placed in &mut record.paths {
            placed.keep = Some(kept.contains(&placed.path));
        }
        // Published *before* the first undo, with the complete per-path
        // decision. A failure to publish leaves every placement as it is and
        // reports: an undo whose direction recovery cannot know must not be
        // started.
        record
            .advance(Phase::RollingBack)
            .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
        records
            .publish(record)
            .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
        self.reached(Phase::RollingBack);

        let keeping: BTreeSet<String> = kept.iter().cloned().collect();
        let unresolved: Vec<String> = placement
            .undo_selected(&|path: &str| keeping.contains(path))
            .into_iter()
            .filter_map(|outcome| match outcome {
                super::place::Undone::Reported { path, detail } => {
                    Some(format!("{path}: {detail}"))
                }
                super::place::Undone::Restored { .. }
                | super::place::Undone::RenamedAway { .. } => None,
            })
            .collect();
        // A kept path still owes step 2's retention: the file it displaced is
        // beside it under a temporary name and must not be left there.
        let previous_directory = self.git_dir.join("pbps-ui").join("previous");
        let _ = std::fs::create_dir_all(&previous_directory);
        if let Ok(previous) = Dir::open_root(&previous_directory) {
            // The kept paths owe their displaced originals; the undone ones
            // owe the replacements that were briefly at the path.
            placement.finish(&previous);
            placement.retain_undone(&previous, &record.id);
        }
        // The prepared index is discarded without being installed, and the
        // locks go with it. The record stays only if something could not be
        // put back, which `undo_selected` reports through the page.
        held.release();
        // Held from before the tip was read until after every selected path
        // has been undone: releasing it the moment the tip was read would let
        // another worktree advance that ref during the decision, and the files
        // would then be kept or undone against a tip the checkout no longer
        // has.
        if let Some(lock) = deciding_lock {
            let _ = lock.release();
        }
        let why = Refusal::CommitStandsButCheckoutMoved {
            commit: commit.to_owned(),
            branch: lease.reference.clone(),
            deciding: deciding.unwrap_or_else(|| "a ref with no commit".to_owned()),
            kept,
            undone: selected,
        };
        if unresolved.is_empty() {
            let _ = records.remove(record);
            Err(why)
        } else {
            // The record stays: the checkout is not what the decision says it
            // is, and this record is the only thing that can tell the next
            // launch which paths are still out of place.
            Err(Refusal::RollbackIncomplete {
                why: Box::new(why),
                unresolved,
                record: record.id.clone(),
            })
        }
    }

    /// The tip that decides, read from the name its own lock holds.
    ///
    /// `None` where nothing can be held still long enough to decide by: a
    /// chain, a lock another `git` holds, or a target with no commit. Every
    /// one of those undoes step 2 for every path, which asks no tip at all.
    fn deciding_tip(&self, record: &mut Record, lease: &Lease) -> (Option<String>, Option<Held>) {
        let Ok(target) = refs::head_names(self.git) else {
            return (None, None);
        };
        let Ok(lock_file) = locks::lock_file_for(self.git, &target) else {
            return (None, None);
        };
        // Taken here the way the others are, and named in the record before it
        // is relied on, so a crash in the rollback leaves one the record can
        // both name and tell from another `git`'s.
        record.locks.push(RefLock {
            reference: target.clone(),
            lock_file: lock_file.display().to_string(),
        });
        let Ok(held) = locks::take(
            self.git.root(),
            &lock_file,
            record.id.as_bytes(),
            &record.id,
        ) else {
            return (None, None);
        };
        let tip = refs::must_be_direct(self.git, &target)
            .ok()
            .and_then(|()| refs::tip_of(self.git, &target).ok());
        let _ = lease;
        // The lock goes back to the caller, not released here: the value it
        // holds still has to be true when the per-path decision is published
        // and acted on.
        (tip, Some(held))
    }

    /// One path's whole entry in a tree: the mode and the object id.
    fn entry_at(&self, tree_ish: &str, path: &str) -> Option<(u32, String)> {
        let answer = self
            .git
            .run(&["ls-tree", "-z", tree_ish, "--", path])
            .ok()?;
        if !answer.ok() {
            return None;
        }
        let record = answer.stdout.split(|b| *b == 0).find(|r| !r.is_empty())?;
        let tab = record.iter().position(|b| *b == b'\t')?;
        let head = String::from_utf8_lossy(&record[..tab]).into_owned();
        let mut fields = head.split_whitespace();
        let mode = u32::from_str_radix(fields.next()?, 8).ok()?;
        let _kind = fields.next()?;
        Some((mode, fields.next()?.to_owned()))
    }

    /// The destination, and whether the remote already has the branch.
    ///
    /// Answered before anything is placed. The branch is *not* created here
    /// even when the remote lacks it: publishing a branch is a visible act the
    /// user confirms, and a preview must leave the remote as it found it.
    fn destination_for(
        &self,
        request: &Request,
        lease: &Lease,
    ) -> Result<(Option<push::Destination>, bool), Refusal> {
        if request.remote.is_empty() {
            return Ok((None, true));
        }
        let destination = push::destination(self.git, &request.remote, self.remote_name.clone())?;
        let has_branch = match push::remote_tip(self.git, &destination, &lease.reference)? {
            None => false,
            Some(there) if there == lease.tip => true,
            Some(there) => {
                return Err(Refusal::Push(PushRefusal::Unpushed {
                    branch: lease.reference.clone(),
                    local: lease.tip.clone(),
                    remote: there,
                }));
            }
        };
        Ok((Some(destination), has_branch))
    }

    fn mode_for(&self, listed: &[listing::Input], path: &RepoPath, placement: &Placement) -> u32 {
        // For a listed file, which keeps the bytes and the mode the user gave
        // it, the mode `git add` would record: the user may have changed that
        // bit along with the content, or instead of it, and the tip's mode
        // would leave the path modified after a compose that was supposed to
        // leave `git status` clean. Under `core.fileMode=false` `git` does not
        // read the bit at all, so the tip's mode is what it would record.
        let honoured = self.file_mode_honoured();
        match listed.iter().find(|input| input.path == *path) {
            Some(input) => match (honoured, input.executable, input.recorded.as_ref()) {
                (true, Some(true), _) => 0o100755,
                (true, Some(false), _) => 0o100644,
                (_, _, Some((mode, _))) => *mode,
                (_, _, None) => 0o100644,
            },
            None => placement.mode_for_new(path, honoured),
        }
    }

    fn file_mode_honoured(&self) -> bool {
        self.git
            .run(&["config", "--type=bool", "core.fileMode"])
            .ok()
            .and_then(|answer| answer.line().ok())
            != Some("false".to_owned())
    }

    /// The index entry for a path must be the tip's entry.
    ///
    /// Staged content of the user's own is work not yet committed, and step 6
    /// would replace it (**measured**: a version staged and a different one in
    /// the working tree left the staged blob unreachable from the index after
    /// the refresh, with `git status` clean).
    fn index_entry_is_the_tips(&self, path: &RepoPath, tip: &str) -> Result<(), Refusal> {
        let name = path
            .to_text()
            .ok_or_else(|| Refusal::Io("a listed path is not text git can be given".to_owned()))?;
        let staged = self
            .git
            .run(&["ls-files", "-s", "-z", "--", name])
            .map_err(|e| Refusal::Io(e.to_string()))?;
        let recorded = self
            .git
            .run(&["ls-tree", "-z", tip, "--", name])
            .map_err(|e| Refusal::Io(e.to_string()))?;
        // `ls-files -s` prints `<mode> <object> <stage>\t<path>` and `ls-tree`
        // prints `<mode> <type> <object>\t<path>`, so the two are compared by
        // the mode and the object rather than by the whole line.
        let of = |record: &[u8]| -> Option<(String, String)> {
            let tab = record.iter().position(|b| *b == b'\t')?;
            let head = String::from_utf8_lossy(&record[..tab]).into_owned();
            let mut fields = head.split_whitespace();
            let mode = fields.next()?.to_owned();
            let rest: Vec<&str> = fields.collect();
            let object = rest.iter().find(|field| field.len() >= 40)?;
            Some((mode, (*object).to_owned()))
        };
        let staged_entry = staged
            .stdout
            .split(|b| *b == 0)
            .find(|r| !r.is_empty())
            .and_then(of);
        let recorded_entry = recorded
            .stdout
            .split(|b| *b == 0)
            .find(|r| !r.is_empty())
            .and_then(of);
        if staged_entry == recorded_entry {
            Ok(())
        } else {
            Err(Refusal::Place(PlaceRefusal::Changed {
                path: format!("{name} is staged with a change of your own"),
            }))
        }
    }

    fn project_file_matches_the_tip(&self, root: &Dir, tip: &str) -> Result<(), Refusal> {
        let name = self.project_file.to_text().ok_or_else(|| {
            Refusal::Io("this project's file name is not text git can be given".to_owned())
        })?;
        // Not `git status`, which the user's own index flags can silence.
        let tagged = self
            .git
            .run(&["ls-files", "-v", "-z", "--", name])
            .map_err(|e| Refusal::Io(e.to_string()))?;
        let tag = tagged.stdout.first().copied().unwrap_or(b'?');
        if tag != b'H' {
            return Err(Refusal::ConfigurationUncommitted {
                path: name.to_owned(),
            });
        }
        let entries = snapshot::entries(self.git, tip, name)?;
        let Some(entry) = entries.first() else {
            return Err(Refusal::ConfigurationUncommitted {
                path: name.to_owned(),
            });
        };
        match self.hash_of(root, self.project_file) {
            Some(blob) if blob == entry.blob => Ok(()),
            _ => Err(Refusal::ConfigurationUncommitted {
                path: name.to_owned(),
            }),
        }
    }

    fn inputs_inside(
        &self,
        snapshot_project: &Path,
        inputs: &super::cli::Where,
    ) -> Result<(Option<RepoPath>, RepoPath), Refusal> {
        // `Project::schema_dir` and `Project::ids_file` join the configured
        // path to the root, and a configured absolute path discards the root:
        // an intent command pointed at the snapshot would otherwise read the
        // live declarations and write the live ids file before any of the
        // protocol had begun.
        let outside = || Refusal::InputsOutside {
            declarations: inputs.declarations.clone(),
            ids: inputs.identity_file.clone(),
        };
        // `doctor` answers with the path as the CLI resolved it from the
        // project it was given, which is relative where the configuration is
        // relative and absolute where it is absolute — and an absolute
        // `schema_dir` is exactly the layout this refuses (**measured**: with
        // one in `pbps.yml`, both came back absolute and outside the project).
        // Both shapes are resolved against the snapshot's project directory
        // and then required to lie under it.
        let base = snapshot_project.canonicalize().map_err(|_| outside())?;
        // The answer may name the project directory itself. `schema_dir: .`
        // is a supported layout, and for a project at the worktree root
        // `doctor` then answers `./.`, which normalises to *nothing* relative
        // to the root — a path `RepoPath` refuses, and rightly, since the
        // empty path and `.` are not names. So the two answers are resolved
        // to an `Option`: `None` is the root, which is a place, not a missing
        // one. Requiring a `RepoPath` here refused every compose in that
        // layout.
        let within = |answer: &str| -> Option<PathBuf> {
            let given = Path::new(answer);
            let joined = if given.is_absolute() {
                given.to_path_buf()
            } else {
                base.join(given)
            };
            let stripped = normalize(&joined).strip_prefix(&base).ok()?.to_owned();
            Some(match self.project.and_then(RepoPath::to_text) {
                None => stripped,
                Some(relative) => Path::new(relative).join(stripped),
            })
        };
        let declarations = match within(&inputs.declarations).ok_or_else(outside)? {
            path if path.as_os_str().is_empty() => None,
            path => Some(
                RepoPath::new(path.to_str().ok_or_else(outside)?.as_bytes())
                    .map_err(|_| outside())?,
            ),
        };
        // The ids file is a file, so an empty answer for it is not a layout
        // but a broken one.
        let ids_path = within(&inputs.identity_file).ok_or_else(outside)?;
        let ids_file = RepoPath::new(ids_path.to_str().ok_or_else(outside)?.as_bytes())
            .map_err(|_| outside())?;
        Ok((declarations, ids_file))
    }

    /// What the working tree holds for one input: the blob `git` would store
    /// for its bytes, the execute bit `git add` would record — or the reason
    /// this is not a file the compose can read at all.
    fn look_at(&self, root: &Dir, path: &RepoPath) -> listing::Seen {
        let Ok((parent, leaf)) = root.walk_to_parent(path) else {
            return listing::Seen::Unusable("under something this UI will not follow".to_owned());
        };
        let look = match parent.look(&leaf) {
            Ok(Some(look)) => look,
            Ok(None) => return listing::Seen::Absent,
            Err(e) => return listing::Seen::Unusable(e.to_string()),
        };
        if look.is_link {
            return listing::Seen::Unusable("a symbolic link".to_owned());
        }
        if !look.is_regular {
            return listing::Seen::Unusable("not a regular file".to_owned());
        }
        match parent.open_file(&leaf).and_then(|file| file.read()) {
            Ok(bytes) => match super::attributes::hash(self.git, &bytes) {
                Ok(blob) => listing::Seen::Present {
                    blob,
                    executable: look.mode & 0o111 != 0,
                },
                Err(e) => listing::Seen::Unusable(e.to_string()),
            },
            Err(e) => listing::Seen::Unusable(format!("unreadable: {e}")),
        }
    }

    fn hash_of(&self, root: &Dir, path: &RepoPath) -> Option<String> {
        match self.look_at(root, path) {
            listing::Seen::Present { blob, .. } => Some(blob),
            listing::Seen::Absent | listing::Seen::Unusable(_) => None,
        }
    }

    /// Every hash the UI takes of a working-tree file is taken over bytes read
    /// through the no-follow handle, never by handing `git` the path to
    /// reopen: a directory component swapped for a link in between would have
    /// `git` read an outside file.
    fn read_through_handle(&self, root: &Dir, path: &RepoPath) -> Option<Vec<u8>> {
        let (parent, leaf) = root.walk_to_parent(path).ok()?;
        let look = parent.look(&leaf).ok()??;
        if look.is_link || !look.is_regular {
            return None;
        }
        parent.open_file(&leaf).ok()?.read().ok()
    }

    /// Lay the working tree's version of each listed file over the snapshot,
    /// and *remove* each listed file the working tree no longer has — a table
    /// or a role is dropped or renamed by deleting or renaming its declaration
    /// file, and a snapshot that still held the old file would give the
    /// command nothing that disappeared to resolve.
    fn overlay(&self, root: &Dir, into: &Path, listed: &[listing::Input]) -> Result<(), Refusal> {
        for input in listed {
            let name = input
                .path
                .to_text()
                .ok_or_else(|| Refusal::Io("a listed path is not text".to_owned()))?;
            let target = into.join(name);
            match input.state {
                State::Deleted => {
                    let _ = std::fs::remove_file(&target);
                }
                State::Unchanged => {}
                State::Edited | State::New => {
                    let bytes = self.read_through_handle(root, &input.path).ok_or_else(|| {
                        Refusal::Place(PlaceRefusal::Changed {
                            path: input.path.to_string(),
                        })
                    })?;
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| Refusal::Io(e.to_string()))?;
                    }
                    std::fs::write(&target, bytes).map_err(|e| Refusal::Io(e.to_string()))?;
                }
            }
        }
        Ok(())
    }

    /// Every regular file the command left different from the snapshot it was
    /// given, the ids file among them — the same set a CLI user commits after
    /// `pbps rename`.
    fn changed_by_command(
        &self,
        snapshot_directory: &Path,
        recorded: &[snapshot::Entry],
        listed: &[listing::Input],
    ) -> Result<BTreeMap<RepoPath, Vec<u8>>, Refusal> {
        let mut before: BTreeMap<RepoPath, String> = BTreeMap::new();
        for entry in recorded {
            before.insert(entry.path.clone(), entry.blob.clone());
        }
        // The overlay replaced some of them, so the comparison is against what
        // was laid in, not against the tip.
        let overlaid: BTreeSet<RepoPath> = listed
            .iter()
            .filter(|input| input.state != State::Unchanged)
            .map(|input| input.path.clone())
            .collect();
        let mut changed = BTreeMap::new();
        for path in walk(snapshot_directory, snapshot_directory) {
            let Ok(relative) = RepoPath::new(path.0.as_bytes()) else {
                continue;
            };
            let bytes = path.1;
            let now = super::attributes::hash(self.git, &bytes)
                .map_err(|e| Refusal::Place(PlaceRefusal::Attribute(e)))?;
            let unchanged = match before.get(&relative) {
                Some(blob) if !overlaid.contains(&relative) => *blob == now,
                _ => false,
            };
            if unchanged {
                continue;
            }
            // A file the overlay itself put there is not a change the command
            // made; it is already in the working tree.
            if overlaid.contains(&relative)
                && listed
                    .iter()
                    .find(|input| input.path == relative)
                    .is_some_and(|input| {
                        self.read_through_handle(
                            &Dir::open_root(self.git.root()).expect("the root is open"),
                            &input.path,
                        )
                        .map(|bytes| super::attributes::hash(self.git, &bytes).ok())
                            == Some(Some(now.clone()))
                    })
            {
                continue;
            }
            changed.insert(relative, bytes);
        }
        Ok(changed)
    }

    /// Take `<index>.lock`, then fill it from the index.
    ///
    /// In that order, and the order is the whole of it. Reading the index
    /// first and linking the copy afterwards leaves a window in which another
    /// `git` takes the lock, installs a *newer* index and releases it — and
    /// step 6 then renames this stale snapshot over it, silently discarding
    /// whatever that `git` staged. Creating the name first is what stops
    /// every other `git` from touching the index at all, so the bytes read
    /// after it cannot go out of date while this compose holds it.
    ///
    /// The lock is filled in place through the handle that created it, which
    /// is safe here and nowhere else: `EEXIST` has already established that
    /// this process is the only one that can be writing it, and the record
    /// carries the file's hash after each write so recovery can still tell
    /// the UI's copy from a running `git`'s.
    fn copy_index(&self, index_file: &Path, index_lock: &Path) -> Result<(), Refusal> {
        let taken = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(index_lock)
            .map_err(|e| {
                Refusal::Lock(LockRefusal::Held {
                    path: index_lock.to_path_buf(),
                    holding: if e.kind() == std::io::ErrorKind::AlreadyExists {
                        String::new()
                    } else {
                        e.to_string()
                    },
                })
            })?;
        // Only now, with every other `git` locked out of it.
        let bytes = std::fs::read(index_file).unwrap_or_default();
        let written = (|| {
            use std::io::Write as _;
            let mut taken = taken;
            taken.write_all(&bytes)?;
            taken.sync_all()
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(index_lock);
            return Err(Refusal::Io(e.to_string()));
        }
        Ok(())
    }
}

/// The locks a compose holds, released together whatever the outcome.
///
/// The release is in `Drop` and not only in a call, because "whatever the
/// outcome" has to include the outcomes nobody enumerated. A refusal anywhere
/// between step 0 and step 6 — an attribute check, `validate`, a `write-tree`
/// — leaves `.git/index.lock` and `HEAD.lock` on disk, and those two files
/// stop *every* `git` command in the checkout, not just this UI's. With the
/// record removed on the way out there would then be nothing on disk from
/// which recovery could clear them, so a single refused compose would leave
/// the repository unusable until someone found the files by hand.
///
/// `release` stays as the explicit call on the paths that have something to
/// say afterwards; `Drop` is what makes forgetting it unrepresentable.
struct Guard {
    index_lock: PathBuf,
    head: Option<Held>,
    branch: Option<Held>,
}

impl Guard {
    /// Idempotent: each lock is taken out of its `Option`, and removing a
    /// file that is already gone — which is what step 6's rename leaves —
    /// is not an error.
    fn release(&mut self) {
        if let Some(head) = self.head.take() {
            let _ = head.release();
        }
        if let Some(branch) = self.branch.take() {
            let _ = branch.release();
        }
        let _ = std::fs::remove_file(&self.index_lock);
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.release();
    }
}

pub(super) fn hash_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    // A cheap content identity, not a cryptographic one: what it has to do is
    // tell the UI's own copy of the index from a running `git`'s.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!("{hash:016x}"))
}

/// Which placed paths the deciding tip keeps, and which are undone.
///
/// A pure function, because it is the one decision in this protocol that has
/// to be right in states nobody can arrange on demand — `HEAD` redirected in
/// a window measured in milliseconds — and a decision that can only be
/// observed through such a race is one nothing can pin.
///
/// A path is kept only where the deciding tip holds **exactly** what step 3
/// recorded for it: the mode and the object id, never the id alone, since the
/// same blob at `100755` is a file the placed one does not match and a
/// `120000` entry with that id is not a file at all. Everything else is
/// undone, including every path when there is no tip to decide by.
pub fn decide_paths(
    deciding: Option<&str>,
    placed: &[Placed],
    entry_at: impl Fn(&str, &str) -> Option<(u32, String)>,
) -> (Vec<String>, Vec<String>) {
    let mut kept = Vec::new();
    let mut undone = Vec::new();
    for path in placed {
        let keep = match (deciding, &path.entry_mode, &path.entry_blob) {
            (Some(tip), Some(mode), Some(blob)) => {
                entry_at(tip, &path.path) == Some((*mode, blob.clone()))
            }
            _ => false,
        };
        if keep {
            kept.push(path.path.clone());
        } else {
            undone.push(path.path.clone());
        }
    }
    (kept, undone)
}

/// Resolve `.` and `..` without touching the filesystem.
///
/// Lexical rather than `canonicalize`, because the ids file may not exist yet
/// — a project that has never had an intent recorded has no identities file —
/// and a path that cannot be resolved is not the same as one that escapes.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other @ (std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::Normal(_)) => out.push(other),
        }
    }
    out
}

/// Every regular file under a directory, with its bytes, named relative to it.
fn walk(base: &Path, here: &Path) -> Vec<(String, Vec<u8>)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(here) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            found.extend(walk(base, &path));
        } else if kind.is_file()
            && let Ok(relative) = path.strip_prefix(base)
            && let Some(name) = relative.to_str()
            && let Ok(bytes) = std::fs::read(&path)
        {
            found.push((name.to_owned(), bytes));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::record::Identity;

    fn placed(path: &str, mode: Option<u32>, blob: Option<&str>) -> Placed {
        Placed {
            path: path.to_owned(),
            temporary: format!("{path}.tmp"),
            original: None,
            replacement: Identity {
                blob: "r".to_owned(),
                device: 1,
                inode: 2,
            },
            entry_mode: mode,
            entry_blob: blob.map(str::to_owned),
            keep: None,
            retained_as: None,
            undone: false,
        }
    }

    #[test]
    fn a_path_is_kept_only_where_the_deciding_tip_holds_exactly_what_was_recorded() {
        let paths = [
            placed("schema/a.yml", Some(0o100644), Some("aaa")),
            placed("schema/b.yml", Some(0o100644), Some("bbb")),
        ];
        let (kept, undone) = decide_paths(Some("tip"), &paths, |_, path| match path {
            "schema/a.yml" => Some((0o100644, "aaa".to_owned())),
            _ => Some((0o100644, "something else".to_owned())),
        });
        assert_eq!(kept, vec!["schema/a.yml"]);
        assert_eq!(undone, vec!["schema/b.yml"]);
    }

    #[test]
    fn the_same_blob_at_another_mode_is_a_file_the_placed_one_does_not_match() {
        // The mode is read with the object id and not after it: a declaration
        // committed executable is not the file that was placed, and keeping it
        // would leave the checkout disagreeing with the tip it was kept for.
        let paths = [placed("schema/a.yml", Some(0o100644), Some("aaa"))];
        let (kept, undone) = decide_paths(Some("tip"), &paths, |_, _| {
            Some((0o100755, "aaa".to_owned()))
        });
        assert!(kept.is_empty(), "{kept:?}");
        assert_eq!(undone, vec!["schema/a.yml"]);
    }

    #[test]
    fn a_link_or_a_submodule_carrying_that_id_is_not_a_file_either() {
        let paths = [placed("schema/a.yml", Some(0o100644), Some("aaa"))];
        for mode in [0o120000, 0o160000] {
            let (kept, _) =
                decide_paths(Some("tip"), &paths, |_, _| Some((mode, "aaa".to_owned())));
            assert!(kept.is_empty(), "mode {mode:o} was kept");
        }
    }

    #[test]
    fn nothing_is_kept_where_there_is_no_tip_to_decide_by() {
        // A chain, a lock another `git` holds, a detached `HEAD` with no
        // branch to ask about, an unborn target, a target no tree can be read
        // from: every one of them decides nothing, so step 2 is undone for
        // every path. The shapes differ; the answer does not.
        let paths = [
            placed("schema/a.yml", Some(0o100644), Some("aaa")),
            placed("schema/b.yml", Some(0o100644), Some("bbb")),
        ];
        let (kept, undone) =
            decide_paths(None, &paths, |_, _| panic!("no tip means no entry is read"));
        assert!(kept.is_empty());
        assert_eq!(undone.len(), 2);
    }

    #[test]
    fn a_path_the_deciding_tip_does_not_hold_at_all_is_undone() {
        let paths = [placed("schema/new.yml", Some(0o100644), Some("aaa"))];
        let (kept, undone) = decide_paths(Some("tip"), &paths, |_, _| None);
        assert!(kept.is_empty());
        assert_eq!(undone, vec!["schema/new.yml"]);
    }

    #[test]
    fn a_path_step_three_never_recorded_an_entry_for_is_undone_rather_than_guessed() {
        let paths = [placed("schema/a.yml", None, None)];
        let (kept, undone) = decide_paths(Some("tip"), &paths, |_, _| {
            Some((0o100644, "aaa".to_owned()))
        });
        assert!(kept.is_empty());
        assert_eq!(undone, vec!["schema/a.yml"]);
    }
}
