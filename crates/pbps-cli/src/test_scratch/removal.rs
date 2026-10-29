//! The one way `test_scratch` removes a resource or deletes a record
//! (ADR-0019 section 3, "Normal teardown and later recovery").
//!
//! **The rule, stated once: evidence is persisted before any removal, and a
//! record is deleted only after confirmed absence.** Normal teardown, the
//! recovery of a dead run's leftovers and the settling of a failed start all
//! come through [`remove_run`], so none of them can forget what another would
//! have kept. Nothing else here deletes on a guess: the runtime has to say a
//! thing is absent for it to count as absent, an unreadable inspection and a
//! timeout are two other results and neither is "gone", and a resource goes
//! only when its labels and immutable id match the record.

use super::record::{Record, State, StateDir};
use super::runtime::{Fail, Inspected, Runtime};
use super::{CleanupFailure, Error, FailureKind};

/// The label that says a container belongs to a test run.
pub(super) const RUN_LABEL: &str = "pbps.test.run";

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

impl StateDir {
    /// Deletes the run's record, temporary and lock. Private: only this module
    /// may, and only after absence is confirmed.
    fn forget(&self, run_id: &str) -> Result<(), Error> {
        for extension in ["key", "json", "json.tmp", "lock"] {
            let path = self.file(run_id, extension);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::State(format!("{}: {e}", path.display()))),
            }
        }
        Ok(())
    }
}

/// A lock file that has no record: the record is written before anything is
/// created, so there is nothing to confirm and nothing it could name.
pub(super) fn drop_unrecorded(dir: &StateDir, run_id: &str) -> Result<(), Error> {
    debug_assert!(matches!(dir.read(run_id), Ok(None)));
    dir.forget(run_id)
}

fn unread(dir: &StateDir, run_id: &str, what: &str, fail: Fail) -> CleanupFailure {
    let kind = match fail {
        Fail::Unreadable(_) => FailureKind::Unreadable,
        Fail::TimedOut(_) => FailureKind::TimedOut,
    };
    failure(dir, run_id, kind, vec![what.to_owned()], fail.to_string())
}

/// Nothing but `except` uses a volume the run is about to lose.
async fn unshared(
    dir: &StateDir,
    run_id: &str,
    rt: &impl Runtime,
    volume: &str,
    except: &str,
) -> Result<(), CleanupFailure> {
    let what = format!("the volume {volume}");
    let users = rt
        .users(volume)
        .await
        .map_err(|f| unread(dir, run_id, &what, f))?;
    if users.iter().any(|user| user != except) {
        let detail = format!("the volume is used by {}", users.join(", "));
        return Err(failure(
            dir,
            run_id,
            FailureKind::Conflict,
            vec![what],
            detail,
        ));
    }
    Ok(())
}

/// Removes everything `record` names, confirms it gone, and only then deletes
/// the record. A record with no container id is resolved by listing the exact
/// run label, and an unreadable listing is not an empty one.
pub(super) async fn remove_run(
    dir: &StateDir,
    record: &Record,
    rt: &impl Runtime,
) -> Result<(), CleanupFailure> {
    let run_id = record.run_id.as_str();
    let fail = |kind, resources, detail: String| failure(dir, run_id, kind, resources, detail);
    let unread = |what: &str, f: Fail| unread(dir, run_id, what, f);
    // Reported by a removal that could not say what it left; only a person can.
    if !record.leftovers.is_empty() {
        let note = "an earlier removal left these unconfirmed; remove them by hand".to_owned();
        return Err(fail(
            FailureKind::NotRemoved,
            record.leftovers.clone(),
            note,
        ));
    }
    let ids = match &record.container.id {
        Some(id) => vec![id.clone()],
        None => rt
            .list(&format!("{RUN_LABEL}={run_id}"))
            .await
            .map_err(|f| unread("the containers of the run", f))?,
    };
    let mut record = record.clone();
    for id in ids {
        let what = format!("the container {id}");
        // The runtime says there is none. A different container that now
        // holds the recorded name is not the original and is not touched.
        let Some(found) = rt.inspect(&id).await.map_err(|f| unread(&what, f))? else {
            continue;
        };
        // The evidence: the id and volumes of the container found, which a
        // pending record has not had until now.
        let mut next = record.clone();
        next.state = State::Removing;
        if next.container.id.is_none() {
            next.container.id = Some(id.clone());
            next.volumes.clone_from(&found.volumes);
        }
        ours(&next, &found).map_err(|why| fail(FailureKind::Conflict, vec![what.clone()], why))?;
        for volume in &found.volumes {
            unshared(dir, run_id, rt, volume, &found.id).await?;
        }
        if next != record {
            dir.write(&next)
                .map_err(|e| fail(FailureKind::Unprovable, vec![what.clone()], e.to_string()))?;
        }
        record = next;
        let note = "removal was not confirmed".to_owned();
        rt.remove(&found.id)
            .await
            .map_err(|left| fail(FailureKind::NotRemoved, left, note))?;
    }
    // The container being gone says nothing of its volumes: a removal that
    // died between the two leaves them, and the record is their only owner.
    for volume in &record.volumes {
        unshared(dir, run_id, rt, volume, "").await?;
        let note = "removal was not confirmed".to_owned();
        rt.remove_volume(volume)
            .await
            .map_err(|left| fail(FailureKind::NotRemoved, left, note))?;
    }
    dir.forget(run_id)
        .map_err(|e| fail(FailureKind::Unprovable, Vec::new(), e.to_string()))
}
