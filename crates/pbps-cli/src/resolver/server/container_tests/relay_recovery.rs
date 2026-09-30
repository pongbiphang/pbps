//! Actual-caller recovery controls. Completed failure injection proves
//! propagation; only the real fixture observations establish native residuals.

use crate::resolver::docker::{CandidateRun, Error as DockerError, StartFailure};
use std::future::{Future, pending, poll_fn};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Site {
    Admin,
    Scratch,
    Janitor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Prelaunch,
    CompletedEmpty,
    CompletedNamed,
    CancelFirstRealPoll,
}

const UNRELATED: &str = "unrelated-run-owned-recovery-obligation";

#[derive(Clone, Debug, Default)]
struct Observations {
    prepared_names: Vec<String>,
    start_names: Vec<String>,
    real_start_polls: usize,
}

#[derive(Clone)]
struct Control {
    site: Site,
    mode: Mode,
    observations: Arc<Mutex<Observations>>,
    reached: Arc<Notify>,
}

impl Control {
    fn new(site: Site, mode: Mode) -> Self {
        Self {
            site,
            mode,
            observations: Arc::new(Mutex::new(Observations::default())),
            reached: Arc::new(Notify::new()),
        }
    }

    fn observed(&self) -> Observations {
        self.observations.lock().unwrap().clone()
    }
}

tokio::task_local! {
    static CONTROL: Control;
}

fn active(site: Site) -> Option<Control> {
    CONTROL
        .try_with(|control| (control.site == site).then(|| control.clone()))
        .ok()
        .flatten()
}

/// Seed a prior obligation without claiming that the sentinel is a measured
/// native object. The actual caller must clear only its attempted relay.
pub(crate) fn before_prepare(
    site: Site,
    name: &str,
    unconfirmed: &mut Vec<String>,
) -> Result<(), DockerError> {
    let Some(control) = active(site) else {
        return Ok(());
    };
    control
        .observations
        .lock()
        .unwrap()
        .prepared_names
        .push(name.to_owned());
    if !unconfirmed.iter().any(|held| held == UNRELATED) {
        unconfirmed.push(UNRELATED.to_owned());
    }
    if control.mode == Mode::Prelaunch {
        Err(DockerError::RuntimeChanged)
    } else {
        Ok(())
    }
}

/// Leave recovery changes entirely to the production caller. With no matching
/// task scope this awaits the original lifecycle future unchanged.
pub(crate) async fn start<F>(
    site: Site,
    name: &str,
    real_start: F,
) -> Result<CandidateRun, StartFailure>
where
    F: Future<Output = Result<CandidateRun, StartFailure>>,
{
    let Some(control) = active(site) else {
        return real_start.await;
    };
    control
        .observations
        .lock()
        .unwrap()
        .start_names
        .push(name.to_owned());
    match control.mode {
        Mode::Prelaunch => panic!("a definitive prelaunch failure reached container launch"),
        Mode::CompletedEmpty | Mode::CompletedNamed => Err(StartFailure {
            cause: DockerError::RuntimeChanged,
            recovery_names: if control.mode == Mode::CompletedNamed {
                vec![name.to_owned()]
            } else {
                Vec::new()
            },
        }),
        Mode::CancelFirstRealPoll => {
            let mut real_start = std::pin::pin!(real_start);
            poll_fn(|context| {
                control.observations.lock().unwrap().real_start_polls += 1;
                match real_start.as_mut().poll(context) {
                    Poll::Pending => Poll::Ready(()),
                    Poll::Ready(_) => {
                        panic!("the real create-capable future completed before cancellation")
                    }
                }
            })
            .await;
            // Keep the real start future alive until the caller drops this
            // exact operation. Its supervisor retains native cleanup ownership.
            control.reached.notify_one();
            pending::<()>().await;
            unreachable!("a cancellation control cannot return a candidate")
        }
    }
}

