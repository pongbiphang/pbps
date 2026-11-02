//! `pbps test fixtures repin` (ADR-0019 §5, DEC-1165.12): advance one fixture's
//! pin, but only when the edited fixture provably leaves the same data.
//!
//! The original file loads at its old pin and replays to the new revision; the
//! edited file loads at the new revision. Each runs in its own fresh owned
//! scratch, and the rows and next allocator values of both are compared. The
//! manifest changes only on a match with both cleanups confirmed; everything
//! else leaves it byte-for-byte as it was.

use std::path::Path;
use std::time::Instant;

use pbps_config::test_manifest::{CommitId, ManifestError};
use sha2::{Digest as _, Sha256};

use super::execution::{Control, elapsed, ms};
use super::report::{ManifestWrite, Phase, Repin, Side, Status, TestData};
use super::runner::{Replay, locate, publish, reconcile, replay_phases, run_replay};
use super::{
    ChainProblem, ConsumedFile, GitProblem, Inputs, Outcome, Refusal, ReplayChain, Revision, Role,
    chain, git, revision, snapshot,
};
use crate::{deploy::test_run, test_scratch};

/// A validated re-pin: everything known without a database.
struct Plan {
    inputs: Inputs,
    index: usize,
    to: CommitId,
    /// The fixture as committed at `HEAD`: what the original side loads.
    old_sql: String,
    original: ReplayChain,
    proposed: ReplayChain,
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn refuse(message: String) -> Vec<Refusal> {
    vec![Refusal::Repin(message)]
}

fn git_refusal(what: &str, problem: GitProblem) -> Refusal {
    Refusal::Git {
        what: what.to_owned(),
        problem,
    }
}

fn chain_refusals(problems: Vec<ChainProblem>) -> Vec<Refusal> {
    problems.into_iter().map(Refusal::Chain).collect()
}

/// The baseline is what git has at `HEAD`, independent of the schema commit in
/// `at`; the edit is the working file. Every baseline file must be committed.
/// All but the selected fixture must also equal its working copy: a re-pin edits
/// one pin and one fixture. Returns the committed files and the old fixture SQL.
fn baseline(
    inputs: &Inputs,
    head: &CommitId,
    index: usize,
    manifest_bytes: &[u8],
) -> Result<(Vec<ConsumedFile>, String), Vec<Refusal>> {
    let (mut files, mut old_sql, mut refusals) = (Vec::new(), String::new(), Vec::new());
    let manifest = &inputs.frozen.manifest_file;
    let wanted = std::iter::once((
        "the manifest".to_owned(),
        manifest.path.as_str(),
        Role::Manifest,
        Some(manifest_bytes),
    ))
    .chain(inputs.frozen.fixtures.iter().enumerate().map(|(i, f)| {
        let same = (i != index).then_some(f.text.as_bytes());
        (
            format!("fixture `{}`", f.name),
            f.file.path.as_str(),
            Role::Fixture,
            same,
        )
    }));
    // The manifest's own spelling of each fixture path. The frozen path is the
    // resolved one, so a fixture replaced by a link would otherwise be read from
    // HEAD at the link's target and compared against the wrong original.
    let dir = manifest.path.rsplit_once('/').map_or("", |(dir, _)| dir);
    for (f, declared) in inputs
        .frozen
        .fixtures
        .iter()
        .zip(&inputs.frozen.manifest.fixtures)
    {
        let declared = declared_path(dir, &declared.file);
        if declared != f.file.path {
            refusals.extend(refuse(format!(
                "fixture `{}` is declared at `{declared}` but resolves to `{}`; a re-pin needs \
                 a plain file there, not a link",
                f.name, f.file.path
            )));
        }
    }
    for (what, path, role, same) in wanted {
        let committed =
            revision::committed_file(&inputs.root, head, path, role).map_err(|problem| {
                vec![Refusal::At {
                    revision: Revision::Commit(head.clone()),
                    problem,
                }]
            })?;
        match committed {
            None => refusals.extend(refuse(format!(
                "{what} (`{path}`) is not committed at HEAD; commit the original first"
            ))),
            Some((bytes, _)) if same.is_some_and(|w| w != bytes) => {
                refusals.extend(refuse(format!(
                    "{what} (`{path}`) has uncommitted changes; a re-pin edits one pin and one \
                 fixture, so commit or revert them first"
                )))
            }
            Some((bytes, file)) => {
                if same.is_none() {
                    match String::from_utf8(bytes) {
                        Ok(text) => old_sql = text,
                        Err(_) => refusals
                            .extend(refuse(format!("the committed `{path}` is not valid UTF-8"))),
                    }
                }
                files.push(file);
            }
        }
    }
    if refusals.is_empty() {
        Ok((files, old_sql))
    } else {
        Err(refusals)
    }
}

/// `file` relative to the manifest's project directory, with `.` and `..`
/// folded the way a resolved path is.
fn declared_path(dir: &str, file: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in file.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// A re-pin only advances: `to` is a commit this repository has, other than
/// and descending from the current pin.
fn advance(root: &Path, name: &str, from: &CommitId, to: &CommitId) -> Result<(), Vec<Refusal>> {
    git::present(root, to).map_err(|p| vec![git_refusal("the --at revision", p)])?;
    if to == from {
        return Err(refuse(format!(
            "fixture `{name}` is already pinned at {to}"
        )));
    }
    match git::is_ancestor(root, from, to) {
        Ok(true) => Ok(()),
        Ok(false) => Err(refuse(format!(
            "{to} does not descend from the current pin {from}; a re-pin only advances"
        ))),
        Err(problem) => Err(vec![git_refusal("the current pin and --at", problem)]),
    }
}

/// The groups that replay through `to` alone: pins newer than it are neither
/// loaded nor replayed to, in either side. Fixture indices stay the manifest's.
fn up_to(root: &Path, to: &CommitId, pins: &[CommitId]) -> Result<ReplayChain, Vec<Refusal>> {
    let ancestor = |a: &CommitId, b: &CommitId| git::is_ancestor(root, a, b);
    let mut kept = Vec::new();
    for (i, pin) in pins.iter().enumerate() {
        if ancestor(pin, to).map_err(|p| vec![git_refusal("the order of the pins", p)])? {
            kept.push(i);
        }
    }
    let wanted: Vec<CommitId> = kept.iter().map(|&i| pins[i].clone()).collect();
    let distance = |a: &CommitId, b: &CommitId| git::distance(root, a, b);
    let mut ordered = chain::order(to, &wanted, to, ancestor, distance).map_err(chain_refusals)?;
    for f in ordered.groups.iter_mut().flat_map(|g| &mut g.fixtures) {
        *f = kept[*f];
    }
    Ok(ordered)
}

fn validate(
    root: &Path,
    manifest_path: &Path,
    name: &str,
    at: &str,
    data: &mut TestData,
) -> Result<Plan, Vec<Refusal>> {
    let to = CommitId::parse(at).ok_or_else(|| {
        vec![Refusal::Manifest(ManifestError::InvalidCommit {
            field: "--at".to_owned(),
            value: at.to_owned(),
        })]
    })?;
    let mut inputs = super::validate_recorded(root, manifest_path, &mut data.observed_input)?;
    let root = inputs.root.clone();
    let manifest = inputs.frozen.manifest.clone();
    let index = manifest
        .fixtures
        .iter()
        .position(|f| f.name == name)
        .ok_or_else(|| {
            let known: Vec<_> = manifest
                .fixtures
                .iter()
                .map(|f| format!("`{}`", f.name))
                .collect();
            refuse(format!(
                "the manifest has no fixture named `{name}`; it has {}",
                if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                }
            ))
        })?;
    let from = manifest.fixtures[index].at.clone();
    let head = git::resolve_commit(&root, "HEAD").map_err(|p| vec![git_refusal("HEAD", p)])?;
    // The baseline is read at `HEAD` whatever the candidate is, so the final
    // recheck must watch it even when a committed candidate did not capture it.
    inputs.head.get_or_insert_with(|| head.clone());
    let manifest_file = inputs.frozen.manifest_file.clone();
    let working = std::fs::read(inputs.frozen.manifest_target())
        .map_err(|e| refuse(format!("the manifest cannot be read again: {e}")))?;
    if sha256(&working) != manifest_file.sha256 {
        return Err(vec![Refusal::Changed {
            path: manifest_file.path.clone(),
            change: super::Change::Edited,
        }]);
    }
    // Both are reported together, so one run names every reason to stop.
    let (committed, advanced) = (
        baseline(&inputs, &head, index, &working),
        advance(&root, name, &from, &to),
    );
    let (mut original_inputs, old_sql) = match (committed, advanced) {
        (Ok(found), Ok(())) => found,
        (committed, advanced) => {
            let mut refusals = committed.err().unwrap_or_default();
            refusals.extend(advanced.err().unwrap_or_default());
            return Err(refusals);
        }
    };

    // The whole proposed manifest must still be one chain that ends at the base.
    let mut proposed_pins: Vec<CommitId> = manifest.fixtures.iter().map(|f| f.at.clone()).collect();
    proposed_pins[index] = to.clone();
    let tip = &inputs.identity.candidate.commit;
    chain::order(
        &manifest.base,
        &proposed_pins,
        tip,
        |a, b| git::is_ancestor(&root, a, b),
        |a, b| git::distance(&root, a, b),
    )
    .map_err(chain_refusals)?;

    if !inputs.revisions.contains_key(&to) {
        let found = revision::read_commit_recorded(&root, &to, &mut data.observed_input.files)
            .and_then(|raw| revision::assemble(raw, manifest.dialect, None))
            .map_err(|problems| {
                let revision = Revision::Commit(to.clone());
                problems
                    .into_iter()
                    .map(|problem| Refusal::At {
                        revision: revision.clone(),
                        problem,
                    })
                    .collect::<Vec<_>>()
            })?;
        inputs.identity.files.extend(found.files.clone());
        inputs.identity.files.sort();
        inputs.identity.files.dedup();
        inputs.revisions.insert(to.clone(), found);
    }
    let original_pins: Vec<CommitId> = manifest.fixtures.iter().map(|f| f.at.clone()).collect();
    let original = up_to(&root, &to, &original_pins)?;
    let proposed = up_to(&root, &to, &proposed_pins)?;
    let files = |chain: &ReplayChain| -> Vec<ConsumedFile> {
        let revisions = &inputs.revisions;
        chain
            .groups
            .iter()
            .flat_map(|g| revisions[&g.commit].files.clone())
            .collect()
    };
    let fixture = &inputs.frozen.fixtures[index];
    // The proposed side replays every fixture in its chain, not only the edited one.
    let mut proposed_inputs = vec![manifest_file.clone()];
    proposed_inputs.extend(
        proposed
            .groups
            .iter()
            .flat_map(|g| &g.fixtures)
            .map(|&i| inputs.frozen.fixtures[i].file.clone()),
    );
    proposed_inputs.extend(files(&proposed));
    original_inputs.extend(files(&original));
    for list in [&mut original_inputs, &mut proposed_inputs] {
        list.sort();
        list.dedup();
    }
    data.repin = Some(Repin {
        fixture: name.to_owned(),
        path: fixture.file.path.clone(),
        from,
        to: to.clone(),
        original_inputs,
        proposed_inputs,
        sides: Vec::new(),
        tables: Vec::new(),
        allocators: Vec::new(),
        manifest: ManifestWrite {
            status: "not_attempted".to_owned(),
            old_sha256: manifest_file.sha256,
            new_sha256: None,
            reason: Some(
                "this build verifies the re-pin but does not yet write the manifest".to_owned(),
            ),
        },
    });
    Ok(Plan {
        inputs,
        index,
        to,
        old_sql,
        original,
        proposed,
    })
}

pub(crate) fn command(
    start: &Path,
    name: &str,
    at: &str,
    manifest: Option<&Path>,
    json: bool,
) -> anyhow::Result<()> {
    let mut data = TestData::for_repin();
    data.phases
        .push(Phase::new("input", "manifest, pins and edited fixture"));
    let started = Instant::now();
    // A relative walk stops at the working directory, missing a project above it.
    let start = match std::path::absolute(start) {
        Ok(start) => start,
        Err(error) => {
            data.failure(0, "test.input", error.to_string(), true);
            return publish(data.finish(), json);
        }
    };
    let (root, manifest) = locate(&start, manifest);
    let checked = validate(&root, &manifest, name, at, &mut data);
    data.phases[0].elapsed_ms = elapsed(started);
    let plan = match checked {
        Ok(plan) => plan,
        Err(refusals) => {
            for r in refusals {
                let unknown = r.outcome() == Outcome::Unanswerable;
                data.failure(0, "test.input", r.to_string(), unknown);
            }
            return publish(data.finish(), json);
        }
    };
    data.input = Some(plan.inputs.identity.clone());
    if plan.inputs.identity.dialect != pbps_config::DialectName::Postgres {
        data.failure(
            0,
            "test.input",
            "pbps test currently supports PostgreSQL; SQL Server follows in #1165".into(),
            false,
        );
        return publish(data.finish(), json);
    }
    data.phases[0].outcome = Status::Passed;
    match crate::db::runtime() {
        Ok(runtime) => runtime.block_on(execute(&plan, &mut data)),
        Err(error) => data.failure(0, "test.infrastructure", error.to_string(), true),
    }
    publish(data.finish(), json)
}

/// One comparison database: its phases are listed first so a stopped run still
/// shows the rest as skipped, then (when `run`) it replays and is read.
async fn side(
    plan: &Plan,
    label: &'static str,
    dir: &test_scratch::StateDir,
    control: &mut Control,
    data: &mut TestData,
    run: bool,
) -> Option<snapshot::Snapshot> {
    let timeouts = plan.inputs.frozen.manifest.timeouts;
    let original = label == "original";
    let fixtures = plan
        .inputs
        .frozen
        .fixtures
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let text = if original && i == plan.index {
                plan.old_sql.as_str()
            } else {
                f.text.as_str()
            };
            (f.name.as_str(), text)
        })
        .collect();
    let replay = Replay {
        chain: if original {
            &plan.original
        } else {
            &plan.proposed
        },
        revisions: &plan.inputs.revisions,
        fixtures,
        side: Some(label),
        timeouts,
    };
    let provision = data.phases.len();
    let mut p = Phase::new("provision", "owned PostgreSQL scratch");
    p.side = Some(label.to_owned());
    data.phases.push(p);
    let first = data.phases.len();
    replay_phases(&replay, &mut data.phases);
    let read = data.phases.len();
    let mut p = Phase::new("snapshot", "rows and allocators");
    p.side = Some(label.to_owned());
    data.phases.push(p);
    if !run {
        return None;
    }
    let scratch =
        test_run::boxed_future(|| control.start(dir, &plan.inputs.identity.image, data, provision))
            .await?;
    let mut found = None;
    if let Ok(session) = control.provision(&scratch, data, provision).await {
        let started = Instant::now();
        // Namespaces are provisioning prerequisites of every revision replayed.
        let namespaces = control
            .bounded(ms(timeouts.startup_ms), async {
                for group in &replay.chain.groups {
                    session
                        .namespaces(&replay.revisions[&group.commit].loaded.schema)
                        .await?;
                }
                Ok(())
            })
            .await;
        data.phases[provision].record(started, "test.infrastructure", &namespaces);
        if namespaces.is_ok()
            && test_run::boxed_future(|| run_replay(&replay, &session, data, control, first))
                .await
                .is_some()
        {
            let started = Instant::now();
            let schema = &plan.inputs.revisions[&plan.to].loaded.schema;
            let result = control
                .bounded(
                    ms(timeouts.step_ms),
                    test_run::boxed_future(|| snapshot::read(&session, schema)),
                )
                .await;
            data.phases[read].elapsed_ms = elapsed(started);
            match result {
                Ok(snapshot) => {
                    data.phases[read].outcome = Status::Passed;
                    found = Some(snapshot);
                }
                Err(error) => {
                    data.failure(read, "test.repin.unestablished", format!("{error:#}"), true);
                }
            }
        }
    }
    if let Some(repin) = &mut data.repin {
        repin.sides.push(Side {
            label: label.to_owned(),
            run_id: data.run_id.clone(),
            acquired_image: data.acquired_image.clone(),
            engine_build: data.engine_build.clone(),
        });
    }
    control.finish(scratch, data).await;
    if let Some(cleanup) = data.phases.last_mut() {
        cleanup.side = Some(label.to_owned());
    }
    found
}

