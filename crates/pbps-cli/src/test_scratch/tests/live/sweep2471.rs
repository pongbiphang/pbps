//! A resolver sweep must not destroy a pending test run's recovery evidence.

use super::*;
use pbps_cli::resolver::server::scratch::container::{Held, journal};

#[derive(Default)]
struct Made {
    containers: Vec<String>,
    volumes: Vec<String>,
}

impl Drop for Made {
    fn drop(&mut self) {
        for id in &self.containers {
            let _ = std::process::Command::new("docker")
                .args(["rm", "--force", "--volumes", id])
                .output();
        }
        for volume in &self.volumes {
            let _ = std::process::Command::new("docker")
                .args(["volume", "rm", volume])
                .output();
        }
    }
}

impl Made {
    async fn create(&mut self, args: &[&str]) -> String {
        let id = docker(args).await.unwrap().trim().to_owned();
        self.containers.push(id.clone());
        id
    }

    async fn candidate(
        &mut self,
        run: &str,
        driver: Driver,
        image: &str,
        test: bool,
        location: Option<&str>,
    ) -> (String, Vec<String>) {
        let name = format!("pbps-scratch-{run}");
        let token = format!("pbps.resolver.token={run}");
        let engine = format!(
            "pbps.resolver.engine={}",
            match driver {
                Driver::Postgres => "postgres",
                Driver::Mssql => "mssql",
            }
        );
        let mut args = vec![
            "create",
            "--pull=never",
            "--name",
            &name,
            "--label",
            "pbps.resolver=scratch",
            "--label",
            &token,
            "--label",
            &engine,
            "--volume",
            "/pbps-issue-2471",
            "--entrypoint",
            "/bin/true",
        ];
        let run_label = format!("{RUN_LABEL}={run}");
        let location_label = format!(
            "{}={}",
            journal::LOCATION_LABEL,
            location.unwrap_or_default()
        );
        if test {
            args.extend(["--label", &run_label]);
            if location.is_some() {
                args.extend(["--label", &location_label]);
            }
        }
        args.push(image);
        let id = self.create(&args).await;
        let found = Docker.inspect(&id).await.unwrap().unwrap();
        assert!(
            !found.volumes.is_empty(),
            "real anonymous volumes on both engines"
        );
        self.volumes.extend(found.volumes.clone());
        // Exited candidates need no age or keeper assumption to be swept.
        docker(&["start", "--attach", &id]).await.unwrap();
        (id, found.volumes)
    }
}

async fn volumes_gone(volumes: &[String]) {
    for volume in volumes {
        let why = docker(&["volume", "inspect", volume]).await.unwrap_err();
        assert!(
            pbps_cli::resolver::server::scratch::container::no_such_volume(&why),
            "{why}"
        );
    }
}

pub(super) async fn sweep_before_recovery(
    d: &Dir,
    alive: &TestScratch<'_>,
    driver: Driver,
    image: &str,
) {
    // The sweeping invocation uses a different registry: its default directory
    // cannot tell whether this pending run still has its journal.
    let registry = StateDir::open(d.base.join("sweep-runs")).unwrap();
    let location = journal::location(registry.path()).unwrap();
    let live_bytes = std::fs::read(d.dir.file(alive.run_id(), "json")).unwrap();
    for blocked in [true, false] {
        let mut made = Made::default();
        let run = record::new_run_id();
        let mut pending = record(&run, State::Pending, None);
        pending.volumes.clear();
        pending.daemon = alive.journal.record.daemon.clone();
        registry.write(&pending).unwrap();
        let (id, volumes) = made
            .candidate(&run, driver, image, true, Some(&location))
            .await;
        let blocker = if blocked {
            // A second container's reference makes volume deletion really fail
            // if a resolver removes the pending container before test recovery.
            let mount = format!("{}:/held", volumes[0]);
            Some(
                made.create(&[
                    "create",
                    "--pull=never",
                    "--volume",
                    &mount,
                    "--entrypoint",
                    "/bin/true",
                    image,
                ])
                .await,
            )
        } else {
            None
        };
        let (ordinary, ordinary_volumes) = made
            .candidate(&record::new_run_id(), driver, image, false, None)
            .await;
        let (residual, residual_volumes) = made
            .candidate(&record::new_run_id(), driver, image, true, Some(&location))
            .await;
        let (unknown, _) = made
            .candidate(&record::new_run_id(), driver, image, true, None)
            .await;

        // This ordinary resolver acquisition runs the production sweep.
        // The SQL Server startup future is large; keep it off the test thread's
        // stack as the surrounding lifecycle helpers do.
        let resolver = Box::pin(Held::start(driver, image, PullPolicy::Never))
            .await
            .unwrap();
        resolver.remove().await.unwrap();
        let preserved = exists(&id).await;
        assert!(!exists(&ordinary).await && !exists(&residual).await);
        volumes_gone(&ordinary_volumes).await;
        volumes_gone(&residual_volumes).await;
        assert!(
            exists(&unknown).await,
            "a test run without a location is unknown"
        );
        assert_eq!(registry.read(&run).unwrap(), Some(pending));

        let done = reconcile(&registry, &Docker, &alive.journal.record.daemon, soon())
            .await
            .unwrap();
        let done = if let Some(blocker) = blocker {
            assert!(
                done.removed.is_empty() && done.failures.len() == 1,
                "a volume still used by another container must not be reported cleaned: {done:?}"
            );
            assert_eq!(done.failures[0].kind, FailureKind::Conflict);
            assert!(registry.read(&run).unwrap().is_some());
            assert!(exists(&id).await);
            docker(&["volume", "inspect", &volumes[0]]).await.unwrap();
            docker(&["rm", "--force", "--volumes", &blocker])
                .await
                .unwrap();
            reconcile(&registry, &Docker, &alive.journal.record.daemon, soon())
                .await
                .unwrap()
        } else {
            done
        };
        assert!(
            preserved,
            "the resolver left the pending container to test recovery"
        );
        assert_eq!(done.removed.as_slice(), std::slice::from_ref(&run));
        assert!(done.failures.is_empty() && done.live.is_empty(), "{done:?}");
        assert!(registry.read(&run).unwrap().is_none());
        assert!(!exists(&id).await);
        volumes_gone(&volumes).await;
        assert!(exists(alive.journal.record.container.id.as_deref().unwrap()).await);
        assert_eq!(
            std::fs::read(d.dir.file(alive.run_id(), "json")).unwrap(),
            live_bytes
        );
    }
}