async fn cancel_at_real_start<F: Future>(control: &Control, operation: F) {
    let reached = control.reached.clone();
    let signal = reached.notified();
    let mut operation = Box::pin(CONTROL.scope(control.clone(), operation));
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::select! {
            _ = signal => {},
            _ = &mut operation => panic!("the caller returned before its real-start cancellation point"),
        }
    })
    .await
    .expect("the actual caller must poll its create-capable lifecycle future");
    drop(operation);
}

async fn admin_failure(run: &mut super::ScratchRun) -> Option<super::ServerFailure> {
    let login = run.inner.admin_login("postgres");
    let (super::RunControl::Container(owner), Some(super::RunSession::Container(scratch))) =
        (&mut run.inner, run.scratch.as_ref())
    else {
        panic!("the actual admin caller requires the existing container runtime arm");
    };
    match owner.open_channel(login, Some(scratch.as_ref())).await {
        Err(failure) => Some(failure),
        Ok(session) => {
            // An unexpected success still hands its native relay back to the
            // real owner before any property assertion can fail.
            owner.retire(session);
            None
        }
    }
}

fn recovery(outcome: &Result<(), super::ServerFailure>) -> Vec<String> {
    outcome
        .as_ref()
        .err()
        .map(|failure| failure.recovery_names.clone())
        .unwrap_or_default()
}

