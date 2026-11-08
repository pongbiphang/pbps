//! The bounded re-read of the owned capture (#2715, DEC-2715.1). Every case
//! is deterministic: the private `qualify` hook runs after the first complete
//! catalog read and before the closing witness, and a second session commits
//! from it, so no sleep or scheduler race decides whether a change lands in
//! the window.

use super::role_tests::committed;
use super::*;
use pbps_db::{Conn, Driver};
use std::time::{Duration, Instant};

#[test]
fn only_a_changed_read_earns_another_attempt_and_only_inside_both_bounds() {
    let mut attempts = Attempts::new();
    for _ in 1..ATTEMPTS {
        assert_eq!(attempts.changed(), Ok(PAUSE));
    }
    let refused = attempts.changed().unwrap_err();
    assert_eq!(
        refused,
        Failure::Unsettled {
            attempts: ATTEMPTS,
            seconds: 10
        }
    );
    let message = refused.to_string();
    assert!(
        message.contains("5 reads over 10 s") && message.contains("retry when it is quiet"),
        "{message}"
    );
    // The time bound holds before the count does: an attempt takes seconds.
    let mut slow = Attempts::new();
    slow.started = Instant::now()
        .checked_sub(BUDGET - Duration::from_millis(50))
        .unwrap();
    assert_eq!(
        slow.changed(),
        Err(Failure::Unsettled {
            attempts: 1,
            seconds: 10
        })
    );
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_catalog_that_settles_is_sealed_settled_and_one_that_never_does_is_refused_by_name() {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        assert!(
            !base.contains("://"),
            "fixture requires libpq keyword settings"
        );
        let name = format!(
            "pbps_settle2715_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        admin
            .execute(&format!("CREATE ROLE {name}_s NOLOGIN"))
            .await
            .unwrap();
        let task = tokio::spawn(exercise(format!("{base} dbname={name}"), name.clone())).await;
        admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        let leftovers = admin
            .query(&format!(
                "SELECT rolname::text AS n FROM pg_catalog.pg_roles WHERE rolname LIKE '{name}\\_%'"
            ))
            .await
            .unwrap();
        for row in leftovers {
            let role = row.try_get::<&str>("n").unwrap().unwrap().to_owned();
            admin.execute(&format!("DROP ROLE {role}")).await.unwrap();
        }
        task.expect("settle fixture failed after cleanup");
    }
}

async fn exercise(connection: String, name: String) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    let subject = format!("{name}_s");
    let none = BTreeSet::new();
    let scope = super::super::CaptureScope {
        retained: Default::default(),
        candidates: Default::default(),
    };

    // Churn on the first attempt only: the second read is consistent and is
    // the one returned, so it carries the role that arrived.
    let arriving = format!("{name}_a");
    let mut calls = 0;
    let read = owned(&mut conn, &none, |_, _| {
        calls += 1;
        if calls == 1 {
            committed(&connection, format!("CREATE ROLE {arriving} NOLOGIN"))?;
        }
        Ok(Selection::default())
    })
    .await
    .expect("a change that stops is absorbed");
    assert_eq!(calls, 2, "one re-read");
    assert!(
        read.catalog.rows["pg_roles"]
            .iter()
            .any(|r| r["rolname"].as_str() == Some(&arriving)),
        "the accepted read is the settled one"
    );

    // A depended-on role changed on the first attempt: the new value is what
    // is read, and a recheck against the seal taken before it still refuses
    // (DEC-616.1): the retry hides nothing from the apply's comparison.
    let before = super::super::capture(&mut conn, &scope).await.unwrap();
    let mut calls = 0;
    let read = owned(&mut conn, &none, |_, _| {
        calls += 1;
        if calls == 1 {
            committed(&connection, format!("ALTER ROLE {subject} CREATEDB"))?;
        }
        Ok(Selection::default())
    })
    .await
    .unwrap();
    assert_eq!(calls, 2);
    let row = read.catalog.rows["pg_roles"]
        .iter()
        .find(|r| r["rolname"].as_str() == Some(&subject))
        .unwrap();
    assert_eq!(row["rolcreatedb"], serde_json::json!(true), "the new value");
    let (_, differences) = super::super::recapture(&mut conn, &before).await.unwrap();
    assert!(
        !differences.is_empty(),
        "the older seal must still refuse the changed role"
    );

    // Churn on every attempt: refused by name within the bound. On a slow
    // server the time budget can end the loop before the attempt bound does,
    // which is equally correct, so this asserts that the refusal names the
    // reads actually made rather than a fixed five. The exact attempt bound
    // and the time bound are pinned without a server by the unit test above.
    let mut calls = 0;
    let started = Instant::now();
    let refused = owned(&mut conn, &none, |_, _| {
        calls += 1;
        committed(&connection, format!("CREATE ROLE {name}_c{calls} NOLOGIN"))?;
        Ok(Selection::default())
    })
    .await;
    assert!((1..=ATTEMPTS).contains(&calls), "within the bound: {calls}");
    let message = match refused {
        Err(error @ Failure::Unsettled { attempts, seconds }) => {
            assert_eq!(attempts, calls, "the refusal names the reads made");
            assert_eq!(seconds, BUDGET.as_secs());
            error.to_string()
        }
        other => panic!("endless churn must be Unsettled, got {:?}", other.err()),
    };
    assert!(
        message.contains(&format!("{calls} reads over {} s", BUDGET.as_secs())),
        "{message}"
    );
    assert!(started.elapsed() < BUDGET + Duration::from_secs(5));

    // Anything but a changed catalog is returned after one attempt.
    for error in [Failure::Incomplete, Failure::Read, Failure::Version] {
        let expected = error.clone();
        let mut calls = 0;
        let returned = owned(&mut conn, &none, |_, _| {
            calls += 1;
            Err(error.clone())
        })
        .await;
        assert_eq!(calls, 1, "{error:?} is not retried");
        assert_eq!(returned.err(), Some(expected));
    }

    // The apply's bracket is untouched: a change inside it refuses at once.
    conn.execute("BEGIN ISOLATION LEVEL READ COMMITTED")
        .await
        .unwrap();
    let mut calls = 0;
    let bracket = in_callers_transaction(&mut conn, &none, |_, _| {
        calls += 1;
        committed(&connection, format!("ALTER ROLE {subject} NOCREATEDB"))?;
        Ok(Selection::default())
    })
    .await;
    conn.execute("ROLLBACK").await.unwrap();
    assert_eq!(calls, 1, "the bracket is never retried");
    assert!(
        matches!(bracket, Err(Failure::Changed)),
        "{:?}",
        bracket.err()
    );
}
