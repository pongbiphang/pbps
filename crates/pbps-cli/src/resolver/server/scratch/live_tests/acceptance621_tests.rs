//! Real reported-library differences and backend replacement (ADR-0016 14/23).
use super::*;
use pbps_db::resolver::environment::{FactStatus, Verdict};
use pbps_pg::resolver::session::{Reset, RestoreError, SessionState, reset_and_restore};
use std::process::Command;

struct LibraryTarget {
    id: Option<String>,
    connection: String,
}

impl LibraryTarget {
    async fn start(image: &str, setting: &str) -> Self {
        // local_preload_libraries searches only the plugins subdirectory.
        // Install a real shipped module there in this private target container;
        // an empty list or an unloaded spelling would not exercise the guard.
        let install = r#"mkdir -p "/usr/lib/postgresql/$PG_MAJOR/lib/plugins"
ln -s ../auto_explain.so "/usr/lib/postgresql/$PG_MAJOR/lib/plugins/auto_explain.so"
exec /usr/local/bin/docker-entrypoint.sh postgres -c "$1""#;
        let name = format!("pbps-2487-libraries-{:032x}", rand::random::<u128>());
        let started = Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &name,
                "--label",
                "pbps.test.issue=2487",
                "-e",
                "POSTGRES_PASSWORD=Pbps!Test12345",
                "-e",
                "POSTGRES_DB=pbps_test",
                "-p",
                "127.0.0.1::5432",
                "--entrypoint",
                "/bin/sh",
                image,
                "-ec",
                install,
                "library-target",
                &format!("{setting}=auto_explain"),
            ])
            .output()
            .expect("Docker starts the private target");
        assert!(started.status.success(), "docker run: {started:?}");
        let id = String::from_utf8(started.stdout).unwrap().trim().to_owned();
        assert_eq!(id.len(), 64, "Docker returned a container id");
        let mut owned = Self {
            id: Some(id),
            connection: String::new(),
        };
        let endpoint = Command::new("docker")
            .args(["port", owned.id.as_ref().unwrap(), "5432/tcp"])
            .output()
            .unwrap();
        assert!(endpoint.status.success(), "docker port: {endpoint:?}");
        let endpoint = String::from_utf8(endpoint.stdout).unwrap();
        let port: u16 = endpoint
            .trim()
            .strip_prefix("127.0.0.1:")
            .expect("the test publishes only loopback")
            .parse()
            .unwrap();
        owned.connection = format!(
            "host=127.0.0.1 port={port} user=postgres password=Pbps!Test12345 dbname=pbps_test sslmode=disable"
        );
        for _ in 0..120 {
            if let Ok(mut conn) = Conn::connect(Driver::Postgres, &owned.connection).await {
                let value = conn
                    .query(&format!("SELECT current_setting('{setting}') AS library"))
                    .await
                    .unwrap();
                assert_eq!(
                    value[0].try_get::<&str>("library").unwrap(),
                    Some("auto_explain"),
                    "the differing setting was loaded by the real server"
                );
                return owned;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let logs = Command::new("docker")
            .args(["logs", owned.id.as_ref().unwrap()])
            .output()
            .unwrap();
        panic!("private library target did not become ready: {logs:?}");
    }

    fn fixture(&self) -> Fixture {
        Fixture {
            server: self.connection.clone(),
            token: format!("{:016x}", rand::random::<u64>()),
            databases: Vec::new(),
            roles: Vec::new(),
        }
    }

    fn remove(mut self) {
        let removed = Command::new("docker")
            .args(["rm", "-fv", self.id.as_ref().unwrap()])
            .output()
            .unwrap();
        assert!(
            removed.status.success(),
            "private target cleanup: {removed:?}"
        );
        self.id = None;
    }
}

impl Drop for LibraryTarget {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            // Panic cleanup addresses the recorded id, never a reused name.
            let _ = Command::new("docker").args(["rm", "-fv", id]).output();
        }
    }
}

