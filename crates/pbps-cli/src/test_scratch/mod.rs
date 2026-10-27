//! The scratch a plan test owns: a run record before anything is created, a
//! container whose id is verified before anything connects to it, and a
//! teardown that is confirmed or fails the run (ADR-0019 section 3).
//!
//! It adds to the resolver's recovery (#2410, DEC-2410.1) what a test run needs
//! beyond it and does not repeat it: the container is still the resolver's
//! (`Held`, its labels, its keeper, its removal), so a resolver run's sweep
//! sees a live test run as alive. What this adds is the client-side record that
//! makes the owner's death provable without a clock, and a cleanup that fails
//! the command instead of warning.

mod owner;
mod reconcile;
mod record;
mod runtime;

use std::fs::File;

use pbps_config::resolver::PullPolicy;
use pbps_db::Driver;

use pbps_cli::resolver::server::ProduceError;
use pbps_cli::resolver::server::scratch::container::{Held, Owner as ContainerOwner};

pub(crate) use reconcile::Reconciliation;
use reconcile::{RUN_LABEL, clean, failure, ours};
pub(crate) use record::StateDir;
use record::default_dir;
use record::{Container, Record, State, VERSION};
use runtime::{Docker, Runtime};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    #[cfg(not(target_os = "linux"))]
    #[error("run records are not supported on this platform")]
    Unsupported,
    #[error("{0}")]
    State(String),
    #[error("the container runtime: {0}")]
    Runtime(String),
    #[error("the scratch could not be started: {0}")]
    Start(String),
}

/// Why a run's resources could not be confirmed gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    /// The runtime could not be read; that is not absence.
    Unreadable,
    TimedOut,
    /// Labels, ids, volumes or mounts disagree with the record: not ours to remove.
    Conflict,
    /// The owner's death, or the record, could not be proven.
    Unprovable,
    /// Removal ran and the runtime did not confirm it.
    NotRemoved,
}

/// A cleanup that is not confirmed. It fails the command (`test.cleanup`,
/// unanswerable) however the assertions went.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "an unconfirmed cleanup must fail the run"]
pub(crate) struct CleanupFailure {
    pub run_id: String,
    pub kind: FailureKind,
    pub resources: Vec<String>,
    pub detail: String,
    /// How to inspect and remove it by hand.
    pub remedy: String,
}

impl std::fmt::Display for CleanupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the test run {} left resources that are not confirmed removed: {}",
            self.run_id, self.detail
        )?;
        if !self.resources.is_empty() {
            write!(f, " ({})", self.resources.join("; "))?;
        }
        write!(f, ". To resolve it: {}", self.remedy)
    }
}

/// The per-user directory of run records, created private on first use;
/// `var` reads the environment (`std::env::var_os`).
pub(crate) fn state_dir(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<StateDir, Error> {
    StateDir::open(default_dir(var)?)
}

/// Reconciles every record in `dir` against the runtime this process talks to,
/// within `deadline` (what `pbps test` and `pbps test --cleanup` do first).
pub(crate) async fn recover(
    dir: &StateDir,
    deadline: std::time::Instant,
) -> Result<Reconciliation, Error> {
    // Discovery is bounded by the same deadline as the cleanup it serves.
    let discovery =
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), runtime::current());
    let daemon = discovery
        .await
        .map_err(|_| Error::Runtime("the runtime did not answer before the deadline".into()))?
        .map_err(|f| Error::Runtime(f.to_string()))?;
    reconcile::reconcile(dir, &Docker, &daemon, deadline).await
}

/// What a failed start owes before its record may go. The start's own removal
/// reports what it left (a volume, say) in the error alone, and the record does
/// not hold it yet, so an empty listing must not be read as "gone" then.
async fn settle_failed_start(
    dir: &StateDir,
    record: &Record,
    error: &ProduceError,
    rt: &impl Runtime,
) -> Result<(), CleanupFailure> {
    if matches!(error, ProduceError::Cleanup(_)) {
        let detail = "the start's own removal was not confirmed".into();
        return Err(failure(
            dir,
            &record.run_id,
            FailureKind::NotRemoved,
            Vec::new(),
            detail,
        ));
    }
    clean(dir, record, rt).await
}

/// A run's record and lock, advanced as the run advances.
struct Journal<'a> {
    dir: &'a StateDir,
    record: Record,
    /// Held for the whole run; the kernel releases it when the process dies.
    _lock: File,
}

impl Journal<'_> {
    fn advance(&mut self, change: impl FnOnce(&mut Record)) -> Result<(), Error> {
        change(&mut self.record);
        self.dir.write(&self.record)
    }
}

/// Bridges the resolver's container start to the journal.
struct RunOwner<'j, 'd> {
    journal: &'j mut Journal<'d>,
}

impl ContainerOwner for RunOwner<'_, '_> {
    fn token(&self) -> String {
        self.journal.record.run_id.clone()
    }

    fn labels(&self) -> Vec<String> {
        self.journal
            .record
            .labels
            .iter()
            .filter(|(key, _)| key.starts_with("pbps.test."))
            .map(|(key, value)| format!("{key}={value}"))
            .collect()
    }

