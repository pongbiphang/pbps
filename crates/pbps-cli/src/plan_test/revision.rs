//! One revision's inputs, read once and checked the way `plan` checks them.
//!
//! A revision of the project is read from a git commit or from the working
//! tree into the same [`Raw`] — the bytes of `pbps.yml`, of every declaration
//! file and of the identity file — and everything after that is one function,
//! so a commit and a working tree holding the same bytes get the same answer
//! and the same hashes. The bytes that are hashed are the bytes that are
//! loaded.
//!
//! What is proved about a revision is that its declarations load, that the
//! dialect accepts them, and that its committed identity file already records
//! everything `plan` would record: nothing about a rename or drop is left for a
//! human, and no uid is missing. It is not a proof that the jump between two
//! revisions will plan; that needs the database the replay builds, and the
//! ordinary differ decides it there. A half-proof here would only
//! refuse plans that are valid.

use std::path::{Path, PathBuf};

use pbps_config::test_manifest::CommitId;
use pbps_config::{Config, DialectName};
use pbps_model::IdsFile;
use pbps_model::escape::Escaped;
use sha2::{Digest as _, Sha256};

use super::{ConsumedFile, Revision, Role};
use crate::baseline::{git_objects, paths_from, relative_to, under};

/// The bytes of one revision's inputs, with the project-relative path each was
/// read from.
pub(super) struct Raw {
    pub revision: Revision,
    pub config: (String, Vec<u8>),
    pub declarations: Vec<(String, Vec<u8>)>,
    /// `None` is a revision with no identity file: absent, which is not empty.
    pub ids: Option<(String, Vec<u8>)>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Problem {
    #[error("it has no `pbps.yml`, so its dialect and paths are unknown")]
    ConfigAbsent,
    #[error("`{}` is not a regular file ({what})", Escaped(path))]
    NotRegular { path: String, what: String },
    #[error("`{}` is not valid UTF-8", Escaped(path))]
    NotUtf8 { path: String },
    #[error("`{}` cannot be read: {}", Escaped(path), Escaped(reason))]
    Unreadable { path: String, reason: String },
    #[error("`pbps.yml` does not parse: {}", Escaped(.0))]
    Config(String),
    #[error("{}", Escaped(.0))]
    Paths(String),
    #[error("its `pbps.yml` configures {config}, but the manifest says {manifest}")]
    DialectMismatch {
        config: DialectName,
        manifest: DialectName,
    },
    #[error("it has no declarations directory `{}`", Escaped(.0))]
    DeclarationsAbsent(String),
    #[error("the declarations do not load: {}", Escaped(&.0.join("; ")))]
    Declarations(Vec<String>),
    #[error("{}", Escaped(&.0.join("; ")))]
    Collisions(Vec<String>),
    #[error("the dialect refuses the declarations: {}", Escaped(&.0.join("; ")))]
    Invalid(Vec<String>),
    #[error(
        "the identity file `{}` is malformed: {}",
        Escaped(path),
        Escaped(reason)
    )]
    IdsMalformed { path: String, reason: String },
    #[error(
        "the identity file `{}` is inconsistent: {}",
        Escaped(path),
        Escaped(reason)
    )]
    IdsInconsistent { path: String, reason: String },
    #[error(
        "identity intent is missing ({} decision(s)); record it with the ordinary commands \
         (`pbps plan` lists them) and commit the result",
        .0.len()
    )]
    Blockers(Vec<pbps_diff::Blocker>),
    #[error(
        "the identity file does not record every object the declarations name, or a decision \
         the declarations carry; run `pbps plan` and commit the identity file it writes"
    )]
    IdsOutOfDate,
    #[error("the policies in `pbps.yml` have problems: {}", Escaped(&.0.join("; ")))]
    PoliciesInvalid(Vec<String>),
    #[error("the project's policies refuse the declarations: {}", Escaped(&.0.join("; ")))]
    Policy(Vec<String>),
}

