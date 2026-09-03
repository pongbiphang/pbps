//! Where "the current state" comes from.
//!
//! Attribute changes (type, nullability, …) can only be computed against a
//! baseline, and the identity file deliberately stores only uid-to-name, from
//! which attributes cannot be derived. There are three sources, in this order of
//! precedence:
//!
//! 1. `--base <file>`: an explicitly named state snapshot. It is **never read or
//!    written automatically** — it is an escape hatch for environments without
//!    git (export-style checkouts, air-gapped archives), not a second artifact to
//!    maintain.
//! 2. A revision in git (`HEAD` by default). What the baseline is stays obvious,
//!    and `--since v1.2.0` reads as "since that release".
//! 3. An empty baseline. Everything is listed as newly created, which is correct
//!    on a first run but dangerous if mistaken for a real plan, so it is always
//!    said out loud.
//!
//! Whichever it is, anything computed offline is a **preview**. A plan that is
//! actually going to be applied to an environment must be based on that
//! environment's database as queried (Phase 3).

use anyhow::Context as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use pbps_config::Project;
use pbps_model::{IdsFile, Schema, StateSnapshot};

#[derive(Debug, Clone)]
pub enum Source {
    File(PathBuf),
    Git { rev: String },
    Empty,
}

/// A baseline state, together with a human-readable note about where it came
/// from.
///
/// Identity and state have to travel together: without the ids, uid matching is
/// impossible and a rename degrades into a drop plus an add.
pub struct Baseline {
    pub schema: Schema,
    pub ids: IdsFile,
    /// The annotations that travelled beside that revision's declarations.
    ///
    /// Only `depends_on:` survives usefully — it is an ordering edge the
    /// identifier scan cannot find, so rebuilding the baseline without it can
    /// fail on declarations that were always valid. A baseline read from a state
    /// file has none: a snapshot records the database, and hints are not in it.
    pub hints: pbps_model::Hints,
    pub description: String,
    /// An empty baseline has to be flagged, or "everything is new" reads as a
    /// real plan.
    pub is_empty_fallback: bool,
}

pub fn load(project: &Project, source: &Source) -> anyhow::Result<Baseline> {
    match source {
        Source::File(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| {
                anyhow::anyhow!("cannot read baseline file `{}`: {e}", path.display())
            })?;
            let snap: StateSnapshot = serde_json::from_str(&text).map_err(|e| {
                anyhow::anyhow!("baseline file `{}` is malformed: {e}", path.display())
            })?;
            snap.check_version()
                .map_err(|e| anyhow::anyhow!("baseline file `{}`: {e}", path.display()))?;
            Ok(Baseline {
                schema: snap.schema,
                ids: snap.ids,
                hints: pbps_model::Hints::default(),
                description: format!("baseline file {}", path.display()),
                is_empty_fallback: false,
            })
        }
        Source::Git { rev } => load_from_git(project, rev),
        Source::Empty => Ok(Baseline {
            schema: Schema::default(),
            ids: IdsFile::default(),
            hints: pbps_model::Hints::default(),
            description: "empty baseline".into(),
            is_empty_fallback: true,
        }),
    }
}

/// Picks the default source: `HEAD` inside a git repo, an empty baseline
/// otherwise.
pub fn default_source(project: &Project) -> Source {
    if in_git_repo(&project.root) {
        Source::Git {
            rev: "HEAD".to_owned(),
        }
    } else {
        Source::Empty
    }
}

fn in_git_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--show-toplevel"]).is_ok()
}

