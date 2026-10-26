//! The git questions the plan test asks, each answered or refused: a commit
//! that cannot be found, one that git is unsure of and a history that cannot
//! be read are three different facts, and none of them is "no".

use std::path::Path;

use pbps_config::test_manifest::CommitId;
use pbps_model::escape::Escaped;

use crate::baseline::ancestry::{self, Evidence, SHALLOW_REMEDY};
use crate::baseline::git_command;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum GitProblem {
    #[error("this project is not inside a git checkout")]
    NoCheckout,
    #[error("git cannot be run")]
    GitUnavailable,
    #[error("it is not a commit this repository has{}", remedy(*shallow))]
    Missing { shallow: bool },
    #[error("git warned about it, so it may not mean what was intended: {}", Escaped(.0))]
    Ambiguous(String),
    #[error(
        "this shallow clone cannot show whether one commit is an ancestor of the other{SHALLOW_REMEDY}"
    )]
    Incomplete,
    #[error("git could not answer: {}", Escaped(.0))]
    Unreadable(String),
}

fn remedy(shallow: bool) -> &'static str {
    if shallow { SHALLOW_REMEDY } else { "" }
}

impl GitProblem {
    /// `None` for the two answers that are not problems: ancestor and not.
    pub(super) fn from_evidence(evidence: Evidence) -> Option<Self> {
        match evidence {
            Evidence::Contains | Evidence::Diverged => None,
            Evidence::NoCheckout => Some(Self::NoCheckout),
            Evidence::GitUnavailable => Some(Self::GitUnavailable),
            Evidence::Missing { shallow } => Some(Self::Missing { shallow }),
            Evidence::Incomplete => Some(Self::Incomplete),
            Evidence::Unreadable(reason) => Some(Self::Unreadable(reason)),
        }
    }
}

/// The commit `spec` names, once. The caller records the id, never the spelling:
/// a branch moves between this call and the run.
///
/// Git prints a warning, and still exits 0, for a name that is both a branch
/// and a tag; `--quiet` hides it. Any output beside the answer is refused, so
/// a reworded warning is still caught.
pub(super) fn resolve_commit(root: &Path, spec: &str) -> Result<CommitId, GitProblem> {
    let (_, shallow) = ancestry::repository(root).map_err(|evidence| {
        GitProblem::from_evidence(evidence).expect("a repository evidence is a problem")
    })?;
    let rev = format!("{spec}^{{commit}}");
    let run = |quiet: bool| {
        let mut command = git_command(root);
        command.env("LC_ALL", "C").args([
            "-c",
            "core.warnAmbiguousRefs=true",
            "rev-parse",
            "--verify",
        ]);
        if quiet {
            command.arg("--quiet");
        }
        command.args(["--end-of-options", &rev]).output()
    };
    let unreadable = |e: std::io::Error| GitProblem::Unreadable(e.to_string());
    let out = run(false).map_err(unreadable)?;
    if out.status.success() {
        let noise = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        if !noise.is_empty() {
            // An ambiguity warning and a failing object store both exit 0 with
            // stderr text. Only the first says the name may mean something
            // else; the second says git may not have seen every object.
            return Err(if noise.to_ascii_lowercase().contains("ambiguous") {
                GitProblem::Ambiguous(noise)
            } else {
                GitProblem::Unreadable(noise)
            });
        }
        let text = String::from_utf8_lossy(&out.stdout);
        return CommitId::parse(text.trim())
            .ok_or_else(|| GitProblem::Unreadable(format!("git answered `{}`", text.trim())));
    }
    // Not found exits 1 and says nothing; anything else, or an exit 1 that
    // complains of the object store (an alternate that is gone), is git failing
    // to look and must not read as "no such commit".
    let probe = run(true).map_err(unreadable)?;
    let said = String::from_utf8_lossy(&probe.stderr).trim().to_owned();
    if probe.status.code() == Some(1) && said.is_empty() {
        return Err(GitProblem::Missing { shallow });
    }
    let said = if said.is_empty() {
        String::from_utf8_lossy(&out.stderr).trim().to_owned()
    } else {
        said
    };
    Err(GitProblem::Unreadable(said))
}

/// Whether `ancestor` is an ancestor of, or the same commit as, `descendant`.
pub(super) fn is_ancestor(
    root: &Path,
    ancestor: &CommitId,
    descendant: &CommitId,
) -> Result<bool, GitProblem> {
    let evidence = ancestry::check_between(root, ancestor.as_str(), descendant.as_str());
    match evidence {
        Evidence::Contains => Ok(true),
        Evidence::Diverged => Ok(false),
        Evidence::NoCheckout
        | Evidence::GitUnavailable
        | Evidence::Missing { .. }
        | Evidence::Incomplete
        | Evidence::Unreadable(_) => Err(GitProblem::from_evidence(evidence)
            .expect("every evidence but the two answers is a problem")),
    }
}

/// The commit `id` as a commit this repository holds.
pub(super) fn present(root: &Path, id: &CommitId) -> Result<(), GitProblem> {
    match GitProblem::from_evidence(ancestry::commit(root, id.as_str())) {
        None => Ok(()),
        Some(problem) => Err(problem),
    }
}

/// `git rev-list --count from..to`: merged history counts. A cost indicator,
/// not an instruction to apply every commit between.
pub(super) fn distance(root: &Path, from: &CommitId, to: &CommitId) -> Result<u64, GitProblem> {
    let range = format!("{from}..{to}");
    let out = git_command(root)
        .env("LC_ALL", "C")
        .args(["rev-list", "--count", &range])
        .output()
        .map_err(|e| GitProblem::Unreadable(e.to_string()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if out.status.success()
        && let Ok(count) = text.trim().parse()
    {
        return Ok(count);
    }
    Err(GitProblem::Unreadable(
        String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    ))
}
