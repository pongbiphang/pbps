//! What `pbps test` can know before it provisions anything (ADR-0019 §2).
//!
//! The manifest and every file it names are read, checked and hashed once,
//! here, and the bytes that were hashed are the bytes later phases use: a
//! fixture is never read a second time by the runner, so the hash in the
//! report is the hash of what ran. [`Frozen::reverify`] is the recheck the
//! ADR requires before a result is published.
//!
//! The commits it names are checked here too: each resolves to one id, the
//! fixture pins form one ancestry chain that ends at the base, and the base is
//! behind the candidate ([`validate_history`]).
//!
//! Nothing in this module starts a container, opens a connection, runs SQL or
//! writes a file. Every refusal is a [`Refusal`] value, so a caller renders
//! the same facts as text or JSON and cannot reach a database past one.

mod chain;
mod files;
mod git;

use std::path::{Path, PathBuf};

pub(crate) use chain::{ChainProblem, ReplayChain};
pub(crate) use files::{Frozen, freeze};
pub(crate) use git::GitProblem;

use pbps_config::test_manifest::{Candidate, CommitId, Entries, ManifestError};
use pbps_model::escape::Escaped;

/// What a consumed file is for. Declaration inputs join these when revisions
/// are loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Manifest,
    Fixture,
    Assertion,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Role::Manifest => "manifest",
            Role::Fixture => "fixture",
            Role::Assertion => "assertion",
        })
    }
}

/// One file the run consumed: its path relative to the project root (`/`
/// separated), and the SHA-256 of its raw bytes, with no newline or SQL
/// normalization.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub(crate) struct ConsumedFile {
    pub path: String,
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
            | Refusal::Chain(_) => Outcome::Finding,
        }
    }
}

/// How the candidate is named in a report. A working tree stays uncommitted
/// even when its bytes match `HEAD`: it does not claim that git supplied them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CandidateKind {
    WorkingTree,
    Commit,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Pin {
    pub fixture: String,
    pub commit: CommitId,
}

/// The commits of a run, checked and ordered, with the files the manifest
/// names read once. Revisions' contents are not read here.
pub(crate) struct History {
    pub base: CommitId,
    pub pins: Vec<Pin>,
    pub candidate: CandidateIdentity,
    pub chain: ReplayChain,
    pub frozen: Frozen,
    root: PathBuf,
    /// The `HEAD` captured for a working-tree candidate.
    head: Option<CommitId>,
}

fn pin_label(manifest: &pbps_config::test_manifest::TestManifest, commit: &CommitId) -> String {
    if *commit == manifest.base {
        return "the base".to_owned();
    }
    let names: Vec<String> = manifest
        .fixtures
        .iter()
        .filter(|f| f.at == *commit)
        .map(|f| format!("fixture `{}`", Escaped(&f.name)))
        .collect();
    format!("the pin of {}", names.join(", "))
}

/// Reads the manifest and its files, and refuses everything about the commits
/// that can be known without reading them: each pin and the base must be a
/// commit this repository holds, an explicit candidate must name exactly one,
/// the pins must replay in one order that ends at the base, and the base must
/// lie behind the candidate (or the `HEAD` a working tree sits on). Every
/// refusal found is returned, not the first.
pub(crate) fn validate_history(
    project_root: &Path,
    manifest_path: &Path,
) -> Result<History, Vec<Refusal>> {
    let frozen = freeze(project_root, manifest_path)?;
    let root = project_root
        .canonicalize()
        .expect("the project root was canonicalized when the manifest was read");
    let manifest = &frozen.manifest;
    let mut refusals = Vec::new();

    let pins: Vec<CommitId> = manifest.fixtures.iter().map(|f| f.at.clone()).collect();
    let distinct: std::collections::BTreeSet<&CommitId> =
        pins.iter().chain([&manifest.base]).collect();
    for commit in distinct {
        if let Err(problem) = git::present(&root, commit) {
            refusals.push(Refusal::Git {
                what: pin_label(manifest, commit),
                problem,
            });
        }
    }

    // `HEAD` is read once: for a working tree it is the commit the candidate
    // sits on, which a pin may not be newer than.
    let (spec, what) = match &manifest.candidate {
        Candidate::WorkingTree => (
            "HEAD",
            "HEAD (the commit a working-tree candidate sits on)".to_owned(),
        ),
        Candidate::Revision(spec) => (spec.as_str(), format!("the candidate `{}`", Escaped(spec))),
    };
    let tip = match git::resolve_commit(&root, spec) {
        Ok(tip) => Some(tip),
        Err(problem) => {
            refusals.push(Refusal::Git { what, problem });
            None
        }
    };

    let chain = match &tip {
        Some(tip) if refusals.is_empty() => order_or_refuse(&root, manifest, &pins, tip),
        Some(_) | None => Err(Vec::new()),
    };
    let chain = match chain {
        Ok(chain) => Some(chain),
        Err(problems) => {
            refusals.extend(problems);
            None
        }
    };
    let (Some(chain), Some(tip), true) = (chain, tip, refusals.is_empty()) else {
        return Err(refusals);
    };
    let (candidate, head) = match manifest.candidate {
        Candidate::WorkingTree => (CandidateIdentity::working_tree(tip.clone()), Some(tip)),
        Candidate::Revision(_) => (CandidateIdentity::commit(tip), None),
    };
    Ok(History {
        base: manifest.base.clone(),
        pins: manifest
            .fixtures
            .iter()
            .map(|f| Pin {
                fixture: f.name.clone(),
                commit: f.at.clone(),
            })
            .collect(),
        candidate,
        chain,
        frozen,
        root,
        head,
    })
}

fn order_or_refuse(
    root: &Path,
    manifest: &pbps_config::test_manifest::TestManifest,
    pins: &[CommitId],
    tip: &CommitId,
) -> Result<ReplayChain, Vec<Refusal>> {
    chain::order(
        &manifest.base,
        pins,
        tip,
        |a, b| git::is_ancestor(root, a, b),
        |a, b| git::distance(root, a, b),
    )
    .map_err(|problems| problems.into_iter().map(Refusal::Chain).collect())
}

impl History {
    /// Rechecks what the run depends on and refuses if any of it moved: the
    /// manifest's files, and for a working-tree candidate the `HEAD` it sat on.
    /// Run it before a result is published.
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
mod history_tests;
#[cfg(test)]
mod tests;
