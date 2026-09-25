//! Two-session controls for the public authorization boundary. Each fixture
//! owns its database and cluster roles; shared server settings stay untouched.

use super::*;
use pbps_db::{Conn, Driver};

fn committed(connection: &str, sql: String) -> Result<(), Failure> {
    let connection = connection.to_owned();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Failure::Read)?
            .block_on(async {
                let mut writer = Conn::connect(Driver::Postgres, &connection)
                    .await
                    .map_err(|_| Failure::Read)?;
                writer
                    .execute("SET lock_timeout='2s'; SET statement_timeout='10s'")
                    .await
                    .map_err(|_| Failure::Read)?;
                writer.execute(&sql).await.map_err(|_| Failure::Read)
            })
    })
    .join()
    .map_err(|_| Failure::Read)?
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn public_role_changes_after_the_snapshot_refuse_without_authentication_catalog_access() {
    let mut failures = Vec::new();
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        assert!(
            !base.contains("://"),
            "fixture requires libpq keyword settings"
        );
        let name = format!(
            "pbps_role882_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        let reader = format!("{name}_r");
        let subject = format!("{name}_s");
        let member = format!("{name}_m");
        let arriving = format!("{name}_a");
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        admin.execute(&format!("CREATE ROLE {reader} NOLOGIN IN ROLE pg_read_all_settings; CREATE ROLE {subject} NOLOGIN VALID UNTIL '2037-01-02 03:04:05+00'; CREATE ROLE {member} NOLOGIN")).await.unwrap();
        let task = tokio::spawn(exercise(
            format!("{base} dbname={name}"),
            reader.clone(),
            subject.clone(),
            member.clone(),
            arriving.clone(),
        ))
        .await;
        // An assertion in the isolated task still reaches owned fixture cleanup.
        admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        admin
            .execute(&format!(
                "DROP ROLE IF EXISTS {arriving}, {subject}_renamed, {subject}, {member}, {reader}"
            ))
            .await
            .unwrap();
        failures.extend(task.expect("public role fixture failed after cleanup"));
    }
    assert!(
        failures.is_empty(),
        "fresh public role comparison failures: {failures:?}"
    );
}

