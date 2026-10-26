//! What `pbps test` can know before it provisions anything (ADR-0019 §2).
//!
//! The manifest and every file it names are read, checked and hashed once,
//! here, and the bytes that were hashed are the bytes later phases use: a
//! fixture is never read a second time by the runner, so the hash in the
//! report is the hash of what ran. [`Frozen::reverify`] is the recheck the
//! ADR requires before a result is published.
//!
//! Nothing in this module starts a container, opens a connection, runs SQL or
//! writes a file. Every refusal is a [`Refusal`] value, so a caller renders
//! the same facts as text or JSON and cannot reach a database past one.

mod files;

pub(crate) use files::{Frozen, freeze};

use pbps_config::test_manifest::{Entries, ManifestError};
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
            // two manifest-side answers that cannot be established.
            Refusal::Manifest(ManifestError::UnsupportedVersion(_))
            | Refusal::Input {
                problem: Problem::Unreadable(_),
                ..
            }
            | Refusal::Changed { .. } => Outcome::Unanswerable,
            Refusal::Manifest(_) | Refusal::Input { .. } | Refusal::DuplicateFile { .. } => {
                Outcome::Finding
            }
        }
    }
}

#[cfg(test)]
mod tests;
