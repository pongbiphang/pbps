//! Cancellation is observed inside a live server, before any recovery sweep.
use super::*;
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(20);

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn marker() -> String {
    format!("cancel1519_{:016x}", rand::random::<u64>())
}

async fn request(
    scratch: String,
    target: String,
    sql: String,
) -> Result<ResolvedPlan, ProduceError> {
    // These inputs are dropped with the caller. The independent owner
    // cannot borrow either the declarations or the key file for cleanup.
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    super::super::AFTER_STAGING
        .scope(Some(sql), produce(&scratch, &target, &inputs, &key))
        .await
}

async fn barrier(fixture: &Fixture, database: &str, marker: &str) -> i64 {
    let mut admin = fixture.admin().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let rows = admin
                .query(&format!(
                    "SELECT pid::bigint AS pid FROM pg_catalog.pg_stat_activity \
                 WHERE datname = {} AND query LIKE {} AND state = 'active' \
                 AND wait_event_type = 'Lock' AND pid <> pg_backend_pid()",
                    literal(database),
                    literal(&format!("%/* {marker} */%")),
                ))
                .await
                .unwrap();
            if let [row] = rows.as_slice() {
                return row.try_get::<i64>("pid").unwrap().unwrap();
            }
            assert!(
                rows.len() <= 1,
                "the cancellation marker must identify one owned query"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the real producer did not reach its post-acquisition barrier")
}

struct Gate {
    connection: Conn,
    key: i64,
    tag: String,
}

impl Gate {
    async fn new(fixture: &Fixture, database: &str) -> Self {
        let mut connection = Conn::connect(Driver::Postgres, &fixture.on(database))
            .await
            .unwrap();
        let key = rand::random::<i64>();
        connection
            .execute(&format!("SELECT pg_advisory_lock(({key})::bigint)"))
            .await
            .unwrap();
        Self {
            connection,
            key,
            tag: marker(),
        }
    }

    fn sql(&self) -> String {
        format!(
            "SELECT pg_advisory_xact_lock(({})::bigint) /* {} */",
            self.key, self.tag
        )
    }

    async fn release(mut self) {
        let rows = self
            .connection
            .query(&format!(
                "SELECT pg_advisory_unlock(({})::bigint) AS unlocked",
                self.key,
            ))
            .await
            .unwrap();
        assert_eq!(rows[0].try_get::<bool>("unlocked").unwrap(), Some(true));
    }
}

struct Owned {
    database: String,
    roles: Vec<pbps_model::RoleName>,
    token: String,
}

impl Owned {
    async fn observe(fixture: &Fixture, before: &(Vec<String>, Vec<String>)) -> Self {
        let after = fixture.inventory().await;
        let databases: Vec<_> = after
            .0
            .iter()
            .filter(|name| !before.0.contains(name))
            .collect();
        assert_eq!(
            databases.len(),
            1,
            "a real run-owned database must exist before cancellation"
        );
        let database = databases[0].clone();
        let token = database.strip_prefix("pbps_scratch_").unwrap().to_owned();
        assert_eq!(token.len(), 32);
        let roles: Vec<pbps_model::RoleName> = after
            .1
            .iter()
            .filter(|name| !before.1.contains(name) && name.ends_with(&token))
            .map(|name| name.as_str().into())
            .collect();
        assert!(
            roles
                .iter()
                .any(|name| name.raw() == format!("pbps_run_{token}"))
        );
        assert!(
            roles.len() > 1,
            "run-local authorization roles must also exist"
        );
        Self {
            database,
            roles,
            token,
        }
    }

    async fn remove(&self, fixture: &Fixture) {
        let mut admin = fixture.admin().await;
        // Cleanup is limited to exact names independently observed before the
        // cancellation. This also keeps a failing counterfactual isolated.
        pbps_pg::resolver::drop_database(&mut admin, &self.database, true)
            .await
            .unwrap();
        let remaining = pbps_pg::resolver::drop_roles(&mut admin, &self.roles).await;
        assert!(
            remaining.is_empty(),
            "owned fixture roles could not be removed: {remaining:?}"
        );
    }

    async fn terminate_keeper(&self, fixture: &Fixture) {
        let mut admin = fixture.admin().await;
        let rows = admin
            .query(&format!(
                "SELECT pid::bigint AS pid FROM pg_catalog.pg_stat_activity \
             WHERE application_name = {} AND pid <> pg_backend_pid()",
                literal(&format!("pbps-{}", self.token)),
            ))
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "one checked admin keeper belongs to this run"
        );
        let pid = rows[0].try_get::<i64>("pid").unwrap().unwrap();
        let stopped = admin
            .query(&format!(
                "SELECT pg_catalog.pg_terminate_backend({pid}) AS stopped"
            ))
            .await
            .unwrap();
        assert_eq!(stopped[0].try_get::<bool>("stopped").unwrap(), Some(true));
    }
}

