//! What `pbps test` can know before it provisions anything (ADR-0019 §2).
//!
//! The manifest and every file it names are read, checked and hashed once,
//! here, and the bytes that were hashed are the bytes later phases use: a
//! fixture is never read a second time by the runner, so the hash in the
//! report is the hash of what ran. [`Frozen::reverify`] is the recheck the
//! ADR requires before a result is published.
//!
//! Input validation starts no container, opens no connection and writes no
//! file. Every refusal is a [`Refusal`] value, so a caller renders
//! the same facts as text or JSON and cannot reach a database past one.

mod chain;
mod runner;
pub(crate) use runner::command;
pub(crate) mod execution;
mod files;
mod git;
pub(crate) mod report;
pub(crate) mod revision;
pub(crate) mod session;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) use chain::{ChainProblem, ReplayChain};
pub(crate) use files::Frozen;
#[cfg(test)]
use files::freeze;
pub(crate) use git::GitProblem;
pub(crate) use revision::{Assembled, Problem as RevisionProblem};

use pbps_config::test_manifest::{Candidate, CommitId, Entries, ManifestError, TestManifest};
use pbps_model::escape::Escaped;

/// What a consumed file is for. Hints live in the declaration files.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Manifest,
    Fixture,
    Assertion,
    Config,
    Declaration,
    Ids,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Role::Manifest => "manifest",
            Role::Fixture => "fixture",
            Role::Assertion => "assertion",
            Role::Config => "project configuration",
            Role::Declaration => "declaration",
            Role::Ids => "identity file",
        })
    }
}

/// Where a file's bytes came from: the working tree as it is, or one commit.
/// An uncommitted working tree is `Working` even when its bytes happen to
/// equal `HEAD`'s: it does not claim that git supplied them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, schemars::JsonSchema)]
#[schemars(with = "String")]
pub(crate) enum Revision {
    Working,
    Commit(CommitId),
}

impl serde::Serialize for Revision {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Revision::Working => serializer.serialize_str("working"),
            Revision::Commit(commit) => serializer.serialize_str(commit.as_str()),
        }
    }
}

impl std::fmt::Display for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Revision::Working => f.write_str("the working tree"),
            Revision::Commit(commit) => write!(f, "commit {commit}"),
        }
    }
}

/// One file the run consumed: its path relative to the project root (`/`
/// separated), and the SHA-256 of its raw bytes, with no newline or SQL
/// normalization.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct ConsumedFile {
    pub path: String,
    pub revision: Revision,
    pub role: Role,
    pub sha256: String,
}

/// Why one input cannot be used. Absent, not a file and unreadable are
/// different facts: a file that cannot be read is never treated as missing
/// or as empty.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Problem {
    #[error("does not exist")]
    Missing,
    #[error("is not a regular file")]
    NotRegular,
    #[error("resolves outside the project")]
    OutsideProject,
    #[error("is not valid UTF-8")]
    NotUtf8,
    #[error("cannot be read: {0}")]
    Unreadable(String),
}

/// What happened to an input between the freeze and the recheck.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Change {
    #[error("changed")]
    Edited,
    #[error("now resolves to another file")]
    Retargeted,
    #[error("was added")]
    Added,
    #[error("was removed")]
    Removed,
    #[error("can no longer be used: it {0}")]
    Unusable(Problem),
}

/// How a completed refusal is classified (ADR-0019 §9): the input is wrong
/// and the user can fix it, or the answer cannot be established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Finding,
    Unanswerable,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Refusal {
    #[error("the manifest: {0}")]
    Manifest(ManifestError),
    #[error("{}: `{}` {problem}", subject(*role, name.as_deref()), Escaped(path))]
    Input {
        role: Role,
        name: Option<String>,
        /// As the user spelled it.
        path: String,
        problem: Problem,
    },
    #[error(
        "{list} `{}` and `{}` are the same file, `{}`",
        Escaped(first),
        Escaped(second),
        Escaped(path)
    )]
    DuplicateFile {
        list: Entries,
        first: String,
        second: String,
        path: String,
    },
    #[error(
        "`{}` {change} after it was read; the result cannot be qualified",
        Escaped(path)
    )]
    Changed { path: String, change: Change },
    /// A commit (`what` says whose pin, or which candidate) git cannot give.
    #[error("{what}: {problem}")]
    Git { what: String, problem: GitProblem },
    #[error("{0}")]
    Chain(ChainProblem),
    #[error("{revision}: {problem}")]
    At {
        revision: Revision,
        problem: RevisionProblem,
    },
    #[error(
        "HEAD was {captured} when the inputs were read and is {now} now; the result cannot be \
         qualified"
    )]
    HeadMoved { captured: CommitId, now: CommitId },
}