async fn exercise(
    connection: String,
    reader: String,
    subject: String,
    member: String,
    arriving: String,
) -> Vec<String> {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    conn.execute(&format!(
        "SET ROLE {reader}; SET timezone='Pacific/Honolulu'; SET datestyle='SQL, DMY'"
    ))
    .await
    .unwrap();
    let privileges = conn.query("SELECT pg_catalog.has_table_privilege(current_user, 'pg_catalog.pg_authid', 'SELECT')::text AS authid, current_setting('server_version_num') AS version").await.unwrap();
    assert_eq!(
        privileges[0].try_get::<&str>("authid").unwrap(),
        Some("false")
    );
    let version = privileges[0]
        .try_get::<&str>("version")
        .unwrap()
        .unwrap()
        .to_owned();
    let scope = super::super::CaptureScope {
        retained: Default::default(),
        candidates: Default::default(),
    };
    let before = super::super::capture(&mut conn, &scope).await.unwrap();
    let (_, differences) = super::super::recapture(&mut conn, &before).await.unwrap();
    assert!(
        differences.is_empty(),
        "ordinary unchanged capture must pass with noncanonical caller settings"
    );
    eprintln!(
        "role freshness {version}: unchanged ordinary reader passed; pg_authid access absent"
    );
    let mut failures = Vec::new();
    // The private pre-render qualifier runs after the first complete snapshot
    // read. Joining a separate writer commits at that exact boundary; no sleep
    // or scheduler race chooses whether the authorization change is observed.
    for (label, change, restore) in [
        (
            "superuser",
            format!("ALTER ROLE {subject} SUPERUSER"),
            format!("ALTER ROLE {subject} NOSUPERUSER"),
        ),
        (
            "inherit",
            format!("ALTER ROLE {subject} NOINHERIT"),
            format!("ALTER ROLE {subject} INHERIT"),
        ),
        (
            "bypassrls",
            format!("ALTER ROLE {subject} BYPASSRLS"),
            format!("ALTER ROLE {subject} NOBYPASSRLS"),
        ),
        (
            "createrole",
            format!("ALTER ROLE {subject} CREATEROLE"),
            format!("ALTER ROLE {subject} NOCREATEROLE"),
        ),
        (
            "createdb",
            format!("ALTER ROLE {subject} CREATEDB"),
            format!("ALTER ROLE {subject} NOCREATEDB"),
        ),
        (
            "canlogin",
            format!("ALTER ROLE {subject} LOGIN"),
            format!("ALTER ROLE {subject} NOLOGIN"),
        ),
        (
            "replication",
            format!("ALTER ROLE {subject} REPLICATION"),
            format!("ALTER ROLE {subject} NOREPLICATION"),
        ),
        (
            "connlimit",
            format!("ALTER ROLE {subject} CONNECTION LIMIT 3"),
            format!("ALTER ROLE {subject} CONNECTION LIMIT -1"),
        ),
        (
            "validuntil",
            format!("ALTER ROLE {subject} VALID UNTIL '2038-02-03 04:05:06+00'"),
            format!("ALTER ROLE {subject} VALID UNTIL '2037-01-02 03:04:05+00'"),
        ),
        (
            "config",
            format!("ALTER ROLE {subject} SET search_path=pg_catalog"),
            format!("ALTER ROLE {subject} RESET ALL"),
        ),
        (
            "rename",
            format!("ALTER ROLE {subject} RENAME TO {subject}_renamed"),
            format!("ALTER ROLE {subject}_renamed RENAME TO {subject}"),
        ),
        (
            "addition",
            format!("CREATE ROLE {arriving} NOLOGIN"),
            format!("DROP ROLE {arriving}"),
        ),
        (
            "removal",
            format!("DROP ROLE {member}"),
            format!("CREATE ROLE {member} NOLOGIN"),
        ),
        (
            "membership",
            format!("GRANT {subject} TO {member}"),
            format!("REVOKE {subject} FROM {member}"),
        ),
    ] {
        let observed = owned(&mut conn, |catalog, _| {
            assert!(
                catalog.rows["pg_roles"]
                    .iter()
                    .any(|r| r["rolname"].as_str() == Some(&subject))
            );
            committed(&connection, change)?;
            Ok(Selection::default())
        })
        .await;
        committed(&connection, restore).unwrap();
        let refused = matches!(observed, Err(Failure::Changed));
        eprintln!("role freshness {version}: {label} refused={refused}");
        if !refused {
            failures.push(format!("{version} {label}"));
        }
    }
    // Membership removal has its own physical witness, even when both public
    // role records retain the same identity and properties.
    committed(&connection, format!("GRANT {subject} TO {member}")).unwrap();
    let revoked = owned(&mut conn, |_, _| {
        committed(&connection, format!("REVOKE {subject} FROM {member}"))?;
        Ok(Selection::default())
    })
    .await;
    assert!(
        matches!(revoked, Err(Failure::Changed)),
        "removed role membership must refuse"
    );

    // Equality of safe public values is not a history/ABA detector. Do not
    // acquire password-verifier access to claim a stronger interval boundary.
    let restored = owned(&mut conn, |_, _| {
        committed(
            &connection,
            format!("ALTER ROLE {subject} NOINHERIT; ALTER ROLE {subject} INHERIT"),
        )?;
        Ok(Selection::default())
    })
    .await;
    assert!(
        restored.is_ok(),
        "restored public values are an explicit observation limit"
    );
    let unreadable = owned(&mut conn, |_, _| {
        committed(
            &connection,
            "REVOKE SELECT ON pg_catalog.pg_roles FROM PUBLIC".into(),
        )?;
        Ok(Selection::default())
    })
    .await;
    committed(
        &connection,
        "GRANT SELECT ON pg_catalog.pg_roles TO PUBLIC".into(),
    )
    .unwrap();
    assert!(
        unreadable.is_err(),
        "unreadable public roles must not become an empty set"
    );
    let rows = conn.query("SELECT current_setting('timezone') AS zone, current_setting('datestyle') AS style, current_setting('transaction_isolation') AS isolation").await.unwrap();
    assert_eq!(
        rows[0].try_get::<&str>("zone").unwrap(),
        Some("Pacific/Honolulu")
    );
    assert_eq!(rows[0].try_get::<&str>("style").unwrap(), Some("SQL, DMY"));
    assert_eq!(
        rows[0].try_get::<&str>("isolation").unwrap(),
        Some("read committed")
    );
    failures
}
