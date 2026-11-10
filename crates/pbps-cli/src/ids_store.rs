//! Where identities are read from and written to: the identity tree under
//! `ids_dir` (#2600, design §3, §6).
//!
//! Every reader of identities goes through [`read_opt`], and every writer
//! through [`plan`] and [`TreeWrite::apply`] (or [`write`], which does both).
//! `IdsFile` stays the in-memory model; the tree is how it rests on disk.
//!
//! Absent, empty and unreadable are three different things here. A project with
//! neither a tree nor the legacy `schema.ids.json` is a fresh project. A project
//! with only the legacy file must run `pbps ids migrate`: reading it silently
//! would leave two sources of truth, and writing the tree beside it would split
//! the history in two. A tree beside the legacy file is an unfinished migration.
//!
//! **The write is the sync of design §3.4.** [`plan`] decides the whole new
//! tree, every refusal included, before [`TreeWrite::apply`] writes the first
//! byte, and it proves the result reads back to the very mapping it was made
//! from. An identity keeps its file unless its declaration moved: then the file
//! is written at the path the declaration mirrors and the old one removed
//! (step 2). A dropped object keeps its file (step 4), and one a live
//! declaration now wants the path of is sent aside to `<name>.<uid>.json`
//! (step 5). A table that becomes, stops being, or changes parent as a
//! partition has its record moved between files (step 6). An identity the
//! mapping no longer has (a withdrawn rebuild takes its replacement's away) has
//! its file removed: the mapping is the resolver's to decide, and the proof
//! below that the tree reads back to it is what keeps the store honest. What is
//! still refused is a live declaration with no place.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use pbps_config::Project;
use pbps_load::{DeclaredPaths, read_ids_tree};
use pbps_model::ids_tree::{self, Body, Placement, TreeFile};
use pbps_model::{IdsFile, Uid, UidKind};

use crate::ids_cmd::{Dirs, declared_placement, new_config_text, shown, write_atomic};

// ------------------------------------------------------------------ read ----

fn legacy_present(project: &Project) -> anyhow::Result<bool> {
    let path = project.ids_file();
    match std::fs::symlink_metadata(&path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => {
            Err(e).with_context(|| format!("cannot inspect identity file `{}`", path.display()))
        }
    }
}

/// The project's identities, or `None` for a fresh project that has none yet.
///
/// A legacy `schema.ids.json` is never read: it is a refusal that says what to
/// run. `None` is only ever "there is nothing", never "I could not look".
pub(crate) fn read_opt(project: &Project) -> anyhow::Result<Option<IdsFile>> {
    let dir = project.config.ids_dir();
    let tree = read_ids_tree(&project.ids_dir())?;
    match (legacy_present(project)?, tree.is_empty()) {
        (false, true) => Ok(None),
        (true, true) => bail!(
            "this project still keeps its identities in `{}`; run `pbps ids migrate` to move them \
             into the identity tree `{}`",
            pbps_model::Text::new(project.ids_file().display().to_string()).shown(),
            pbps_model::Text::new(dir.display().to_string()).shown()
        ),
        (true, false) => bail!(
            "`{}` and the identity tree `{}` both exist, so an earlier `pbps ids migrate` did not \
             finish; run it again",
            pbps_model::Text::new(project.ids_file().display().to_string()).shown(),
            pbps_model::Text::new(dir.display().to_string()).shown()
        ),
        (false, false) => ids_tree::assemble(&tree).map(Some).with_context(|| {
            format!(
                "identity tree `{}` is inconsistent",
                pbps_model::Text::new(dir.display().to_string()).shown()
            )
        }),
    }
}

/// Every file git has under `ids_dir_rel` (as git names it) at `rev`, by path
/// relative to it, for reading history. A symlink, submodule or file that is
/// not a `.json` text file is refused, as the working-tree reader refuses it.
pub(crate) fn tree_at(
    root: &Path,
    rev: &str,
    ids_dir_rel: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let below = format!("{ids_dir_rel}/");
    let mut files = BTreeMap::new();
    crate::baseline::git_objects::read_with(
        root,
        rev,
        ids_dir_rel,
        // The directory's own entry too: at a revision where `ids_dir` is a
        // symlink, a file or a submodule, the listing names that one entry and
        // nothing below it, which must not read as an empty tree.
        |path, _mode| path == ids_dir_rel || path.starts_with(&below),
        |path, mode, bytes| {
            let shown = pbps_model::Text::new(path.to_owned()).shown().to_string();
            anyhow::ensure!(
                path != ids_dir_rel,
                "`{shown}` at `{rev}` is not a directory of identity files (git mode {mode})"
            );
            anyhow::ensure!(
                matches!(mode, "100644" | "100755") && ids_tree::is_identity_file(path),
                "`{shown}` at `{rev}` is not {}",
                ids_tree::IDENTITY_FILE_RULE
            );
            let text = std::str::from_utf8(bytes)
                .with_context(|| format!("`{shown}` at `{rev}` is not UTF-8"))?;
            files.insert(path[below.len()..].to_owned(), text.to_owned());
            Ok(())
        },
    )?;
    Ok(files)
}