async fn inventory_returns(fixture: &Fixture, before: &(Vec<String>, Vec<String>)) -> bool {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if &fixture.inventory().await == before {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL target and separate scratch server"]
async fn an_aborted_run_owned_request_releases_its_login_and_roles_in_the_live_server() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let database = target.target().await;
    let before = scratch.inventory().await;
    let gate = Gate::new(&target, &database).await;
    let task = tokio::spawn(request(
        scratch.server.clone(),
        target.on(&database),
        gate.sql(),
    ));
    let _pid = barrier(&target, &database, &gate.tag).await;
    let owned = Owned::observe(&scratch, &before).await;
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    // Runtime and server remain alive. No second acquisition can hide a
    // missing release by invoking #2410's recovery sweep.
    gate.release().await;
    let released = inventory_returns(&scratch, &before).await;
    owned.remove(&scratch).await;
    let after = scratch.inventory().await;
    target.drop().await;
    assert!(
        released,
        "cancelling the caller lost prompt run-owned release"
    );
    assert_eq!(before, after);
}

async fn supplied_state(fixture: &Fixture, database: &str, login: &str) -> String {
    let mut admin = Conn::connect(Driver::Postgres, &fixture.on(database))
        .await
        .unwrap();
    let rows = admin.query(&format!(
        "SELECT jsonb_build_object(\
         'database', (SELECT jsonb_build_array(datacl,datallowconn,datistemplate) \
                      FROM pg_catalog.pg_database WHERE datname=current_database()), \
         'public', (SELECT jsonb_build_array(nspowner,nspacl) FROM pg_catalog.pg_namespace WHERE nspname='public'), \
         'defaults', (SELECT jsonb_agg(to_jsonb(s) ORDER BY setdatabase,setrole) FROM pg_catalog.pg_db_role_setting s \
                      WHERE setdatabase=(SELECT oid FROM pg_catalog.pg_database WHERE datname=current_database()) \
                         OR setrole=(SELECT oid FROM pg_catalog.pg_roles WHERE rolname={})))::text AS state",
        literal(login),
    )).await.unwrap();
    rows[0]
        .try_get::<&str>("state")
        .unwrap()
        .unwrap()
        .to_owned()
}

async fn login_gone(fixture: &Fixture, login: &str) {
    let mut admin = fixture.admin().await;
    tokio::time::timeout(DEADLINE, async {
        loop {
            let rows = admin
                .query(&format!(
                    "SELECT 1 FROM pg_catalog.pg_stat_activity WHERE usename={}",
                    literal(login),
                ))
                .await
                .unwrap();
            if rows.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the supervisor did not close its checked supplied session");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_dropped_supplied_request_restores_standard_and_the_next_request_proceeds() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let database = fixture.target().await;
        let (login, scratch) = fixture.confined().await;
        let before = supplied_state(&fixture, &scratch, &login.0).await;
        let gate = Gate::new(&fixture, &database).await;
        let mut call = Box::pin(request(
            fixture.as_login(&scratch, &login),
            fixture.on(&database),
            gate.sql(),
        ));
        let _pid = tokio::select! {
            pid = barrier(&fixture, &database, &gate.tag) => pid,
            result = &mut call => panic!("the producer ended before cancellation: {:?}", result.err()),
        };
        let staged = fixture.foreign_objects(&scratch).await;
        assert_ne!(
            staged.1, 0,
            "the supplied database must have real run-owned objects"
        );
        drop(call);
        gate.release().await;
        // A multi-query catalog inventory cannot read coherently while DROP
        // OWNED is changing it. The keeper's departure proves the supervisor
        // has finished release; only then make the authoritative inventory.
        login_gone(&fixture, &login.0).await;
        let released = fixture.foreign_objects(&scratch).await.1 == 0;
        let after = supplied_state(&fixture, &scratch, &login.0).await;
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let target_url = fixture.on(&database);
        let mut target_session = Conn::connect(Driver::Postgres, &target_url).await.unwrap();
        let identity = pbps_pg::resolver::scratch::cluster_identity(&mut target_session)
            .await
            .unwrap();
        // All SQL inside this measured call belongs to the supervisor. A lost
        // task-local scope must not silently weaken the existing batch guard.
        let (next, requests) = pbps_db::request_count::scope(super::super::produce_unsealed(
            Driver::Postgres,
            &fixture.as_login(&scratch, &login),
            &target_url,
            &identity,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
            &Standard::default(),
            None,
            &super::super::Provenance::server(),
        ))
        .await;
        let next = match next {
            Ok(unsealed) => {
                let rechecked = unsealed
                    .recheck(&mut target_session)
                    .await
                    .unwrap_or_else(|error| panic!("{}", error.error));
                let changes = rechecked.changes().clone();
                rechecked
                    .seal(changes, inputs.base(), inputs.desired())
                    .map_err(ProduceError::Run)
            }
            Err(error) => Err(error),
        };
        drop(target_session);
        fixture.drop().await;
        assert!(
            released,
            "{server}: dropping the request lost supplied release"
        );
        assert_eq!(
            before, after,
            "{server}: cleanup did not restore the recorded standard"
        );
        assert_ne!(
            requests, 0,
            "the supervisor lost its actual SQL request-count scope"
        );
        assert_answers_the_overload(&next.unwrap());
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL target and separate scratch server"]
async fn a_cancelled_cleanup_failure_reports_exact_names_and_an_observed_one_is_returned() {
    for cancelled in [true, false] {
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let scratch = Fixture::new(SCRATCH_SERVER);
        let database = target.target().await;
        let before = scratch.inventory().await;
        let gate = Gate::new(&target, &database).await;
        let task = tokio::spawn(request(
            scratch.server.clone(),
            target.on(&database),
            gate.sql(),
        ));
        let _pid = barrier(&target, &database, &gate.tag).await;
        let owned = Owned::observe(&scratch, &before).await;
        owned.terminate_keeper(&scratch).await;
        let names = if cancelled {
            task.abort();
            assert!(matches!(task.await, Err(error) if error.is_cancelled()));
            gate.release().await;
            tokio::time::timeout(DEADLINE, async {
                loop {
                    let reports = super::super::recovery::take();
                    if let Some(names) = reports
                        .cancelled
                        .into_iter()
                        .find(|names| names.iter().any(|name| name.contains(&owned.database)))
                    {
                        break names;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the detached cleanup lost its exact recovery names")
        } else {
            gate.release().await;
            match tokio::time::timeout(DEADLINE, task).await.unwrap().unwrap() {
                Err(ProduceError::Cleanup(names)) => names,
                result => panic!(
                    "failed cleanup must retain the ordinary return path: {:?}",
                    result.err()
                ),
            }
        };
        let retained = scratch.inventory().await;
        owned.remove(&scratch).await;
        let after = scratch.inventory().await;
        target.drop().await;
        assert!(
            retained.0.contains(&owned.database),
            "a failed checked connection did not remove the database"
        );
        assert!(
            names.iter().any(|name| name.contains(&owned.database)),
            "{names:?}"
        );
        for role in &owned.roles {
            assert!(
                names.iter().any(|name| name.contains(role.raw())),
                "{} missing in {names:?}",
                role.raw()
            );
        }
        assert_eq!(
            before, after,
            "only independently observed fixture resources were cleaned"
        );
    }
}
