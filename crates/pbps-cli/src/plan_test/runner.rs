//! Orchestration over frozen inputs and the owned-only adapters (DEC-1165.9).
use super::{
    Assembled, Inputs, Outcome, ReplayChain,
    execution::{Control, elapsed, ms},
    report::{Assertion, Phase, Status, TestData},
    session::{HopContext, Session},
};
use crate::{deploy::test_run, test_scratch};
use pbps_config::test_manifest::{CommitId, Timeouts};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// One pinned replay: bootstrap the oldest pin, hop between pins, grant and
/// load each group's fixtures. `pbps test` replays to the base; a re-pin replays
/// the same way, twice, to the new revision (ADR-0019 §4, §5).
pub(super) struct Replay<'a> {
    /// The engine of the run, which its phases read their error numbers by.
    pub driver: pbps_db::Driver,
    pub chain: &'a ReplayChain,
    pub revisions: &'a BTreeMap<CommitId, Assembled>,
    /// Name, SQL and path by manifest index; an entry the chain does not name is unused.
    pub fixtures: Vec<(&'a str, &'a str, &'a str)>,
    pub side: Option<&'a str>,
    pub timeouts: Timeouts,
}

impl Inputs {
    fn replay(&self) -> Replay<'_> {
        Replay {
            driver: crate::db::driver_for(self.identity.dialect),
            chain: &self.chain,
            revisions: &self.revisions,
            fixtures: self
                .frozen
                .fixtures
                .iter()
                .map(|f| (f.name.as_str(), f.text.as_str(), f.file.path.as_str()))
                .collect(),
            side: None,
            timeouts: self.frozen.manifest.timeouts,
        }
    }
}

pub(super) fn replay_phases(r: &Replay<'_>, phases: &mut Vec<Phase>) {
    let first = phases.len();
    for (index, group) in r.chain.groups.iter().enumerate() {
        if index == 0 {
            phases.push(Phase::on(r.driver, "bootstrap", group.commit.to_string()));
        } else {
            let hop = &r.chain.hops[index - 1];
            for phase in ["replay_plan", "replay_apply"] {
                let mut p = Phase::on(r.driver, phase, group.commit.to_string());
                p.from = Some(hop.from.to_string());
                p.to = Some(hop.to.to_string());
                p.distance = Some(hop.distance);
                phases.push(p);
            }
        }
        for f in &group.fixtures {
            phases.push(Phase::on(r.driver, "fixture", r.fixtures[*f].0));
        }
    }
    for p in &mut phases[first..] {
        p.side = r.side.map(str::to_owned);
    }
}

pub(super) fn scratch_label(driver: pbps_db::Driver) -> &'static str {
    match driver {
        pbps_db::Driver::Postgres => "owned PostgreSQL scratch",
        pbps_db::Driver::Mssql => "owned SQL Server scratch",
    }
}

