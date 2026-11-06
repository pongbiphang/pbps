//! Fails closed on a migrated project until the switch (#2600).
//!
//! `pbps ids migrate` deletes `schema.ids.json`, and nothing but the `ids`
//! commands reads the tree yet. A reader that took "no legacy file" for "no
//! identities" would mint fresh uids for every object, silently. So wherever
//! the legacy file is read and found absent, a tree beside it is a refusal, in
//! the working tree and in a revision alike. The switch (PR 5) makes the tree
//! the thing that is read and removes this guard with the legacy reader.

use std::path::Path;

use anyhow::Context;
use pbps_config::Project;

/// What the user is told, naming the directory as the project spells it.
pub(crate) fn message(ids_dir: &str) -> String {
    format!(
        "this project keeps its identities in the identity tree `{}`, and this build does not read it yet \
         (the read and write switch is #2600 PR 5). Nothing was read as \"no identities\"; use a pbps that \
         reads the tree, or restore `schema.ids.json` from git",
        pbps_model::Text::new(ids_dir.to_owned()).shown()
    )
}

/// Whether `dir` holds an identity tree. A directory with entries, or a path
/// that is not a directory at all, counts: either is somebody's identities, and
/// "cannot tell" is not "none". Only an absent or empty directory is no tree.
fn tree_present(dir: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(std::fs::read_dir(dir)?.next().is_some()),
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Refuses when the working tree has an identity tree. Call where the legacy
/// file was found absent.
pub(crate) fn refuse_tree(project: &Project) -> anyhow::Result<()> {
    let dir = project.ids_dir();
    let present = tree_present(&dir)
        .with_context(|| format!("cannot inspect identity directory `{}`", dir.display()))?;
    if present {
        anyhow::bail!(
            "{}",
            message(&project.config.ids_dir().display().to_string())
        );
    }
    Ok(())
}

/// Whether the working tree has an identity tree at `dir` (a path as the
/// config spells it, under `root`).
pub(crate) fn working_tree_present(root: &Path, dir: &Path) -> std::io::Result<bool> {
    tree_present(&root.join(dir))
}

/// Whether `rev` records any file under `ids_dir_rel` (as git names it).
pub(crate) fn tree_present_at(root: &Path, rev: &str, ids_dir_rel: &str) -> anyhow::Result<bool> {
    let listed = crate::baseline::git(
        root,
        &["ls-tree", "-r", "--name-only", "-z", rev, "--", ids_dir_rel],
    )
    .with_context(|| format!("cannot list `{ids_dir_rel}` at `{rev}`"))?;
    Ok(!listed.is_empty())
}
