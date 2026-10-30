//! The owned-only adapter to ordinary deployment services (DEC-1165.9).
pub(crate) use super::services::Progress;
use super::services::{Attempt, Declarations, PlanResult, Planned};
use crate::plan_test::report::{Phase, Status};
use crate::plan_test::revision::Assembled;
use crate::plan_test::session::Session;

fn declarations(input: &Assembled) -> Declarations {
    Declarations {
        loaded: input.loaded.clone(),
        ids: input.ids.clone(),
    }
}
pub(crate) async fn bootstrap(
    session: &Session<'_>,
    input: &Assembled,
    phase: &mut Phase,
) -> anyhow::Result<i64> {
    session.prepare(&input.loaded.schema).await?;
    let project = session.project(input);
    let built = super::bootstrap_async(
        &project,
        Some(session.target()),
        None,
        declarations(input),
        crate::dialect(&project)?,
        "pbps-test",
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("connected bootstrap produced no result"))?;
    finish_attempt(
        phase,
        Attempt::Applied {
            entry: built.entry,
            released: built.released,
        },
    )?;
    Ok(built.entry)
}
pub(crate) async fn plan(
    session: &Session<'_>,
    input: &Assembled,
    phase: &mut Phase,
) -> anyhow::Result<Box<Planned>> {
    session.prepare(&input.loaded.schema).await?;
    let project = session.project(input);
    let selected = project.config.select_resolver(None, Some("test"))?;
    let mut carried = Vec::new();
    let result = super::plan_db_async(
        &project,
        session.target(),
        declarations(input),
        crate::dialect(&project)?,
        false,
        false,
        true,
        selected,
        false,
        &mut carried,
        true,
    )
    .await;
    phase.findings.extend(carried);
    match result? {
        PlanResult::Refused(report) => {
            phase.findings.extend(report.findings);
            anyhow::bail!("ordinary planning refused the test hop");
        }
        PlanResult::Planned(planned) => {
            phase.findings.clone_from(&planned.findings);
            phase.checksum = Some(planned.artifact.checksum());
            phase.changes = Some(planned.data.changes);
            phase.statements = Some(planned.statements);
            phase.risks = planned.data.risks.iter().map(|r| (*r).to_owned()).collect();
            Ok(planned)
        }
    }
}
pub(crate) async fn apply(
    session: &Session<'_>,
    input: &Assembled,
    planned: &Planned,
    phase: &mut Phase,
    approved: &str,
    progress: &Progress,
) -> anyhow::Result<()> {
    let project = session.project(input);
    let plan = planned.artifact.wire();
    let allow = plan.changes.gated_risks();
    let request = super::ApplyRequest {
        plan_path: std::path::Path::new("in-memory test plan"),
        approved_checksum: approved,
        allow: &allow,
        staged: false,
        resume: false,
    };
    let result = super::services::apply_async(
        &project,
        session.target(),
        &request,
        plan,
        &planned.artifact.checksum(),
        &crate::dialect(&project)?,
        "pbps-test",
        Some(progress),
    )
    .await;
    phase.accepted = progress.accepted.get();
    phase.started = progress.started.get();
    phase
        .findings
        .extend(progress.findings.borrow().iter().cloned());
    finish_attempt(phase, result?)
}

fn finish_attempt(phase: &mut Phase, attempt: Attempt) -> anyhow::Result<()> {
    let (result, released) = match attempt {
        Attempt::Empty => return Ok(()),
        Attempt::Applied { entry, released } => {
            // A committed entry remains evidence even when cleanup fails.
            phase.entry = Some(entry);
            (Ok(()), released)
        }
        Attempt::Failed { error, released } => (Err(error), released),
    };
    if let Err(error) = released {
        phase.outcome = Status::Unanswerable;
        phase.findings.push(crate::output::Finding::error(
            "test.cleanup",
            format!("deployment lock release failed: {error}"),
        ));
        // The ordinary deployment error is primary; cleanup has its own finding.
        return result.and(Err(error.into()));
    }
    result
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
