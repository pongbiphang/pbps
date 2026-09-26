//! Fail real qualification and cleanup only within this test's observer.

use super::*;
use rustix::net::{Shutdown, shutdown, sockopt::socket_peercred};
use rustix::process::{
    Pid, PidfdFlags, PidfdGetfdFlags, Resource, Rlimit, pidfd_getfd, pidfd_open, prlimit,
};
use std::cell::RefCell;
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

tokio::task_local! {
    static FAULT: Rc<RefCell<Fault>>;
}

struct Fault {
    daemon: PathBuf,
    docker_binary: PathBuf,
    cut_api: bool,
    late_admission: bool,
    skip_sessions: usize,
    name: Option<String>,
    id: Option<String>,
    guard: Option<ProcessLease>,
    limit: Option<Rlimit>,
    mask: Option<PathBuf>,
}

impl Fault {
    fn new(daemon: PathBuf, cut_api: bool, late_admission: bool) -> Self {
        Self {
            daemon,
            docker_binary: PathBuf::from(std::env::var("PBPS_ADMISSION_RECOVERY_DOCKER").unwrap()),
            cut_api,
            late_admission,
            skip_sessions: 0,
            name: None,
            id: None,
            guard: None,
            limit: None,
            mask: None,
        }
    }

    fn docker(&self, args: &[&str]) -> std::io::Result<std::process::Output> {
        assert!(self.docker_binary.is_absolute());
        Command::new(&self.docker_binary)
            .args(["--host", &format!("unix://{}", self.daemon.display())])
            .args(args)
            .output()
    }

    fn apply(&mut self, forwarder: &Forwarder) {
        assert!(
            self.name.is_none(),
            "one explicitly scoped session is changed"
        );
        self.name = Some(forwarder.resource_name().to_owned());
        self.id = Some(forwarder.container_id().to_owned());
        let guard = ProcessLease::capture(forwarder.pid().unwrap()).unwrap();
        let pid = Pid::from_raw(guard.observer_pid().unwrap().try_into().unwrap()).unwrap();
        let original = prlimit(
            Some(pid),
            Resource::Nofile,
            Rlimit {
                current: Some(1024),
                maximum: Some(2048),
            },
        )
        .unwrap();
        assert_eq!(original.current, Some(1024));
        assert_eq!(original.maximum, Some(1024));
        self.limit = Some(original);
        self.guard = Some(guard);
        if !self.cut_api {
            return;
        }

        // A paused owned forwarder cannot auto-remove itself when its attach
        // stream closes. This makes the unconfirmed resource observable.
        assert!(
            self.docker(&["pause", self.id.as_ref().unwrap()])
                .unwrap()
                .status
                .success()
        );
        let probe = UnixStream::connect(&self.daemon).unwrap();
        let peer = socket_peercred(&probe).unwrap();
        assert_eq!(peer.uid.as_raw(), 0);
        assert!(
            std::fs::metadata(&self.daemon)
                .unwrap()
                .file_type()
                .is_socket()
        );
        let empty = std::env::temp_dir().join(format!(
            "pbps-unavailable-api-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&empty)
            .unwrap();
        let mounted = Command::new("/usr/bin/mount")
            .arg("--bind")
            .arg(&empty)
            .arg(&self.daemon)
            .status()
            .unwrap()
            .success();
        if !mounted {
            std::fs::remove_file(&empty).unwrap();
        }
        assert!(mounted);
        self.mask = Some(empty);
        assert!(std::fs::metadata(&self.daemon).unwrap().is_file());

        // Break only this test process's Unix connections to the selected
        // daemon. The daemon, other clients and host socket remain intact.
        // Hiding the path also prevents the cleanup-only reconnect, which
        // otherwise would correctly recover a broken first connection.
        let descriptors: Vec<i32> = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_str()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect();
        let own = pidfd_open(
            Pid::from_raw(std::process::id().try_into().unwrap()).unwrap(),
            PidfdFlags::empty(),
        )
        .unwrap();
        let mut cut = 0;
        for raw in descriptors {
            let fd = match pidfd_getfd(&own, raw, PidfdGetfdFlags::empty()) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::BADF) => continue,
                Err(error) => panic!("cannot duplicate this observer's descriptor: {error}"),
            };
            if socket_peercred(&fd).ok() == Some(peer) {
                let result = shutdown(&fd, Shutdown::Both);
                assert!(result.is_ok() || result == Err(rustix::io::Errno::NOTCONN));
                cut += 1;
            }
        }
        assert!(cut >= 2, "the real API and attach connections were cut");
    }

