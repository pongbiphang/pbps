//! Finding what dead runs left (ADR-0019 section 3, "Normal teardown and later
//! recovery"): which runs are live, which owners are provably gone, and the
//! removal of the latter's resources, which [`remove_run`] alone performs.

use std::time::Instant;

use super::owner::{self, Owner};
use super::record::{Daemon, StateDir};
use super::removal::{drop_unrecorded, failure, remove_run};
use super::runtime::Runtime;
use super::{CleanupFailure, FailureKind};

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

fn refuse(done: &mut Reconciliation, dir: &StateDir, run_id: &str, kind: FailureKind, why: String) {
    done.failures
        .push(failure(dir, run_id, kind, Vec::new(), why));
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
                drop_unrecorded(dir, &run_id)?;
                done.removed.push(run_id);
                continue;
            }
            Err(e) => {
                refuse(
                    &mut done,
                    dir,
                    &run_id,
                    FailureKind::Unprovable,
                    e.to_string(),
                );
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
                refuse(&mut done, dir, &run_id, FailureKind::Unprovable, why);
                continue;
            }
        }
        if record.daemon != *daemon {
            refuse(
                &mut done,
                dir,
                &run_id,
                FailureKind::Unprovable,
                format!(
                    "the run used the runtime {} ({}), and this process talks to {} ({})",
                    record.daemon.endpoint,
                    record.daemon.identity,
                    daemon.endpoint,
                    daemon.identity
                ),
            );
            continue;
        }
        if Instant::now() >= deadline {
            refuse(
                &mut done,
                dir,
                &run_id,
                FailureKind::TimedOut,
                "the cleanup deadline passed before this run was reached".into(),
            );
            continue;
        }
        let cleaned = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            remove_run(dir, &record, rt),
        )
        .await;
        match cleaned {
            Ok(Ok(())) => done.removed.push(run_id),
            Ok(Err(failed)) => done.failures.push(failed),
            Err(_) => refuse(
                &mut done,
                dir,
                &run_id,
                FailureKind::TimedOut,
                "the cleanup deadline passed during this run's cleanup".into(),
            ),
        }
    }
    Ok(done)
}