fn load_from_git(project: &Project, rev: &str) -> anyhow::Result<Baseline> {
    let root = &project.root;
    git(root, &["rev-parse", "--show-toplevel"]).map_err(|e| {
        anyhow::anyhow!(
            "this is not a git working tree; name a baseline file with --base instead: {e}"
        )
    })?;

    // A brand-new repo has no commits, so HEAD does not resolve. That is not an
    // error, it just means there is no previous version yet: fall back to an empty
    // baseline and say so.
    if git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
    )
    .is_err()
    {
        return Ok(Baseline {
            schema: Schema::default(),
            ids: IdsFile::default(),
            hints: pbps_model::Hints::default(),
            description: format!(
                "empty baseline (`{rev}` does not exist yet; this repo has no commits)"
            ),
            is_empty_fallback: true,
        });
    }

    // git paths are relative to the repo root, whereas the declarations directory
    // is relative to the project root.
    let rel = relative_to(&project.schema_dir())?;

    // `--full-tree` because git resolves an `ls-tree` pathspec against the
    // current directory, while `<rev>:<path>` below resolves against the repo
    // root. Without it a project in a subdirectory lists nothing, and — since
    // the identity file is still found — the plan comes back as "no changes"
    // against a baseline that holds no tables at all.
    let mut args = vec!["ls-tree", "-r", "--full-tree", "--name-only", rev];
    // `schema_dir: .` puts the declarations at the repo root, where the relative
    // path is empty — and an empty pathspec is an error, not "everything". The
    // whole tree is what "everything" looks like as arguments.
    if !rel.is_empty() {
        args.push("--");
        args.push(&rel);
    }
    let listing = git(root, &args).map_err(|e| {
        anyhow::anyhow!(
            "cannot read `{}` at `{rev}`: {e}",
            if rel.is_empty() { "." } else { &rel }
        )
    })?;

    let mut schema = Schema::default();
    let mut hints = pbps_model::Hints::default();
    let mut count = 0usize;
    for path in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !(path.ends_with(".yml") || path.ends_with(".yaml")) {
            continue;
        }
        let text = git(root, &["show", &format!("{rev}:{path}")])?;
        match pbps_load::load_file_str(Path::new(path), &text) {
            Ok(pbps_load::LoadedFile::Table(t)) => {
                schema.tables.insert(t.name, t.table);
                count += 1;
            }
            // A module has no identity to reconstruct, so the baseline needs
            // nothing from it but the state itself (ADR-0002).
            Ok(pbps_load::LoadedFile::Module(m)) => {
                if !m.depends_on.is_empty() {
                    hints.module_deps.insert(m.name.clone(), m.depends_on);
                }
                schema.modules.insert(m.name, m.module);
                count += 1;
            }
            // A role's identity is in the ids file at that revision, which the
            // caller reads separately; the baseline needs only the state.
            Ok(pbps_load::LoadedFile::Role(r)) => {
                schema.roles.insert(r.name, r.role);
                count += 1;
            }
            Err(errs) => {
                // The baseline is a historical version. Its being broken should not
                // halt current work, but it does have to be reported.
                anyhow::bail!(
                    "`{path}` at `{rev}` does not parse (the baseline version itself is broken): {}",
                    errs.first().map(ToString::to_string).unwrap_or_default()
                );
            }
        }
    }

    // The identity file must come from the same revision: using the current one as
    // the baseline would hide renames.
    let ids = ids_at(project, rev)?;

    Ok(Baseline {
        schema,
        ids,
        hints,
        description: format!("git {rev} ({count} objects)"),
        is_empty_fallback: count == 0,
    })
}

/// The identity file as it was at `rev`.
///
/// Empty when that revision has none — a first run — which is the right
/// answer rather than a failure: everything is new against it.
pub fn ids_at(project: &Project, rev: &str) -> anyhow::Result<IdsFile> {
    let root = &project.root;
    let ids_rel = relative_to(&project.ids_file())?;
    match git(root, &["show", &format!("{rev}:{ids_rel}")]) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("the identity file at `{rev}` is malformed: {e}")),
        Err(_) => Ok(IdsFile::default()),
    }
}