/// The files `plan` and the other commands must not write over: the project
/// file, the legacy identity file if one is still there, and every file of the
/// tree that exists. A file of the tree not yet written is not listed, so every
/// command that writes somewhere the user names also calls
/// [`refuse_output_inside`].
pub(crate) fn protected(project: &Project) -> Vec<PathBuf> {
    let mut out = vec![project.config_file(), project.ids_file()];
    if let Ok(tree) = read_ids_tree(&project.ids_dir()) {
        out.extend(tree.keys().map(|rel| project.ids_dir().join(rel)));
    }
    out
}

/// Refuses an output destination inside the identity tree, whether or not the
/// file exists yet and however the path reaches it (a symlinked parent
/// included): a file written there becomes an identity file or collides with
/// one the next `plan` writes.
pub(crate) fn refuse_output_inside(project: &Project, destination: &Path) -> anyhow::Result<()> {
    // By the tree's own key: on a case-insensitive filesystem `.PBPS/IDS/x` is
    // inside `.pbps/ids`.
    let key = PathKey::probe(&project.ids_dir()).with_context(|| {
        format!(
            "cannot tell whether `{}` is case-sensitive",
            project.ids_dir().display()
        )
    })?;
    refuse_output_inside_keyed(project, destination, key)
}

fn refuse_output_inside_keyed(
    project: &Project,
    destination: &Path,
    key: PathKey,
) -> anyhow::Result<()> {
    let inside = key
        .strip_prefix(
            &crate::paths::absolute(destination)?,
            &crate::paths::absolute(&project.ids_dir())?,
        )
        .is_some();
    if inside {
        bail!(
            "`{}` is inside the identity tree `{}`, which only pbps writes; write it elsewhere",
            pbps_model::Text::new(destination.display().to_string()).shown(),
            pbps_model::Text::new(project.config.ids_dir().display().to_string()).shown()
        );
    }
    Ok(())
}

// ----------------------------------------------------------------- write ----

/// A write that cannot be made, decided before anything is written.
fn refused(detail: String) -> anyhow::Error {
    anyhow::anyhow!("cannot write the identity tree: {detail}; nothing was written")
}

/// How the filesystem the tree lives on tells paths apart (#2770). Every
/// collision decision of a write (the planner's, the read-back proof's, the
/// removal check's, the generated names') compares `of(path)`, never the raw
/// path: on a case-insensitive filesystem `Customer.json` and `customer.json`
/// are one file, and a comparison that calls them two loses one of them.
/// Where case is significant the key is the path itself.
///
/// Folding is lower-casing, which is not every filesystem's table (NTFS has its
/// own up-case table, APFS also normalises Unicode); `same_file` at removal time
/// still asks the filesystem about aliases this key does not model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PathKey {
    fold: bool,
}

impl PathKey {
    pub(crate) fn of(self, path: &str) -> String {
        if self.fold {
            path.to_lowercase()
        } else {
            path.to_owned()
        }
    }

    pub(crate) fn path(self, path: &Path) -> String {
        self.of(&path.to_string_lossy())
    }

    /// What is left of `path` below `prefix`, comparing component by component
    /// by key (`Some("")`-like empty when they are equal); `None` when `path`
    /// is not `prefix` or below it.
    pub(crate) fn strip_prefix(self, path: &Path, prefix: &Path) -> Option<PathBuf> {
        let mut rest = path.components();
        for want in prefix.components() {
            let got = rest.next()?;
            if self.of(&got.as_os_str().to_string_lossy())
                != self.of(&want.as_os_str().to_string_lossy())
            {
                return None;
            }
        }
        Some(rest.as_path().to_path_buf())
    }

