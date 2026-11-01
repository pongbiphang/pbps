//! Target-only admission; owned scratch keeps its existing provisioning contract.
use super::{AssertionResult, run_on};
use pbps_db::{Conn, DbError, Driver};
use std::time::Duration;

/// Read from the planning connection, never assembled from connection strings.
#[derive(Debug)]
pub struct Target {
    cluster: String,
    database: String,
    login: String,
}
impl Target {
    pub async fn read(conn: &mut Conn) -> Result<Self, DbError> {
        if conn.driver() != Driver::Postgres {
            return Err(DbError::Refused(
                "PostgreSQL assertion target requires PostgreSQL".into(),
            ));
        }
        let cluster = crate::resolver::scratch::cluster_identity(conn).await?;
        let rows = conn.query("SELECT oid::text AS database, session_user::text AS login FROM pg_catalog.pg_database WHERE datname = pg_catalog.current_database()").await?;
        let row = rows
            .first()
            .ok_or_else(|| DbError::BadRow("missing assertion database identity".into()))?;
        Ok(Self {
            cluster,
            database: row
                .try_get::<&str>("database")?
                .ok_or_else(|| DbError::BadRow("NULL assertion database identity".into()))?
                .into(),
            login: row
                .try_get::<&str>("login")?
                .ok_or_else(|| DbError::BadRow("NULL assertion login".into()))?
                .into(),
        })
    }
}

/// Checks and SQL share one fresh connection; no checked session is substituted.
/// A failed admission is separate from the assertion's execution outcome.
pub async fn run_target(
    connection: &str,
    expected: &Target,
    sql: &str,
    timeout: Duration,
) -> Result<AssertionResult, DbError> {
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::time::timeout_at(deadline, async {
        let mut conn = Conn::connect(Driver::Postgres, connection).await?;
        let actual = Target::read(&mut conn).await?;
        if actual.cluster != expected.cluster || actual.database != expected.database {
            return Err(DbError::Refused(
                "PBPS_ASSERTION_CONN does not identify the planning target database".into(),
            ));
        }
        if actual.login == expected.login {
            return Err(DbError::Refused(
                "PBPS_ASSERTION_CONN requires a separate assertion login".into(),
            ));
        }
        privileges(&mut conn).await?;
        Ok(run_on(conn, sql, deadline).await)
    })
    .await
    .map_err(|_| {
        DbError::Refused("assertion deadline exceeded during target checks or execution".into())
    })?
}