/// The objects whose identity differs between `before` and `now`, as a policy
/// finding names them: a table under either of its names, a column's table,
/// a role as `role <name>` (ADR-0008 decision 7).
///
/// By uid, not by file: a renamed table counts as changed under both names,
/// and a file moved between directories does not count at all.
pub fn changed_subjects(
    before: &IdsFile,
    now: &IdsFile,
    before_schema: &pbps_model::Schema,
    now_schema: &pbps_model::Schema,
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for (uid, name) in before.tables.iter().chain(&now.tables) {
        if before.tables.get(uid) != now.tables.get(uid) {
            out.insert(name.to_string());
        }
    }
    for (uid, column) in before.columns.iter().chain(&now.columns) {
        if before.columns.get(uid) != now.columns.get(uid) {
            out.insert(column.table.to_string());
        }
    }
    for (uid, role) in before.roles.iter().chain(&now.roles) {
        if before.roles.get(uid) != now.roles.get(uid) {
            out.insert(format!("role {role}"));
        }
    }
    // The declarations themselves, matched through the uid so a rename does
    // not hide a change behind it: a table that kept its name and gained an
    // index, changed a type or grew its `data:` block is the object the
    // revision touched, and identity alone never saw it.
    for (name, table) in &now_schema.tables {
        let was = now
            .table_uid(name)
            .and_then(|uid| before.tables.get(uid))
            .and_then(|old_name| before_schema.tables.get(old_name));
        if was != Some(table) {
            out.insert(name.to_string());
        }
    }
    for (name, role) in &now_schema.roles {
        let was = now
            .role_uid(name)
            .and_then(|uid| before.roles.get(uid))
            .and_then(|old_name| before_schema.roles.get(old_name));
        if was != Some(role) {
            out.insert(format!("role {name}"));
        }
    }
    out
}