/// A revision that passed, with what a replay needs from it.
pub(crate) struct Assembled {
    pub revision: Revision,
    pub config: Config,
    pub loaded: pbps_load::Loaded,
    pub ids: IdsFile,
    pub files: Vec<ConsumedFile>,
}

/// `path` relative to `base`, both given from the same root, with `..` where
/// `path` is not below `base`.
fn relative(base: &str, path: &str) -> String {
    let base: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    let path: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let shared = base.iter().zip(&path).take_while(|(a, b)| a == b).count();
    std::iter::repeat_n("..", base.len() - shared)
        .chain(path[shared..].iter().copied())
        .collect::<Vec<_>>()
        .join("/")
}

fn utf8(path: &str, bytes: &[u8]) -> Result<String, Problem> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| Problem::NotUtf8 {
            path: path.to_owned(),
        })
}

fn consumed(role: Role, revision: &Revision, path: &str, bytes: &[u8]) -> ConsumedFile {
    ConsumedFile {
        path: path.to_owned(),
        revision: revision.clone(),
        role,
        sha256: format!("{:x}", Sha256::digest(bytes)),
    }
}

impl Raw {
    pub(super) fn files(&self) -> Vec<ConsumedFile> {
        std::iter::once((Role::Config, &self.config))
            .chain(self.ids.iter().map(|ids| (Role::Ids, ids)))
            .chain(self.declarations.iter().map(|d| (Role::Declaration, d)))
            .map(|(role, (path, bytes))| consumed(role, &self.revision, path, bytes))
            .collect()
    }
}

/// What git has at `commit:path` and below: nothing, the entries that are
/// regular files, or a reason it cannot say.
fn tree_entries(
    root: &Path,
    commit: &CommitId,
    path: &str,
    wanted: impl Fn(&str, &str) -> bool,
    project: &str,
) -> Result<Vec<(String, Vec<u8>)>, Problem> {
    let mut found = Vec::new();
    let mut refusal = None;
    let shown = |repo_path: &str| relative(project, repo_path);
    git_objects::read_with(root, commit.as_str(), path, wanted, |entry, mode, bytes| {
        if mode == "100644" || mode == "100755" {
            found.push((shown(entry), bytes.to_vec()));
        } else {
            refusal.get_or_insert(Problem::NotRegular {
                path: shown(entry),
                what: if mode == git_objects::SUBMODULE {
                    "it is a submodule, whose files this commit does not hold".to_owned()
                } else {
                    format!("git mode {mode}")
                },
            });
        }
        Ok(())
    })
    .map_err(|e| Problem::Unreadable {
        path: shown(path),
        reason: format!("{e:#}"),
    })?;
    refusal.map_or(Ok(found), Err)
}

/// The one file git has at `commit:path`: `None` when there is no entry, and a
/// refusal for a directory standing where the file should be.
fn tree_file(
    root: &Path,
    commit: &CommitId,
    path: &str,
    project: &str,
) -> Result<Option<(String, Vec<u8>)>, Problem> {
    let below = format!("{path}/");
    let mut found = tree_entries(
        root,
        commit,
        path,
        |entry, _mode| entry == path || entry.starts_with(&below),
        project,
    )?;
    let named = relative(project, path);
    match found.pop() {
        None => Ok(None),
        Some(file) if found.is_empty() && file.0 == named => Ok(Some(file)),
        Some(_) => Err(Problem::NotRegular {
            path: named,
            what: "it is a directory".to_owned(),
        }),
    }
}

