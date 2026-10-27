//! Confirming a run's resources gone, and finding what dead runs left
//! (ADR-0019 section 3, "Normal teardown and later recovery").
//!
//! Nothing here deletes on a guess. The runtime has to say a thing is absent
//! for it to count as absent; an unreadable inspection and a timeout are two
//! other results, and neither is "gone". A resource goes only when its labels
//! and immutable id match the record, and a record goes only after that.

use std::time::Instant;

use super::owner::{self, Owner};
use super::record::{Daemon, Record, StateDir};
use super::runtime::{Fail, Inspected, Runtime};
use super::{CleanupFailure, FailureKind};

/// The label that says a container belongs to a test run.
pub(super) const RUN_LABEL: &str = "pbps.test.run";

/// What one reconciliation did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Reconciliation {
    /// Runs whose resources are confirmed gone and whose records were deleted.
    pub removed: Vec<String>,
    /// Runs whose owner holds the lock: left alone.
    pub live: Vec<String>,
    /// Runs that could not be reconciled. Any of these fails the command.
    pub failures: Vec<CleanupFailure>,
}

pub(super) fn failure(
    dir: &StateDir,
    run_id: &str,
    kind: FailureKind,
    resources: Vec<String>,
    detail: String,
) -> CleanupFailure {
    CleanupFailure {
        run_id: run_id.to_owned(),
        kind,
        resources,
        detail,
        remedy: format!(
            "inspect with `docker ps --all --filter label={RUN_LABEL}={run_id}`, remove what is \
             yours with `docker rm --force --volumes <id>`, then delete {}/{run_id}.json and its \
             .lock",
            dir.path().display(),
        ),
    }
}

fn from_fail(dir: &StateDir, run_id: &str, what: &str, fail: Fail) -> CleanupFailure {
    let kind = match fail {
        Fail::Unreadable(_) => FailureKind::Unreadable,
        Fail::TimedOut(_) => FailureKind::TimedOut,
    };
    failure(dir, run_id, kind, vec![what.to_owned()], fail.to_string())
}

/// `found` is the container the record is about: same id, same name, every
/// label the run asked for, and only named volumes.
pub(super) fn ours(record: &Record, found: &Inspected) -> Result<(), String> {
    if let Some(id) = &record.container.id
        && *id != found.id
    {
        return Err(format!("its id is {}, not the recorded {id}", found.id));
    }
    if found.name != record.container.name {
        return Err(format!(
            "its name is {}, not the recorded {}",
            found.name, record.container.name
        ));
    }
    for (key, value) in &record.labels {
        if found.labels.get(key) != Some(value) {
            return Err(format!("its label {key} is not {value}"));
        }
    }
    if !found.other_mounts.is_empty() {
        return Err(format!(
            "it mounts something that is not a named volume ({})",
            found.other_mounts.join(", ")
        ));
    }
    if record.container.id.is_some() && found.volumes.iter().any(|v| !record.volumes.contains(v)) {
        return Err("it mounts a volume the run did not record".into());
    }
    Ok(())
}

async fn remove_verified(
    dir: &StateDir,
    record: &Record,
    rt: &impl Runtime,
    id: &str,
) -> Result<(), CleanupFailure> {
    let named = format!("the container {id}");
    let Some(found) = rt
        .inspect(id)
        .await
        .map_err(|f| from_fail(dir, &record.run_id, &named, f))?
    else {
        // The runtime says there is none. A different container that now
        // holds the recorded name is not the original and is not touched.
        return Ok(());
    };
    ours(record, &found).map_err(|why| {
        failure(
            dir,
            &record.run_id,
            FailureKind::Conflict,
            vec![named.clone()],
            why,
        )
    })?;
    for volume in &found.volumes {
        let users = rt
            .users(volume)
            .await
            .map_err(|f| from_fail(dir, &record.run_id, &format!("the volume {volume}"), f))?;
        if users.iter().any(|user| *user != found.id) {
            return Err(failure(
                dir,
                &record.run_id,
                FailureKind::Conflict,
                vec![format!("the volume {volume}")],
                format!("the volume is also used by {}", users.join(", ")),
            ));
        }
    }
    rt.remove(&found.id).await.map_err(|left| {
        failure(
            dir,
            &record.run_id,
            FailureKind::NotRemoved,
            left,
            "removal was not confirmed".into(),
        )
    })
}