/// The declarations as they were at `rev`: every file under the schema
/// directory at that revision, checked out into a scratch directory and read
/// by the ordinary loader, so `--since` compares what a reader of that commit
/// would have seen. A revision with no declarations is an empty schema, which
/// makes everything new — the right answer for a project's first policy run.
pub fn schema_at(project: &Project, rev: &str) -> anyhow::Result<pbps_model::Schema> {
    let root = &project.root;
    // A revision that does not exist yet is the empty schema, the same answer
    // `load_from_git` gives for the identities; anything else git refuses is
    // an error, not "no declarations" — an unreadable tree read as an empty
    // one would call every table new.
    if git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
    )
    .is_err()
    {
        return Ok(pbps_model::Schema::default());
    }
    let dir_rel = relative_to(&project.schema_dir())?;
    // `--full-tree` and the empty-path rule for the same reasons as in
    // `load_from_git`: git resolves the pathspec against the current
    // directory, so a project in a subdirectory listed nothing here and
    // `--since` called every table changed.
    let mut args = vec!["ls-tree", "-r", "--full-tree", "--name-only", rev];
    if !dir_rel.is_empty() {
        args.push("--");
        args.push(&dir_rel);
    }
    let listing = git(root, &args).map_err(|e| {
        anyhow::anyhow!(
            "cannot read `{}` at `{rev}`: {e}",
            if dir_rel.is_empty() { "." } else { &dir_rel }
        )
    })?;
    let scratch = std::env::temp_dir().join(format!(
        "pbps-since-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&scratch)
        .with_context(|| format!("cannot create `{}`", scratch.display()))?;
    let result = (|| {
        for path in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if !(path.ends_with(".yml") || path.ends_with(".yaml")) {
                continue;
            }
            let inside = path.strip_prefix(dir_rel.as_str()).unwrap_or(path);
            let inside = inside.trim_start_matches('/');
            let text = git(root, &["show", &format!("{rev}:{path}")])?;
            let target = scratch.join(inside);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, text)?;
        }
        pbps_load::load_schema_dir(&scratch)
            .map(|l| l.schema)
            .map_err(|errs| {
                anyhow::anyhow!(
                    "the declarations at `{rev}` do not load: {}",
                    errs.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            })
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// Rewrites a path relative to the repo root, which is what git's path arguments
/// are resolved against.
///
/// # Why git is asked instead of the path being computed
///
/// The obvious version canonicalizes the path and strips the toplevel prefix.
/// That works on Linux and silently produces nonsense on Windows: `canonicalize`
/// returns a `\\?\` verbatim path while `rev-parse --show-toplevel` returns a
/// plain one, and a temp directory arrives in 8.3 short form (`RUNNER~1`) on one
/// side and long form on the other. The prefix then never matches, an absolute
/// path reaches `git show`, and what comes back is the root tree rather than the
/// blob — which parses as neither YAML nor JSON, so the failure surfaces as
/// "the identity file is malformed" a long way from its cause.
///
/// `rev-parse --show-prefix` asks git for the same answer in git's own terms, so
/// only git's notion of the path has to be right.
fn relative_to(path: &Path) -> anyhow::Result<String> {
    // A directory can be asked about directly, and has to be: `schema_dir: .`
    // makes the declarations directory the project root, whose parent is
    // normally outside the worktree — asking git there would fail on a
    // perfectly valid configuration. Everything else is resolved through its
    // parent, because the path itself may not exist yet (a first run has no
    // identity file).
    if path.is_dir() {
        let prefix = git(path, &["rev-parse", "--show-prefix"])?;
        // At the repo root the prefix is empty, which is the right pathspec for
        // "everything"; otherwise it ends in a slash that git does not need.
        return Ok(prefix.trim().trim_end_matches('/').to_owned());
    }
    let Some(name) = path.file_name() else {
        anyhow::bail!("`{}` names no file", path.display());
    };
    // A project discovered as a bare `pbps.yml` has an empty root, so the parent
    // comes out empty rather than absent: that is the current directory.
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let prefix = git(dir, &["rev-parse", "--show-prefix"])?;
    // `--show-prefix` is empty at the repo root and otherwise ends in a slash.
    Ok(format!("{}{}", prefix.trim(), name.to_string_lossy()))
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("cannot run git: {e}"))?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, ColumnType, Schema, Table};
    use std::str::FromStr;

    fn table(cols: &[&str]) -> Table {
        let mut t = Table::default();
        for c in cols {
            t.columns.insert(
                (*c).to_owned(),
                Column::new(ColumnType::from_str("int").unwrap()),
            );
        }
        t
    }

    /// A rename is a change under both names; a content change with the
    /// same identity is a change too — the case identity alone never saw.
    #[test]
    fn a_table_changed_in_content_only_is_a_changed_subject() {
        let uid: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
        let mut before_ids = IdsFile::default();
        before_ids
            .tables
            .insert(uid.clone(), "dbo.t".parse().unwrap());
        let now_ids = before_ids.clone();
        let mut before = Schema::default();
        before
            .tables
            .insert("dbo.t".parse().unwrap(), table(&["id"]));
        let mut now = before.clone();
        assert!(
            changed_subjects(&before_ids, &now_ids, &before, &now).is_empty(),
            "untouched"
        );
        now.tables
            .insert("dbo.t".parse().unwrap(), table(&["id", "added"]));
        let changed = changed_subjects(&before_ids, &now_ids, &before, &now);
        assert_eq!(changed.into_iter().collect::<Vec<_>>(), ["dbo.t"]);

        // Renamed and unchanged inside: both names, through the uid.
        let mut renamed_ids = before_ids.clone();
        renamed_ids.tables.insert(uid, "dbo.t2".parse().unwrap());
        let mut renamed = Schema::default();
        renamed
            .tables
            .insert("dbo.t2".parse().unwrap(), table(&["id"]));
        let changed = changed_subjects(&before_ids, &renamed_ids, &before, &renamed);
        assert_eq!(changed.into_iter().collect::<Vec<_>>(), ["dbo.t", "dbo.t2"]);
    }
}