/// Whether the side runs: the first always does, the second only after the
/// first completed and its scratch is confirmed gone, so a later cleanup can
/// never stand in for an earlier one that failed.
fn runs(second: bool, completed: usize, cleanup_confirmed: bool) -> bool {
    completed == usize::from(second) && (!second || cleanup_confirmed)
}

async fn execute(plan: &Plan, data: &mut TestData) {
    let timeouts = plan.inputs.frozen.manifest.timeouts;
    let Some(dir) = reconcile(data, ms(timeouts.cleanup_ms)).await else {
        return;
    };
    let mut control = Control::new(timeouts, tokio::signal::ctrl_c());
    let mut snapshots = Vec::new();
    // Stop at the first side that does not complete; the other is listed as skipped.
    for label in ["original", "proposed"] {
        let run = runs(label == "proposed", snapshots.len(), data.cleanup_confirmed);
        if let Some(found) = side(plan, label, &dir, &mut control, data, run).await {
            snapshots.push(found);
        }
    }
    let rows = data.phases.len();
    data.phases
        .push(Phase::new("compare_rows", "all managed tables"));
    data.phases
        .push(Phase::new("compare_allocators", "every sequence"));
    data.phases.push(Phase::new(
        "manifest_write",
        plan.inputs.frozen.manifest_file.path.clone(),
    ));
    if let [original, proposed] = &snapshots[..] {
        compare(data, rows, original, proposed);
    }
    data.reverify(&plan.inputs);
}