/// Reads the inputs of `commit` from git, without checking it out.
///
/// An entry git has no record of is absent; one it cannot read is a refusal,
/// never an empty file (#1814 is the same mistake in `baseline::ids_from`,
/// which this does not use).
pub(super) fn read_commit(root: &Path, commit: &CommitId) -> Result<Raw, Vec<Problem>> {
    let revision = Revision::Commit(commit.clone());
    let prefix = relative_to(root).map_err(|e| {
        vec![Problem::Unreadable {
            path: root.display().to_string(),
            reason: format!("{e:#}"),
        }]
    })?;
    let config_rel = under(&prefix, Path::new(pbps_config::CONFIG_FILE))
        .map_err(|e| vec![Problem::Paths(format!("{e:#}"))])?;
    let config = tree_file(root, commit, &config_rel, &prefix)
        .map_err(|p| vec![p])?
        .ok_or_else(|| vec![Problem::ConfigAbsent])?;
    let parsed = Config::parse(
        &utf8(&config.0, &config.1).map_err(|p| vec![p])?,
        Path::new(&config.0),
    )
    .map_err(|e| vec![Problem::Config(e.to_string())])?;
    let (schema_rel, ids_rel) = paths_from(&parsed, &config_rel, &prefix, commit.as_str())
        .map_err(|e| vec![Problem::Paths(format!("{e:#}"))])?;

    let mut problems = Vec::new();
    // A directory that is not in the tree is absent; one with no declaration
    // in it is empty, and loads as an empty schema.
    let tree = git_objects_tree_exists(root, commit, &schema_rel);
    let declarations = match tree {
        Ok(true) => tree_entries(
            root,
            commit,
            &schema_rel,
            // A submodule anywhere below the directory is wanted whatever it
            // is called: `plan` reads what is checked out under it, and a
            // commit that only records its id cannot, so it must not read as
            // a directory with fewer declarations.
            |entry, mode| {
                entry.ends_with(".yml")
                    || entry.ends_with(".yaml")
                    || mode == git_objects::SUBMODULE
            },
            &prefix,
        )
        .map_err(|p| problems.push(p))
        .unwrap_or_default(),
        Ok(false) => {
            problems.push(Problem::DeclarationsAbsent(relative(&prefix, &schema_rel)));
            Vec::new()
        }
        Err(p) => {
            problems.push(p);
            Vec::new()
        }
    };
    let ids = tree_file(root, commit, &ids_rel, &prefix)
        .map_err(|p| problems.push(p))
        .unwrap_or_default();
    if problems.is_empty() {
        Ok(Raw {
            revision,
            config,
            declarations,
            ids,
        })
    } else {
        Err(problems)
    }
}

/// Whether `commit` has a directory at `path`. A failure to ask is not "no",
/// and a file standing where the directory should be is neither.
pub(super) fn git_objects_tree_exists(
    root: &Path,
    commit: &CommitId,
    path: &str,
) -> Result<bool, Problem> {
    let unreadable = |reason: String| Problem::Unreadable {
        path: path.to_owned(),
        reason,
    };
    let git = |args: &[&str]| {
        crate::baseline::git_command(root)
            .env("LC_ALL", "C")
            .args(args)
            .output()
            .map_err(|e| unreadable(e.to_string()))
    };
    let named = format!("{commit}:{path}");
    let found = git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        &named,
    ])?;
    // Not found exits 1 and says nothing. An exit 1 that complains of the
    // object store (an alternate that is gone) is git failing to look.
    match found.status.code() {
        Some(0) => {}
        Some(1) if found.stderr.trim_ascii().is_empty() => return Ok(false),
        Some(_) | None => {
            return Err(unreadable(
                String::from_utf8_lossy(&found.stderr).trim().to_owned(),
            ));
        }
    }
    let oid = String::from_utf8_lossy(&found.stdout).trim().to_owned();
    let kind = git(&["cat-file", "-t", &oid])?;
    if !kind.status.success() {
        return Err(unreadable(
            String::from_utf8_lossy(&kind.stderr).trim().to_owned(),
        ));
    }
    if kind.stdout.trim_ascii() == b"tree" {
        Ok(true)
    } else {
        Err(Problem::NotRegular {
            path: path.to_owned(),
            what: "it is not a directory".to_owned(),
        })
    }
}