    /// [`strip_prefix`](Self::strip_prefix) for `/`-separated text, as git and
    /// the configuration spell paths.
    pub(crate) fn strip_prefix_str<'a>(self, path: &'a str, prefix: &str) -> Option<&'a str> {
        let head = path.get(..prefix.len())?;
        (self.of(head) == self.of(prefix)).then(|| &path[prefix.len()..])
    }

    /// The key of the filesystem under `dir` (or the nearest ancestor that
    /// exists, when the tree is not there yet). An answer that cannot be had is
    /// an error, never "case-sensitive": that guess is the one that loses files.
    pub(crate) fn probe(dir: &Path) -> std::io::Result<Self> {
        Self::probe_on(dir, &device_of)
    }

    /// [`probe`](Self::probe) with the device lookup given, so a mount boundary
    /// can be tested without a mount.
    fn probe_on(
        dir: &Path,
        device: &dyn Fn(&Path) -> std::io::Result<u64>,
    ) -> std::io::Result<Self> {
        let base = dir
            .ancestors()
            .find(|d| d.is_dir())
            .unwrap_or(Path::new("."));
        // Looked at without writing: `plan --check` must work on a read-only
        // checkout. The tree's own directory first; when it is empty, each
        // directory above it on the same device (the project directory holds
        // `pbps.yml`, so this normally answers). A directory on another device
        // is another filesystem and says nothing about this one. Case folding
        // can be set per directory (ext4 casefold, NTFS), so an ancestor's
        // answer is the best a read-only look can give, never a better one than
        // the directory's own.
        let here = device(base)?;
        for (depth, above) in base.ancestors().enumerate() {
            let above = if above.as_os_str().is_empty() {
                Path::new(".")
            } else {
                above
            };
            if depth > 0 && device(above).ok() != Some(here) {
                continue;
            }
            let entries = match std::fs::read_dir(above) {
                Ok(entries) => entries,
                // A directory above that cannot be listed offers nothing; the
                // tree's own must be readable.
                Err(e) if depth > 0 && e.kind() == std::io::ErrorKind::PermissionDenied => continue,
                Err(e) => return Err(e),
            };
            // One listing: the filesystem's own answer to which spellings exist.
            let listed: BTreeSet<String> = entries
                .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect::<Result<_, _>>()?;
            for name in &listed {
                let twin = if name.to_uppercase() != *name {
                    name.to_uppercase()
                } else {
                    name.to_lowercase()
                };
                if twin == *name {
                    continue;
                }
                // Both spellings listed as entries: two files, a definite answer
                // (hard links or symlinks to one file do not make it fold: they
                // are two entries).
                if listed.contains(&twin) {
                    return Ok(Self { fold: false });
                }
                // Only one spelling listed: if the other still resolves, the
                // filesystem folds. Not followed through a link: the entry
                // itself is what is asked for.
                return match std::fs::symlink_metadata(above.join(&twin)) {
                    Ok(_) => Ok(Self { fold: true }),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self { fold: false }),
                    Err(e) => Err(e),
                };
            }
        }
        // Nothing to look at anywhere: make an entry, look for its twin, take it
        // away.
        let probe = base.join(format!(".PbpsCaseProbe{}", std::process::id()));
        match std::fs::write(&probe, b"") {
            Ok(()) => {}
            // This process cannot write here, so a write of the tree is refused
            // anyway; the key only decides what a check or a refusal says. Take
            // the answer that cannot lose a file: fold. The cost is a refusal
            // of two paths that differ only in case, on a directory that cannot
            // be written.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
                ) =>
            {
                return Ok(Self { fold: true });
            }
            Err(e) => return Err(e),
        }
        let found =
            std::fs::symlink_metadata(base.join(format!(".pbpscaseprobe{}", std::process::id())));
        std::fs::remove_file(&probe)?;
        match found {
            Ok(_) => Ok(Self { fold: true }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self { fold: false }),
            Err(e) => Err(e),
        }
    }

    /// A key that folds case whatever the disk says, for tests on Linux.
    #[cfg(test)]
    pub(crate) fn folding() -> Self {
        Self { fold: true }
    }
}

/// What one write changes, decided and checked, not yet written.
#[derive(Debug, Default)]
pub(crate) struct TreeWrite {
    dir: PathBuf,
    /// Files whose bytes change or that do not exist yet, by path relative to
    /// `dir`.
    files: BTreeMap<String, String>,
    /// Files that stop existing: the old path of an identity file that moved,
    /// and the own file of a table that became a partition.
    removals: Vec<String>,
    /// `pbps.yml` with `ids_dir` named, when this write creates the tree.
    config: Option<(PathBuf, String)>,
    /// Whether the filesystem holding the tree folds case, as the plan decided.
    key: PathKey,
}

impl TreeWrite {
    /// The names a temp file or an aside must not take: every planned
    /// destination and each of its ancestors (a declaration may be called
    /// `.a.json.tmp/b.yml`), by key, on top of what already exists on disk.
    fn reserved_names(&self) -> BTreeSet<String> {
        self.files
            .keys()
            .flat_map(|rel| Path::new(rel).ancestors())
            .map(|a| self.key.path(&self.dir.join(a)))
            .collect()
    }