/// Removes everything the record names and confirms it gone: `Ok` only when
/// the runtime says so. A record that never got a container id is resolved by
/// listing the exact run label, and an unreadable listing is not an empty one.
pub(super) async fn clean(
    dir: &StateDir,
    record: &Record,
    rt: &impl Runtime,
) -> Result<(), CleanupFailure> {
    let ids = match &record.container.id {
        Some(id) => vec![id.clone()],
        None => rt
            .list(&format!("{RUN_LABEL}={}", record.run_id))
            .await
            .map_err(|f| from_fail(dir, &record.run_id, "the containers of the run", f))?,
    };
    for id in ids {
        remove_verified(dir, record, rt, &id).await?;
    }
    Ok(())
}

/// Reconciles every record in `dir` against `rt`, within `deadline`: live runs
/// are left alone, dead ones are cleaned, and what cannot be proven is
/// reported with how to inspect it. `daemon` is the runtime this process talks
/// to; a record made against another one is not judged here.
pub(super) async fn reconcile(
    dir: &StateDir,
    rt: &impl Runtime,
    daemon: &Daemon,
    deadline: Instant,
) -> Result<Reconciliation, super::Error> {
    let mut done = Reconciliation::default();
    let boot_now = owner::boot_id()?;
    for run_id in dir.runs()? {
        let Some(_lock) = dir.try_lock_existing(&run_id)? else {
            done.live.push(run_id);
            continue;
        };
        let record = match dir.read(&run_id) {
            Ok(Some(record)) => record,
            // A lock with no record: the record is written before anything is
            // created, so there is nothing to confirm.
            Ok(None) => {
                dir.remove(&run_id)?;
                done.removed.push(run_id);
                continue;
            }
            Err(e) => {
                done.failures.push(failure(
                    dir,
                    &run_id,
                    FailureKind::Unprovable,
                    Vec::new(),
                    e.to_string(),
                ));
                continue;
            }
        };
        match owner::judge(
            &record.process,
            true,
            &boot_now,
            owner::lookup(record.process.pid),
        ) {
            Owner::Dead(_) => {}
            Owner::Alive => {
                done.live.push(run_id);
                continue;
            }
            Owner::Unprovable(why) => {
                done.failures.push(failure(
                    dir,
                    &record.run_id,
                    FailureKind::Unprovable,
                    Vec::new(),
                    why,
                ));
                continue;
            }
        }
        if record.daemon != *daemon {
            done.failures.push(failure(
                dir,
                &record.run_id,
                FailureKind::Unprovable,
                Vec::new(),
                format!(
                    "the run used the runtime {} ({}), and this process talks to {} ({})",
                    record.daemon.endpoint,
                    record.daemon.identity,
                    daemon.endpoint,
                    daemon.identity
                ),
            ));
            continue;
        }
        if Instant::now() >= deadline {
            done.failures.push(failure(
                dir,
                &record.run_id,
                FailureKind::TimedOut,
                Vec::new(),
                "the cleanup deadline passed before this run was reached".into(),
            ));
            continue;
        }
        let cleaned = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            clean(dir, &record, rt),
        )
        .await;
        match cleaned {
            Ok(Ok(())) => {
                dir.remove(&run_id)?;
                done.removed.push(run_id);
            }
            Ok(Err(failed)) => done.failures.push(failed),
            Err(_) => done.failures.push(failure(
                dir,
                &record.run_id,
                FailureKind::TimedOut,
                Vec::new(),
                "the cleanup deadline passed during this run's cleanup".into(),
            )),
        }
    }
    Ok(done)
}