/// Reads the inputs of the working tree at `root` (canonical): the files
/// `plan` reads, in the bytes they have now.
pub(super) fn read_working(root: &Path) -> Result<Raw, Vec<Problem>> {
    let shown = |path: &Path| {
        let normal = |p: &Path| {
            let mut parts: Vec<String> = Vec::new();
            for component in p.components() {
                match component {
                    std::path::Component::ParentDir => {
                        parts.pop();
                    }
                    std::path::Component::Normal(part) => {
                        parts.push(part.to_string_lossy().into_owned());
                    }
                    std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
                    | std::path::Component::CurDir => {}
                }
            }
            parts.join("/")
        };
        relative(&normal(root), &normal(path))
    };
    let regular = |path: &Path| -> Result<Option<Vec<u8>>, Problem> {
        let named = shown(path);
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(Problem::Unreadable {
                    path: named,
                    reason: e.to_string(),
                });
            }
        }
        // Through a symlink, as the ordinary loader reads; a dangling one is
        // damaged history, not an absent file.
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => {
                std::fs::read(path)
                    .map(Some)
                    .map_err(|e| Problem::Unreadable {
                        path: named,
                        reason: e.to_string(),
                    })
            }
            Ok(_) => Err(Problem::NotRegular {
                path: named,
                what: "it is not a file".to_owned(),
            }),
            Err(e) => Err(Problem::Unreadable {
                path: named,
                reason: e.to_string(),
            }),
        }
    };
    let config_path = root.join(pbps_config::CONFIG_FILE);
    let config_bytes = regular(&config_path)
        .map_err(|p| vec![p])?
        .ok_or_else(|| vec![Problem::ConfigAbsent])?;
    let config_name = shown(&config_path);
    let parsed = Config::parse(
        &utf8(&config_name, &config_bytes).map_err(|p| vec![p])?,
        &config_path,
    )
    .map_err(|e| vec![Problem::Config(e.to_string())])?;

    let mut problems = Vec::new();
    let schema_dir = root.join(&parsed.schema_dir);
    // As `Project::load` refuses it, from the directory that exists.
    if let (Ok(dir), Ok(base)) = (schema_dir.canonicalize(), root.canonicalize())
        && base.starts_with(&dir)
    {
        problems.push(Problem::Paths(
            "`schema_dir` is the project root or a directory above it, so it would read \
             `pbps.yml` as a declaration"
                .to_owned(),
        ));
    }
    let declarations = match pbps_load::schema_files(&schema_dir) {
        Ok(files) => files
            .iter()
            .filter_map(|path| match regular(path) {
                Ok(Some(bytes)) => Some((shown(path), bytes)),
                Ok(None) => {
                    problems.push(Problem::Unreadable {
                        path: shown(path),
                        reason: "it disappeared while the declarations were read".to_owned(),
                    });
                    None
                }
                Err(p) => {
                    problems.push(p);
                    None
                }
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            problems.push(Problem::DeclarationsAbsent(shown(&schema_dir)));
            Vec::new()
        }
        Err(e) => {
            problems.push(Problem::Unreadable {
                path: shown(&schema_dir),
                reason: e.to_string(),
            });
            Vec::new()
        }
    };
    let ids_path = root.join(&parsed.ids_file);
    let ids = regular(&ids_path)
        .map_err(|p| problems.push(p))
        .unwrap_or_default()
        .map(|bytes| (shown(&ids_path), bytes));
    if problems.is_empty() {
        Ok(Raw {
            revision: Revision::Working,
            config: (config_name, config_bytes),
            declarations,
            ids,
        })
    } else {
        Err(problems)
    }
}

