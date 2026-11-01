//! Target admission shares #1165's driver/transaction controls and fixture.
use super::*;
use pbps_pg::assertions::{Target, run_target};

#[tokio::test]
#[ignore = "needs PostgreSQL 16 and 18"]
async fn a_target_assertion_checks_effective_write_and_ddl_rights_before_user_sql() {
    for server in servers() {
        let mut f = Fixture::new(server, "targetpriv2401").await;
        let target = Target::read(&mut f.admin).await.unwrap();
        let public = f.admin.query(&format!("SELECT has_table_privilege('{}','pg_catalog.pg_settings','UPDATE') AS session_only", f.role)).await.unwrap();
        assert_eq!(
            public[0].try_get::<bool>("session_only").unwrap(),
            Some(true)
        );
        assert!(matches!(
            run_target(&f.assertion, &target, "SELECT 1 WHERE false", DEADLINE)
                .await
                .unwrap(),
            AssertionResult::Pass(_)
        ));
        let role = &f.role;
        let group = format!("{role}_writer");
        f.admin
            .execute(&format!("CREATE ROLE {group}; GRANT {group} TO {role}"))
            .await
            .unwrap();
        let mut grants = vec![
            (
                format!("GRANT UPDATE ON app.orders TO {role}"),
                format!("REVOKE UPDATE ON app.orders FROM {role}"),
                "UPDATE",
            ),
            (
                format!("GRANT INSERT(total) ON app.orders TO {role}"),
                format!("REVOKE INSERT(total) ON app.orders FROM {role}"),
                "INSERT",
            ),
            (
                format!("GRANT DELETE ON app.orders TO {group}"),
                format!("REVOKE DELETE ON app.orders FROM {group}"),
                "DELETE",
            ),
            (
                format!("GRANT CREATE ON SCHEMA app TO {role}"),
                format!("REVOKE CREATE ON SCHEMA app FROM {role}"),
                "CREATE",
            ),
            (
                format!("GRANT CREATE ON DATABASE {} TO {role}", f.database),
                format!("REVOKE CREATE ON DATABASE {} FROM {role}", f.database),
                "CREATE",
            ),
            (
                format!(
                    "ALTER TABLE app.orders OWNER TO {role}; REVOKE ALL ON app.orders FROM {role}"
                ),
                format!(
                    "ALTER TABLE app.orders OWNER TO postgres; GRANT SELECT ON app.orders TO {role}"
                ),
                "ownership",
            ),
            (
                format!("GRANT ALTER SYSTEM ON PARAMETER work_mem TO {role}"),
                format!("REVOKE ALTER SYSTEM ON PARAMETER work_mem FROM {role}"),
                "ALTER SYSTEM",
            ),
        ];
        let version = f
            .admin
            .query("SELECT current_setting('server_version_num')::int AS version")
            .await
            .unwrap();
        if version[0].try_get::<i32>("version").unwrap().unwrap() >= 170000 {
            f.admin
                .execute("CREATE MATERIALIZED VIEW app.observed AS SELECT * FROM app.orders")
                .await
                .unwrap();
            grants.push((
                format!("GRANT MAINTAIN ON app.observed TO {role}"),
                format!("REVOKE MAINTAIN ON app.observed FROM {role}"),
                "MAINTAIN",
            ));
        }
        for (grant, revoke, permission) in grants {
            f.admin.execute(&grant).await.unwrap();
            // If admission is bypassed, the sentinel is an execution SqlError,
            // not this named pre-execution refusal.
            let error = run_target(&f.assertion, &target, "SELECT 1/0", DEADLINE)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(permission), "{grant}: {error:?}");
            f.admin.execute(&revoke).await.unwrap();
        }
        // A role reached by SET then INHERIT, including column-only rights.
        let leaf = format!("{role}_leaf");
        f.admin.execute(&format!("CREATE ROLE {leaf}; GRANT {leaf} TO {group} WITH INHERIT TRUE, SET FALSE; GRANT {group} TO {role} WITH INHERIT FALSE, SET TRUE; GRANT UPDATE(total) ON app.orders TO {leaf}")).await.unwrap();
        let error = run_target(&f.assertion, &target, "SELECT 1/0", DEADLINE)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("UPDATE"), "{error:?}");
        f.admin.execute(&format!("REVOKE UPDATE(total) ON app.orders FROM {leaf}; REVOKE {leaf} FROM {group}; REVOKE {group} FROM {role}; DROP ROLE {leaf}; DROP ROLE {group}")).await.unwrap();
        // Same-name lookups cannot hide rights on an unmanaged object.
        f.admin.execute(&format!("CREATE SCHEMA hidden; CREATE TABLE hidden.unmanaged(n integer); GRANT INSERT ON hidden.unmanaged TO {role}")).await.unwrap();
        assert!(
            run_target(&f.assertion, &target, "SELECT 1/0", DEADLINE)
                .await
                .unwrap_err()
                .to_string()
                .contains("INSERT")
        );
        f.admin
            .execute(&format!("REVOKE INSERT ON hidden.unmanaged FROM {role}"))
            .await
            .unwrap();
        assert!(matches!(
            run_target(&f.assertion, &target, "SELECT 1 WHERE false", DEADLINE)
                .await
                .unwrap(),
            AssertionResult::Pass(_)
        ));
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs PostgreSQL 16 and 18"]
async fn target_identity_and_permission_reads_cannot_be_replaced_by_empty_answers() {
    for server in servers() {
        let mut f = Fixture::new(server, "targetid2401").await;
        let mut other = Fixture::new(f.base.clone(), "targetother2401").await;
        let target = Target::read(&mut other.admin).await.unwrap();
        let error = run_target(&f.assertion, &target, "SELECT 1/0", DEADLINE)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("target database"), "{error:?}");
        let target = Target::read(&mut f.admin).await.unwrap();
        let admin = settings(&f.base, &[("dbname", &f.database)]);
        assert!(
            run_target(&admin, &target, "SELECT 1/0", DEADLINE)
                .await
                .unwrap_err()
                .to_string()
                .contains("separate assertion login")
        );
        f.admin
            .execute("REVOKE SELECT ON pg_catalog.pg_type FROM PUBLIC")
            .await
            .unwrap();
        let error = run_target(&f.assertion, &target, "SELECT 1 WHERE false", DEADLINE)
            .await
            .unwrap_err();
        assert_eq!(
            error.server_error_code().as_deref(),
            Some("42501"),
            "{error:?}"
        );
        f.finish().await;
        other.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs PostgreSQL 16 and 18"]
async fn target_queries_keep_the_original_read_only_transaction_and_result_contract() {
    for server in servers() {
        let mut f = Fixture::new(server, "targetquery2401").await;
        let target = Target::read(&mut f.admin).await.unwrap();
        for sql in ["SELECT NULL", "SELECT false", "SELECT ''"] {
            assert!(
                matches!(
                    run_target(&f.assertion, &target, sql, DEADLINE)
                        .await
                        .unwrap(),
                    AssertionResult::Violation(_)
                ),
                "{sql}"
            );
        }
        for (sql, state) in [
            ("SELECT 1/0", "22012"),
            ("SELECT app.definer_write()", "25006"),
            ("COMMIT; SELECT app.definer_write()", "42601"),
            ("SET TRANSACTION READ WRITE", "25001"),
        ] {
            sql_error(
                run_target(&f.assertion, &target, sql, DEADLINE)
                    .await
                    .unwrap(),
                state,
            );
        }
        for sql in ["COMMIT", "ROLLBACK", "COMMIT AND CHAIN"] {
            assert!(
                matches!(
                    run_target(&f.assertion, &target, sql, DEADLINE)
                        .await
                        .unwrap(),
                    AssertionResult::BoundaryViolation
                ),
                "{sql}"
            );
            assert!(matches!(
                run_target(&f.assertion, &target, "SELECT 1 WHERE false", DEADLINE)
                    .await
                    .unwrap(),
                AssertionResult::Pass(_)
            ));
        }
        let rows = f
            .admin
            .query("SELECT count(*)::int AS n FROM app.orders")
            .await
            .unwrap();
        assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(1));
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs PostgreSQL 16 and 18"]
async fn a_different_cluster_with_the_same_database_name_is_not_the_target() {
    let mut a = Fixture::new(servers()[0].clone(), "targetcluster2401").await;
    let b = Fixture::new(servers()[1].clone(), "targetcluster2401").await;
    assert_eq!(a.database, b.database);
    let target = Target::read(&mut a.admin).await.unwrap();
    let error = run_target(&b.assertion, &target, "SELECT 1/0", DEADLINE)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("target database"), "{error:?}");
    a.finish().await;
    b.finish().await;
}

// Keep authority reachability in admission, not in a second assertion runner.
async fn host_capability_is_refused(capability: &str, tag: &str) {
    for server in servers() {
        let mut f = Fixture::new(server, tag).await;
        let target = Target::read(&mut f.admin).await.unwrap();
        let role = f.role.clone();
        let parent = format!("{role}_set");
        let leaf = format!("{role}_leaf");
        let marker = format!("/tmp/issue-2401-{tag}-{}", std::process::id());
        let effect = match capability {
            "pg_read_server_files" => "PERFORM pg_read_file('/etc/hostname')".into(),
            "pg_write_server_files" => format!("COPY (SELECT 2401) TO '{marker}'"),
            "pg_execute_server_program" => format!(
                "COPY (SELECT 2401) TO PROGRAM 'psql -U postgres -d {} -c \"INSERT INTO app.orders VALUES (7,2401)\"'",
                f.database
            ),
            _ => unreachable!(),
        };
        f.admin
            .execute(&format!("CREATE ROLE {parent}; CREATE ROLE {leaf}"))
            .await
            .unwrap();
        let body = if capability == "pg_read_server_files" {
            effect.clone()
        } else {
            format!("EXECUTE '{}'", effect.replace('\'', "''"))
        };
        f.admin.execute(&format!("CREATE FUNCTION app.host_effect() RETURNS boolean LANGUAGE plpgsql SECURITY INVOKER AS $body$ BEGIN {body}; RETURN false; END $body$")).await.unwrap();
        let sql = "SELECT 1 WHERE app.host_effect()";
        // Native negative: without the host capability the invoker function
        // cannot perform the effect, even though it has ordinary EXECUTE.
        sql_error(
            run_target(&f.assertion, &target, sql, DEADLINE)
                .await
                .unwrap(),
            "42501",
        );
        assert!(matches!(
            run_target(&f.assertion, &target, "SELECT 1 WHERE false", DEADLINE)
                .await
                .unwrap(),
            AssertionResult::Pass(_)
        ));
        let mut observations = Vec::new();
        for (shape, grant, revoke) in [
            (
                "direct",
                format!("GRANT {capability} TO {role}"),
                format!("REVOKE {capability} FROM {role}"),
            ),
            (
                "inherit only",
                format!(
                    "GRANT {capability} TO {leaf} WITH INHERIT TRUE, SET FALSE; GRANT {leaf} TO {role} WITH INHERIT TRUE, SET FALSE"
                ),
                format!("REVOKE {leaf} FROM {role}; REVOKE {capability} FROM {leaf}"),
            ),
            (
                "SET then INHERIT",
                format!(
                    "GRANT {capability} TO {leaf} WITH INHERIT TRUE, SET FALSE; GRANT {leaf} TO {parent} WITH INHERIT TRUE, SET FALSE; GRANT {parent} TO {role} WITH INHERIT FALSE, SET TRUE"
                ),
                format!(
                    "REVOKE {parent} FROM {role}; REVOKE {leaf} FROM {parent}; REVOKE {capability} FROM {leaf}"
                ),
            ),
        ] {
            f.admin.execute(&grant).await.unwrap();
            let result = run_target(&f.assertion, &target, sql, DEADLINE).await;
            let named = matches!(&result,Err(e) if e.to_string().contains(capability));
            observations.push((shape, named, format!("{result:?}")));
            f.admin.execute(&revoke).await.unwrap();
        }
        // A membership that permits neither SET nor inheritance is not an
        // effective capability. Do not reject it merely for existing in a graph.
        f.admin
            .execute(&format!(
                "GRANT {capability} TO {leaf}; GRANT {leaf} TO {role} WITH INHERIT FALSE, SET FALSE"
            ))
            .await
            .unwrap();
        assert!(matches!(
            run_target(&f.assertion, &target, "SELECT 1 WHERE false", DEADLINE)
                .await
                .unwrap(),
            AssertionResult::Pass(_)
        ));
        f.admin.execute(&format!("REVOKE {leaf} FROM {role}; REVOKE {capability} FROM {leaf}; DROP ROLE {parent}; DROP ROLE {leaf}")).await.unwrap();
        let rows = f
            .admin
            .query("SELECT count(*)::int AS n FROM app.orders WHERE id=7")
            .await
            .unwrap();
        let writes = rows[0].try_get::<i32>("n").unwrap();
        let files = f
            .admin
            .query(&format!(
                "SELECT (pg_stat_file('{marker}',true)).size IS NOT NULL AS present"
            ))
            .await
            .unwrap();
        let file_present = files[0].try_get::<bool>("present").unwrap();
        // Revert witnesses deliberately enable effects; always clean the owned
        // database/roles before reporting the failed pre-execution assertion.
        f.finish().await;
        assert!(
            observations.iter().all(|(_, named, _)| *named),
            "{capability}: {observations:?}"
        );
        assert_eq!(writes, Some(0));
        assert_eq!(file_present, Some(false));
    }
}

#[tokio::test]
#[ignore = "needs PostgreSQL 16 and 18"]
async fn read_server_files_is_refused_through_the_actor_closure() {
    host_capability_is_refused("pg_read_server_files", "hostread2401").await;
}
#[tokio::test]
#[ignore = "needs PostgreSQL 16 and 18"]
async fn write_server_files_is_refused_through_the_actor_closure() {
    host_capability_is_refused("pg_write_server_files", "hostwrite2401").await;
}
#[tokio::test]
#[ignore = "needs PostgreSQL 16 and 18"]
async fn execute_server_program_is_refused_through_the_actor_closure() {
    host_capability_is_refused("pg_execute_server_program", "hostexec2401").await;
}