fn compare(
    data: &mut TestData,
    rows: usize,
    original: &snapshot::Snapshot,
    proposed: &snapshot::Snapshot,
) {
    let (tables, allocators) = snapshot::compare(original, proposed);
    for t in tables.iter().filter(|t| !t.equal) {
        data.failure(
            rows,
            "test.repin.rows",
            format!(
                "`{}` differs: original {} rows, proposed {} rows, {} differing row(s) shown{}; \
                 the edited fixture does not leave the original data",
                t.table,
                t.original_rows,
                t.proposed_rows,
                t.differences.len(),
                if t.truncated { " (more exist)" } else { "" }
            ),
            false,
        );
    }
    for a in allocators.iter().filter(|a| !a.equal) {
        data.failure(
            rows + 1,
            "test.repin.allocator",
            format!(
                "sequence `{}` (used by {}) hands out {} next in the original and {} in the \
                 proposed; a later fixture would get different values",
                a.sequence,
                a.used_by.as_deref().unwrap_or("no table"),
                a.original_next.as_deref().unwrap_or("nothing"),
                a.proposed_next.as_deref().unwrap_or("nothing")
            ),
            false,
        );
    }
    // Both sides must be the same engine, or equal state means nothing.
    if let Some(repin) = &data.repin
        && let [a, b] = &repin.sides[..]
        && (a.acquired_image != b.acquired_image || a.engine_build != b.engine_build)
    {
        data.failure(
            rows,
            "test.repin.unestablished",
            "the two comparison databases did not run the same image and engine build".into(),
            true,
        );
    }
    for phase in [rows, rows + 1] {
        if matches!(data.phases[phase].outcome, Status::Skipped) {
            data.phases[phase].outcome = Status::Passed;
        }
    }
    if let Some(repin) = &mut data.repin {
        repin.tables = tables;
        repin.allocators = allocators;
    }
}

#[cfg(test)]
mod tests;