fn subject(role: Role, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{role} `{}`", Escaped(name)),
        None => role.to_string(),
    }
}

impl Refusal {
    pub(crate) fn outcome(&self) -> Outcome {
        match self {
            // The ADR names an unknown version and an unreadable file as the
            // answers that cannot be established; so is a history or an object
            // git could not read, and an input that moved after it was read.
            Refusal::Manifest(ManifestError::UnsupportedVersion(_))
            | Refusal::Input {
                problem: Problem::Unreadable(_),
                ..
            }
            | Refusal::Changed { .. }
            | Refusal::HeadMoved { .. }
            | Refusal::At {
                problem:
                    RevisionProblem::Unreadable { .. } | RevisionProblem::IdsUnsupportedVersion { .. },
                ..
            }
            | Refusal::Git {
                problem:
                    GitProblem::NoCheckout
                    | GitProblem::GitUnavailable
                    | GitProblem::Incomplete
                    | GitProblem::Unreadable(_)
                    | GitProblem::Missing { shallow: true },
                ..
            }
            | Refusal::Chain(ChainProblem::Unknown { .. }) => Outcome::Unanswerable,
            Refusal::Manifest(_)
            | Refusal::Input { .. }
            | Refusal::DuplicateFile { .. }
            | Refusal::Git { .. }
            | Refusal::Chain(_)
            | Refusal::At { .. } => Outcome::Finding,
        }
    }
}

/// How the candidate is named in a report. A working tree stays uncommitted
/// even when its bytes match `HEAD`: it does not claim that git supplied them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CandidateKind {
    WorkingTree,
    Commit,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct CandidateIdentity {
    kind: CandidateKind,
    uncommitted: bool,
    /// The captured `HEAD` of a working tree, or the resolved commit.
    commit: CommitId,
}

impl CandidateIdentity {
    fn working_tree(head: CommitId) -> Self {
        Self {
            kind: CandidateKind::WorkingTree,
            uncommitted: true,
            commit: head,
        }
    }