    /// Nothing to write, move or remove: the tree is what the mapping and the
    /// declarations say it should be.
    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty() && self.removals.is_empty() && self.config.is_none()
    }

    /// Writes every file next to its final name first, and only then renames
    /// them into place, so a failure while writing leaves the tree as it was.
    /// What moved is removed after the new files are in place: a crash in
    /// between leaves both, never neither, and the next read names the
    /// duplicate (`ids.duplicate-uid`, remedy: delete the old file). `pbps.yml`
    /// goes last: it is what tells an old pbps not to read the project as
    /// having no identities.
    ///
    /// A move can turn a file into a directory (`a.json` becomes
    /// `a.json/a.json`) or back. The old path then blocks the new one, so it
    /// is first renamed to a sibling `.<name>.aside`, which is not a `.json`
    /// file: a crash with an aside left makes the next read refuse and name
    /// that entry, which holds the only copy of the old file and is renamed
    /// back by hand. A crash before the aside leaves the tree as it was; after
    /// the new files are in place it leaves both copies; the aside is deleted
    /// together with the removals, last.
    pub(crate) fn apply(&self) -> anyhow::Result<()> {
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut asides: Vec<(PathBuf, PathBuf)> = Vec::new();
        // Every directory this write creates, outermost first: only those are
        // taken away again on a failure.
        let mut created: Vec<PathBuf> = Vec::new();
        // Names a temp file or an aside must not take: every planned
        // destination and each of its ancestors (a declaration may be called
        // `.a.json.tmp/b.yml`), on top of what already exists on disk.
        let key = self.key;
        let mut reserved = self.reserved_names();
        let result = (|| -> anyhow::Result<()> {
            for (rel, text) in &self.files {
                let path = self.dir.join(rel);
                let parent = path.parent().unwrap_or(Path::new("."));
                // An ancestor that is a file today is about to be a directory.
                let mut ancestor = self.dir.clone();
                for part in Path::new(rel)
                    .parent()
                    .into_iter()
                    .flat_map(Path::components)
                {
                    ancestor.push(part);
                    if std::fs::symlink_metadata(&ancestor).is_ok_and(|m| !m.is_dir()) {
                        set_aside(&ancestor, &mut asides, &mut reserved, key)?;
                    }
                }
                let mut missing: Vec<PathBuf> = parent
                    .ancestors()
                    // Raw is right: `parent` was built by joining onto `self.dir`.
                    .take_while(|d| d.starts_with(&self.dir) && !d.exists())
                    .map(Path::to_path_buf)
                    .collect();
                missing.reverse();
                created.extend(missing);
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create `{}`", parent.display()))?;
                let tmp = crate::ids_cmd::unused_sibling(&path, "tmp", &mut reserved, key);
                // Recorded before the write: `write` can create the file and
                // then fail (no space, an I/O error), and the rollback must
                // find what the call may have left.
                staged.push((tmp.clone(), path));
                write_staged(&tmp, text)
                    .with_context(|| format!("cannot write `{}`", tmp.display()))?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            // Back to the tree as it was: the temp files, then the directories
            // this write created (they are empty by then), then the asides.
            return Err(match roll_back(&staged, &created, &asides) {
                Ok(()) => e,
                Err(left) => e.context(left),
            });
        }
        for (tmp, path) in &staged {
            // A directory the new file takes the place of (a move the other
            // way round).
            let mut displaced = None;
            if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
                set_aside(path, &mut asides, &mut reserved, key)?;
                displaced = asides.last().cloned();
            }
            if let Err(e) = std::fs::rename(tmp, path) {
                let e =
                    anyhow::Error::new(e).context(format!("cannot replace `{}`", path.display()));
                // The files placed so far stay (both copies of what moved, as
                // after a crash); the rest of the temp files go, and the
                // directory this file was to replace is put back.
                let pending: Vec<_> = staged
                    .iter()
                    // Raw is right: `tmp` is this very generated path, not a spelling of it.
                    .skip_while(|(t, _)| t != tmp)
                    .cloned()
                    .collect();
                let back: Vec<_> = displaced.into_iter().collect();
                return Err(match roll_back(&pending, &[], &back) {
                    Ok(()) => e,
                    Err(left) => e.context(left),
                });
            }
        }
        let placed: Vec<&PathBuf> = staged.iter().map(|(_, path)| path).collect();
        let placed_keys: BTreeSet<String> = placed.iter().map(|p| key.path(p)).collect();
        for rel in &self.removals {
            // What was set aside is removed where it went. The aside was made
            // under the spelling of the new path, the removal carries the old
            // one: they are compared by key.
            let path = redirected(&self.dir.join(rel), &asides, key);
            // A path that is the very file this write just placed is not an old
            // copy: on a case-insensitive filesystem `Customer.json` and
            // `customer.json` are one file, and the rename above already
            // replaced it. Asked of the filesystem, not of a case-folding rule.
            if placed_keys.contains(&key.path(&path))
                || placed.iter().any(|new| same_file(&path, new))
            {
                continue;
            }
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove `{}`", path.display()))?;
            // A directory a move emptied goes too; one that still holds
            // something stays. That covers a case-only directory rename
            // (`Sales/` to `sales/`, one directory on a case-insensitive
            // filesystem): the placed files are in it, so `remove_dir` refuses.
            let mut parent = path.parent();
            // Raw is right: `path` comes from `self.dir` (or an aside beside
            // such a path), so the prefix is `self.dir` as spelled.
            while let Some(dir) = parent.filter(|d| *d != self.dir && d.starts_with(&self.dir)) {
                if std::fs::remove_dir(dir).is_err() {
                    break;
                }
                parent = dir.parent();
            }
        }
        // An aside nothing removed holds a file the write did not account for:
        // keep it, and say where it is.
        if let Some((orig, aside)) = asides.iter().find(|(_, a)| a.exists()) {
            bail!(
                "`{}` was moved aside to `{}` and holds something this write did not remove; \
                 check it and delete or restore it",
                orig.display(),
                aside.display()
            );
        }
        if let Some((path, text)) = &self.config {
            write_atomic(path, text)?;
        }
        Ok(())
    }
}