    fn restore(&mut self) -> std::io::Result<()> {
        if let Some(empty) = self.mask.as_ref() {
            let restored = Command::new("/usr/bin/umount").arg(&self.daemon).status()?;
            if !restored.success() {
                return Err(std::io::Error::other(
                    "cannot restore the private socket view",
                ));
            }
            std::fs::remove_file(empty)?;
            self.mask = None;
        }
        if let Some(limit) = self.limit.take()
            && let Some(pid) = self
                .guard
                .as_ref()
                .and_then(|guard| guard.check().ok().and_then(|()| guard.observer_pid().ok()))
        {
            match prlimit(
                Some(Pid::from_raw(pid.try_into().unwrap()).unwrap()),
                Resource::Nofile,
                limit,
            ) {
                Ok(_) | Err(rustix::io::Errno::SRCH) => (),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn remove_owned(&mut self) {
        if let Some(id) = self.id.as_ref() {
            // Delete only the immutable ID supplied by the live run under test.
            let _ = self.docker(&["unpause", id]);
            let _ = self.docker(&["rm", "--force", "--volumes", id]);
        }
    }
}

impl Drop for Fault {
    fn drop(&mut self) {
        let _ = self.restore();
        self.remove_owned();
    }
}

pub(in crate::resolver::server) fn after_open(forwarder: &Forwarder) {
    let _ = FAULT.try_with(|fault| {
        let mut fault = fault.borrow_mut();
        if !fault.late_admission {
            if fault.skip_sessions > 0 {
                fault.skip_sessions -= 1;
            } else {
                fault.apply(forwarder);
            }
        }
    });
}

pub(in crate::resolver::server) fn after_admission(forwarder: &Forwarder) {
    let _ = FAULT.try_with(|fault| {
        let mut fault = fault.borrow_mut();
        if fault.late_admission {
            fault.apply(forwarder);
        }
    });
}

#[tokio::test]
#[ignore = "requires task-owned dedicated servers and a private observer mount namespace"]
async fn failed_admission_names_every_unconfirmed_forwarder() {
    fixture();
    assert_eq!(
        std::env::var("PBPS_ADMISSION_RECOVERY_FIXTURE").as_deref(),
        Ok("1")
    );
    assert_ne!(
        std::fs::read_link("/proc/self/ns/mnt").unwrap(),
        std::fs::read_link("/proc/1/ns/mnt").unwrap()
    );
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let marker = std::env::var("PBPS_SERVER_MARKER_DATABASE").unwrap();
    let mut target = native_target().await;
    let mut missing = Vec::new();
    for stage in [
        "admission",
        "final admission",
        "scratch",
        "reconstruction",
        "reopened",
    ] {
        let scratch = stage == "scratch";
        let late_admission = stage == "final admission";
        let qualify = matches!(stage, "reconstruction" | "reopened");
        for cut_api in [false, true] {
            let fault = Rc::new(RefCell::new(Fault::new(
                configured.daemon.clone(),
                cut_api,
                late_admission,
            )));
            fault.borrow_mut().skip_sessions = usize::from(stage == "reopened");
            let mut server = if scratch || qualify {
                Some(admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await)
            } else {
                None
            };
            let mut run = if qualify {
                Some(
                    server
                        .as_mut()
                        .unwrap()
                        .open_scratch(&scratch_recipe(&mut target).await)
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            let mut reported = if let Some(run) = run.as_mut() {
                let request = ScopeRequest::default();
                failure(
                    FAULT
                        .scope(fault.clone(), run.qualify(&mut target, &request))
                        .await
                        .expect_err("actual guard-limit loss refuses a qualification session"),
                )
            } else if let Some(server) = server.as_mut() {
                let recipe = scratch_recipe(&mut target).await;
                FAULT
                    .scope(fault.clone(), server.open_scratch(&recipe))
                    .await
                    .err()
                    .expect("actual guard-limit loss refuses the scratch session")
            } else {
                FAULT
                    .scope(
                        fault.clone(),
                        DedicatedServer::admit(endpoint("PBPS_SERVER_ENDPOINT"), &mut target),
                    )
                    .await
                    .err()
                    .expect("actual guard-limit loss refuses admission")
            };
            let (name, id) = {
                let mut state = fault.borrow_mut();
                state.restore().unwrap();
                (state.name.clone().unwrap(), state.id.clone().unwrap())
            };
            let mut api = LocalApi::connect_native(&configured.daemon).await.unwrap();
            // The baseline admission only requests background removal. Let
            // that normal cleanup settle before measuring a real leftover.
            let mut leftover = api.inspect_container(&id).await.unwrap();
            if !cut_api {
                for _ in 0..100 {
                    if leftover.is_none() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    leftover = api.inspect_container(&id).await.unwrap();
                }
            }
            if let Some(state) = &leftover {
                assert_eq!(state["Id"], id);
                assert_eq!(state["Name"], format!("/{name}"));
            }
            fault.borrow_mut().remove_owned();
            let mut gone = false;
            for _ in 0..100 {
                if api.inspect_container(&id).await.unwrap().is_none() {
                    gone = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(gone, "the exact owned forwarder was removed");
            fault.borrow_mut().id = None;
            if let Some(run) = run.as_mut() {
                let cleanup = run.close().await;
                if let Err(removal) = cleanup {
                    reported.recovery_names = removal.recovery_names;
                }
                let retried = run.close().await;
                if retried
                    .err()
                    .is_some_and(|error| error.recovery_names.contains(&name))
                    != cut_api
                {
                    missing.push((stage, cut_api));
                }
            } else if let Some(server) = server.as_mut() {
                for _ in 0..2 {
                    let discarded = server.discard().await;
                    if discarded
                        .err()
                        .is_some_and(|error| error.recovery_names.contains(&name))
                        != cut_api
                    {
                        missing.push((stage, cut_api));
                    }
                }
            }
            drop(run);
            drop(server);
            let mut admin = session(&configured, maintenance()).await;
            assert_eq!(
                exists(&mut admin.connection, "pbps_scratch_").await,
                (false, false)
            );
            assert_eq!(
                exists(&mut admin.connection, "pbps_run_").await,
                (false, false)
            );
            assert!(exists(&mut admin.connection, &marker).await.0);
            admin.close().await;
            assert_eq!(leftover.is_some(), cut_api);
            assert!(!format!("{reported:?}").contains(&configured.password));
            let named = reported.recovery_names.contains(&name);
            assert_eq!(
                reported
                    .recovery_names
                    .iter()
                    .filter(|item| *item == &name)
                    .count(),
                usize::from(named),
                "a forwarder is reported once"
            );
            if !cut_api {
                assert!(reported.recovery_names.is_empty());
            }
            assert!(
                matches!(&reported.cause, Error::Channel(reason) if reason.contains("descriptor limits"))
                    || matches!(&reported.cause, Error::Daemon(_) if late_admission && cut_api)
                    || matches!(&reported.cause, Error::Cleanup if scratch && cut_api),
                "the original refusal or failed SQL cleanup must survive: {}",
                reported.cause
            );
            eprintln!(
                "admission recovery stage={stage} api_lost={cut_api}: actual_leftover={}, returned_error_names_forwarder={named}, owned_cleanup=confirmed",
                leftover.is_some()
            );
            if named != cut_api {
                missing.push((stage, cut_api));
            }
        }
    }
    target.check().await.unwrap();
    assert!(
        missing.is_empty(),
        "unconfirmed forwarders missing from returned errors: {missing:?}"
    );
}

// A step held at its administrative session, with that session's forwarder
// paused and this observer's daemon connections cut, so that nothing can
// remove the forwarder unless cleanup still holds it (#1031).
tokio::task_local! {
    static HOLD: Rc<RefCell<Fault>>;
}

pub(in crate::resolver::server) fn after_admin_open(forwarder: &Forwarder) {
    let _ = HOLD.try_with(|fault| fault.borrow_mut().apply(forwarder));
}

pub(in crate::resolver::server) async fn hold() {
    if HOLD.try_with(|_| ()).is_ok() {
        std::future::pending::<()>().await;
    }
}

#[tokio::test]
#[ignore = "requires task-owned dedicated servers and a private observer mount namespace"]
async fn a_cancelled_step_leaves_its_admin_session_to_cleanup() {
    fixture();
    assert_eq!(
        std::env::var("PBPS_ADMISSION_RECOVERY_FIXTURE").as_deref(),
        Ok("1")
    );
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let mut target = native_target().await;
    let mut missing = Vec::new();
    // A retry takes the held session itself, so a direct close after the
    // cancellation is a case of its own: there cleanup must find the
    // session in the run's state.
    for (stage, retry) in [("qualify", true), ("resolve", true), ("qualify", false)] {
        // Resolution has a PostgreSQL adapter only; SQL Server refuses it
        // before any administrative session opens.
        if stage == "resolve" && driver() != Driver::Postgres {
            continue;
        }
        let mut run = open_when_exclusive(&mut target).await;
        let request = ScopeRequest::default();
        let empty = pbps_model::Schema::default();
        let binding = BindingRequest {
            bootstrap: &[],
            desired: &empty,
            base: &empty,
        };
        if stage == "resolve" {
            run.qualify(&mut target, &request).await.unwrap();
        }
        let fault = Rc::new(RefCell::new(Fault::new(
            configured.daemon.clone(),
            true,
            false,
        )));
        let held = HOLD
            .scope(fault.clone(), async {
                let step = async {
                    match stage {
                        "qualify" => run.qualify(&mut target, &request).await.map(|_| ()),
                        _ => run.resolve(&mut target, &binding).await.map(|_| ()),
                    }
                };
                tokio::time::timeout(std::time::Duration::from_secs(60), step).await
            })
            .await;
        assert!(
            held.is_err(),
            "{stage}: the step was held at its admin session"
        );
        let (name, id) = {
            let mut state = fault.borrow_mut();
            state.restore().unwrap();
            (state.name.clone().unwrap(), state.id.clone().unwrap())
        };
        if retry {
            // With the daemon reachable again, retrying the same step would
            // open a new administrative session in place of the held one; qualify
            // reaches it past every check. The retry must end the run instead.
            // A retry naming another principal would record other run-local
            // roles; the refused retry must leave the cleanup list as it was.
            let roles = run.inner.control.roles.clone();
            let other = ScopeRequest {
                planned: vec![PlannedGrant {
                    principal: "pbps_retry_1031".into(),
                    schema: if driver() == Driver::Postgres {
                        "public".into()
                    } else {
                        "dbo".into()
                    },
                    privilege: "USAGE".into(),
                    revoke: false,
                }],
                ..ScopeRequest::default()
            };
            let retried = match stage {
                "qualify" => run.qualify(&mut target, &other).await.map(|_| ()),
                _ => run.resolve(&mut target, &binding).await.map(|_| ()),
            };
            assert_eq!(
                run.inner.control.roles, roles,
                "{stage}: a refused retry kept the recorded run-local roles"
            );
            assert!(
                matches!(retried, Err(Error::Cancelled)),
                "{stage}: a retry after cancellation ends the run: {retried:?}"
            );
            // The first retry took the held session; a second finds none held
            // and must still meet the recorded refusal, not a later guard.
            let again = match stage {
                "qualify" => run.qualify(&mut target, &other).await.map(|_| ()),
                _ => run.resolve(&mut target, &binding).await.map(|_| ()),
            };
            assert!(
                matches!(again, Err(Error::Cancelled)),
                "{stage}: every later retry keeps the recorded cancellation: {again:?}"
            );
        }
        let closed = run.close().await;
        let named = closed
            .as_ref()
            .err()
            .is_some_and(|error| error.recovery_names.contains(&name));
        let mut api = LocalApi::connect_native(&configured.daemon).await.unwrap();
        let leftover = api.inspect_container(&id).await.unwrap().is_some();
        eprintln!(
            "cancelled {stage} retry={retry}: close_names_admin_forwarder={named} leftover={leftover}"
        );
        // Cleanup either confirmed the forwarder gone or said it could not.
        // Silence with the container still there is the defect.
        if leftover && !named {
            missing.push((stage, retry));
        }
        fault.borrow_mut().remove_owned();
        let mut gone = false;
        for _ in 0..100 {
            if api.inspect_container(&id).await.unwrap().is_none() {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(gone, "the exact owned forwarder was removed");
        fault.borrow_mut().id = None;
        let _ = run.close().await;
        drop(run);
        let mut admin = session(&configured, maintenance()).await;
        assert_eq!(
            exists(&mut admin.connection, "pbps_scratch_").await,
            (false, false)
        );
        assert_eq!(
            exists(&mut admin.connection, "pbps_run_").await,
            (false, false)
        );
        admin.close().await;
    }
    target.check().await.unwrap();
    assert!(
        missing.is_empty(),
        "a cancelled step's admin forwarder was neither removed nor named: {missing:?}"
    );
}
