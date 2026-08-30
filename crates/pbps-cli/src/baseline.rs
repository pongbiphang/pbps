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
            Ok(Baseline {
                schema: snap.schema,
                ids: snap.ids,
                description: format!("baseline file {}", path.display()),
                is_empty_fallback: false,
            })
        }
        Source::Git { rev } => load_from_git(project, rev),
        Source::Empty => Ok(Baseline {
            schema: Schema::default(),
            ids: IdsFile::default(),
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
    let toplevel = git(root, &["rev-parse", "--show-toplevel"]).map_err(|e| {
        anyhow::anyhow!(
            "this is not a git working tree; name a baseline file with --base instead: {e}"
        )
    })?;
    let toplevel = PathBuf::from(toplevel.trim());

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
            description: format!(
                "empty baseline (`{rev}` does not exist yet; this repo has no commits)"
            ),
            is_empty_fallback: true,
        });
    }

    // git paths are relative to the repo root, whereas the declarations directory
    // is relative to the project root.
    let rel = relative_to(&toplevel, &project.schema_dir());

    let listing = git(root, &["ls-tree", "-r", "--name-only", rev, "--", &rel])
        .map_err(|e| anyhow::anyhow!("cannot read `{rel}` at `{rev}`: {e}"))?;

    let mut schema = Schema::default();
    let mut count = 0usize;
    for path in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !(path.ends_with(".yml") || path.ends_with(".yaml")) {
            continue;
        }
        let text = git(root, &["show", &format!("{rev}:{path}")])?;
        match pbps_load::load_table_str(Path::new(path), &text) {
            Ok(t) => {
                schema.tables.insert(t.name, t.table);
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
    let ids_rel = relative_to(&toplevel, &project.ids_file());
    let ids = match git(root, &["show", &format!("{rev}:{ids_rel}")]) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("the identity file at `{rev}` is malformed: {e}"))?,
        // On a first run that revision has no identity file yet; empty is correct.
        Err(_) => IdsFile::default(),
    };

    Ok(Baseline {
        schema,
        ids,
        description: format!("git {rev} ({count} tables)"),
        is_empty_fallback: count == 0,
    })
}

/// Rewrites a path relative to the repo root, which is what git's path arguments
/// are resolved against.
fn relative_to(toplevel: &Path, path: &Path) -> String {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    abs.strip_prefix(toplevel)
        .unwrap_or(&abs)
        .to_string_lossy()
        .replace('\\', "/")
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
