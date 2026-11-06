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
//! **The write is the minimal sync.** [`plan`] decides the whole new tree, every
//! refusal included, before [`TreeWrite::apply`] writes the first byte, and it
//! proves the result reads back to the very mapping it was made from. It places
//! an identity in the file it already has, or at the path its declaration
//! mirrors if it has none, and it writes nothing else. What needs a decision
//! about *moving* files (design §3.4 steps 2, 5 and 6) is refused by name; that
//! is #2600 PR 5b, which extends [`plan`] rather than replacing it. The refused
//! cases are listed in [`Unfinished`].

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
        |path, _mode| path.starts_with(&below),
        |path, mode, bytes| {
            let shown = pbps_model::Text::new(path.to_owned()).shown().to_string();
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
/// tree. A file of the tree not yet written is not listed, so callers that
/// write somewhere new also refuse a destination inside the directory.
pub(crate) fn protected(project: &Project) -> Vec<PathBuf> {
    let mut out = vec![project.config_file(), project.ids_file()];
    if let Ok(tree) = read_ids_tree(&project.ids_dir()) {
        out.extend(tree.keys().map(|rel| project.ids_dir().join(rel)));
    }
    out
}

// ----------------------------------------------------------------- write ----

/// What the write side does not do yet. Each is refused by name, before
/// anything is written, with the same closing sentence; #2600 PR 5b turns each
/// into a step of the sync (design §3.4) and removes it from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unfinished {
    /// A declaration moved or was renamed to another file (§3.4 step 2).
    Relocation,
    /// A new declaration's path is held by another identity's file
    /// (§3.4 step 5, the dropped file renamed aside).
    PathDisplacement,
    /// A table became, or stopped being, a partition child of another table's
    /// file (§3.4 step 6).
    PartitionMove,
    /// An identity file the new mapping no longer holds, or a drop of an
    /// identity that has no file to mark (§3.4 step 4 for an object the tree
    /// never held).
    Removal,
    /// A declaration with an identity and no path to put its file at.
    NoPlace,
}

impl Unfinished {
    fn case(self) -> &'static str {
        match self {
            Self::Relocation => "moving an identity file",
            Self::PathDisplacement => {
                "placing a new declaration over a path another identity holds"
            }
            Self::PartitionMove => "attaching or detaching a partition",
            Self::Removal => "removing an identity file",
            Self::NoPlace => "placing an identity that has no declaration",
        }
    }
}

fn unfinished(case: Unfinished, detail: String) -> anyhow::Error {
    anyhow::anyhow!(
        "cannot write the identity tree: {} is not supported yet ({detail}). The switch to the \
         identity tree is unfinished (#2600 PR 5b); nothing was written",
        case.case()
    )
}

/// What one write changes, decided and checked, not yet written.
#[derive(Debug, Default)]
pub(crate) struct TreeWrite {
    dir: PathBuf,
    /// Files whose bytes change or that do not exist yet, by path relative to
    /// `dir`.
    files: BTreeMap<String, String>,
    /// `pbps.yml` with `ids_dir` named, when this write creates the tree.
    config: Option<(PathBuf, String)>,
}

impl TreeWrite {
    /// Writes every file next to its final name first, and only then renames
    /// them into place, so a failure while writing leaves the tree as it was.
    /// `pbps.yml` goes last: it is what tells an old pbps not to read the
    /// project as having no identities.
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
    if let Some((path, _)) = existing
        .files
        .iter()
        .find(|(u, _)| !objects.contains(*u))
        .map(|(_, v)| v)
    {
        return Err(unfinished(
            Unfinished::Removal,
            format!(
                "`{}` holds an identity the new mapping no longer has",
                shown(&dirs, path)
            ),
        ));
    }

    let mut placement = Placement::default();
    for uid in &objects {
        let live = !after.tombstones.contains_key(uid);
        let old = existing.files.get(uid).map(|(p, f)| (p, f));
        let is_table = uid.kind() == UidKind::Table;
        // A table's place among partitions (§3.4 step 6): a child lives in its
        // parent's file and has none of its own.
        if is_table {
            let was_child = existing.child_home.get(uid);
            let wants = wanted.children.get(uid).filter(|_| live);
            let refuse = |what: String| unfinished(Unfinished::PartitionMove, what);
            if old.is_some() && wants.is_some() {
                return Err(refuse(format!("{} becomes a partition", name_of(uid))));
            }
            if live && was_child.is_some() && wants.is_none() {
                return Err(refuse(format!("{} stops being a partition", name_of(uid))));
            }
            if let (Some(was), Some(now)) = (was_child, wants)
                && was != now
            {
                return Err(refuse(format!("{} changes parent table", name_of(uid))));
            }
            if let Some(parent) = wants.or(was_child) {
                placement.children.insert(uid.clone(), parent.clone());
                continue;
            }
        }
        let new_path = wanted.paths.get(uid);
        let path = match (old, new_path) {
            (Some((p, _)), Some(n)) if live && p != n => {
                return Err(unfinished(
                    Unfinished::Relocation,
                    format!(
                        "{} is recorded in `{}` but its declaration now mirrors `{}`",
                        name_of(uid),
                        shown(&dirs, p),
                        shown(&dirs, n)
                    ),
                ));
            }
            (Some((p, _)), _) => p.clone(),
            (None, Some(n)) if live => {
                if tree.contains_key(n) {
                    return Err(unfinished(
                        Unfinished::PathDisplacement,
                        format!(
                            "{} is declared where `{}` already holds another identity",
                            name_of(uid),
                            shown(&dirs, n)
                        ),
                    ));
                }
                n.clone()
            }
            (None, _) if live => {
                return Err(unfinished(
                    Unfinished::NoPlace,
                    format!("{} has no declaration to mirror", name_of(uid)),
                ));
            }
            (None, _) => {
                return Err(unfinished(
                    Unfinished::Removal,
                    format!(
                        "{} is dropped but the tree holds no file for it",
                        name_of(uid)
                    ),
                ));
            }
        };
        placement.paths.insert(uid.clone(), path);
    }
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
    let files: BTreeMap<String, String> = new
        .into_iter()
        .filter(|(rel, text)| tree.get(rel) != Some(text))
        .collect();
    // The proof: what is on disk after the write reads back to what was asked
    // for. A tree that does not is never written.
    let mut merged = tree.clone();
    merged.extend(files.clone());
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
        config,
    })
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