/// Checks `raw` against the manifest's `dialect` and loads it. With `policies`
/// the project's declaration rules also apply: only the candidate is held to
/// them, since an older pin may predate a rule and must still replay.
///
/// Every problem of a stage is returned, and a stage that fails stops the next,
/// which would only restate it.
pub(super) fn assemble(
    raw: Raw,
    dialect: DialectName,
    project: Option<&Path>,
) -> Result<Assembled, Vec<Problem>> {
    let config_text = utf8(&raw.config.0, &raw.config.1).map_err(|p| vec![p])?;
    let config = Config::parse(&config_text, Path::new(&raw.config.0))
        .map_err(|e| vec![Problem::Config(e.to_string())])?;
    if config.dialect != dialect {
        return Err(vec![Problem::DialectMismatch {
            config: config.dialect,
            manifest: dialect,
        }]);
    }

    let mut problems = Vec::new();
    let mut texts = Vec::new();
    for (path, bytes) in &raw.declarations {
        match utf8(path, bytes) {
            Ok(text) => texts.push((PathBuf::from(path), text)),
            Err(p) => problems.push(p),
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let mut loaded = pbps_load::load_schema_files(&texts).map_err(|errors| {
        vec![Problem::Declarations(
            errors.iter().map(ToString::to_string).collect(),
        )]
    })?;
    let selected = crate::dialect_for(config.dialect);
    let dialect_ref: &dyn pbps_dialect::Dialect = selected.as_ref();
    let collisions = crate::routine_ids_as_the_dialect_spells_them(
        &mut loaded.schema,
        &mut loaded.hints,
        dialect_ref,
    );
    if !collisions.is_empty() {
        return Err(vec![Problem::Collisions(collisions)]);
    }
    dialect_ref.implied_grants(&mut loaded.schema);
    let invalid: Vec<String> = crate::declaration_problems(&loaded, dialect_ref)
        .into_iter()
        .map(|(id, problem)| format!("{id}: {problem}"))
        .collect();
    if !invalid.is_empty() {
        return Err(vec![Problem::Invalid(invalid)]);
    }

    let ids = match &raw.ids {
        None => IdsFile::default(),
        Some((path, bytes)) => {
            let text = utf8(path, bytes).map_err(|p| vec![p])?;
            let ids: IdsFile =
                crate::artifact::decode(&text, serde_json::from_str).map_err(|e| {
                    vec![Problem::IdsMalformed {
                        path: path.clone(),
                        reason: format!("{e:#}"),
                    }]
                })?;
            ids.validate().map_err(|e| {
                vec![Problem::IdsInconsistent {
                    path: path.clone(),
                    reason: e.to_string(),
                }]
            })?;
            ids
        }
    };
    // What `plan --check` asks, without the database: the identity file is
    // complete and no decision is waiting. Neither value is ever written; the
    // operator and date only stamp tombstones, which would make the file differ
    // and refuse it here anyway, so neither is read from this machine.
    let ctx = pbps_diff::Context {
        operator: "pbps-test".to_owned(),
        today: "1970-01-01".to_owned(),
    };
    let mut resolved = pbps_diff::resolve_with_annotations(
        &loaded.schema,
        &ids,
        &loaded.intents,
        loaded.intents.len(),
        &ctx,
    )
    .map_err(|blockers| vec![Problem::Blockers(blockers)])?;
    pbps_diff::conversion::prune(&mut resolved.ids, &loaded.schema, dialect_ref);
    if resolved.ids != ids {
        return Err(vec![Problem::IdsOutOfDate]);
    }

    if let Some(root) = project {
        let project = pbps_config::Project {
            root: root.to_owned(),
            config: config.clone(),
        };
        let policies = config.policies().check();
        if !policies.is_empty() {
            return Err(vec![Problem::PoliciesInvalid(
                policies.iter().map(ToString::to_string).collect(),
            )]);
        }
        let refused: Vec<String> = crate::declaration_policy_findings(
            &project,
            dialect_ref,
            &loaded.schema,
            None,
            crate::unix_seconds(),
        )
        .into_iter()
        .filter(|finding| finding.severity == crate::output::Severity::Error)
        .map(|finding| format!("{}: {}", finding.id, finding.message))
        .collect();
        if !refused.is_empty() {
            return Err(vec![Problem::Policy(refused)]);
        }
    }
    let files = raw.files();
    Ok(Assembled {
        revision: raw.revision,
        config,
        loaded,
        ids,
        files,
    })
}