async fn exercise(site: Site, mode: Mode) {
    let (socket, image, version) = super::fixture();
    super::actual_target_version(version).await;
    let before = super::containers();
    let mut target = super::target().await;
    let mut candidate = super::candidate(&mut target, &socket, image).await;
    let recipe = target.database_recipe().await.unwrap();
    let control = Control::new(site, mode);
    let mut operation_failed = mode == Mode::CancelFirstRealPoll;
    let mut janitor_entry_established = true;
    let first_report;
    let second_report;
    let immediate_report;
    let terminal;
    if site == Site::Scratch {
        let mut unexpectedly_open = None;
        let failure = if mode == Mode::CancelFirstRealPoll {
            cancel_at_real_start(&control, candidate.open_scratch(&recipe)).await;
            None
        } else {
            match CONTROL
                .scope(control.clone(), candidate.open_scratch(&recipe))
                .await
            {
                Err(failure) => Some(failure),
                Ok(run) => {
                    unexpectedly_open = Some(run);
                    None
                }
            }
        };
        operation_failed |= failure.is_some();
        immediate_report = failure
            .as_ref()
            .map(|failure| failure.recovery_names.clone());
        let identity_refused = candidate.identity().is_err();
        let check_refused = candidate.check().await.is_err();
        let retry = candidate.open_scratch(&recipe).await;
        let retry_refused = retry.is_err();
        if let Ok(mut run) = retry {
            let _ = run.close().await;
        }
        if let Some(mut run) = unexpectedly_open {
            let _ = run.close().await;
        }
        terminal = identity_refused && check_refused && retry_refused;
        first_report = recovery(&candidate.discard().await);
        second_report = recovery(&candidate.discard().await);
    } else {
        let mut run = candidate.open_scratch(&recipe).await.unwrap();
        if site == Site::Janitor {
            // An actual failed admin call leaves the existing in-flight
            // ownership flag. Real close must consequently take its janitor
            // path; no test-only removal algorithm is invoked.
            let prepare = Control::new(Site::Admin, Mode::Prelaunch);
            let forced = CONTROL
                .scope(prepare.clone(), admin_failure(&mut run))
                .await;
            let preparation = prepare.observed();
            janitor_entry_established = forced.is_some()
                && preparation.prepared_names.len() == 1
                && preparation.start_names.is_empty()
                && preparation.real_start_polls == 0;
        }
        if mode == Mode::CancelFirstRealPoll {
            if site == Site::Admin {
                cancel_at_real_start(&control, admin_failure(&mut run)).await;
            } else {
                cancel_at_real_start(&control, run.close()).await;
            }
            immediate_report = None;
            first_report = recovery(&run.close().await);
        } else if site == Site::Admin {
            let failure = CONTROL
                .scope(control.clone(), admin_failure(&mut run))
                .await;
            operation_failed = failure.is_some();
            immediate_report = failure.map(|failure| failure.recovery_names);
            first_report = recovery(&run.close().await);
        } else {
            let outcome = CONTROL.scope(control.clone(), run.close()).await;
            operation_failed = outcome.is_err();
            first_report = recovery(&outcome);
            immediate_report = Some(first_report.clone());
        }
        second_report = recovery(&run.close().await);
        let empty = super::Schema::default();
        let binding = super::BindingRequest {
            bootstrap: &[],
            desired: &empty,
            base: &empty,
        };
        terminal = run.check(&mut target).await.is_err()
            && run
                .qualify(&mut target, &super::ScopeRequest::default())
                .await
                .is_err()
            && run.resolve(&mut target, &binding).await.is_err();
    }
    let observed = control.observed();
    // Preserve exact-ID cleanup even when a literal mutant causes the name
    // assertion below to fail. This helper never removes a pre-existing ID.
    super::cleanup_observation(&before, &second_report);
    target.check().await.unwrap();
    assert!(
        janitor_entry_established,
        "the real failed admin path did not establish janitor cleanup ownership"
    );
    assert!(
        operation_failed,
        "{site:?}/{mode:?} did not fail at its actual caller"
    );
    assert_eq!(
        observed.prepared_names.len(),
        1,
        "the actual {site:?} preparation boundary was not reached exactly once"
    );
    let name = &observed.prepared_names[0];
    assert!(
        terminal,
        "{site:?}/{mode:?} must remain permanently refused"
    );
    let expected_starts = usize::from(mode != Mode::Prelaunch);
    assert_eq!(observed.start_names.len(), expected_starts);
    assert_eq!(
        observed.real_start_polls,
        usize::from(mode == Mode::CancelFirstRealPoll),
        "definitive prelaunch cannot poll a create-capable future"
    );
    let retains_attempt = matches!(mode, Mode::CompletedNamed | Mode::CancelFirstRealPoll);
    for (phase, report) in [
        ("first cleanup", &first_report),
        ("repeated cleanup", &second_report),
    ] {
        assert!(
            report.iter().any(|held| held == UNRELATED),
            "{site:?}/{phase} lost an unrelated obligation"
        );
        assert_eq!(
            report.contains(name),
            retains_attempt,
            "{site:?}/{mode:?}/{phase} has incorrect relay recovery: {report:?}; attempted={name}"
        );
    }
    if let Some(report) = immediate_report {
        assert!(report.iter().any(|held| held == UNRELATED));
        assert_eq!(
            report.contains(name),
            retains_attempt,
            "{site:?}/{mode:?} immediate failure has incorrect relay recovery: {report:?}; attempted={name}"
        );
    }
}

async fn all_controls(site: Site) {
    for mode in [
        Mode::Prelaunch,
        Mode::CompletedEmpty,
        Mode::CompletedNamed,
        Mode::CancelFirstRealPoll,
    ] {
        exercise(site, mode).await;
    }
}

#[tokio::test]
#[ignore = "requires the owned native Docker daemon and a pinned PostgreSQL TLS target"]
async fn admin_launch_recovery_distinguishes_absence_from_uncertainty_without_losing_other_owners()
{
    all_controls(Site::Admin).await;
}

#[tokio::test]
#[ignore = "requires the owned native Docker daemon and a pinned PostgreSQL TLS target"]
async fn scratch_launch_recovery_distinguishes_absence_from_uncertainty_without_losing_other_owners()
 {
    all_controls(Site::Scratch).await;
}

#[tokio::test]
#[ignore = "requires the owned native Docker daemon and a pinned PostgreSQL TLS target"]
async fn janitor_launch_recovery_distinguishes_absence_from_uncertainty_without_losing_other_owners()
 {
    all_controls(Site::Janitor).await;
}