/// Where `path` is now: under its aside when it, or a directory above it, was
/// set aside, else where it was. Compared by key: the aside is named after the
/// new path's spelling and `path` carries the old one.
fn redirected(path: &Path, asides: &[(PathBuf, PathBuf)], key: PathKey) -> PathBuf {
    for (orig, aside) in asides {
        if let Some(rest) = key.strip_prefix(path, orig) {
            // `join("")` would add a trailing slash and make the file a directory.
            return if rest.as_os_str().is_empty() {
                aside.clone()
            } else {
                aside.join(rest)
            };
        }
    }
    path.to_path_buf()
}

/// Writes one temp file. Under test a fault can be armed (`tests::fail_write_number`)
/// that creates the file and then fails, as a full disk does part-way.
fn write_staged(tmp: &Path, text: &str) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::fault_now() {
        std::fs::write(tmp, &text[..text.len() / 2])?;
        return Err(std::io::Error::other("injected: no space left on device"));
    }
    std::fs::write(tmp, text)
}

/// The device a path is on: `st_dev` on Unix. Elsewhere the volume (drive or
/// share prefix) stands in, as the standard library offers no volume serial.
fn device_of(path: &Path) -> std::io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(std::fs::metadata(path)?.dev())
    }
    #[cfg(not(unix))]
    {
        use std::hash::{Hash, Hasher};
        let full = std::fs::canonicalize(path)?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        full.components().next().hash(&mut hasher);
        Ok(hasher.finish())
    }
}

/// Whether two paths name one file or directory on disk, decided by the
/// filesystem (device and inode on Unix, the final path the system resolves
/// elsewhere), so a case-insensitive filesystem or a `..` in the path cannot
/// make a file look like two. A path that does not exist is nobody's twin.
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }
}

/// Undoes a write that failed part-way: removes the temp files and the
/// directories the write created (innermost first, and only if empty), then
/// renames each aside back. A step that cannot be undone is not ignored: the
/// error says what is left where, which is what the next read's refusal names.
fn roll_back(
    staged: &[(PathBuf, PathBuf)],
    created: &[PathBuf],
    asides: &[(PathBuf, PathBuf)],
) -> Result<(), String> {
    let mut left = Vec::new();
    for (tmp, _) in staged {
        if let Err(e) = std::fs::remove_file(tmp)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            left.push(format!("could not remove `{}`: {e}", tmp.display()));
        }
    }
    for dir in created.iter().rev() {
        if let Err(e) = std::fs::remove_dir(dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            left.push(format!("could not remove `{}`: {e}", dir.display()));
        }
    }
    for (orig, aside) in asides.iter().rev() {
        if let Err(e) = std::fs::rename(aside, orig) {
            left.push(format!(
                "`{}` is still at `{}` ({e}); rename it back",
                orig.display(),
                aside.display()
            ));
        }
    }
    if left.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the write could not be undone: {}",
            left.join("; ")
        ))
    }
}

/// Renames `path` to an unused `.<name>.aside` sibling and records the pair.
fn set_aside(
    path: &Path,
    asides: &mut Vec<(PathBuf, PathBuf)>,
    reserved: &mut BTreeSet<String>,
    key: PathKey,
) -> anyhow::Result<()> {
    let aside = crate::ids_cmd::unused_sibling(path, "aside", reserved, key);
    std::fs::rename(path, &aside).with_context(|| {
        format!(
            "cannot move `{}` aside to make room for the identity file that takes its place",
            path.display()
        )
    })?;
    asides.push((path.to_owned(), aside));
    Ok(())
}

/// What the tree on disk holds, by uid.
#[derive(Default)]
struct Existing {
    /// The file each table, role, type and extension owns.
    files: BTreeMap<Uid, (String, TreeFile)>,
    /// The table whose file holds each column record, live or dead.
    column_home: BTreeMap<Uid, Uid>,
    /// The table whose file holds each partition child's record.
    child_home: BTreeMap<Uid, Uid>,
    /// Each table file's column records in file order.
    order: BTreeMap<Uid, Vec<Uid>>,
}

fn index(tree: &BTreeMap<String, String>) -> anyhow::Result<Existing> {
    let mut out = Existing::default();
    for (rel, text) in tree {
        let file = ids_tree::parse(rel, text)
            .with_context(|| format!("identity file `{rel}` cannot be read"))?;
        if let Body::Table(t) = &file.body {
            out.order.insert(
                file.uid.clone(),
                t.columns.iter().map(|(u, _)| u.clone()).collect(),
            );
            for (column, _) in &t.columns {
                out.column_home.insert(column.clone(), file.uid.clone());
            }
            for (child, _) in &t.partitions {
                out.child_home.insert(child.clone(), file.uid.clone());
            }
        }
        out.files.insert(file.uid.clone(), (rel.clone(), file));
    }
    Ok(out)
}

