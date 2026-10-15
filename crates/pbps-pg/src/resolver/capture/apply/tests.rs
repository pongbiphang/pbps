//! Apply's captures on both pinned servers (#616 step 6): the per-statement
//! lookups and the bracketing reads the experiment on the issue chose, each
//! case with its negative.

use super::super::{CandidateClass, CandidateSet, CaptureScope, DroppedSignature};
use super::*;
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::{Conn, Driver};

fn key() -> EnvironmentFingerprintKey {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = std::env::temp_dir().join(format!("pbps616-key-{}", crate::catalog::probe_token()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all(b"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=")
        .unwrap();
    let key = EnvironmentFingerprintKey::from_file(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    key
}

/// Every overload of each name a lookup below can find, in each schema it
/// can search: a lookup's result is always among a plan's candidates.
fn scope() -> CaptureScope {
    CaptureScope {
        retained: Default::default(),
        candidates: [("a", "f"), ("b", "f"), ("Q s", "F"), ("b", "own")]
            .into_iter()
            .map(|(namespace, name)| CandidateSet {
                class: CandidateClass::Routine,
                namespace: Some(namespace.into()),
                name: Some(name.into()),
            })
            .collect(),
    }
}

fn lookup(spelled: &str, path: &str) -> DroppedSignature {
    DroppedSignature {
        spelled: spelled.into(),
        path: path.into(),
        kind: "f",
    }
}

/// Several paths, same-name overloads, a quoted identifier, an absent
/// overload, and one the caller's own transaction creates.
fn lookups() -> BTreeSet<DroppedSignature> {
    BTreeSet::from([
        lookup("f(integer)", "a, b"),
        lookup("f(text)", "b"),
        lookup("f(bigint)", "b"),
        lookup("\"F\"(integer)", "\"Q s\", b"),
        lookup("b.f(integer)", "a"),
        lookup("own(integer)", "b"),
    ])
}

fn resolved(manifest: &InputManifest, spelled: &str) -> Option<Vec<String>> {
    manifest
        .identifications()
        .iter()
        .find(|l| l.signature == spelled)
        .unwrap_or_else(|| panic!("{spelled} is looked up"))
        .resolved
        .as_ref()
        .map(|o| o.name.clone())
}

async fn sealed(conn: &mut Conn, key: &EnvironmentFingerprintKey) -> InputManifest {
    super::super::capture_identifying(conn, &scope(), &lookups())
        .await
        .unwrap()
        .seal(key)
        .unwrap()
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn the_apply_capture_reads_the_callers_transaction_and_binds_each_lookup_to_the_seal() {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        let name = format!(
            "pbps_apply616_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let (user, member) = (format!("{name}_u"), format!("{name}_r"));
        admin
            .execute(&format!(
                "CREATE ROLE {user} NOLOGIN IN ROLE pg_read_all_settings; CREATE ROLE {member} NOLOGIN"
            ))
            .await
            .unwrap();
        let connection = format!("{base} dbname={name}");
        let roles = (user.clone(), member.clone());
        let result = tokio::task::LocalSet::new()
            .run_until(async move { tokio::task::spawn_local(exercise(connection, roles)).await })
            .await;
        admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        admin
            .execute(&format!("DROP ROLE {user}; DROP ROLE {member}"))
            .await
            .unwrap();
        result.expect("apply capture fixture failed after cleanup");
    }
}

async fn exercise(connection: String, (user, member): (String, String)) {
    let key = key();
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    conn.execute(&format!(
        "CREATE SCHEMA a; CREATE SCHEMA b; CREATE SCHEMA \"Q s\"; \
         CREATE FUNCTION b.f(integer) RETURNS integer LANGUAGE sql RETURN 1; \
         CREATE FUNCTION b.f(text) RETURNS integer LANGUAGE sql RETURN 2; \
         CREATE FUNCTION \"Q s\".\"F\"(integer) RETURNS integer LANGUAGE sql RETURN 3; \
         CREATE FUNCTION a.f(integer) RETURNS integer LANGUAGE sql RETURN 9; \
         GRANT USAGE ON SCHEMA b, \"Q s\" TO {user}; GRANT USAGE ON SCHEMA a TO {member}; \
         SET search_path = b; SET timezone = 'Pacific/Honolulu'"
    ))
    .await
    .unwrap();
    // Every lookup below runs as a role that cannot use `a` until it is
    // granted `member`.
    conn.execute(&format!("SET ROLE {user}")).await.unwrap();
    let seal = sealed(&mut conn, &key).await;
    assert_eq!(
        resolved(&seal, "f(integer)"),
        Some(vec!["b".into(), "f".into()])
    );
    assert_eq!(
        resolved(&seal, "f(text)"),
        Some(vec!["b".into(), "f".into()])
    );
    assert_eq!(resolved(&seal, "f(bigint)"), None);
    assert_eq!(
        resolved(&seal, "\"F\"(integer)"),
        Some(vec!["Q s".into(), "F".into()])
    );
    assert_eq!(
        resolved(&seal, "b.f(integer)"),
        Some(vec!["b".into(), "f".into()])
    );
    assert_eq!(resolved(&seal, "own(integer)"), None);

    // Outside a transaction there is no caller's transaction to read in.
    assert!(matches!(
        recheck_opening(&mut conn, &seal, &key).await,
        Err(CaptureError::NoTransaction)
    ));

    // Inside one, the same target reads as sealed, and the caller's session
    // and transaction are as they were.
    conn.execute("RESET ROLE; BEGIN ISOLATION LEVEL READ COMMITTED")
        .await
        .unwrap();
    conn.execute(&format!("SET LOCAL ROLE {user}"))
        .await
        .unwrap();
    let fresh = recheck_opening(&mut conn, &seal, &key).await.unwrap();
    assert_eq!(seal.opening_differences(&fresh), Vec::<String>::new());
    let rows = conn
        .query("SELECT current_setting('search_path') AS path, current_setting('timezone') AS zone, current_setting('transaction_read_only') AS ro")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<&str>("path").unwrap(), Some("b"));
    assert_eq!(
        rows[0].try_get::<&str>("zone").unwrap(),
        Some("Pacific/Honolulu")
    );
    assert_eq!(rows[0].try_get::<&str>("ro").unwrap(), Some("off"));

    // The caller's own uncommitted routine is what its lookup names, and a
    // capture outside the transaction cannot see it.
    conn.execute(
        "RESET ROLE; CREATE FUNCTION b.own(integer) RETURNS integer LANGUAGE sql RETURN 7",
    )
    .await
    .unwrap();
    conn.execute(&format!(
        "GRANT EXECUTE ON FUNCTION b.own(integer) TO {user}; SET LOCAL ROLE {user}"
    ))
    .await
    .unwrap();
    let own = recheck_opening(&mut conn, &seal, &key).await.unwrap();
    assert_eq!(
        resolved(&own, "own(integer)"),
        Some(vec!["b".into(), "own".into()])
    );
    let differences = seal.opening_differences(&own);
    assert!(
        differences.contains(&"`own(integer)` under `b` resolves to another routine".to_owned()),
        "{differences:?}"
    );

    // Another session grants the reading role `a`: `b.f(integer)` is
    // untouched, yet `f(integer)` under `a, b` now names `a.f`. The lookup
    // moved, which a check of the old routine alone would accept, and the
    // recheck refuses it (#616 planning review).
    let mut other = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    other
        .execute(&format!("GRANT {member} TO {user}"))
        .await
        .unwrap();
    let granted = recheck_opening(&mut conn, &seal, &key).await.unwrap();
    assert_eq!(
        resolved(&granted, "f(integer)"),
        Some(vec!["a".into(), "f".into()])
    );
    let differences = seal.opening_differences(&granted);
    assert!(
        differences.contains(&"`f(integer)` under `a, b` resolves to another routine".to_owned()),
        "{differences:?}"
    );
    assert_eq!(
        resolved(&granted, "b.f(integer)"),
        resolved(&seal, "b.f(integer)"),
        "the old routine itself never changed"
    );
    other
        .execute(&format!("REVOKE {member} FROM {user}"))
        .await
        .unwrap();
    conn.execute("ROLLBACK").await.unwrap();
    // Negative: the revoked grant leaves a target that reads as sealed again.
    conn.execute(&format!("BEGIN; SET LOCAL ROLE {user}"))
        .await
        .unwrap();
    let back = recheck_opening(&mut conn, &seal, &key).await.unwrap();
    assert_eq!(seal.opening_differences(&back), Vec::<String>::new());
    conn.execute("ROLLBACK").await.unwrap();
}