    fn commit(commit: CommitId) -> Self {
        Self {
            kind: CandidateKind::Commit,
            uncommitted: false,
            commit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct Pin {
    pub fixture: String,
    pub commit: CommitId,
}

/// What a run was given, with no password, connection string or plan in it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct InputIdentity {
    pub tool_version: &'static str,
    pub manifest_version: u32,
    pub dialect: pbps_config::DialectName,
    pub manifest: ConsumedFile,
    pub base: CommitId,
    pub pins: Vec<Pin>,
    pub candidate: CandidateIdentity,
    /// The image as the manifest asks for it; the acquired identity is the
    /// runner's to record.
    pub image: String,
    pub timeouts: pbps_config::test_manifest::Timeouts,
    /// Every consumed file, sorted by path, revision, role and hash.
    pub files: Vec<ConsumedFile>,
}

/// Observations survive a later refusal; they never certify a partial input set.
#[derive(Default, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct ObservedInputs {
    pub files: Vec<ConsumedFile>,
    pub resolved_commits: BTreeMap<String, CommitId>,
}

/// Every input of a run, read once and checked before anything is provisioned.
pub(crate) struct Inputs {
    pub identity: InputIdentity,
    pub chain: ReplayChain,
    /// Each distinct pin and the base, as the loaders see them.
    pub revisions: BTreeMap<CommitId, Assembled>,
    pub candidate: Assembled,
    pub frozen: Frozen,
    root: PathBuf,
    /// The `HEAD` captured for a working-tree candidate.
    head: Option<CommitId>,
}

fn pin_label(manifest: &TestManifest, commit: &CommitId) -> String {
    if *commit == manifest.base {
        return "the base".to_owned();
    }
    let names: Vec<String> = manifest
        .fixtures
        .iter()
        .filter(|f| f.at == *commit)
        .map(|f| format!("`{}`", Escaped(&f.name)))
        .collect();
    format!("the pin of fixture {}", names.join(", "))
}

/// Reads the manifest and every input it leads to, and refuses everything that
/// can be known without a database: the manifest and its files, each pin and
/// the base as a commit with the declarations, identity and intent a replay
/// needs, the candidate as it is now or as a commit, and the order the pins
/// replay in. Every refusal found is returned, not the first.
#[cfg(test)]
pub(crate) fn validate_inputs(
    project_root: &Path,
    manifest_path: &Path,
) -> Result<Inputs, Vec<Refusal>> {
    validate_recorded(project_root, manifest_path, &mut ObservedInputs::default())
}

pub(crate) fn validate_recorded(
    project_root: &Path,
    manifest_path: &Path,
    observed: &mut ObservedInputs,
) -> Result<Inputs, Vec<Refusal>> {
    let frozen = files::freeze_recorded(project_root, manifest_path, &mut observed.files)?;
    let root = project_root
        .canonicalize()
        .expect("the project root was canonicalized when the manifest was read");
    let manifest = &frozen.manifest;
    let mut refusals = Vec::new();

    let pins: Vec<CommitId> = manifest.fixtures.iter().map(|f| f.at.clone()).collect();
    let distinct: std::collections::BTreeSet<&CommitId> =
        pins.iter().chain([&manifest.base]).collect();
    let mut present = Vec::new();
    for commit in distinct {
        match git::present(&root, commit) {
            Ok(()) => {
                observed
                    .resolved_commits
                    .insert(commit.to_string(), commit.clone());
                present.push(commit.clone());
            }
            Err(problem) => refusals.push(Refusal::Git {
                what: pin_label(manifest, commit),
                problem,
            }),
        }
    }

    // `HEAD` once: for a working tree it is the commit the candidate sits on,
    // and the commit a pin may not be newer than.
    let (tip, identity, head) = match &manifest.candidate {
        Candidate::WorkingTree => match git::resolve_commit(&root, "HEAD") {
            Ok(head) => (
                Some(head.clone()),
                Some(CandidateIdentity::working_tree(head.clone())),
                Some(head),
            ),
            Err(problem) => {
                refusals.push(Refusal::Git {
                    what: "HEAD (the commit a working-tree candidate sits on)".to_owned(),
                    problem,
                });
                (None, None, None)
            }
        },
        Candidate::Revision(spec) => match git::resolve_commit(&root, spec) {
            Ok(commit) => (
                Some(commit.clone()),
                Some(CandidateIdentity::commit(commit)),
                None,
            ),
            Err(problem) => {
                refusals.push(Refusal::Git {
                    what: format!("the candidate `{}`", Escaped(spec)),
                    problem,
                });
                (None, None, None)
            }
        },
    };

    if let Some(tip) = &tip {
        observed
            .resolved_commits
            .insert("candidate".into(), tip.clone());
    }
    let mut revisions = BTreeMap::new();
    for commit in &present {
        let loaded = revision::read_commit_recorded(&root, commit, &mut observed.files)
            .and_then(|raw| revision::assemble(raw, manifest.dialect, None));
        match loaded {
            Ok(found) => {
                revisions.insert(commit.clone(), found);
            }
            Err(problems) => refusals.extend(problems.into_iter().map(|problem| Refusal::At {
                revision: Revision::Commit(commit.clone()),
                problem,
            })),
        }
    }
    let candidate_revision = match (&manifest.candidate, &identity) {
        (Candidate::WorkingTree, _) => Some(Revision::Working),
        (Candidate::Revision(_), Some(identity)) => Some(Revision::Commit(identity.commit.clone())),
        (Candidate::Revision(_), None) => None,
    };
    let candidate = candidate_revision.and_then(|revision| {
        let raw = match &revision {
            Revision::Working => revision::read_working_recorded(&root, &mut observed.files),
            Revision::Commit(commit) => {
                revision::read_commit_recorded(&root, commit, &mut observed.files)
            }
        };
        match raw.and_then(|raw| revision::assemble(raw, manifest.dialect, Some(&root))) {
            Ok(found) => Some(found),
            Err(problems) => {
                refusals.extend(problems.into_iter().map(|problem| Refusal::At {
                    revision: revision.clone(),
                    problem,
                }));
                None
            }
        }
    });

    let chain = match &tip {
        Some(tip) if present.contains(&manifest.base) && refusals.is_empty() => chain::order(
            &manifest.base,
            &pins,
            tip,
            |a, b| git::is_ancestor(&root, a, b),
            |a, b| git::distance(&root, a, b),
        )
        .map_err(|problems| {
            refusals.extend(problems.into_iter().map(Refusal::Chain));
        })
        .ok(),
        Some(_) | None => None,
    };
    let (Some(chain), Some(candidate), Some(candidate_identity)) = (chain, candidate, identity)
    else {
        return Err(refusals);
    };
    if !refusals.is_empty() {
        return Err(refusals);
    }

    let mut files: Vec<ConsumedFile> = frozen
        .consumed()
        .into_iter()
        .chain(revisions.values().flat_map(|r| r.files.clone()))
        .chain(candidate.files.clone())
        .collect();
    files.sort();
    files.dedup();
    let identity = InputIdentity {
        tool_version: env!("CARGO_PKG_VERSION"),
        manifest_version: pbps_config::test_manifest::SUPPORTED_VERSION,
        dialect: manifest.dialect,
        manifest: frozen.manifest_file.clone(),
        base: manifest.base.clone(),
        pins: manifest
            .fixtures
            .iter()
            .map(|f| Pin {
                fixture: f.name.clone(),
                commit: f.at.clone(),
            })
            .collect(),
        candidate: candidate_identity,
        image: manifest.image.to_string(),
        timeouts: manifest.timeouts,
        files,
    };
    Ok(Inputs {
        identity,
        chain,
        revisions,
        candidate,
        frozen,
        root,
        head,
    })
}

impl Inputs {
    /// Rereads everything the run depends on and refuses if any of it moved:
    /// the manifest and its files, and for a working-tree candidate its
    /// membership (a declaration added or removed), its bytes and the `HEAD`
    /// it sat on. Run it before a result is published, so no old hash is ever
    /// reported beside new content.
    pub(crate) fn reverify(&self) -> Result<(), Vec<Refusal>> {
        let mut refusals = self.frozen.reverify().err().unwrap_or_default();
        if let Some(captured) = &self.head {
            match git::resolve_commit(&self.root, "HEAD") {
                Ok(now) if now == *captured => {}
                Ok(now) => refusals.push(Refusal::HeadMoved {
                    captured: captured.clone(),
                    now,
                }),
                Err(problem) => refusals.push(Refusal::Git {
                    what: "HEAD".to_owned(),
                    problem,
                }),
            }
            match revision::read_working(&self.root) {
                Err(problems) => refusals.extend(problems.into_iter().map(|problem| Refusal::At {
                    revision: Revision::Working,
                    problem,
                })),
                Ok(raw) => {
                    let key = |f: &ConsumedFile| (f.path.clone(), f.role);
                    let before: BTreeMap<_, _> =
                        self.candidate.files.iter().map(|f| (key(f), f)).collect();
                    let now = raw.files();
                    let after: BTreeMap<_, _> = now.iter().map(|f| (key(f), f)).collect();
                    for (k, file) in &after {
                        let change = match before.get(k) {
                            None => Change::Added,
                            Some(was) if was.sha256 != file.sha256 => Change::Edited,
                            Some(_) => continue,
                        };
                        refusals.push(Refusal::Changed {
                            path: file.path.clone(),
                            change,
                        });
                    }
                    for (k, file) in &before {
                        if !after.contains_key(k) {
                            refusals.push(Refusal::Changed {
                                path: file.path.clone(),
                                change: Change::Removed,
                            });
                        }
                    }
                }
            }
        }
        if refusals.is_empty() {
            Ok(())
        } else {
            Err(refusals)
        }
    }
}

#[cfg(test)]
mod chain_tests;
#[cfg(test)]
mod revision_tests;
#[cfg(test)]
mod tests;
