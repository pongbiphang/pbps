//! `pbps ids migrate | fmt | resolve` (#2600, design §5, §6.1).
//!
//! Three commands that act on the identity tree and nothing else. No other
//! command reads the tree yet: `plan`, `validate` and the rest keep using
//! `schema.ids.json` until the read and write switch. Nothing here chooses an
//! identity for the user: `resolve` merges by uid and refuses what the resolved
//! YAML cannot decide, and there is no `--take`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::Subcommand;
use pbps_config::{Config, Project};
use pbps_load::{DeclaredPaths, Loaded, load_schema_dir, read_ids_tree};
use pbps_model::diagnostic::Declaration as Source;
use pbps_model::ids_check::{self, Declaration, Input, Layout};
use pbps_model::ids_merge::{self, YamlView};
use pbps_model::ids_tree::{self, Placement};
use pbps_model::{IdsFile, Intent, Uid, UidKind};

#[derive(Subcommand)]
pub(crate) enum IdsCommand {
    /// Convert `schema.ids.json` to one identity file per declaration under `ids_dir`
    ///
    /// Writes the tree, checks it reads back to the same mapping, replaces
    /// `ids_file` with `ids_dir` in pbps.yml and deletes the old file. It
    /// stages nothing: review `git diff` and commit it as one change. Run it
    /// again after an interruption and it finishes the job.
    Migrate,

    /// Rewrite every identity file to its canonical bytes
    ///
    /// The meaning never changes. `--check` lists the files that differ and
    /// exits non-zero without writing.
    Fmt {
        /// List non-canonical files instead of rewriting them
        #[arg(long)]
        check: bool,
    },

    /// Merge identity files git left unmerged, after the YAML conflicts are resolved
    ///
    /// Reads base, ours and theirs from the index and merges by uid. A record
    /// the two sides changed differently is decided by the resolved YAML where
    /// it can say (the name it declares) and otherwise refused, naming the uid
    /// and both sides. Writes canonical bytes and stages nothing: `git add`
    /// the ids directory and commit.
    Resolve,
}

pub(crate) fn run(project: &Project, command: IdsCommand) -> anyhow::Result<()> {
    match command {
        IdsCommand::Migrate => migrate(project),
        IdsCommand::Fmt { check } => fmt(project, check),
        IdsCommand::Resolve => resolve(project),
    }
}

// ---------------------------------------------------------------- shared ----

/// `a/./b/` as `a/b`: the project-relative spelling the validator matches on.
fn spelled(path: &Path) -> String {
    path.components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

struct Dirs {
    schema: String,
    ids: String,
}

impl Dirs {
    fn of(project: &Project) -> Self {
        Self {
            schema: spelled(&project.config.schema_dir),
            ids: spelled(&project.config.ids_dir()),
        }
    }
    fn layout(&self) -> Layout<'_> {
        Layout {
            schema_dir: &self.schema,
            ids_dir: &self.ids,
        }
    }
    /// The ids path (relative to the ids directory) of a declaration file.
    fn mirrored(&self, rel_yaml: &str) -> anyhow::Result<String> {
        ids_check::mirrored(self.layout(), &format!("{}/{rel_yaml}", self.schema))
            .with_context(|| format!("`{rel_yaml}` is not a .yml or .yaml declaration"))
    }
}