    async fn before_create(&mut self) -> Result<(), String> {
        self.journal
            .dir
            .write(&self.journal.record)
            .map_err(|e| e.to_string())
    }

    async fn created(&mut self, id: &str) -> Result<(), String> {
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("the runtime returned `{id}` as the container id"));
        }
        // Verified through the runtime before anything connects: the id it
        // names carries this run's name and labels, and mounts only volumes.
        let found = Docker
            .inspect(id)
            .await
            .map_err(|f| f.to_string())?
            .ok_or("the runtime has no container with the id it just returned")?;
        let mut record = self.journal.record.clone();
        record.container.id = Some(id.to_owned());
        record.volumes.clone_from(&found.volumes);
        ours(&record, &found)?;
        self.journal
            .advance(|r| {
                r.container.id = Some(id.to_owned());
                r.volumes = found.volumes;
                r.state = State::Created;
            })
            .map_err(|e| e.to_string())
    }
}

/// A scratch server this run owns. [`TestScratch::finish`] is the only way to
/// publish that it was cleaned up.
#[must_use = "finish() confirms the cleanup; dropping only falls back to a best-effort removal"]
pub(crate) struct TestScratch<'d> {
    held: Held,
    journal: Journal<'d>,
}

impl<'d> TestScratch<'d> {
    /// Records the run, starts the container, verifies it and returns the owned
    /// handle. On failure it confirms nothing is left or says what is.
    pub(crate) async fn start(
        dir: &'d StateDir,
        driver: Driver,
        image: &str,
        pull: PullPolicy,
    ) -> Result<TestScratch<'d>, Error> {
        let run_id = record::new_run_id();
        let daemon = runtime::current()
            .await
            .map_err(|f| Error::Runtime(f.to_string()))?;
        let engine = match driver {
            Driver::Postgres => "postgres",
            Driver::Mssql => "mssql",
        };
        // What `ours` checks of the container later; the resolver's own
        // labels (token, engine, process) are set by its start.
        let labels = [
            ("pbps.resolver", "scratch"),
            (RUN_LABEL, run_id.as_str()),
            ("pbps.test.purpose", "plan-test"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .into();
        let record = Record {
            version: VERSION,
            run_id: run_id.clone(),
            purpose: "plan-test".into(),
            engine: engine.into(),
            process: owner::Process::current()?,
            daemon,
            container: Container {
                name: format!("pbps-scratch-{run_id}"),
                id: None,
            },
            volumes: Vec::new(),
            labels,
            state: State::Pending,
        };
        let lock = dir.lock(&run_id)?;
        let mut journal = Journal {
            dir,
            record,
            _lock: lock,
        };
        match Held::start_owned(
            driver,
            image,
            pull,
            &mut RunOwner {
                journal: &mut journal,
            },
        )
        .await
        {
            Ok(held) => Ok(TestScratch { held, journal }),
            Err(error) => {
                // A start that failed leaves a record; it is deleted only once
                // the runtime confirms nothing of the run exists.
                let cleaned = settle_failed_start(dir, &journal.record, &error, &Docker).await;
                Err(Error::Start(match cleaned {
                    Ok(()) => {
                        dir.remove(&run_id)?;
                        error.to_string()
                    }
                    Err(unconfirmed) => format!("{error}; {unconfirmed}"),
                }))
            }
        }
    }

    /// The superuser connection to the owned server.
    pub(crate) fn connection(&self) -> &str {
        self.held.connection()
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.journal.record.run_id
    }

    /// The normal teardown: the container and its volumes are removed by the
    /// verified id and confirmed absent by the runtime, then the record goes.
    /// Anything less is a [`CleanupFailure`], and the record stays.
    pub(crate) async fn finish(self) -> Result<(), CleanupFailure> {
        self.finish_with(&Docker).await
    }

    async fn finish_with(mut self, rt: &impl Runtime) -> Result<(), CleanupFailure> {
        let _ = self.journal.advance(|r| r.state = State::Removing);
        let cleaned = clean(self.journal.dir, &self.journal.record, rt).await;
        // Confirmed or not, nothing may be removed by name any more: the drop
        // guard stands down and the keeper session closes with the handle.
        self.held.disown();
        cleaned?;
        let run_id = &self.journal.record.run_id;
        self.journal.dir.remove(run_id).map_err(|e| {
            failure(
                self.journal.dir,
                run_id,
                FailureKind::Unprovable,
                Vec::new(),
                e.to_string(),
            )
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;

/// Everywhere else the records refuse, by one typed error (the tests above are
/// Linux-only because they use the same `/proc` the records do).
#[cfg(all(test, not(target_os = "linux")))]
mod unsupported {
    #[test]
    fn run_records_refuse_off_linux() {
        let dir = std::env::temp_dir().join("pbps-test-runs-unsupported");
        assert!(matches!(
            super::StateDir::open(dir),
            Err(super::Error::Unsupported)
        ));
    }
}
