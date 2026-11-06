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
                matches!(mode, "100644" | "100755") && path.ends_with(".json"),
                "`{shown}` at `{rev}` is not a regular .json file"
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
    if crate::paths::is_inside(destination, &project.ids_dir())? {
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
}

impl TreeWrite {
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
    pub(crate) fn apply(&self) -> anyhow::Result<()> {
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
        let result = (|| -> anyhow::Result<()> {
            for (rel, text) in &self.files {
                let path = self.dir.join(rel);
                let parent = path.parent().unwrap_or(Path::new("."));
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create `{}`", parent.display()))?;
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let tmp = parent.join(format!(".{name}.tmp"));
                std::fs::write(&tmp, text)
                    .with_context(|| format!("cannot write `{}`", tmp.display()))?;
                staged.push((tmp, path));
            }
            Ok(())
        })();
        if let Err(e) = result {
            for (tmp, _) in &staged {
                let _ = std::fs::remove_file(tmp);
            }
            return Err(e);
        }
        for (tmp, path) in &staged {
            std::fs::rename(tmp, path)
                .with_context(|| format!("cannot replace `{}`", path.display()))?;
        }
        for rel in &self.removals {
            let path = self.dir.join(rel);
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove `{}`", path.display()))?;
            // A directory a move emptied goes too; one that still holds
            // something stays.
            let mut parent = path.parent();
            while let Some(dir) = parent.filter(|d| *d != self.dir && d.starts_with(&self.dir)) {
                if std::fs::remove_dir(dir).is_err() {
                    break;
                }
                parent = dir.parent();
            }
        }
        if let Some((path, text)) = &self.config {
            write_atomic(path, text)?;
        }
        Ok(())
    }
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
            (false, _) => old.cloned().unwrap_or_else(|| aside_of(after, uid)),
        };
        want.insert(uid.clone(), path);
    }
    // Step 5: a path wanted by a live object and held by a dropped one sends the
    // dropped file aside (`<name>.<uid>.json`); the live object takes the path.
    // Two live objects for one path would be two declarations for one file,
    // which the loader already refuses.
    let mut holders: BTreeMap<String, Vec<Uid>> = BTreeMap::new();
    for (uid, path) in &want {
        holders.entry(path.clone()).or_default().push(uid.clone());
    }
    for (path, uids) in holders.into_iter().filter(|(_, u)| u.len() > 1) {
        let (dead, live): (Vec<_>, Vec<_>) = uids
            .into_iter()
            .partition(|u| after.tombstones.contains_key(u));
        if live.len() > 1 {
            return Err(refused(format!(
                "{} and {} are both declared at `{}`",
                name_of(&live[0]),
                name_of(&live[1]),
                shown(&dirs, &path)
            )));
        }
        // At most one dropped file kept this path, but a dropped object with no
        // file of its own may have been assigned it too; all go aside.
        for uid in dead {
            want.insert(uid.clone(), aside_of(after, &uid));
        }
    }
    let mut placement = Placement {
        paths: want,
        children,
        ..Placement::default()
    };
    // A column tombstone belongs to the file that held its record, even when
    // the table has been renamed since.
    for uid in after.tombstones.keys() {
        if let Some(home) = existing.column_home.get(uid) {
            placement.owners.insert(uid.clone(), home.clone());
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

    let new = ids_tree::split(after, &placement).context("cannot place the identities in files")?;
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
    let mut merged = tree.clone();
    merged.extend(files.clone());
    for rel in &removals {
        merged.remove(rel);
    }
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
    })
}

/// The file a dropped object gets when no declaration names its place.
fn aside_of(after: &IdsFile, uid: &Uid) -> String {
    let was = after
        .tombstones
        .get(uid)
        .map_or_else(|| uid.to_string(), |t| t.was.clone());
    crate::ids_cmd::aside(&was, uid)
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
