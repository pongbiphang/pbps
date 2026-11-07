//! The owned-only adapter to ordinary deployment services (DEC-1165.9).
use super::services::{Attempt, Bootstrap, Declarations, PlanResult};
pub(crate) use super::services::{Planned, Progress};
use crate::plan_test::report::{Phase, Status};
use crate::plan_test::revision::Assembled;
use crate::plan_test::session::Session;

// Reserve a large future's construction space only until it is boxed, before
// polling it. A call-site Box::pin keeps that space in the caller's poll frame.
#[inline(never)]
pub(crate) fn boxed_future<F: std::future::Future>(
    make: impl FnOnce() -> F,
) -> std::pin::Pin<Box<F>> {
    Box::pin(make())
}
fn declarations(input: &Assembled) -> Declarations {
    Declarations {
        loaded: input.loaded.clone(),
        ids: input.ids.clone(),
    }
}
/// `progress` is the caller's: a cancelled or expired phase drops this future, and
/// a collector owned here would take its catalog findings along (#2694).
pub(crate) async fn bootstrap(
    session: &Session<'_>,
    input: &Assembled,
    phase: &mut Phase,
    progress: &Progress,
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
        super::CatalogWarnings::for_progress(Some(progress)),
    )
    .await;
    // Before the cleanup finding below; the caller drains again after a drop.
    collect_findings(phase, progress);
    let built = built?;
    let entry = match &built {
        Bootstrap::Built(built) => Some(built.entry),
        Bootstrap::Script | Bootstrap::Failed { .. } => None,
    };
    // A failed attempt always returns its primary error here, after the cleanup
    // finding has been recorded.
    finish_attempt(phase, bootstrap_attempt(built)?)?;
    entry.ok_or_else(|| anyhow::anyhow!("bootstrap committed no ledger entry"))
}
/// Boxed future of the test hops. #2661: an `async fn` hands its caller the whole
/// future by value, and a debug build reserves that slot in the caller's frame
/// (the lifecycle test's own frame was 610 KB), so a caller that merely wraps the
/// call in `Box::pin` still pays for it. The slot lives in this small wrapper's
/// frame instead, which is gone before the deep calls start. The tests run on
/// libtest's 2 MiB thread; production runs the same planning on the 8 MiB main
/// thread.
pub(crate) type HopFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>> + 'a>>;

pub(crate) fn plan<'a>(
    session: &'a Session<'_>,
    input: &'a Assembled,
    phase: &'a mut Phase,
    carried: &'a mut Vec<crate::output::Finding>,
    progress: &'a Progress,
) -> HopFuture<'a, Box<Planned>> {
    Box::pin(plan_hop(session, input, phase, carried, progress))
}

async fn plan_hop(
    session: &Session<'_>,
    input: &Assembled,
    phase: &mut Phase,
    carried: &mut Vec<crate::output::Finding>,
    progress: &Progress,
) -> anyhow::Result<Box<Planned>> {
    session.prepare(&input.loaded.schema).await?;
    session.revoke_fixture_routines().await?;
    let project = session.project(input);
    let selected = project.config.select_resolver(None, Some("test"))?;
    // Boxed for the same reason: its future is large and this frame is on the
    // test stack (#2661).
    let result = Box::pin(super::plan_db_async(
        &project,
        session.target(),
        declarations(input),
        crate::dialect(&project)?,
        false,
        false,
        true,
        selected,
        false,
        carried,
        Some(progress),
    ))
    .await;
    // Taken, not moved: the caller drains the same two after a dropped future.
    phase.findings.append(carried);
    collect_findings(phase, progress);
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
pub(crate) fn apply<'a>(
    session: &'a Session<'_>,
    input: &'a Assembled,
    planned: &'a Planned,
    phase: &'a mut Phase,
    approved: &'a str,
    progress: &'a Progress,
) -> HopFuture<'a, ()> {
    Box::pin(apply_hop(
        session, input, planned, phase, approved, progress,
    ))
}

async fn apply_hop(
    session: &Session<'_>,
    input: &Assembled,
    planned: &Planned,
    phase: &mut Phase,
    approved: &str,
    progress: &Progress,
) -> anyhow::Result<()> {
    session.revoke_fixture_routines().await?;
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
    collect_progress(phase, progress);
    finish_attempt(phase, result?)
}

/// The caller still owns this after a cancelled future, and taking the findings
/// makes collecting on both completion and cancellation safe without duplicates.
pub(crate) fn collect_progress(phase: &mut Phase, progress: &Progress) {
    phase.accepted = progress.accepted.get();
    phase.started = progress.started.get();
    collect_findings(phase, progress);
}

/// The findings alone, for a phase with no accepted/started facts to report.
pub(crate) fn collect_findings(phase: &mut Phase, progress: &Progress) {
    phase.findings.extend(progress.findings.take());
}

/// A bootstrap as the apply path reports its own: the lock cleanup travels with
/// the primary cause, so `finish_attempt` can keep both (#2547).
fn bootstrap_attempt(result: Bootstrap) -> anyhow::Result<Attempt> {
    match result {
        Bootstrap::Script => anyhow::bail!("connected bootstrap produced no result"),
        Bootstrap::Built(built) => Ok(Attempt::Applied {
            entry: built.entry,
            released: built.released,
        }),
        Bootstrap::Failed { error, released } => Ok(Attempt::Failed { error, released }),
    }
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

/// The hops as a caller that never drops them sees them: nothing outlives the
/// future, so the collectors can be its own. The service tests drive these.
// Gated like its callers: on another platform it would be dead code.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod whole {
    use super::{Assembled, HopFuture, Phase, Planned, Progress, Session};

    pub(crate) async fn bootstrap(
        session: &Session<'_>,
        input: &Assembled,
        phase: &mut Phase,
    ) -> anyhow::Result<i64> {
        super::bootstrap(session, input, phase, &Progress::default()).await
    }

    pub(crate) fn plan<'a>(
        session: &'a Session<'_>,
        input: &'a Assembled,
        phase: &'a mut Phase,
    ) -> HopFuture<'a, Box<Planned>> {
        Box::pin(async move {
            let mut carried = Vec::new();
            super::plan(session, input, phase, &mut carried, &Progress::default()).await
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
