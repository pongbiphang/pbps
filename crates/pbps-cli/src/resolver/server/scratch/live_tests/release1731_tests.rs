//! A backend refusal is not authority to restore another database (DEC-1731.1).

use super::*;
use pbps_pg::resolver::{scratch as sql, standard};

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_release_refused_on_another_backend_does_not_restore_its_database() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let login = fixture
            .login("release1731", "NOSUPERUSER NOCREATEDB NOCREATEROLE")
            .await;
        let database = fixture.database("checked1731", Some(&login.0)).await;
        let foreign = fixture.database("foreign1731", Some(&login.0)).await;
        let mut checked = Conn::connect(Driver::Postgres, &fixture.as_login(&database, &login))
            .await
            .unwrap();
        let backend = sql::backend(&mut checked).await.unwrap();
        let defaults = sql::login_defaults(&mut checked).await.unwrap();
        let acl = standard::read_state(&mut checked).await.unwrap().acl;
        let declared = Standard::default();
        // The same credentials can reach a different database. Only the original
        // backend, not those credentials, authorizes this release.
        let mut other = Conn::connect(Driver::Postgres, &fixture.as_login(&foreign, &login))
            .await
            .unwrap();
        other
            .execute(&format!(
                "SET ROLE pg_database_owner; \
                 COMMENT ON SCHEMA public IS 'foreign1731'; \
                 SET ROLE NONE; \
                 ALTER DATABASE {foreign} SET statement_timeout = '7s'; \
                 CREATE TABLE public.foreign_kept(i integer)"
            ))
            .await
            .unwrap();
        let before = standard::read_state(&mut other).await.unwrap();
        assert!(!sql::on_backend(&mut other, &backend).await.unwrap());
        let refused = super::super::release_supplied(
            &mut other, &backend, &declared, &database, &defaults, &acl,
        )
        .await;
        let after = standard::read_state(&mut other).await.unwrap();
        let kept = other
            .query("SELECT pg_catalog.to_regclass('public.foreign_kept')::text AS kept")
            .await
            .unwrap()[0]
            .try_get::<&str>("kept")
            .unwrap()
            .is_some();
        // A same-backend release still cleans and restores a nonstandard state.
        checked
            .execute(&format!(
                "CREATE TABLE public.removed1731(i integer); \
                 ALTER DATABASE {database} SET statement_timeout = '7s'"
            ))
            .await
            .unwrap();
        let healthy = super::super::release_supplied(
            &mut checked,
            &backend,
            &declared,
            &database,
            &defaults,
            &acl,
        )
        .await;
        let restored = standard::verify(&mut checked, &declared, Some(&acl))
            .await
            .unwrap();
        drop((checked, other));
        fixture.drop().await;
        assert_eq!(
            before, after,
            "{server}: a refused backend must receive no restoration"
        );
        assert!(kept, "{server}: foreign objects must remain untouched");
        let remaining = refused.expect_err("the checked backend changed");
        assert_eq!(remaining.len(), 1, "{server}: {remaining:?}");
        assert!(remaining[0].contains("backend"), "{server}: {remaining:?}");
        assert!(
            !remaining[0].contains("every object"),
            "{server}: name the backend refusal alone: {remaining:?}"
        );
        assert_eq!(healthy, Ok(()), "{server}");
        assert!(restored.is_empty(), "{server}: {restored:?}");
    }
}