/// The new tree for `after`, with every refusal decided and the result proved
/// to read back to `after`. Nothing is written.
///
/// `declared` is where each declaration is, taken from the same load that
/// produced `after`.
pub(crate) fn plan(
    project: &Project,
    after: &IdsFile,
    declared: &DeclaredPaths,
) -> anyhow::Result<TreeWrite> {
    let key = PathKey::probe(&project.ids_dir()).with_context(|| {
        format!(
            "cannot tell whether `{}` is case-sensitive",
            project.ids_dir().display()
        )
    })?;
    plan_keyed(project, after, declared, key)
}

/// [`plan`] with the path key given, so every case can run on any filesystem.
pub(crate) fn plan_keyed(
    project: &Project,
    after: &IdsFile,
    declared: &DeclaredPaths,
    key: PathKey,
) -> anyhow::Result<TreeWrite> {
    after
        .validate()
        .context("refusing to write inconsistent identities")?;
    if legacy_present(project)? {
        // `read_opt` refuses this first; a writer reached some other way must
        // not put a tree beside the file.
        bail!(
            "`{}` still exists; run `pbps ids migrate` before writing identities",
            project.ids_file().display()
        );
    }
    let dirs = Dirs::of(project);
    let tree = read_ids_tree(&project.ids_dir())?;
    let existing = index(&tree)?;
    let wanted = declared_placement(&dirs, after, declared)?;
    let name_of = |uid: &Uid| -> String {
        let shown = |s: &dyn std::fmt::Display| s.to_string();
        let name = after
            .tables
            .get(uid)
            .map(|n| shown(&n.shown()))
            .or_else(|| after.roles.get(uid).map(|n| shown(&n.shown())))
            .or_else(|| after.types.get(uid).map(|n| shown(&n.shown())))
            .or_else(|| after.extensions.get(uid).map(|n| shown(&n.shown())))
            .or_else(|| {
                after
                    .tombstones
                    .get(uid)
                    .map(|t| pbps_model::Text::new(t.was.clone()).shown().to_string())
            });
        name.unwrap_or_else(|| uid.to_string())
    };

    // Every uid that owns, or is held in, a file: live objects and dropped ones.
    let objects: BTreeSet<Uid> = after
        .tables
        .keys()
        .chain(after.roles.keys())
        .chain(after.types.keys())
        .chain(after.extensions.keys())
        .chain(after.tombstones.keys().filter(|u| {
            matches!(
                u.kind(),
                UidKind::Table | UidKind::Role | UidKind::Type | UidKind::Extension
            )
        }))
        .cloned()
        .collect();
    // Where each object goes (design §3.4). A child lives in its parent's file
    // and has none of its own; everything else owns the file its declaration
    // mirrors, or the one it already has, and a dropped object keeps its.
    let mut children: BTreeMap<Uid, Uid> = BTreeMap::new();
    let mut want: BTreeMap<Uid, String> = BTreeMap::new();
    for uid in &objects {
        let live = !after.tombstones.contains_key(uid);
        let old = existing.files.get(uid).map(|(p, _)| p);
        if uid.kind() == UidKind::Table {
            // Step 6: a table that becomes, stops being, or changes parent as a
            // partition. A dropped child stays recorded where it was.
            // A table with no declaration of its own, live or dropped, stays
            // where the tree has it, as a primary identity keeps its file.
            let parent = if live {
                wanted.children.get(uid).or_else(|| {
                    (!wanted.paths.contains_key(uid))
                        .then(|| existing.child_home.get(uid))
                        .flatten()
                })
            } else {
                existing.child_home.get(uid)
            };
            if let Some(parent) = parent {
                children.insert(uid.clone(), parent.clone());
                continue;
            }
        }
        let path = match (live, wanted.paths.get(uid).or(old)) {
            (true, Some(path)) => path.clone(),
            (true, None) => {
                return Err(refused(format!(
                    "{} has no declaration to mirror and no file of its own",
                    name_of(uid)
                )));
            }
            // Step 4: a drop keeps the file it has. One the tree never held gets
            // the aside name, as `migrate` gives a dropped object with no YAML.
            (false, _) => old.cloned().unwrap_or_else(|| aside_of(after, uid, 0)),
        };
        want.insert(uid.clone(), path);
    }
    // Step 5: the final destinations, checked once. Two objects may not share a
    // file (same key) and no destination may be a directory above another (a
    // file cannot also be a directory). A dropped object in the way goes to its
    // aside name (`<name>.<uid>.json`) and the check runs again on the result,
    // since the aside is itself a destination that can collide; a name that does
    // is replaced by the next free one. Two live objects in conflict are two
    // declarations whose files cannot both exist, which the loader refuses.
    let dead = |u: &Uid| after.tombstones.contains_key(u);
    let mut spare: BTreeMap<Uid, usize> = BTreeMap::new();
    while let Some((first, second, same)) = first_conflict(&want, key) {
        let victim = if dead(&first) {
            first.clone()
        } else if dead(&second) {
            second.clone()
        } else if same {
            return Err(refused(format!(
                "{} and {} are both declared at `{}` and `{}`, which are one file here",
                name_of(&first),
                name_of(&second),
                shown(&dirs, &want[&first]),
                shown(&dirs, &want[&second])
            )));
        } else {
            return Err(refused(format!(
                "{} is declared at `{}` and {} below it at `{}`; a file cannot also be a \
                 directory, so rename one of the two YAML files",
                name_of(&first),
                shown(&dirs, &want[&first]),
                name_of(&second),
                shown(&dirs, &want[&second])
            )));
        };
        // The aside name; if the victim is already there and still collides,
        // the next one (`<name>.<uid>.1.json`, ...): a dropped object's aside is
        // nobody's choice, and the reader takes a file by its content.
        let mut n = spare.get(&victim).copied().unwrap_or(0);
        let mut path = aside_of(after, &victim, n);
        while path == want[&victim] {
            n += 1;
            path = aside_of(after, &victim, n);
        }
        spare.insert(victim.clone(), n);
        want.insert(victim, path);
    }
    let mut placement = Placement {
        paths: want,
        children,
        ..Placement::default()
    };
    // A column tombstone belongs to the file that held its record, even when
    // the table has been renamed since. A partition child has no file, so the
    // tombstones of its columns (dropped before it was attached) are kept in
    // its parent's: a tombstone names its column in full and needs no table
    // file of its own to say what it retired.
    let table_named: BTreeMap<String, &Uid> = after
        .tables
        .iter()
        .map(|(u, n)| (n.raw().to_string(), u))
        .chain(
            after
                .tombstones
                .iter()
                .filter(|(u, _)| u.kind() == UidKind::Table)
                .map(|(u, t)| (t.was.clone(), u)),
        )
        .collect();
    for (uid, tomb) in &after.tombstones {
        if uid.kind() != UidKind::Column {
            continue;
        }
        let home = existing.column_home.get(uid).or_else(|| {
            let column: pbps_model::ColumnRef = tomb.was.parse().ok()?;
            table_named.get(&column.table.raw().to_string()).copied()
        });
        if let Some(home) = home {
            let owner = placement.children.get(home).unwrap_or(home);
            placement.owners.insert(uid.clone(), owner.clone());
        }
    }
    // The record order: what is on disk, with a new column after its YAML
    // predecessor and a tombstone where it was.
    for (uid, (_, file)) in &existing.files {
        if matches!(file.body, Body::Table(_)) {
            let columns = declared_columns(after, &wanted, &existing, uid);
            let retired: BTreeSet<Uid> = after
                .tombstones
                .keys()
                .filter(|u| existing.column_home.get(*u) == Some(uid))
                .cloned()
                .collect();
            let on_disk = existing.order.get(uid).cloned().unwrap_or_default();
            placement.order.insert(
                uid.clone(),
                ids_tree::next_order(&on_disk, &columns, &retired),
            );
        }
    }
    for (uid, columns) in &wanted.order {
        placement
            .order
            .entry(uid.clone())
            .or_insert_with(|| columns.clone());
    }

    let new = ids_tree::split(after, &placement).map_err(|e| {
        // Name what the user can see: a bare uid says nothing about which
        // declaration cannot be recorded.
        if let ids_tree::TreeError::NoPath { uid } = &e {
            refused(format!(
                "{} ({uid}) has no identity file to be recorded in",
                name_of(uid)
            ))
        } else {
            anyhow::Error::new(e).context("cannot place the identities in files")
        }
    })?;
    let removals: Vec<String> = tree
        .keys()
        .filter(|rel| !new.contains_key(*rel))
        .cloned()
        .collect();
    let files: BTreeMap<String, String> = new
        .into_iter()
        .filter(|(rel, text)| tree.get(rel) != Some(text))
        .collect();
    // The proof: what is on disk after the write reads back to what was asked
    // for. A tree that does not is never written.
    // What the disk holds afterwards, by key: a placed file replaces the file
    // it is the same as, and a removal of a file the write placed is skipped.
    let placed: BTreeSet<String> = files.keys().map(|rel| key.of(rel)).collect();
    let mut merged: BTreeMap<String, (String, String)> = tree
        .iter()
        .map(|(rel, text)| (key.of(rel), (rel.clone(), text.clone())))
        .collect();
    for (rel, text) in &files {
        merged.insert(key.of(rel), (rel.clone(), text.clone()));
    }
    for rel in &removals {
        if !placed.contains(&key.of(rel)) {
            merged.remove(&key.of(rel));
        }
    }
    let merged: BTreeMap<String, String> = merged.into_values().collect();
    let back = ids_tree::assemble(&merged).context("the new tree does not read back")?;
    if &back != after {
        bail!("the new tree reads back to a different mapping; nothing was written");
    }
    // A project that never named `ids_dir` names it now, so an older pbps stops
    // at the config instead of reading no tree as no identities (§6.2).
    let config = if tree.is_empty() && !files.is_empty() {
        new_config_text(project)?.map(|text| (project.config_file(), text))
    } else {
        None
    };
    Ok(TreeWrite {
        dir: project.ids_dir(),
        files,
        removals,
        config,
        key,
    })
}