fn load(project: &Project) -> anyhow::Result<(Loaded, DeclaredPaths)> {
    let dir = project.schema_dir();
    let loaded = load_schema_dir(&dir).map_err(|errors| {
        anyhow::anyhow!(
            "the declarations do not load; fix them first:\n{}",
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    let paths = loaded.declared_paths(&dir)?;
    Ok((loaded, paths))
}

/// Writes through a temporary name in the same directory, so a crash leaves
/// either the old file or the new one (design §2.4 rule 4).
fn write_atomic(path: &Path, text: &str) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create `{}`", dir.display()))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write `{}`", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace `{}`", path.display()))
}

fn shown(dirs: &Dirs, rel: &str) -> String {
    pbps_model::Text::new(format!("{}/{rel}", dirs.ids))
        .shown()
        .to_string()
}

fn read_tree(project: &Project) -> anyhow::Result<BTreeMap<String, String>> {
    read_ids_tree(&project.ids_dir()).map_err(Into::into)
}

// --------------------------------------------------------------- migrate ----

fn migrate(project: &Project) -> anyhow::Result<()> {
    let dirs = Dirs::of(project);
    let tree = read_tree(project)?;
    let Some(ids) = crate::read_ids_opt(project)? else {
        if tree.is_empty() {
            bail!(
                "nothing to migrate: `{}` does not exist and `{}` holds no identity files",
                project.ids_file().display(),
                dirs.ids
            );
        }
        // The old file is gone and a tree stands: an interrupted run, or one
        // that finished. Only the config can still be wrong.
        return finish_config(project).map(|changed| {
            println!(
                "{}",
                if changed {
                    "finished an interrupted migration: pbps.yml now names `ids_dir`"
                } else {
                    "already migrated: nothing to do"
                }
            );
        });
    };
    let (_, paths) = load(project)?;
    let files = place(&dirs, &ids, &paths)?;
    // Section 6.1 step 4: the tree must read back to the very mapping it came
    // from, or nothing is written.
    let back = ids_tree::assemble(&files).context("the new tree does not read back")?;
    if back != ids {
        bail!("the new tree reads back to a different mapping; nothing was written");
    }
    // A tree beside the file is acceptable only as the half-finished result of
    // this very command; anything else is somebody's work.
    if !tree.is_empty() && tree != files {
        bail!(
            "`{}` already holds an identity tree that differs from `{}`; migrate refuses to \
             overwrite it. Remove one of them after checking `git diff`",
            dirs.ids,
            project.ids_file().display()
        );
    }
    let config = new_config_text(project)?;
    for (rel, text) in &files {
        if tree.get(rel) != Some(text) {
            write_atomic(&project.ids_dir().join(rel), text)?;
        }
    }
    std::fs::remove_file(project.ids_file())
        .with_context(|| format!("cannot remove `{}`", project.ids_file().display()))?;
    if let Some(text) = config {
        write_atomic(&project.config_file(), &text)?;
    }
    println!(
        "migrated {} identities into {} file(s) under {}; review `git diff` and commit",
        ids.tables.len() + ids.roles.len() + ids.types.len() + ids.extensions.len(),
        files.len(),
        dirs.ids
    );
    Ok(())
}

/// The file name a dropped object keeps when no YAML names its place
/// (design §3.4 step 5): its last name and its uid.
fn aside(was: &str, uid: &Uid) -> String {
    let safe: String = was
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.{uid}.json")
}

/// Section 6.1 steps 2 and 3: every object's records into the file at its
/// mirrored path, columns in YAML order.
fn place(
    dirs: &Dirs,
    ids: &IdsFile,
    paths: &DeclaredPaths,
) -> anyhow::Result<BTreeMap<String, String>> {
    fn invert<K: Ord + Clone, V: Ord + Clone>(m: &BTreeMap<K, V>) -> BTreeMap<V, K> {
        m.iter().map(|(k, v)| (v.clone(), k.clone())).collect()
    }
    let (tables, roles) = (invert(&ids.tables), invert(&ids.roles));
    let (types, extensions) = (invert(&ids.types), invert(&ids.extensions));
    let mut placement = Placement::default();
    for (source, file) in &paths.files {
        let uid = match source {
            Source::Table(n) => tables.get(n),
            Source::Role(n) => roles.get(n),
            Source::Type(n) => types.get(n),
            Source::Extension(n) => extensions.get(n),
            Source::Module(_) => None,
        };
        // A declaration the old file never knew gets its identity from `plan`.
        let Some(uid) = uid else { continue };
        placement
            .paths
            .insert(uid.clone(), dirs.mirrored(&file.path)?);
        if let Source::Table(name) = source {
            let order = file.columns.iter().filter_map(|column| {
                ids.columns
                    .iter()
                    .find(|(_, c)| c.table == *name && c.name == *column)
                    .map(|(u, _)| u.clone())
            });
            placement.order.insert(uid.clone(), order.collect());
        }
    }
    for (child, parent) in &paths.children {
        if let (Some(c), Some(p)) = (tables.get(child), tables.get(parent)) {
            placement.children.insert(c.clone(), p.clone());
        }
    }
    let placed =
        |uid: &Uid| placement.paths.contains_key(uid) || placement.children.contains_key(uid);
    let mut missing: Vec<String> = Vec::new();
    for (uid, name) in &ids.tables {
        if !placed(uid) {
            missing.push(format!("table {}", name.shown()));
        }
    }
    missing.extend(
        ids.roles
            .iter()
            .filter(|(u, _)| !placed(u))
            .map(|(_, n)| format!("role {}", n.shown())),
    );
    missing.extend(
        ids.types
            .iter()
            .filter(|(u, _)| !placed(u))
            .map(|(_, n)| format!("type {}", n.shown())),
    );
    missing.extend(
        ids.extensions
            .iter()
            .filter(|(u, _)| !placed(u))
            .map(|(_, n)| format!("extension {}", n.shown())),
    );
    if !missing.is_empty() {
        bail!(
            "these identities have no declaration and no tombstone, so there is no file to put them in:\n  {}\n\
             Restore the declaration, or record the drop with `pbps drop`, then migrate",
            missing.join("\n  ")
        );
    }
    // A dropped object keeps its file; with no YAML to say where, it sits at
    // the root under its last name and uid.
    for (uid, tomb) in &ids.tombstones {
        if matches!(
            uid.kind(),
            UidKind::Table | UidKind::Role | UidKind::Type | UidKind::Extension
        ) {
            placement.paths.insert(uid.clone(), aside(&tomb.was, uid));
        }
    }
    ids_tree::split(ids, &placement).context("cannot place the identities in files")
}

/// `pbps.yml` with `ids_file` removed and `ids_dir` named, or `None` when it
/// already says so. Edited as text so comments and order stay, then proved by
/// reading it back: a spelling the line edit does not understand is refused,
/// never half-applied.
fn new_config_text(project: &Project) -> anyhow::Result<Option<String>> {
    let path = project.config_file();
    let old = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read `{}`", path.display()))?;
    let names = |key: &str| old.lines().any(|l| l.starts_with(&format!("{key}:")));
    if !names("ids_file") && names("ids_dir") {
        return Ok(None);
    }
    let mut text: String = old
        .lines()
        .filter(|l| !l.starts_with("ids_file:"))
        .map(|l| format!("{l}\n"))
        .collect();
    if !names("ids_dir") {
        text.push_str(&format!(
            "ids_dir: {}\n",
            spelled(&project.config.ids_dir())
        ));
    }
    let expect = Config {
        ids_file: PathBuf::from("schema.ids.json"),
        ids_dir: Some(project.config.ids_dir()),
        ..project.config.clone()
    };
    match Config::parse(&text, &path) {
        Ok(read) if read == expect => Ok(Some(text)),
        _ => bail!(
            "cannot edit `{}` safely; replace the `ids_file` line with `ids_dir: {}` by hand and run migrate again",
            path.display(),
            spelled(&project.config.ids_dir())
        ),
    }
}

fn finish_config(project: &Project) -> anyhow::Result<bool> {
    match new_config_text(project)? {
        Some(text) => write_atomic(&project.config_file(), &text).map(|()| true),
        None => Ok(false),
    }
}

// ------------------------------------------------------------------- fmt ----

fn fmt(project: &Project, check: bool) -> anyhow::Result<()> {
    let dirs = Dirs::of(project);
    let tree = read_tree(project)?;
    let (mut changes, mut broken) = (Vec::new(), Vec::new());
    for (rel, text) in &tree {
        match ids_tree::parse(rel, text) {
            Ok(file) => {
                let canonical = ids_tree::render(&file);
                if &canonical != text {
                    changes.push((rel, canonical));
                }
            }
            Err(e) => broken.push(format!("{}: {e}", shown(&dirs, rel))),
        }
    }
    if !broken.is_empty() {
        for line in &broken {
            eprintln!("{line}");
        }
        bail!(
            "{} identity file(s) cannot be read, so none was rewritten; restore them from git or run `pbps ids resolve`",
            broken.len()
        );
    }
    if check {
        for (rel, _) in &changes {
            println!("{}", shown(&dirs, rel));
        }
        if !changes.is_empty() {
            bail!(
                "{} identity file(s) are not canonical; run `pbps ids fmt`",
                changes.len()
            );
        }
        return Ok(());
    }
    for (rel, canonical) in &changes {
        write_atomic(&project.ids_dir().join(rel), canonical)?;
        println!("rewrote {}", shown(&dirs, rel));
    }
    Ok(())
}

// --------------------------------------------------------------- resolve ----

fn has_markers(text: &str) -> bool {
    text.lines()
        .any(|l| l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> ") || l == "=======")
}

/// The old name a `renamed_from` gives a declaration, if it carries one.
fn renamed_from(loaded: &Loaded, kind: UidKind, name: &str) -> Option<String> {
    loaded.intents.iter().find_map(|intent| {
        let (from, to) = if let Intent::RenameTable { from, to } = intent
            && kind == UidKind::Table
        {
            (from.raw().to_string(), to.raw().to_string())
        } else if let Intent::RenameRole { from, to } = intent
            && kind == UidKind::Role
        {
            (from.raw().to_owned(), to.raw().to_owned())
        } else if let Intent::RenameType { from, to } = intent
            && kind == UidKind::Type
        {
            (from.raw().to_string(), to.raw().to_string())
        } else {
            return None;
        };
        (to == name).then_some(from)
    })
}

/// The name a declaration spells and what it declares, for the identity tree.
fn named(source: &Source) -> Option<(UidKind, String)> {
    match source {
        Source::Table(n) => Some((UidKind::Table, n.raw().to_string())),
        Source::Role(n) => Some((UidKind::Role, n.raw().to_owned())),
        Source::Type(n) => Some((UidKind::Type, n.raw().to_string())),
        Source::Extension(n) => Some((UidKind::Extension, n.raw().to_owned())),
        Source::Module(_) => None,
    }
}

/// The declarations as the validator wants them: project-relative path, kind,
/// name, and the `renamed_from` they carry.
fn declarations(dirs: &Dirs, loaded: &Loaded, paths: &DeclaredPaths) -> Vec<Declaration> {
    paths
        .files
        .iter()
        .filter_map(|(source, file)| {
            let (kind, name) = named(source)?;
            Some(Declaration {
                yaml: format!("{}/{}", dirs.schema, file.path),
                renamed_from: renamed_from(loaded, kind, &name),
                kind,
                name,
            })
        })
        .collect()
}

fn unmerged(project: &Project, dirs: &Dirs) -> anyhow::Result<BTreeMap<String, [bool; 3]>> {
    let out = crate::baseline::git(project.root(), &["ls-files", "-u", "-z", "--", &dirs.ids])
        .context("cannot list unmerged files (is this a git repository?)")?;
    let mut stages: BTreeMap<String, [bool; 3]> = BTreeMap::new();
    for entry in out.split('\0').filter(|e| !e.is_empty()) {
        let (meta, path) = entry
            .split_once('\t')
            .context("unexpected `git ls-files` output")?;
        let stage: usize = meta
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .context("unexpected stage")?;
        if let Some(rel) = path.strip_prefix(&format!("{}/", dirs.ids)) {
            stages.entry(rel.to_owned()).or_default()[stage - 1] = true;
        }
    }
    Ok(stages)
}

fn resolve(project: &Project) -> anyhow::Result<()> {
    let dirs = Dirs::of(project);
    // Section 5, step 1: the YAML first. The ids conflict only where the YAML
    // does, so the declarations are what decides a clash, and they must be
    // settled before anything is decided by them.
    let files = pbps_load::schema_files(&project.schema_dir())
        .with_context(|| format!("cannot list `{}`", project.schema_dir().display()))?;
    let marked: Vec<String> = files
        .iter()
        .filter(|f| std::fs::read_to_string(f).is_ok_and(|t| has_markers(&t)))
        .map(|f| f.display().to_string())
        .collect();
    if !marked.is_empty() {
        bail!(
            "these declaration files still have merge conflict markers; resolve the YAML first:\n  {}",
            marked.join("\n  ")
        );
    }
    let (loaded, paths) = load(project)?;
    let mut views: BTreeMap<String, YamlView> = BTreeMap::new();
    for (source, file) in &paths.files {
        let Some((_, name)) = named(source) else {
            continue;
        };
        views.insert(
            dirs.mirrored(&file.path)?,
            YamlView {
                name: Some(name),
                columns: file.columns.iter().map(|c| c.raw().to_owned()).collect(),
            },
        );
    }

    let mut merged = BTreeMap::new();
    let mut refusals: Vec<String> = Vec::new();
    for (rel, present) in unmerged(project, &dirs)? {
        let git_path = format!("{}/{rel}", dirs.ids);
        let stage = |n: usize| -> anyhow::Result<Option<ids_tree::TreeFile>> {
            if !present[n - 1] {
                return Ok(None);
            }
            let text =
                crate::baseline::git(project.root(), &["show", &format!(":{n}:{git_path}")])?;
            Ok(Some(ids_tree::parse(&rel, &text)?))
        };
        let (base, ours, theirs) = match (stage(1), stage(2), stage(3)) {
            (Ok(b), Ok(o), Ok(t)) => (b, o, t),
            (b, o, t) => {
                for e in [b.err(), o.err(), t.err()].into_iter().flatten() {
                    refusals.push(format!("{git_path}: {e}"));
                }
                continue;
            }
        };
        let view = views.get(&rel).cloned().unwrap_or_default();
        match ids_merge::merge(base.as_ref(), ours.as_ref(), theirs.as_ref(), &view) {
            Ok(file) => {
                merged.insert(rel, file);
            }
            Err(conflicts) => {
                for c in conflicts {
                    refusals.push(format!(
                        "{git_path}: {} {} differs: ours {}, theirs {}\n  fix: {}",
                        c.section, c.uid, c.ours, c.theirs, c.remedy
                    ));
                }
            }
        }
    }
    // All or nothing: a half-resolved tree is worse than a conflicted one.
    if !refusals.is_empty() {
        for line in &refusals {
            eprintln!("{line}");
        }
        bail!(
            "{} conflict(s) the YAML cannot decide; nothing was written",
            refusals.len()
        );
    }
    for (rel, file) in &merged {
        let path = project.ids_dir().join(rel);
        match file {
            Some(file) => write_atomic(&path, &ids_tree::render(file))?,
            None => std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove `{}`", path.display()))?,
        }
        println!(
            "{} {}",
            if file.is_some() { "merged" } else { "removed" },
            shown(&dirs, rel)
        );
    }

    // Section 5, step 3: the validator over what is on disk now. The sync that
    // creates, relocates and drops files is `plan`'s (PR 5); what it would do
    // shows here as findings, with `pbps plan` as the remedy.
    let tree = read_tree(project)?;
    let declared = declarations(&dirs, &loaded, &paths);
    let legacy = std::fs::symlink_metadata(project.ids_file()).is_ok();
    let findings = ids_check::check(&Input {
        layout: dirs.layout(),
        declared: &declared,
        ids: &tree,
        legacy_file: legacy,
    });
    for f in &findings {
        println!(
            "{} {}: {}\n  fix: {}",
            f.code.as_str(),
            f.path,
            f.message,
            f.remedy
        );
    }
    if !findings.is_empty() {
        bail!("{} finding(s) remain after the merge", findings.len());
    }
    if merged.is_empty() {
        println!("no unmerged identity files; the tree is consistent");
    } else {
        println!("resolved; now `git add {}` and commit", dirs.ids);
    }
    Ok(())
}