fn incompatible(result: &Result<ResolvedPlan, ProduceError>, setting: &str) -> bool {
    matches!(result, Err(ProduceError::Run(Error::Incompatible(_))))
        && result.as_ref().err().unwrap().to_string().contains(setting)
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and Docker images"]
async fn resolver_refuses_a_scratch_whose_preloaded_libraries_differ_from_the_target() {
    for &(server, image_var) in DOCKER_PAIRS {
        let image = setting(image_var);
        let key = ProjectKey::new(true);
        let inputs = Inputs::overload();
        // Matching empty values pass the full producer on both scratch kinds.
        let mut control = Fixture::new(server);
        let target_db = control.target().await;
        let (login, scratch_db) = control.confined().await;
        let native = produce(
            &control.as_login(&scratch_db, &login),
            &control.on(&target_db),
            &inputs,
            &key,
        )
        .await;
        let docker = produce_in_docker(
            &image,
            pbps_config::resolver::PullPolicy::Never,
            &control.on(&target_db),
            None,
            &inputs,
            &key,
            None,
            pbps_model::resolver::OutsideWrites::Refused,
        )
        .await;
        control.drop().await;
        assert_answers_the_overload(&native.unwrap());
        assert_answers_the_overload_from(
            &docker.unwrap(),
            pbps_model::resolver::ScratchSource::Docker {
                image: image.clone().into(),
            },
        );
        for library in [
            "shared_preload_libraries",
            "session_preload_libraries",
            "local_preload_libraries",
        ] {
            let owned = LibraryTarget::start(&image, library).await;
            let mut target = owned.fixture();
            let target_db = target.target().await;
            let mut scratch = Fixture::new(server);
            let (login, scratch_db) = scratch.confined().await;
            let before = scratch.inventory().await;
            let native = produce(
                &scratch.as_login(&scratch_db, &login),
                &target.on(&target_db),
                &inputs,
                &key,
            )
            .await;
            let after = scratch.inventory().await;
            let empty = scratch.foreign_objects(&scratch_db).await;
            let containers = scratch_containers();
            let docker = produce_in_docker(
                &image,
                pbps_config::resolver::PullPolicy::Never,
                &target.on(&target_db),
                None,
                &inputs,
                &key,
                None,
                pbps_model::resolver::OutsideWrites::Refused,
            )
            .await;
            let containers_after = scratch_containers();
            scratch.drop().await;
            target.drop().await;
            owned.remove();
            assert!(
                incompatible(&native, library),
                "{server}/{library}/server: {:?}",
                native.err()
            );
            assert!(
                incompatible(&docker, library),
                "{server}/{library}/docker: {:?}",
                docker.err()
            );
            assert_eq!(before, after, "refusal left no run-owned names");
            assert_eq!(empty.1, 0, "refusal left no supplied objects: {empty:?}");
            assert_eq!(
                containers, containers_after,
                "the refused Docker scratch was removed"
            );
        }
    }
}

async fn replacement(connection: &str) {
    use pbps_pg::resolver::environment::{Scope, read};
    let mut checked = Conn::connect(Driver::Postgres, connection).await.unwrap();
    checked
        .execute("CREATE SCHEMA pbps_acceptance621")
        .await
        .unwrap();
    let schemas = ["pbps_acceptance621".to_owned()];
    let scope = Scope {
        schemas: &schemas,
        write_path_extras: &[],
    };
    let original = read(&mut checked, &scope).await.unwrap();
    let backend = pbps_pg::resolver::scratch::backend(&mut checked)
        .await
        .unwrap();
    let state = SessionState::checked(backend, None);
    let mut other = Conn::connect(Driver::Postgres, connection).await.unwrap();
    let fresh = read(&mut other, &scope).await.unwrap();
    assert_eq!(
        pbps_pg::resolver::compatibility::compare_reported(&original, &fresh).verdict(),
        Verdict::Verified,
        "a fresh connection can qualify matching facts"
    );
    // A real effective setting differs on the replacement, not a fabricated fact.
    other
        .execute("SET default_text_search_config = 'pg_catalog.simple'")
        .await
        .unwrap();
    let changed = read(&mut other, &scope).await.unwrap();
    let report = pbps_pg::resolver::compatibility::compare_reported(&original, &changed);
    assert!(
        matches!(
            report.facts.get("setting:default_text_search_config"),
            Some(FactStatus::Mismatch { .. })
        ),
        "{report:?}"
    );
    let moved = reset_and_restore(&mut other, &state, Reset::All).await;
    assert!(
        matches!(moved, Err(RestoreError::MovedBackend)),
        "{moved:?}"
    );
    reset_and_restore(&mut checked, &state, Reset::All)
        .await
        .unwrap();
    // Even equal facts cannot reuse the old backend's registered continuation.
    let moved = reset_and_restore(&mut other, &state, Reset::All).await;
    assert!(
        matches!(moved, Err(RestoreError::MovedBackend)),
        "{moved:?}"
    );
    let backend = pbps_pg::resolver::scratch::backend(&mut other)
        .await
        .unwrap();
    let restarted = SessionState::checked(backend, None);
    reset_and_restore(&mut other, &restarted, Reset::All)
        .await
        .unwrap();
    let fresh = read(&mut other, &scope).await.unwrap();
    assert_eq!(
        pbps_pg::resolver::compatibility::compare_reported(&original, &fresh).verdict(),
        Verdict::Verified,
        "only fresh state may use the replacement"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and Docker images"]
async fn resolver_replacement_backend_invalidates_the_registered_session() {
    for &(server, image_var) in DOCKER_PAIRS {
        let mut fixture = Fixture::new(server);
        let database = fixture.database("backend621", None).await;
        let target = fixture.on(&database);
        let connection = target.clone();
        // A failing guard witness must still release its fixture. Collect a
        // child panic before asserting, as the producer fixtures do for errors.
        let native = tokio::spawn(async move { replacement(&connection).await }).await;
        let identity = identity_of(&target).await;
        let image = setting(image_var);
        let containers = scratch_containers();
        let docker = super::super::container::with_container(
            Driver::Postgres,
            &target,
            &identity,
            &image,
            pbps_config::resolver::PullPolicy::Never,
            async |connection: &str| {
                let connection = connection.to_owned();
                Ok(tokio::spawn(async move { replacement(&connection).await }).await)
            },
        )
        .await;
        let after = scratch_containers();
        fixture.drop().await;
        native.expect("server backend witness failed after fixture cleanup");
        docker
            .expect("Docker acquisition and cleanup")
            .expect("Docker backend witness");
        assert_eq!(
            containers, after,
            "replacement fixture removed its Docker scratch"
        );
    }
}