/// The file a dropped object gets when no declaration names its place.
fn aside_of(after: &IdsFile, uid: &Uid, n: usize) -> String {
    let was = after
        .tombstones
        .get(uid)
        .map_or_else(|| uid.to_string(), |t| t.was.clone());
    let name = crate::ids_cmd::aside(&was, uid);
    match n {
        0 => name,
        // `<name>.<uid>.json` -> `<name>.<uid>.<n>.json`
        _ => format!("{}.{n}.json", name.trim_end_matches(".json")),
    }
}

/// The first two destinations that cannot both exist, by key: the same file, or
/// one a directory above the other. `true` when they are the same file.
fn first_conflict(want: &BTreeMap<Uid, String>, key: PathKey) -> Option<(Uid, Uid, bool)> {
    let mut by_key: BTreeMap<String, &Uid> = BTreeMap::new();
    for (uid, path) in want {
        if let Some(other) = by_key.insert(key.of(path), uid) {
            return Some((other.clone(), uid.clone(), true));
        }
    }
    for (uid, path) in want {
        let folded = key.of(path);
        // Every directory above this destination, outermost first.
        for (at, _) in folded.match_indices('/') {
            if let Some(above) = by_key.get(&folded[..at]) {
                return Some(((*above).clone(), uid.clone(), false));
            }
        }
    }
    None
}

