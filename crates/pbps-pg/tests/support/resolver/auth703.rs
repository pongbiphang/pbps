use pbps_db::{Conn, Driver};
use pbps_dialect::Dialect;
use pbps_pg::resolver::authorization::{AuthorizationContext, RoleMap, read, reconstruct, verify};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const OPTIONS: [(bool, bool); 4] = [(false, true), (true, false), (false, false), (true, true)];

fn ident(name: &str) -> String {
    pbps_pg::Postgres::new().quote_ident(name).unwrap()
}

#[derive(Clone)]
struct Names {
    token: String,
    target: String,
    scratch: String,
    deployer: String,
    login: String,
    members: [String; 4],
    unrelated: String,
}

impl Names {
    fn new() -> Self {
        let token = format!(
            "{:x}_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::process::id()
        );
        Self {
            target: format!("pbps703_t_{token}"),
            scratch: format!("pbps703_s_{token}"),
            deployer: format!("pbps703_d_{token}"),
            login: format!("pbps703_l_{token}"),
            members: std::array::from_fn(|i| format!("pbps703_r{i}_{token}")),
            unrelated: format!("pbps703_u_{token}"),
            token,
        }
    }

    fn roles(&self) -> Vec<String> {
        let mut roles = vec![
            self.deployer.clone(),
            self.login.clone(),
            self.unrelated.clone(),
        ];
        roles.extend(self.members.iter().cloned());
        roles
    }
}

fn login_url(server: &str, database: &str, role: &str) -> String {
    let endpoint = server
        .split_whitespace()
        .filter(|p| !p.starts_with("user=") && !p.starts_with("password="))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{endpoint} dbname={database} user={role} password=auth703")
}

async fn identity(conn: &mut Conn, effective: &str, login: &str) {
    let rows = conn
        .query("SELECT current_user::text AS effective, session_user::text AS login, current_setting('is_superuser') AS elevated")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].try_get::<&str>("effective").unwrap(),
        Some(effective)
    );
    assert_eq!(rows[0].try_get::<&str>("login").unwrap(), Some(login));
    assert_eq!(rows[0].try_get::<&str>("elevated").unwrap(), Some("off"));
}

async fn answers(conn: &mut Conn) -> BTreeMap<String, (bool, bool, bool)> {
    // No closure filter from the production reader: a missing FALSE/FALSE
    // member must remain a question this independent oracle asks.
    conn.query("SELECT rolname::text AS name, pg_has_role(oid, 'MEMBER')::text AS member, pg_has_role(oid, 'SET')::text AS switch, pg_has_role(oid, 'USAGE')::text AS inherit FROM pg_roles")
        .await
        .unwrap()
        .iter()
        .map(|row| {
            let flag = |key| row.try_get::<&str>(key).unwrap().unwrap().parse::<bool>().unwrap();
            (row.try_get::<&str>("name").unwrap().unwrap().to_owned(),
             (flag("member"), flag("switch"), flag("inherit")))
        })
        .collect()
}

async fn switching(conn: &mut Conn, role: &str, allowed: bool, effective: &str, login: &str) {
    identity(conn, effective, login).await;
    let result = conn.execute(&format!("SET ROLE {}", ident(role))).await;
    if allowed {
        result.unwrap();
        identity(conn, role, login).await;
        conn.execute("RESET ROLE").await.unwrap();
        // RESET ROLE returns to session_user, not to the previously entered
        // mapped deployer. An admin switched down cannot prove this denial.
        identity(conn, login, login).await;
        if effective != login {
            conn.execute(&format!("SET ROLE {}", ident(effective)))
                .await
                .unwrap();
        }
    } else {
        let error = result.expect_err("SET FALSE must deny a real role switch");
        assert_eq!(error.server_error_code().as_deref(), Some("42501"));
    }
    identity(conn, effective, login).await;
}

fn mapped(map: &RoleMap, target: &AuthorizationContext, role: &str) -> String {
    // This clone is only a lookup through the public mapping API; it is never
    // used as a read, reconstruction recipe, or verification expectation.
    let mut lookup = target.clone();
    lookup.principal.effective = role.into();
    map.deployer(&lookup).unwrap().raw().to_owned()
}

