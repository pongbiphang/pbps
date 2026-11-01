//! Orchestration over frozen inputs and the owned-only adapters (DEC-1165.9).
use super::{
    Inputs, Outcome,
    execution::{Control, elapsed, ms},
    report::{Assertion, Phase, Status, TestData},
    session::Session,
};
use crate::{deploy::test_run, test_scratch};
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn prepare(data: &mut TestData, inputs: &Inputs) {
    data.phases
        .push(Phase::new("provision", "owned PostgreSQL scratch"));
    for (index, group) in inputs.chain.groups.iter().enumerate() {
        if index == 0 {
            data.phases
                .push(Phase::new("bootstrap", group.commit.to_string()));
        } else {
            let hop = &inputs.chain.hops[index - 1];
            for phase in ["replay_plan", "replay_apply"] {
                let mut p = Phase::new(phase, group.commit.to_string());
                p.from = Some(hop.from.to_string());
                p.to = Some(hop.to.to_string());
                p.distance = Some(hop.distance);
                data.phases.push(p);
            }
        }
        for f in &group.fixtures {
            data.phases
                .push(Phase::new("fixture", &inputs.frozen.fixtures[*f].name));
        }
    }
    data.phases.push(Phase::new(
        "candidate_plan",
        inputs.candidate.revision.to_string(),
    ));
    data.phases.push(Phase::new(
        "candidate_apply",
        inputs.candidate.revision.to_string(),
    ));
    for a in &inputs.frozen.assertions {
        data.phases.push(Phase::new("assertion", &a.name));
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
async fn work(inputs: &Inputs, session: &Session<'_>, data: &mut TestData, control: &mut Control) {
    let timeouts = inputs.frozen.manifest.timeouts;
    let mut phase = 2;
    for (index, group) in inputs.chain.groups.iter().enumerate() {
        let input = &inputs.revisions[&group.commit];
        if index == 0 {
            let Ok(entry) = control
                .bootstrap(session, input, &mut data.phases[phase])
                .await
            else {
                return;
            };
            data.phases[phase].entry = Some(entry);
            phase += 1;
        } else {
            let start = Instant::now();
            let passed = hop(session, input, data, phase, control, false).await;
            data.replay_hops += 1;
            data.replay_ms += elapsed(start);
            if !passed {
                return;
            }
            phase += 2;
        }
        if control
            .grants(session, input, &mut data.phases[phase - 1])
            .await
            .is_err()
        {
            return;
        }
        if !group.fixtures.is_empty() {
            let mut conn = match control
                .bounded(ms(timeouts.fixture_ms), session.fixtures())
                .await
            {
                Ok(c) => c,
                Err(error) => {
                    data.failure(phase, "test.infrastructure", format!("{error:#}"), true);
                    return;
                }
            };
            for f in &group.fixtures {
                let start = Instant::now();
                let result = control
                    .bounded(ms(timeouts.fixture_ms), async {
                        conn.execute(&format!(
                            "SET statement_timeout = '{}'",
                            timeouts.fixture_ms.get()
                        ))
                        .await?;
                        conn.execute(&inputs.frozen.fixtures[*f].text).await?;
                        Ok(())
                    })
                    .await;
                data.phases[phase].record(start, "test.fixture", &result);
                if result.is_err() {
                    return;
                }
                phase += 1;
            }
            // No fixture session survives into the next capture/apply.
            drop(conn);
        }
    }
    if !hop(session, &inputs.candidate, data, phase, control, true).await {
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
    for (index, assertion) in inputs.frozen.assertions.iter().enumerate() {
        use pbps_pg::assertions::AssertionResult as A;
        let start = Instant::now();
        let result = control
            .bounded(ms(timeouts.assertion_ms), async {
                Ok(pbps_pg::assertions::run(
                    session.assertion(),
                    &assertion.text,
                    ms(timeouts.assertion_ms),
                )
                .await)
            })
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => A::Unanswerable(pbps_db::DbError::BadRow(error.to_string())),
        };
        let (outcome, finding, evidence) = match result {
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
        };
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
async fn hop(
    session: &Session<'_>,
    input: &super::Assembled,
    data: &mut TestData,
    phase: usize,
    control: &mut Control,
    candidate: bool,
) -> bool {
    let Ok(planned) = control.plan(session, input, &mut data.phases[phase]).await else {
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

pub(crate) fn command(
    start: &Path,
    manifest: Option<&Path>,
    cleanup: bool,
    json: bool,
) -> anyhow::Result<()> {
    let mut data = TestData::new(cleanup);
    data.phases
        .push(Phase::new("input", "manifest and frozen revisions"));
    let started = Instant::now();
    let inputs = if cleanup {
        None
    } else {
        // Locate, but do not parse configuration twice or lose its hash on a refusal.
        let root = start
            .ancestors()
            .find(|p| {
                let found = std::fs::symlink_metadata(p.join(pbps_config::CONFIG_FILE));
                !matches!(found, Err(e) if e.kind() == std::io::ErrorKind::NotFound)
            })
            .unwrap_or(start);
        let manifest = root.join(
            manifest.unwrap_or_else(|| Path::new(pbps_config::test_manifest::DEFAULT_MANIFEST)),
        );
        let checked = super::validate_recorded(root, &manifest, &mut data.observed_input);
        data.phases[0].elapsed_ms = elapsed(started);
        match checked {
            Ok(inputs) => {
                data.input = Some(inputs.identity.clone());
                if inputs.identity.dialect != pbps_config::DialectName::Postgres {
                    data.failure(
                        0,
                        "test.input",
                        "pbps test currently supports PostgreSQL; SQL Server follows in #1165"
                            .into(),
                        false,
                    );
                    return publish(data.finish(), json);
                }
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
    let dir = match test_scratch::state_dir(|name| std::env::var_os(name)) {
        Ok(dir) => dir,
        Err(error) => {
            data.failure(0, "test.cleanup", error.to_string(), true);
            return;
        }
    };
    match test_scratch::recover(&dir, Instant::now() + allowance).await {
        Ok(recovered) => {
            data.recovery_live = recovered.live;
            data.recovery_removed = recovered.removed;
            for error in recovered.failures {
                data.failure(0, "test.cleanup", error.to_string(), true);
            }
            if !data.phases[0].findings.is_empty() {
                return;
            }
        }
        Err(error) => {
            data.failure(0, "test.cleanup", error.to_string(), true);
            return;
        }
    }
    let Some(inputs) = inputs else {
        data.cleanup_confirmed = true;
        return;
    };
    let mut control = Control::new(inputs.frozen.manifest.timeouts, tokio::signal::ctrl_c());
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

fn publish(report: crate::output::Report<TestData>, json: bool) -> anyhow::Result<()> {
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