async fn privileges(conn: &mut Conn) -> Result<(), DbError> {
    // has_*_privilege includes PUBLIC and inherited grants. Ask it under every
    // SET-able identity too: SET then INHERIT need not be SET-able from the login.
    let mut parts = vec![
        "SELECT 'administrator'::text AS permission, rolname::text AS object FROM pg_catalog.pg_roles WHERE (rolsuper OR rolcreaterole OR rolcreatedb OR rolreplication OR rolbypassrls) AND pg_catalog.pg_has_role(session_user, oid, 'MEMBER')".to_owned(),
        // READ ONLY cannot contain server-file effects or another connection
        // opened by a server program. Reuse actors for SET then INHERIT too.
        "SELECT 'host file/program', r.rolname::text FROM pg_catalog.pg_roles r CROSS JOIN actors a WHERE r.rolname IN ('pg_read_server_files','pg_write_server_files','pg_execute_server_program') AND pg_catalog.pg_has_role(a.oid,r.oid,'USAGE') IS DISTINCT FROM false".into(),
        "SELECT 'ADMIN OPTION', r.rolname::text FROM pg_catalog.pg_auth_members m JOIN pg_catalog.pg_roles r ON r.oid=m.roleid CROSS JOIN actors a WHERE m.admin_option AND pg_catalog.pg_has_role(a.oid,m.member,'USAGE')".into(),
        "SELECT 'ALTER SYSTEM', p.parname::text FROM pg_catalog.pg_parameter_acl p CROSS JOIN actors a WHERE pg_catalog.has_parameter_privilege(a.oid,p.parname,'ALTER SYSTEM') IS DISTINCT FROM false".into(),
        "SELECT 'CREATE', d.datname::text FROM pg_catalog.pg_database d CROSS JOIN actors a WHERE d.datname=pg_catalog.current_database() AND pg_catalog.has_database_privilege(a.oid,d.oid,'CREATE') IS DISTINCT FROM false".into(),
        "SELECT 'CREATE', n.nspname::text FROM pg_catalog.pg_namespace n CROSS JOIN actors a WHERE n.oid<>pg_catalog.pg_my_temp_schema() AND NOT pg_catalog.pg_is_other_temp_schema(n.oid) AND pg_catalog.has_schema_privilege(a.oid,n.oid,'CREATE') IS DISTINCT FROM false".into(),
    ];
    for (function, suffix, privileges) in [
        (
            "has_table_privilege",
            "",
            &[
                "INSERT",
                "UPDATE",
                "DELETE",
                "TRUNCATE",
                "REFERENCES",
                "TRIGGER",
                "MAINTAIN",
            ][..],
        ),
        (
            "has_any_column_privilege",
            " (column)",
            &["INSERT", "UPDATE", "REFERENCES"][..],
        ),
    ] {
        for privilege in privileges {
            // PUBLIC's built-in pg_settings UPDATE changes only this session's
            // GUCs (measured on PG16/18), not target data or DDL permissions.
            let scope = if *privilege == "UPDATE" {
                " AND c.oid<>'pg_catalog.pg_settings'::regclass"
            } else {
                ""
            };
            let check = format!("pg_catalog.{function}(a.oid,c.oid,'{privilege}')");
            // MAINTAIN starts at PG17. CASE keeps PG16 from evaluating an
            // unsupported privilege while every readable PG18 answer is checked.
            let check = if *privilege == "MAINTAIN" {
                format!(
                    "CASE WHEN pg_catalog.current_setting('server_version_num')::integer >= 170000 THEN {check} ELSE false END"
                )
            } else {
                check
            };
            parts.push(format!("SELECT '{privilege}{suffix}', pg_catalog.quote_ident(n.nspname)||'.'||pg_catalog.quote_ident(c.relname) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace CROSS JOIN actors a WHERE c.relkind IN ('r','p','v','m','f'){scope} AND ({check}) IS DISTINCT FROM false"));
        }
    }
    parts.push("SELECT 'USAGE/UPDATE (sequence)', pg_catalog.quote_ident(n.nspname)||'.'||pg_catalog.quote_ident(c.relname) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace CROSS JOIN actors a WHERE c.relkind='S' AND pg_catalog.has_sequence_privilege(a.oid,c.oid,'USAGE,UPDATE') IS DISTINCT FROM false".into());
    // Ownership permits DDL even after the owner revokes its own ordinary ACL.
    // These are the owner-bearing catalogs, not just managed tables/schemas.
    for (catalog, owner) in [
        ("pg_database", "datdba"),
        ("pg_namespace", "nspowner"),
        ("pg_class", "relowner"),
        ("pg_type", "typowner"),
        ("pg_proc", "proowner"),
        ("pg_operator", "oprowner"),
        ("pg_opclass", "opcowner"),
        ("pg_opfamily", "opfowner"),
        ("pg_collation", "collowner"),
        ("pg_conversion", "conowner"),
        ("pg_language", "lanowner"),
        ("pg_extension", "extowner"),
        ("pg_ts_dict", "dictowner"),
        ("pg_ts_config", "cfgowner"),
        ("pg_foreign_data_wrapper", "fdwowner"),
        ("pg_foreign_server", "srvowner"),
        ("pg_event_trigger", "evtowner"),
        ("pg_publication", "pubowner"),
        ("pg_statistic_ext", "stxowner"),
        ("pg_subscription", "subowner"),
        ("pg_tablespace", "spcowner"),
        ("pg_largeobject_metadata", "lomowner"),
    ] {
        let scope = if catalog == "pg_database" {
            " AND t.datname=pg_catalog.current_database()"
        } else {
            ""
        };
        parts.push(format!("SELECT 'ownership (DDL)', '{catalog} '||t.oid::text FROM pg_catalog.{catalog} t CROSS JOIN actors a WHERE pg_catalog.pg_has_role(a.oid,t.{owner},'USAGE'){scope}"));
    }
    let sql = format!(
        "WITH actors AS (SELECT oid FROM pg_catalog.pg_roles WHERE pg_catalog.pg_has_role(session_user,oid,'SET')), violations AS ({}) SELECT permission,object FROM violations LIMIT 1",
        parts.join(" UNION ALL ")
    );
    if let Some(row) = conn.query(&sql).await?.first() {
        let permission = row
            .try_get::<&str>("permission")?
            .ok_or_else(|| DbError::BadRow("unreadable assertion privilege".into()))?;
        let object = row
            .try_get::<&str>("object")?
            .ok_or_else(|| DbError::BadRow("unreadable assertion privilege object".into()))?;
        return Err(DbError::Refused(format!(
            "assertion login has {} permission on {}; require a separate login without write or DDL permissions",
            pbps_model::escape::Escaped(permission),
            pbps_model::escape::Escaped(object)
        )));
    }
    Ok(())
}