async fn exercise(server: String, names: Names, mapped_names: Arc<Mutex<Vec<String>>>) {
    let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
    for database in [&names.target, &names.scratch] {
        admin
            .execute(&format!(
                "CREATE DATABASE {} TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'",
                ident(database)
            ))
            .await
            .unwrap();
    }
    for role in names.roles() {
        admin
            .execute(&format!(
                "CREATE ROLE {} {}",
                ident(&role),
                if role == names.deployer || role == names.login {
                    "LOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB PASSWORD 'auth703'"
                } else {
                    "NOLOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB"
                }
            ))
            .await
            .unwrap();
    }
    for (role, (can_set, inherits)) in names.members.iter().zip(OPTIONS) {
        admin
            .execute(&format!(
                "GRANT {} TO {} WITH SET {can_set}, INHERIT {inherits}",
                ident(role),
                ident(&names.deployer)
            ))
            .await
            .unwrap();
    }
    let mut target_admin = Conn::connect(
        Driver::Postgres,
        &format!("{server} dbname={}", names.target),
    )
    .await
    .unwrap();
    target_admin.execute(&format!("CREATE SCHEMA app; REVOKE ALL ON SCHEMA app FROM PUBLIC; GRANT USAGE, CREATE ON SCHEMA app TO {}", ident(&names.members[0]))).await.unwrap();
    let mut target_conn = Conn::connect(
        Driver::Postgres,
        &login_url(&server, &names.target, &names.deployer),
    )
    .await
    .unwrap();
    identity(&mut target_conn, &names.deployer, &names.deployer).await;
    let target_answers = answers(&mut target_conn).await;
    let schemas = ["app".to_owned()];
    let target = read(&mut target_conn, &schemas).await.unwrap();
    for (role, (can_set, inherits)) in names.members.iter().zip(OPTIONS) {
        assert_eq!(target_answers[role], (true, can_set, inherits));
        let recorded = target
            .roles
            .get(role.as_str())
            .expect("even a FALSE/FALSE membership belongs to the closure");
        assert_eq!(
            recorded.can_set, can_set,
            "switchability of {role} is SET, not MEMBER"
        );
        assert_eq!(recorded.inherits, inherits);
        switching(
            &mut target_conn,
            role,
            can_set,
            &names.deployer,
            &names.deployer,
        )
        .await;
    }
    assert_eq!(target_answers[&names.unrelated], (false, false, false));
    assert!(!target.roles.contains_key(names.unrelated.as_str()));
    target_conn
        .execute(
            "CREATE TABLE app.inherited_permission (id int); DROP TABLE app.inherited_permission",
        )
        .await
        .unwrap();

    let map = RoleMap::generate(&target, &[], &names.login, &names.token);
    let run_names = map
        .run_local_names()
        .iter()
        .map(|n| n.raw().to_owned())
        .collect::<Vec<_>>();
    let existing = admin
        .query("SELECT rolname::text AS name FROM pg_roles")
        .await
        .unwrap();
    assert!(!existing.iter().any(|row| {
        run_names
            .iter()
            .any(|name| row.try_get::<&str>("name").unwrap() == Some(name.as_str()))
    }));
    *mapped_names.lock().unwrap() = run_names;
    let mut scratch_admin = Conn::connect(
        Driver::Postgres,
        &format!("{server} dbname={}", names.scratch),
    )
    .await
    .unwrap();
    reconstruct(&mut scratch_admin, &map, &target, &names.scratch)
        .await
        .unwrap();
    let mut run = Conn::connect(
        Driver::Postgres,
        &login_url(&server, &names.scratch, &names.login),
    )
    .await
    .unwrap();
    let deployer = mapped(&map, &target, &names.deployer);
    run.execute(&format!("SET ROLE {}", ident(&deployer)))
        .await
        .unwrap();
    identity(&mut run, &deployer, &names.login).await;
    let scratch_answers = answers(&mut run).await;
    for (logical, (can_set, inherits)) in names.members.iter().zip(OPTIONS) {
        let role = mapped(&map, &target, logical);
        assert_eq!(scratch_answers[&role], (true, can_set, inherits));
        switching(&mut run, &role, can_set, &deployer, &names.login).await;
    }
    run.execute(
        "CREATE TABLE app.inherited_permission (id int); DROP TABLE app.inherited_permission",
    )
    .await
    .unwrap();
    assert!(
        verify(&mut run, &map, &target, &schemas)
            .await
            .unwrap()
            .is_empty()
    );

    // Broaden only scratch. A verifier repeating a mistaken MEMBER answer
    // must not hide that this deployer now has an actual extra capability.
    let blocked = mapped(&map, &target, &names.members[0]);
    scratch_admin
        .execute(&format!(
            "GRANT {} TO {} WITH SET TRUE, INHERIT TRUE",
            ident(&blocked),
            ident(&deployer)
        ))
        .await
        .unwrap();
    switching(&mut run, &blocked, true, &deployer, &names.login).await;
    let differences = verify(&mut run, &map, &target, &schemas).await.unwrap();
    assert!(
        differences.contains(&format!("role:{}", names.members[0])),
        "{differences:?}"
    );
    scratch_admin
        .execute(&format!(
            "GRANT {} TO {} WITH SET FALSE, INHERIT TRUE",
            ident(&blocked),
            ident(&deployer)
        ))
        .await
        .unwrap();
    switching(&mut run, &blocked, false, &deployer, &names.login).await;
    assert!(
        verify(&mut run, &map, &target, &schemas)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn one(server: &str, major: u32) -> Result<String, String> {
    let names = Names::new();
    let mut admin = Conn::connect(Driver::Postgres, server).await.unwrap();
    let rows = admin
        .query("SELECT current_setting('server_version_num') AS number, version() AS build")
        .await
        .unwrap();
    let version = rows[0]
        .try_get::<&str>("number")
        .unwrap()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert_eq!(version / 10000, major);
    let build = rows[0]
        .try_get::<&str>("build")
        .unwrap()
        .unwrap()
        .to_owned();
    // Confirm absence before recording a lease; a colliding existing name is
    // never ours to remove, even when setup or an assertion later fails.
    for (sql, key, planned) in [
        (
            "SELECT datname::text AS name FROM pg_database",
            "name",
            vec![names.target.clone(), names.scratch.clone()],
        ),
        (
            "SELECT rolname::text AS name FROM pg_roles",
            "name",
            names.roles(),
        ),
    ] {
        let rows = admin.query(sql).await.unwrap();
        assert!(!rows.iter().any(|row| {
            planned
                .iter()
                .any(|name| row.try_get::<&str>(key).unwrap() == Some(name.as_str()))
        }));
    }
    let mapped_names = Arc::new(Mutex::new(Vec::new()));
    // Cleanup lives outside the assertion task, so both intentional revert
    // failures and partial setup still release every recorded owned name.
    let outcome = tokio::spawn(exercise(
        server.to_owned(),
        names.clone(),
        mapped_names.clone(),
    ))
    .await;
    let mut cleanup_errors = Vec::new();
    for database in [&names.target, &names.scratch] {
        if let Err(error) = admin
            .execute(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                ident(database)
            ))
            .await
        {
            cleanup_errors.push(error.to_string());
        }
    }
    let mut roles = names.roles();
    roles.extend(mapped_names.lock().unwrap().iter().cloned());
    roles.sort();
    roles.dedup();
    for role in roles {
        if let Err(error) = admin
            .execute(&format!("DROP ROLE IF EXISTS {}", ident(&role)))
            .await
        {
            cleanup_errors.push(error.to_string());
        }
    }
    if !cleanup_errors.is_empty() {
        return Err(format!(
            "PG{major} case {outcome:?}; owned cleanup: {cleanup_errors:?}"
        ));
    }
    outcome
        .map(|()| build)
        .map_err(|error| format!("PG{major}: {error}"))
}

pub(super) async fn switchability_is_reproduced_independently() {
    let mut failures = Vec::new();
    for (key, major) in [("PBPS_TEST_PG_DB", 18), ("PBPS_TEST_PG_OLD_DB", 16)] {
        let server = std::env::var(key).expect("both required PostgreSQL endpoints must exist");
        match one(&server, major).await {
            Ok(build) => println!("auth703 completed PG{major}: {build}"),
            Err(error) => failures.push(error),
        }
    }
    assert!(
        failures.is_empty(),
        "authorization switchability failed: {failures:?}"
    );
}