/// Whether the tree on disk is not what `after` and the declarations in
/// `loaded` say it should be: a file to write, move or remove (design §3.5).
/// This is `plan --check`'s "stale", and it also sees a declaration that moved
/// or bytes that are not canonical when the mapping itself is unchanged.
pub(crate) fn is_stale(
    project: &Project,
    after: &IdsFile,
    loaded: &pbps_load::Loaded,
) -> anyhow::Result<bool> {
    let paths = loaded.declared_paths(&project.schema_dir())?;
    Ok(!plan(project, after, &paths)?.is_empty())
}

/// The live columns of the table that owns `file`, in YAML order when its
/// declaration says, else in the order the file already has.
fn declared_columns(
    after: &IdsFile,
    wanted: &Placement,
    existing: &Existing,
    table: &Uid,
) -> Vec<Uid> {
    if let Some(order) = wanted.order.get(table) {
        return order.clone();
    }
    let Some(name) = after.tables.get(table) else {
        return Vec::new();
    };
    existing
        .order
        .get(table)
        .into_iter()
        .flatten()
        .filter(|u| after.columns.get(*u).is_some_and(|c| &c.table == name))
        .cloned()
        .collect()
}

/// The validator's findings for the tree on disk against the declarations in
/// `loaded`, as `validate` reports them.
///
/// Two codes are left to their own commands: a declaration with no identity
/// file yet is what `pbps plan` mints (and `plan --check` reports as stale), and
/// bytes that are merely not canonical are `pbps ids fmt --check`'s. The legacy
/// layout cannot be here: reading it was refused before this ran.
pub(crate) fn validator_findings(
    project: &Project,
    loaded: &pbps_load::Loaded,
) -> Vec<crate::output::Finding> {
    use pbps_model::ids_check::{self, Code, Input};
    let dirs = Dirs::of(project);
    let Ok(tree) = read_ids_tree(&project.ids_dir()) else {
        return Vec::new();
    };
    let Ok(paths) = loaded.declared_paths(&project.schema_dir()) else {
        return Vec::new();
    };
    let declared = crate::ids_cmd::declarations(&dirs, loaded, &paths);
    ids_check::check(&Input {
        layout: dirs.layout(),
        declared: &declared,
        ids: &tree,
        legacy_file: false,
    })
    .into_iter()
    .filter(|f| !matches!(f.code, Code::YamlWithoutIds | Code::NotCanonical))
    .map(|f| {
        crate::output::Finding::error(f.code.as_str(), f.message)
            .at(project.root().join(&f.path), None)
            .remedy(f.remedy)
    })
    .collect()
}

/// Plans and applies in one step, for the commands that already hold the
/// declarations on disk.
pub(crate) fn write(project: &Project, after: &IdsFile) -> anyhow::Result<()> {
    after
        .validate()
        .context("refusing to write inconsistent identities")?;
    let (_, paths) = crate::ids_cmd::load(project)?;
    plan(project, after, &paths)?.apply()
}

/// Writes `ids` as a tree for a test, each object at `<uid>.json` and with no
/// regard for any declaration: a unit test's fixture, not what `plan` writes.
#[cfg(test)]
pub(crate) fn write_ids_tree(project: &Project, ids: &IdsFile) {
    let mut placement = Placement::default();
    let uids = ids
        .tables
        .keys()
        .chain(ids.roles.keys())
        .chain(ids.types.keys())
        .chain(ids.extensions.keys())
        .chain(ids.tombstones.keys().filter(|u| {
            matches!(
                u.kind(),
                UidKind::Table | UidKind::Role | UidKind::Type | UidKind::Extension
            )
        }));
    for uid in uids {
        placement.paths.insert(uid.clone(), format!("{uid}.json"));
    }
    for (rel, text) in ids_tree::split(ids, &placement).unwrap() {
        write_atomic(&project.ids_dir().join(rel), &text).unwrap();
    }
}

#[cfg(test)]
mod tests;
