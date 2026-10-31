//! In-memory boundaries used by the ordinary deployment commands (DEC-1165.8).
//!
//! File loading/publication and public hooks stay at the command boundary.
//! The checked bodies consume loaded inputs; no refusal or write moves
//! across another check when its command boundary is extracted.
//! Connected work futures stay boxed so composing phases does not embed their
//! large execution frames in the caller (DEC-1165.9).

use crate::db::Target;
use crate::output::{Finding, Report};
use anyhow::Context as _;
use pbps_config::Project;
use pbps_diff::order::ArtifactView;
use pbps_model::IdsFile;

mod apply;
pub(super) use apply::{apply_async, apply_identified};

#[derive(Default)]
pub(crate) struct Progress {
    pub accepted: std::cell::Cell<bool>,
    pub started: std::cell::Cell<bool>,
    pub findings: std::cell::RefCell<Vec<Finding>>,
}

// Public commands keep their exact text; an owned run retains it as ordinary
// findings so its command boundary alone renders the existing envelope.
pub(super) fn notice(progress: Option<&Progress>, id: &str, message: String) {
    if let Some(progress) = progress {
        progress
            .findings
            .borrow_mut()
            .push(Finding::note(id, message));
    } else {
        println!("{message}");
    }
}

pub(super) struct Declarations {
    pub loaded: pbps_load::Loaded,
    pub ids: IdsFile,
}
impl Declarations {
    fn read(project: &Project, dialect: &dyn pbps_dialect::Dialect) -> anyhow::Result<Self> {
        let loaded = crate::load(project, dialect)?;
        let ids = crate::read_ids(project)?;
        Ok(Self { loaded, ids })
    }
}

/// A committed build and its separately reported lock cleanup.
pub(super) struct Bootstrapped {
    pub entry: i64,
    pub tables: usize,
    pub target: String,
    pub released: Result<bool, pbps_db::DbError>,
}

pub(crate) fn cmd_bootstrap(
    project: &Project,
    target: Option<&Target>,
    sql_out: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let operator = crate::operator(project.root())?;
    let dialect = crate::dialect(project)?;
    let declarations = Declarations::read(project, dialect.as_ref())?;
    if let Some(built) =
        super::bootstrap(project, target, sql_out, declarations, dialect, &operator)?
    {
        println!(
            "Bootstrapped `{}`: {} table(s) created, recorded as entry #{}.",
            pbps_model::escape::Escaped(&built.target),
            built.tables,
            built.entry
        );
        // Both the DDL and ledger row committed before lock cleanup began.
        built.released?;
    }
    Ok(())
}

/// A ready plan retains its seal; a refusal retains its ordinary findings.
pub(super) enum PlanResult {
    Planned(Box<Planned>),
    Refused(Report<()>),
}
pub(crate) struct Planned {
    pub artifact: Box<dyn ArtifactView>,
    pub script: Option<String>,
    pub statements: usize,
    pub data: crate::PlanData,
    pub findings: Vec<Finding>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_plan_db(
    project: &Project,
    target: &Target,
    out: Option<&std::path::Path>,
    sql_out: Option<&std::path::Path>,
    staged: bool,
    json: bool,
    resolver_selection: Option<pbps_config::resolver::ResolverSelection>,
    no_rehearse: bool,
    carried: &mut Vec<Finding>,
) -> anyhow::Result<()> {
    let dialect = crate::dialect(project)?;
    let declarations = Declarations::read(project, dialect.as_ref())?;
    match super::plan_db(
        project,
        target,
        declarations,
        dialect,
        sql_out.is_some(),
        staged,
        json,
        resolver_selection,
        no_rehearse,
        carried,
    )? {
        PlanResult::Planned(planned) => planned.publish(out, sql_out, json),
        PlanResult::Refused(report) => {
            if json {
                report.emit_json()
            } else {
                report.outcome()
            }
        }
    }
}

impl Planned {
    fn publish(
        self,
        out: Option<&std::path::Path>,
        sql_out: Option<&std::path::Path>,
        json: bool,
    ) -> anyhow::Result<()> {
        let Self {
            artifact,
            script,
            statements: _,
            data,
            findings,
        } = self;
        let rendered = out
            .map(|_| crate::rendered_plan(artifact.as_ref()))
            .transpose()?;
        // Both rendered before either is written, and the SQL written first
        // (DEC-1516.1): a failed write never leaves a new plan beside an old
        // script. Each is written in place, so the file keeps its owner, group
        // and permissions, as it always has.
        if let (Some(path), Some(script)) = (sql_out, &script) {
            std::fs::write(path, script)
                .with_context(|| format!("cannot write `{}`", path.display()))?;
        }
        if let (Some(path), Some(rendered)) = (out, &rendered) {
            std::fs::write(path, rendered)
                .with_context(|| format!("cannot write `{}`", path.display()))?;
        }
        if let Some(path) = out
            && !json
        {
            println!(
                "\nwrote {} (checksum {})",
                path.display(),
                artifact.checksum()
            );
        }
        if let Some(path) = sql_out
            && !json
        {
            println!("wrote {}", path.display());
        }
        if out.is_none() && !json {
            println!(
                "\nThis plan was not saved. `--out plan.json` writes the artifact `pbps apply` takes."
            );
        }
        if json {
            let findings = findings
                .into_iter()
                .chain(super::recovery_findings())
                .collect();
            return crate::output::Report::new("plan", findings, Some(data)).emit_json();
        }
        Ok(())
    }
}

/// How an attempt on an identified artifact ended.
///
/// The three outcomes are one type so that [`super::cmd_apply`] has one place to
/// report them: every path out of [`apply_identified`] arrives here, and the
/// `on_apply_attempt` hook fires from that one place. Earlier the failure hook
/// was called at each site that could fail after the lock, and the refusals
/// before it — a stale `--checksum`, an unapproved risk — returned past the
/// hook entirely, so the audit sink advertised as seeing every attempt never
/// saw the rejected artifact.
pub(super) enum Attempt {
    /// Nothing to do; not an attempt in the hook's sense.
    Empty,
    /// The plan ran and its ledger entry is durable. `released` is the lock
    /// cleanup, reported separately because a failed DELETE of the lock row
    /// cannot turn committed DDL plus its success row into a failed deployment.
    Applied {
        entry: i64,
        released: Result<bool, pbps_db::DbError>,
    },
    /// The plan was run under the lock and did not complete. The lock cleanup
    /// travels with the error so the caller can warn about it.
    Failed {
        error: anyhow::Error,
        released: Result<bool, pbps_db::DbError>,
    },
}

#[cfg(test)]
mod tests;