fn prepare(data: &mut TestData, inputs: &Inputs) {
    let driver = crate::db::driver_for(inputs.identity.dialect);
    data.phases
        .push(Phase::on(driver, "provision", scratch_label(driver)));
    replay_phases(&inputs.replay(), &mut data.phases);
    data.phases.push(Phase::on(
        driver,
        "candidate_plan",
        inputs.candidate.revision.to_string(),
    ));
    data.phases.push(Phase::on(
        driver,
        "candidate_apply",
        inputs.candidate.revision.to_string(),
    ));
    for a in &inputs.frozen.assertions {
        data.phases.push(Phase::on(driver, "assertion", &a.name));
        data.assertions.push(Assertion {
            name: a.name.clone(),
            path: a.file.path.clone(),
            sha256: a.file.sha256.clone(),
            outcome: "skipped".into(),
            columns: Vec::new(),
            rows: Vec::new(),
            truncated: false,
        });
    }
}
/// Runs the phases [`replay_phases`] listed, from `phase`; `None` when one
/// failed (its finding is recorded), else the index after the last.
pub(super) async fn run_replay(
    r: &Replay<'_>,
    session: &Session<'_>,
    data: &mut TestData,
    control: &mut Control,
    mut phase: usize,
) -> Option<usize> {
    let timeouts = r.timeouts;
    for (index, group) in r.chain.groups.iter().enumerate() {
        let input = &r.revisions[&group.commit];
        if index == 0 {
            let Ok(entry) = control
                .bootstrap(session, input, &mut data.phases[phase])
                .await
            else {
                return None;
            };
            data.phases[phase].entry = Some(entry);
            phase += 1;
        } else {
            let start = Instant::now();
            // The catalog stands at the previous group's revision (#2552).
            let previous = &r.revisions[&r.chain.groups[index - 1].commit];
            let passed = hop(
                session,
                HopContext::after(previous, input),
                data,
                phase,
                control,
                false,
            )
            .await;
            data.replay_hops += 1;
            data.replay_ms += elapsed(start);
            if !passed {
                return None;
            }
            phase += 2;
        }
        if control
            .grants(session, input, &mut data.phases[phase - 1])
            .await
            .is_err()
        {
            return None;
        }
        if !group.fixtures.is_empty() {
            let mut conn = match control
                .bounded(ms(timeouts.fixture_ms), session.fixtures())
                .await
            {
                Ok(c) => c,
                Err(error) => {
                    data.failure(phase, "test.infrastructure", format!("{error:#}"), true);
                    return None;
                }
            };
            for f in &group.fixtures {
                let start = Instant::now();
                // Whole milliseconds, never zero: PostgreSQL reads zero as no limit.
                let timeout_ms = control
                    .deadline(ms(timeouts.fixture_ms))
                    .saturating_duration_since(tokio::time::Instant::now())
                    .as_nanos()
                    .div_ceil(1_000_000)
                    .clamp(1, i32::MAX as u128);
                let result = control
                    .bounded(
                        ms(timeouts.fixture_ms),
                        session.run_fixture(&mut conn, r.fixtures[*f].1, timeout_ms),
                    )
                    .await
                    .map_err(|error| {
                        error.context(format!(
                            "fixture `{}`",
                            pbps_model::escape::Escaped(r.fixtures[*f].2)
                        ))
                    });
                data.phases[phase].record(start, "test.fixture", &result);
                if result.is_err() {
                    return None;
                }
                phase += 1;
            }
            // The fixture's client is closed before the next capture/apply, but the
            // server ends its backend later: a session of its own can still be
            // listed for a moment (#2681).
            drop(conn);
        }
    }
    Some(phase)
}
async fn work(inputs: &Inputs, session: &Session<'_>, data: &mut TestData, control: &mut Control) {
    let timeouts = inputs.frozen.manifest.timeouts;
    let driver = crate::db::driver_for(inputs.identity.dialect);
    let replay = inputs.replay();
    let Some(mut phase) =
        test_run::boxed_future(|| run_replay(&replay, session, data, control, 2)).await
    else {
        return;
    };
    // The replay left the catalog at its last group, the base. A chain always
    // holds the base, so the absent branch is a chain no run reaches.
    let last = replay
        .chain
        .groups
        .last()
        .map(|group| &replay.revisions[&group.commit]);
    let candidate = last.map_or(HopContext::first(&inputs.candidate), |previous| {
        HopContext::after(previous, &inputs.candidate)
    });
    if !hop(session, candidate, data, phase, control, true).await {
        return;
    }
    phase += 2;
    if control
        .grants(session, &inputs.candidate, &mut data.phases[phase - 1])
        .await
        .is_err()
    {
        return;
    }
    // The write boundary comes after the last grants and before any assertion
    // SQL; a run with no assertion has nothing to seal.
    if !inputs.frozen.assertions.is_empty()
        && control
            .seal(session, &mut data.phases[phase - 1])
            .await
            .is_err()
    {
        return;
    }
    for (index, assertion) in inputs.frozen.assertions.iter().enumerate() {
        use pbps_db::query::AssertionResult as A;
        let start = Instant::now();
        let result = control
            .bounded(ms(timeouts.assertion_ms), async {
                Ok(session
                    .run_assertion(&assertion.text, ms(timeouts.assertion_ms))
                    .await)
            })
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => A::Unanswerable(pbps_db::DbError::BadRow(error.to_string())),
        };
        let (outcome, finding, evidence) = judge(driver, result);
        let a = &mut data.assertions[index];
        a.outcome = outcome.into();
        if let Some(e) = evidence {
            a.evidence(e);
        }
        data.phases[phase].elapsed_ms = elapsed(start);
        if let Some((id, message, unknown)) = finding {
            data.failure(phase, id, message, unknown);
            let f = data.phases[phase]
                .findings
                .last_mut()
                .expect("the failure just recorded a finding");
            *f = f.clone().at(&assertion.file.path, None);
            return;
        }
        data.phases[phase].outcome = Status::Passed;
        phase += 1;
    }
}
/// An assertion's outcome name, the finding it raises (id, message, whether the
/// run could not look) and its evidence.
type Judged = (
    &'static str,
    Option<(&'static str, String, bool)>,
    Option<pbps_db::query::QueryEvidence>,
);

/// What one assertion's result means for the run: its outcome name, the finding
/// it raises (id, message, whether the run could not look), and its evidence.
fn judge(driver: pbps_db::Driver, result: pbps_db::query::AssertionResult) -> Judged {
    use pbps_db::query::AssertionResult as A;
    match result {
        A::Pass(e) => ("pass", None, Some(e)),
        A::Violation(e) => (
            "violation",
            Some((
                "test.assertion.rows",
                "assertion returned violation rows".into(),
                false,
            )),
            Some(e),
        ),
        A::InvalidResult { message } => (
            "invalid_result",
            Some(("test.assertion.invalid-result", message, false)),
            None,
        ),
        // A coded engine fault (resources, storage, a session the server ended)
        // says the run could not look, not what the SQL did: the same reading
        // `Phase::record` gives a step's error.
        A::SqlError(e) if super::report::engine_fault(driver, &e) => (
            "unanswerable",
            Some(("test.assertion.unanswerable", e.to_string(), true)),
            None,
        ),
        A::SqlError(e) => (
            "sql_error",
            Some(("test.assertion.sql", e.to_string(), false)),
            None,
        ),
        A::BoundaryViolation => (
            "boundary_violation",
            Some((
                "test.assertion.boundary",
                "assertion changed its read-only transaction".into(),
                false,
            )),
            None,
        ),
        A::Unanswerable(e) => (
            "unanswerable",
            Some(("test.assertion.unanswerable", e.to_string(), true)),
            None,
        ),
    }
}
async fn hop(
    session: &Session<'_>,
    context: HopContext<'_>,
    data: &mut TestData,
    phase: usize,
    control: &mut Control,
    candidate: bool,
) -> bool {
    let input = context.incoming;
    let Ok(planned) = control
        .plan(session, context, &mut data.phases[phase])
        .await
    else {
        return false;
    };
    let checksum = planned.artifact.checksum();
    if candidate {
        data.candidate_sealed_checksum = Some(checksum.clone());
    }
    let apply = &mut data.phases[phase + 1];
    apply.checksum = Some(checksum.clone());
    let result = control
        .apply(session, input, &planned, apply, &checksum)
        .await;
    if candidate {
        data.candidate_accepted_checksum = apply.accepted.then(|| checksum.clone());
        data.candidate_executed_checksum = apply.started.then_some(checksum);
    }
    result.is_ok()
}

/// The project root, found without parsing configuration twice or losing its
/// hash on a refusal, and the manifest path (default `tests/pbps-tests.yml`).
pub(super) fn locate(start: &Path, manifest: Option<&Path>) -> (PathBuf, PathBuf) {
    let root = start
        .ancestors()
        .find(|p| {
            let found = std::fs::symlink_metadata(p.join(pbps_config::CONFIG_FILE));
            !matches!(found, Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        })
        .unwrap_or(start);
    let manifest = root
        .join(manifest.unwrap_or_else(|| Path::new(pbps_config::test_manifest::DEFAULT_MANIFEST)));
    (root.to_owned(), manifest)
}

/// Reconciles this user's recorded leftovers before any new work. `None` means
/// the findings are recorded and nothing may start.
pub(super) async fn reconcile(
    data: &mut TestData,
    allowance: Duration,
) -> Option<test_scratch::StateDir> {
    let dir = match test_scratch::state_dir(|name| std::env::var_os(name)) {
        Ok(dir) => dir,
        Err(error) => {
            data.failure(0, "test.cleanup", error.to_string(), true);
            return None;
        }
    };
    match test_scratch::recover(&dir, Instant::now() + allowance).await {
        Ok(recovered) => {
            data.recovery_live = recovered.live;
            data.recovery_removed = recovered.removed;
            for error in recovered.failures {
                data.failure(0, "test.cleanup", error.to_string(), true);
            }
            data.phases[0].findings.is_empty().then_some(dir)
        }
        Err(error) => {
            data.failure(0, "test.cleanup", error.to_string(), true);
            None
        }
    }
}

pub(crate) fn command(
    start: &Path,
    manifest: Option<&Path>,
    cleanup: bool,
    json: bool,
) -> anyhow::Result<()> {
    let mut data = TestData::new(cleanup);
    data.phases
        .push(Phase::offline("input", "manifest and frozen revisions"));
    let started = Instant::now();
    let inputs = if cleanup {
        None
    } else {
        // A relative walk stops at the working directory, missing a project above it.
        let start = match std::path::absolute(start) {
            Ok(start) => start,
            Err(error) => {
                data.phases[0].elapsed_ms = elapsed(started);
                data.failure(0, "test.input", error.to_string(), true);
                return publish(data.finish(), json);
            }
        };
        let (root, manifest) = locate(&start, manifest);
        let root = root.as_path();
        let checked = super::validate_recorded(root, &manifest, &mut data.observed_input);
        data.phases[0].elapsed_ms = elapsed(started);
        match checked {
            Ok(inputs) => {
                data.input = Some(inputs.identity.clone());
                prepare(&mut data, &inputs);
                Some(inputs)
            }
            Err(refusals) => {
                for r in refusals {
                    data.failure(
                        0,
                        "test.input",
                        r.to_string(),
                        r.outcome() == Outcome::Unanswerable,
                    );
                }
                return publish(data.finish(), json);
            }
        }
    };
    data.phases[0].outcome = Status::Passed;
    data.phases[0].elapsed_ms = elapsed(started);
    match crate::db::runtime() {
        Ok(runtime) => runtime.block_on(execute(inputs.as_ref(), &mut data)),
        Err(error) => data.failure(0, "test.infrastructure", error.to_string(), true),
    }
    if let Some(inputs) = &inputs {
        data.reverify(inputs);
    }
    publish(data.finish(), json)
}

async fn execute(inputs: Option<&Inputs>, data: &mut TestData) {
    let allowance = inputs.map_or(Duration::from_secs(30), |i| {
        ms(i.frozen.manifest.timeouts.cleanup_ms)
    });
    let Some(dir) = reconcile(data, allowance).await else {
        return;
    };
    let Some(inputs) = inputs else {
        data.cleanup_confirmed = true;
        return;
    };
    let mut control = Control::new(inputs.frozen.manifest.timeouts, tokio::signal::ctrl_c())
        .with_driver(crate::db::driver_for(inputs.identity.dialect));
    let Some(scratch) =
        test_run::boxed_future(|| control.start(&dir, &inputs.identity.image, data, 1)).await
    else {
        return;
    };
    if let Ok(session) = control.provision(&scratch, data, 1).await {
        let started = Instant::now();
        // Namespace prerequisites are not managed identities. Later namespaces
        // must exist for replay; future roles still wait for their own hop.
        let namespaces = control
            .bounded(ms(inputs.frozen.manifest.timeouts.startup_ms), async {
                for input in inputs.revisions.values().chain([&inputs.candidate]) {
                    session.namespaces(&input.loaded.schema).await?;
                }
                Ok(())
            })
            .await;
        data.phases[1].record(started, "test.infrastructure", &namespaces);
        if namespaces.is_ok() {
            test_run::boxed_future(|| work(inputs, &session, data, &mut control)).await;
        }
    }
    control.finish(scratch, data).await;
}

pub(super) fn publish(report: crate::output::Report<TestData>, json: bool) -> anyhow::Result<()> {
    let mut report = report;
    report.findings.extend(
        report
            .data
            .as_ref()
            .into_iter()
            .flat_map(|d| d.summary(!json)),
    );
    let result = if json {
        report.emit_json()
    } else {
        print!("{}", crate::output::human(&report.findings));
        report.outcome()
    };
    if matches!(report.result, crate::output::Outcome::Unanswerable) {
        anyhow::bail!("test could not be completed; see the test findings")
    }
    result
}

#[cfg(test)]
mod tests;
