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
    /// The blob id of each file the page was shown, by path. The locks stop
    /// `git`, not an editor saving the same file.
    #[serde(default)]
    pub shown: BTreeMap<String, String>,
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
    /// Another compose is running in this checkout right now.
    AlreadyComposing,
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
            Self::Io(detail) => write!(f, "{detail}"),
        }
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

    fn reached(&self, phase: Phase) {
        if let Some(watching) = self.watching {
            watching(phase);
        }
    }

    /// The whole of it. A `Refusal` here means the checkout is as it was.
    pub fn run(&self, request: &Request) -> Result<Composed, Refusal> {
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

        let outcome = self.compose(request, &records, &mut record);
        if outcome.is_err() {
            // A refused compose still owns its record until everything it
            // made has been put back; `compose` has done that by here, so the
            // record goes last, as it does on the success path.
            let _ = records.remove(&record);
        }
        outcome
    }

    /// A record whose process is still alive is a compose someone is running
    /// right now, in this UI or another launched beside it: nothing of it is
    /// reclaimed and this UI refuses to compose while it stands, because every
    /// leftover it would otherwise recognise as its own kind may be that
    /// compose's, in use this instant.
    fn someone_else_is_composing(&self, records: &Records) -> Result<bool, Refusal> {
        for name in records.list().map_err(|e| Refusal::Io(e.to_string()))? {
            if let Ok(found) = records.read(&name)
                && found.process.alive()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn compose(
        &self,
        request: &Request,
        records: &Records,
        record: &mut Record,
    ) -> Result<Composed, Refusal> {
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
                declarations: &declarations,
                ids_file: &ids_file,
            },
            |path| self.hash_of(&root, path),
        )?;

        // The overlay, and then the command.
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
        record.locks.push(RefLock {
            reference: "HEAD".to_owned(),
            lock_file: head_lock.path().display().to_string(),
        });
        records
            .publish(record)
            .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
        self.reached(Phase::Locking);

        let held = Guard {
            index_lock: index_lock.clone(),
            head: Some(head_lock),
            branch: None,
        };

        // From here a refusal has locks and possibly files to put back, so the
        // work happens in one place and the cleanup is the same either way.
        let mut placement = Placement::new(
            Dir::open_root(self.git.root()).map_err(|e| Refusal::Io(e.to_string()))?,
        );
        let result = self.place_and_commit(
            request,
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
        );
        if result.is_err() {
            // Every undo publishes `rolling-back` first; the record's per-path
            // evidence is what makes retrying an interrupted undo safe.
            if record.advance(Phase::RollingBack).is_ok() {
                let _ = records.publish(record);
                self.reached(Phase::RollingBack);
            }
            placement.undo_all();
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn place_and_commit(
        &self,
        request: &Request,
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
    ) -> Result<Composed, Refusal> {
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
                shown: request.shown.get(&path.to_string()).cloned(),
            });
        }
        let untouched: Vec<&listing::Input> = listed
            .iter()
            .filter(|input| !changed.contains_key(&input.path))
            .filter(|input| input.state != State::Unchanged)
            .collect();

        // Step 1's per-path checks, for everything that will be committed.
        for input in listed {
            self.index_entry_is_the_tips(&input.path, &lease.tip)?;
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

        // Step 6: the rename that is exactly the commit step of `git`'s own
        // lock. Only a compose that has never entered `rolling-back` gets here.
        std::fs::rename(index_lock, index_file).map_err(|e| Refusal::Io(e.to_string()))?;
        record
            .advance(Phase::Installed)
            .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
        records
            .publish(record)
            .map_err(|e| Refusal::Unrecordable(e.to_string()))?;
        self.reached(Phase::Installed);

        let previous_directory = self.git_dir.join("pbps-ui").join("previous");
        std::fs::create_dir_all(&previous_directory).map_err(|e| Refusal::Io(e.to_string()))?;
        let previous =
            Dir::open_root(&previous_directory).map_err(|e| Refusal::Io(e.to_string()))?;
        let reported = placement.finish(&previous);
        let retained: Vec<String> = reported
            .iter()
            .map(|outcome| format!("{outcome:?}"))
            .collect();

        held.release();
        let _ = records.remove(record);
        let _ = std::fs::remove_dir_all(intent_directory);

        // The push, last, and only once the checkout is the commit's.
        let destination = self.publish(request, lease, &commit)?;

        Ok(Composed {
            commit: commit.clone(),
            branch: lease.reference.clone(),
            signature: tree::signature_state(self.git, &commit),
            destination,
            retained,
            paths: entries
                .iter()
                .map(|entry| entry.path.to_string())
                .chain(removals.iter().map(RepoPath::to_string))
                .collect(),
            hooks_did_not_run: true,
        })
    }

    fn publish(
        &self,
        request: &Request,
        lease: &Lease,
        commit: &str,
    ) -> Result<Option<String>, Refusal> {
        if request.remote.is_empty() {
            return Ok(None);
        }
        let destination = push::destination(self.git, &request.remote, self.remote_name.clone())?;
        match push::remote_tip(self.git, &destination, &lease.reference)? {
            None => {
                push::publish_branch(self.git, &destination, &lease.reference, &lease.tip)?;
            }
            Some(there) if there == lease.tip => {}
            Some(there) => {
                return Err(Refusal::Push(PushRefusal::Unpushed {
                    branch: lease.reference.clone(),
                    local: lease.tip.clone(),
                    remote: there,
                }));
            }
        }
        Ok(Some(push::push(
            self.git,
            &destination,
            &lease.reference,
            &lease.tip,
            commit,
        )?))
    }

    fn mode_for(&self, listed: &[listing::Input], path: &RepoPath, placement: &Placement) -> u32 {
        // For a path step 2 placed, the tip's mode, since the exchange refuses
        // a working-tree mode that differs from it. For a listed file, which
        // keeps the bytes and the mode the user gave it, the mode `git add`
        // would record — the user may have changed that bit along with the
        // content, or instead of it, and the tip's mode would leave the path
        // modified after a compose that was supposed to leave `git status`
        // clean.
        listed
            .iter()
            .find(|input| input.path == *path)
            .and_then(|input| input.recorded.as_ref().map(|(mode, _)| *mode))
            .unwrap_or_else(|| placement.mode_for_new(path, self.file_mode_honoured()))
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
    ) -> Result<(RepoPath, RepoPath), Refusal> {
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
        let relative = |answer: &str| -> Option<RepoPath> {
            let given = Path::new(answer);
            let joined = if given.is_absolute() {
                given.to_path_buf()
            } else {
                base.join(given)
            };
            let stripped = normalize(&joined).strip_prefix(&base).ok()?.to_owned();
            let within = match self.project.and_then(RepoPath::to_text) {
                None => stripped,
                Some(relative) => Path::new(relative).join(stripped),
            };
            RepoPath::new(within.to_str()?.as_bytes()).ok()
        };
        let declarations = relative(&inputs.declarations).ok_or_else(outside)?;
        let ids_file = relative(&inputs.identity_file).ok_or_else(outside)?;
        Ok((declarations, ids_file))
    }

    fn hash_of(&self, root: &Dir, path: &RepoPath) -> Option<String> {
        let bytes = self.read_through_handle(root, path)?;
        super::attributes::hash(self.git, &bytes).ok()
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

    fn copy_index(&self, index_file: &Path, index_lock: &Path) -> Result<(), Refusal> {
        // The index lock is a *copy of the index*, not a lock holding an id:
        // a byte added to it stops being an index at all.
        if index_lock.exists() {
            return Err(Refusal::Lock(LockRefusal::Held {
                path: index_lock.to_path_buf(),
                holding: String::new(),
            }));
        }
        let bytes = std::fs::read(index_file).unwrap_or_default();
        let writing = PathBuf::from(format!("{}.pbps-ui-writing", index_lock.display()));
        std::fs::write(&writing, &bytes).map_err(|e| Refusal::Io(e.to_string()))?;
        match std::fs::hard_link(&writing, index_lock) {
            Ok(()) => {
                let _ = std::fs::remove_file(&writing);
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_file(&writing);
                Err(Refusal::Lock(LockRefusal::Held {
                    path: index_lock.to_path_buf(),
                    holding: e.to_string(),
                }))
            }
        }
    }
}

/// The locks a compose holds, released together whatever the outcome.
struct Guard {
    index_lock: PathBuf,
    head: Option<Held>,
    branch: Option<Held>,
}

impl Guard {
    fn release(mut self) {
        if let Some(head) = self.head.take() {
            let _ = head.release();
        }
        if let Some(branch) = self.branch.take() {
            let _ = branch.release();
        }
        let _ = std::fs::remove_file(&self.index_lock);
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