use crate::resolver::server::engine as for_tests;
use pbps_pg::resolver::recovery as record;

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_backend_hop_after_cleanup_cannot_authorize_restoration() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let login = fixture
            .login("hop1731", "NOSUPERUSER NOCREATEDB NOCREATEROLE")
            .await;
        let database = fixture.database("checked1731", Some(&login.0)).await;
        let foreign = fixture.database("foreign1731", Some(&login.0)).await;
        let mut checked = fixture.reconnect(&database, &login).await;
        let backend = sql::backend(&mut checked).await.unwrap();
        let defaults = sql::login_defaults(&mut checked).await.unwrap();
        let acl = standard::read_state(&mut checked).await.unwrap().acl;
        checked
            .execute("CREATE TABLE public.cleaned1731(i integer)")
            .await
            .unwrap();
        let mut other = fixture.reconnect(&foreign, &login).await;
        other
            .execute(&format!(
                "SET ROLE pg_database_owner; COMMENT ON SCHEMA public IS 'foreign1731'; \
             SET ROLE NONE; ALTER DATABASE {foreign} SET statement_timeout = '7s'"
            ))
            .await
            .unwrap();
        let before = standard::read_state(&mut other).await.unwrap();
        // Replace only after the production DROP OWNED has returned. This
        // deterministic hop stands in for a proxy's next transaction routing.
        for_tests::hop_release(&database, other);
        let refused = super::super::release_supplied(
            &mut checked,
            &backend,
            &Standard::default(),
            &database,
            &defaults,
            &acl,
        )
        .await;
        let after = standard::read_state(&mut checked).await.unwrap();
        let mut observer = Conn::connect(Driver::Postgres, &fixture.on(&database))
            .await
            .unwrap();
        let cleaned = observer
            .query("SELECT pg_catalog.to_regclass('public.cleaned1731') IS NULL AS cleaned")
            .await
            .unwrap()[0]
            .try_get::<bool>("cleaned")
            .unwrap()
            .unwrap();
        drop((checked, observer));
        fixture.drop().await;
        assert!(
            cleaned,
            "{server}: the real cleanup committed before the hop"
        );
        assert_eq!(
            before, after,
            "{server}: the post-cleanup hop must receive no restoration"
        );
        let remaining =
            refused.expect_err("a successful cleanup does not pin the next transaction");
        assert_eq!(remaining.len(), 1, "{server}: {remaining:?}");
        assert!(remaining[0].contains("backend"), "{server}: {remaining:?}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn an_unanswerable_backend_preserves_standard_and_objects() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let login = fixture
            .login("unread1731", "NOSUPERUSER NOCREATEDB NOCREATEROLE")
            .await;
        let database = fixture.database("checked1731", Some(&login.0)).await;
        let mut conn = fixture.reconnect(&database, &login).await;
        let backend = sql::backend(&mut conn).await.unwrap();
        let defaults = sql::login_defaults(&mut conn).await.unwrap();
        let acl = standard::read_state(&mut conn).await.unwrap().acl;
        conn.execute(&format!(
            "SET ROLE pg_database_owner; COMMENT ON SCHEMA public IS 'unreadable1731'; \
             SET ROLE NONE; ALTER DATABASE {database} SET statement_timeout = '7s'; \
             CREATE TABLE public.kept1731(i integer)"
        ))
        .await
        .unwrap();
        let before = standard::read_state(&mut conn).await.unwrap();
        let pid = conn
            .query("SELECT pg_catalog.pg_backend_pid() AS pid")
            .await
            .unwrap()[0]
            .try_get::<i32>("pid")
            .unwrap()
            .unwrap();
        let ended = fixture
            .admin()
            .await
            .query(&format!(
                "SELECT pg_catalog.pg_terminate_backend({pid}) AS ended"
            ))
            .await
            .unwrap()[0]
            .try_get::<bool>("ended")
            .unwrap()
            .unwrap();
        let refused = super::super::release_supplied(
            &mut conn,
            &backend,
            &Standard::default(),
            &database,
            &defaults,
            &acl,
        )
        .await;
        drop(conn);
        let mut fresh = fixture.reconnect(&database, &login).await;
        let after = standard::read_state(&mut fresh).await.unwrap();
        let kept = fresh
            .query("SELECT pg_catalog.to_regclass('public.kept1731') IS NOT NULL AS kept")
            .await
            .unwrap()[0]
            .try_get::<bool>("kept")
            .unwrap()
            .unwrap();
        drop(fresh);
        fixture.drop().await;
        assert!(ended, "{server}: termination is an observed setup event");
        assert_eq!(
            before, after,
            "{server}: an unknown backend authorizes no standard write"
        );
        assert!(kept, "{server}");
        let remaining = refused.expect_err("a lost connection cannot authorize restoration");
        assert_eq!(remaining.len(), 1, "{server}: {remaining:?}");
        assert!(remaining[0].contains("backend"), "{server}: {remaining:?}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn restoration_keeps_one_transaction_and_retains_partial_repairs() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let (login, database) = fixture.confined().await;
        let mut conn = fixture.reconnect(&database, &login).await;
        let backend = sql::backend(&mut conn).await.unwrap();
        let defaults = sql::login_defaults(&mut conn).await.unwrap();
        let acl = standard::read_state(&mut conn).await.unwrap().acl;
        let mut receipts = Vec::new();
        for partial in [false, true] {
            if partial {
                fixture
                    .admin()
                    .await
                    .execute(&format!(
                        "ALTER DATABASE {database} SET log_min_duration_statement = 5"
                    ))
                    .await
                    .unwrap();
            }
            conn.execute(&format!(
                "SET ROLE pg_database_owner; COMMENT ON SCHEMA public IS 'repair1731'; \
                 SET ROLE NONE; ALTER DATABASE {database} SET statement_timeout = '7s'"
            ))
            .await
            .unwrap();
            for_tests::trace_release(&database);
            let released = super::super::release_supplied(
                &mut conn,
                &backend,
                &Standard::default(),
                &database,
                &defaults,
                &acl,
            )
            .await;
            let trace = for_tests::take_release_trace(&database);
            let state = standard::read_state(&mut conn).await.unwrap();
            let same = sql::on_backend(&mut conn, &backend).await.unwrap();
            receipts.push((partial, released, trace, state, same));
        }
        drop(conn);
        fixture.drop().await;
        for (partial, released, trace, state, same) in receipts {
            assert!(same, "{server}");
            assert_eq!(
                trace.len(),
                2,
                "{server}: both real transaction observations ran"
            );
            assert_eq!(
                trace[0], trace[1],
                "{server}: check, restoration and verification share one transaction"
            );
            assert_eq!(trace[0].principal, login.0, "{server}");
            assert!(
                !state.settings.contains_key("statement_timeout"),
                "{server}: ordinary repairs persist"
            );
            assert_eq!(
                state.public.unwrap().comment,
                Standard::default().public_comment,
                "{server}"
            );
            if partial {
                let remaining = released.expect_err("a forbidden setting remains named");
                assert!(
                    remaining
                        .iter()
                        .any(|s| s.contains("log_min_duration_statement=5")),
                    "{server}: {remaining:?}"
                );
                assert!(
                    remaining.iter().any(|s| s.starts_with("refused: ")),
                    "{server}: {remaining:?}"
                );
                assert_eq!(
                    state
                        .settings
                        .get("log_min_duration_statement")
                        .map(String::as_str),
                    Some("5")
                );
            } else {
                assert_eq!(released, Ok(()), "{server}");
                assert!(state.settings.is_empty(), "{server}: {:?}", state.settings);
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_refused_rehearsal_role_keeps_the_claim_without_undoing_other_cleanup() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let login = fixture
            .login("roles1731", "NOSUPERUSER NOCREATEDB CREATEROLE")
            .await;
        let database = fixture.database("checked1731", Some(&login.0)).await;
        let foreign = fixture.database("foreign1731", Some(&login.0)).await;
        let mut conn = fixture.reconnect(&database, &login).await;
        let backend = sql::backend(&mut conn).await.unwrap();
        let defaults = sql::login_defaults(&mut conn).await.unwrap();
        let acl = standard::read_state(&mut conn).await.unwrap().acl;
        let token = format!("{:032x}", rand::random::<u128>());
        let free = format!("pbps_rehearsal_0_{token}");
        let held = format!("pbps_rehearsal_1_{token}");
        conn.execute(&format!(
            "CREATE ROLE {free} NOLOGIN; CREATE ROLE {held} NOLOGIN"
        ))
        .await
        .unwrap();
        fixture.roles.extend([free.clone(), held.clone()]);
        fixture.run(&foreign, &[&format!(
            "CREATE TABLE public.role_kept1731(i integer); ALTER TABLE public.role_kept1731 OWNER TO {held}"
        )]).await;
        record::claim(
            &mut conn,
            &record::Claim::new(&token, defaults.clone(), acl.clone()),
        )
        .await
        .unwrap();
        record::record_roles(&mut conn, &[free.clone(), held.clone()])
            .await
            .unwrap();
        conn.execute(&format!(
            "ALTER DATABASE {database} SET statement_timeout = '7s'"
        ))
        .await
        .unwrap();
        let refused = super::super::release_supplied(
            &mut conn,
            &backend,
            &Standard::default(),
            &database,
            &defaults,
            &acl,
        )
        .await;
        let state = standard::read_state(&mut conn).await.unwrap();
        let claim_kept = record::read_claim(&mut conn).await.unwrap();
        let roles = fixture.admin().await.query(&format!(
            "SELECT rolname::text AS name FROM pg_catalog.pg_roles WHERE rolname IN ('{free}', '{held}') ORDER BY 1"
        )).await.unwrap().into_iter().map(|r| r.try_get::<&str>("name").unwrap().unwrap().to_owned()).collect::<Vec<_>>();
        fixture
            .run(&foreign, &["DROP TABLE public.role_kept1731"])
            .await;
        let recovered = super::super::release_supplied(
            &mut conn,
            &backend,
            &Standard::default(),
            &database,
            &defaults,
            &acl,
        )
        .await;
        let claim_gone = record::read_claim(&mut conn).await.unwrap().is_none();
        drop(conn);
        fixture.drop().await;
        assert_eq!(refused, Err(vec![format!("role {held}")]), "{server}");
        assert!(
            state.settings.is_empty(),
            "{server}: a role failure cannot undo the standard repair"
        );
        assert_eq!(
            roles.as_slice(),
            std::slice::from_ref(&held),
            "{server}: the independently removable role was removed"
        );
        assert_eq!(
            claim_kept
                .expect("a partial release retains its claim")
                .unwrap()
                .roles,
            [free, held]
        );
        assert_eq!(recovered, Ok(()), "{server}");
        assert!(
            claim_gone,
            "{server}: only the fully successful release removes the claim"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_backend_refusal_stays_terminal_even_if_a_later_backend_would_match() {
    for server in SERVERS {
        for unreadable in [false, true] {
            let mut fixture = Fixture::new(server);
            let login = fixture
                .login("terminal1731", "NOSUPERUSER NOCREATEDB NOCREATEROLE")
                .await;
            let database = fixture.database("checked1731", Some(&login.0)).await;
            let foreign = fixture.database("foreign1731", Some(&login.0)).await;
            let mut checked = fixture.reconnect(&database, &login).await;
            let backend = sql::backend(&mut checked).await.unwrap();
            let defaults = sql::login_defaults(&mut checked).await.unwrap();
            let acl = standard::read_state(&mut checked).await.unwrap().acl;
            checked
                .execute(&format!(
                    "ALTER DATABASE {database} SET statement_timeout = '7s'; \
                 CREATE TABLE public.kept1731(i integer)"
                ))
                .await
                .unwrap();
            let before = standard::read_state(&mut checked).await.unwrap();
            let mut other = fixture.reconnect(&foreign, &login).await;
            if unreadable {
                let pid = other
                    .query("SELECT pg_catalog.pg_backend_pid() AS pid")
                    .await
                    .unwrap()[0]
                    .try_get::<i32>("pid")
                    .unwrap()
                    .unwrap();
                let ended = fixture
                    .admin()
                    .await
                    .query(&format!(
                        "SELECT pg_catalog.pg_terminate_backend({pid}) AS ended"
                    ))
                    .await
                    .unwrap()[0]
                    .try_get::<bool>("ended")
                    .unwrap()
                    .unwrap();
                assert!(ended);
            }
            // Let the next transaction return to the original live backend.
            // This isolates terminal refusal from the independent fresh check:
            // that check could pass, but it must never be reached after refusal.
            for_tests::hop_release(&database, checked);
            let refused = super::super::release_supplied(
                &mut other,
                &backend,
                &Standard::default(),
                &database,
                &defaults,
                &acl,
            )
            .await;
            let matches = sql::on_backend(&mut other, &backend).await.unwrap();
            let after = standard::read_state(&mut other).await.unwrap();
            let kept = other
                .query("SELECT pg_catalog.to_regclass('public.kept1731') IS NOT NULL AS kept")
                .await
                .unwrap()[0]
                .try_get::<bool>("kept")
                .unwrap()
                .unwrap();
            drop(other);
            fixture.drop().await;
            assert!(
                matches,
                "{server}: a later check would match after this controlled return"
            );
            assert_eq!(
                before, after,
                "{server}: a backend refusal is terminal, even after returning"
            );
            assert!(kept, "{server}");
            let remaining = refused.expect_err("the first unconfirmed backend is terminal");
            assert_eq!(remaining.len(), 1, "{server}: {remaining:?}");
            assert!(remaining[0].contains("backend"), "{server}: {remaining:?}");
        }
    }
}
