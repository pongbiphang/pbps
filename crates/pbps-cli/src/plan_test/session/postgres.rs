//! The PostgreSQL half of [`TestEngine`]: the original provisioning, the
//! administrator connection, the incoming-hop preparation, the comparison
//! snapshot, and the grant, fixture and assertion protocol, moved here
//! unchanged from `session.rs` and `snapshot.rs` (#2798).
use super::{
    AssertionFuture, Conn, HopContext, Provisioned, Session, SessionFuture, TestEngine,
    TestScratch, prerequisites,
};
use crate::{
    db::Target,
    plan_test::snapshot::{Allocator, Rows, Snapshot, insert},
};
use anyhow::{Context as _, ensure};
use pbps_db::{Driver, query::AssertionResult};
use pbps_dialect::Dialect as _;
use pbps_model::{Schema, TableName};
use std::{collections::BTreeMap, time::Duration};

/// PostgreSQL's owned-test session, one per engine record.
pub(crate) struct PostgresTest;

impl TestEngine for PostgresTest {
    fn open<'a>(&self, scratch: &'a TestScratch<'a>) -> SessionFuture<'a, Provisioned> {
        Box::pin(open(scratch))
    }
    fn admin<'a>(&self, scratch: &'a TestScratch<'_>) -> SessionFuture<'a, Conn> {
        Box::pin(admin(scratch))
    }
    fn prepare<'a>(&self, session: &'a Session<'_>, hop: HopContext<'a>) -> SessionFuture<'a, ()> {
        Box::pin(prepare(session, hop))
    }
    fn namespaces<'a>(
        &self,
        session: &'a Session<'_>,
        schema: &'a Schema,
    ) -> SessionFuture<'a, ()> {
        Box::pin(namespaces(session, schema))
    }
    fn snapshot<'a>(
        &self,
        session: &'a Session<'_>,
        schema: &'a Schema,
    ) -> SessionFuture<'a, Snapshot> {
        Box::pin(snapshot(session, schema))
    }
    fn revoke_fixture_routines<'a>(&self, session: &'a Session<'_>) -> SessionFuture<'a, ()> {
        Box::pin(revoke_fixture_routines(session))
    }
    fn grants<'a>(&self, session: &'a Session<'_>, schema: &'a Schema) -> SessionFuture<'a, ()> {
        Box::pin(grants(session, schema))
    }
    fn fixtures<'a>(&self, session: &'a Session<'_>) -> SessionFuture<'a, Conn> {
        Box::pin(fixtures(session))
    }
    fn run_fixture<'a>(
        &self,
        conn: &'a mut Conn,
        sql: &'a str,
        timeout_ms: u128,
    ) -> SessionFuture<'a, ()> {
        Box::pin(run_fixture(conn, sql, timeout_ms))
    }
    fn seal<'a>(&self, _session: &'a Session<'_>) -> SessionFuture<'a, ()> {
        // Nothing to seal: the write boundary of a PostgreSQL assertion is the
        // READ ONLY transaction each one opens for itself (`assertion`), so there
        // is no database-wide mode to switch before the first of them.
        Box::pin(std::future::ready(Ok(())))
    }
    fn run_assertion<'a>(
        &self,
        session: &'a Session<'_>,
        sql: &'a str,
        timeout: Duration,
    ) -> AssertionFuture<'a> {
        Box::pin(assertion(session, sql, timeout))
    }
}

async fn open(scratch: &TestScratch<'_>) -> anyhow::Result<Provisioned> {
    let admin = scratch.connection();
    ensure!(
        !admin.contains("://"),
        "the owned adapter must return a keyword connection"
    );
    let mut conn = Conn::connect(Driver::Postgres, admin).await?;
    let rows = conn.query("SELECT current_user::text AS role").await?;
    let provision_role: pbps_model::RoleName = rows
        .first()
        .context("missing owned provision role result")?
        .try_get::<&str>("role")?
        .context("NULL owned provision role")?
        .into();
    // The frozen declarations must never capture an infrastructure identity.
    let roles = ["apply", "fixture", "assertion"]
        .map(|kind| format!("pbps_test_{kind}_{}", scratch.run_id()));
    let mut passwords = Vec::new();
    for role in &roles {
        let password = format!("{:032x}", rand::random::<u128>());
        let sql = format!(
            concat!(
                "CREATE ROLE {role} LOGIN NOSUPERUSER NOCREATEDB ",
                "NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD '{password}'"
            ),
            role = role,
            password = password
        );
        conn.execute(&sql).await?;
        passwords.push(password);
    }
    let [apply, fixture, assertion] = &roles;
    conn.execute(&format!("CREATE DATABASE pbps_test OWNER {apply}"))
        .await?;
    let permissions = format!(
        concat!(
            "REVOKE ALL ON DATABASE pbps_test FROM PUBLIC; ",
            "GRANT CONNECT ON DATABASE pbps_test TO {apply}, {fixture}, {assertion}; ",
            "GRANT TEMP ON DATABASE pbps_test TO {fixture}"
        ),
        apply = apply,
        fixture = fixture,
        assertion = assertion
    );
    conn.execute(&permissions).await?;
    drop(conn);
    let db = format!("{admin} dbname=pbps_test");
    let mut conn = Conn::connect(Driver::Postgres, &db).await?;
    conn.execute("REVOKE CREATE ON SCHEMA public FROM PUBLIC")
        .await?;
    drop(conn);
    let connection = |role: &str, password: &str| format!("{db} user={role} password={password}");
    Ok(Provisioned {
        target: Target::test_owned(
            connection(apply, &passwords[0]),
            roles
                .iter()
                .map(|r| r.as_str().into())
                .chain([provision_role])
                .collect(),
        ),
        fixture: connection(fixture, &passwords[1]),
        assertion: connection(assertion, &passwords[2]),
        roles,
    })
}

async fn admin(scratch: &TestScratch<'_>) -> anyhow::Result<Conn> {
    let db = format!("{} dbname=pbps_test", scratch.connection());
    Ok(Conn::connect(Driver::Postgres, &db).await?)
}

async fn prepare(session: &Session<'_>, hop: HopContext<'_>) -> anyhow::Result<()> {
    let schema = &hop.incoming.loaded.schema;
    let mut conn = session.admin().await?;
    // Ordinary bootstrap requires cluster roles to exist (ADR-0010 §3).
    // Only this owned cluster's administrator supplies those prerequisites;
    // roles keep their ordinary scope and existing image roles stay untouched.
    let roles = prerequisites::roles(hop);
    prerequisites::complete_renames(&mut conn, &roles.renames).await?;
    prerequisites::create_missing(&mut conn, &roles.introduced).await?;
    namespaces(session, schema).await
}

async fn namespaces(session: &Session<'_>, schema: &Schema) -> anyhow::Result<()> {
    let mut conn = session.admin().await?;
    let apply = &session.roles[0];
    // Namespaces are provisioning prerequisites of ordinary bootstrap,
    // not managed changes. Reuse its inventory in this fresh owned database.
    for namespace in crate::deploy::schemas_declared(schema).into_keys() {
        conn.execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS {} AUTHORIZATION {apply}",
            quoted(&namespace)?
        ))
        .await?;
    }
    Ok(())
}

/// Reads rows first, then probes the allocators: a probe consumes a value, and
/// this database is discarded afterwards. `nextval` is the engine's own answer
/// for increments, caches and exhaustion; `last_value + 1` is a guess.
async fn snapshot(session: &Session<'_>, schema: &Schema) -> anyhow::Result<Snapshot> {
    let mut conn = session.admin().await?;
    let mut tables = BTreeMap::new();
    for name in schema.tables.keys() {
        let relation = table(name)?;
        let literal = relation.replace('\'', "''");
        let sql = format!(
            "SELECT a.attname::text AS name FROM pg_catalog.pg_attribute a \
             WHERE a.attrelid = '{literal}'::pg_catalog.regclass AND a.attnum > 0 \
             AND NOT a.attisdropped ORDER BY a.attname COLLATE \"C\""
        );
        let mut columns = Vec::new();
        for row in conn.query(&sql).await? {
            columns.push(
                row.try_get::<&str>("name")?
                    .context("a column has no name")?
                    .to_owned(),
            );
        }
        let cells = columns
            .iter()
            .map(|c| quoted(c).map(|c| format!("{c}::pg_catalog.text")))
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(", ");
        // jsonb text keeps NULL apart from '' and every value as its own text.
        let sql = format!(
            "SELECT pg_catalog.to_jsonb(ARRAY[{cells}]::pg_catalog.text[])::pg_catalog.text AS row, \
             pg_catalog.count(*)::pg_catalog.text AS n FROM {relation} GROUP BY 1"
        );
        let mut rows = BTreeMap::new();
        for row in conn.query(&sql).await? {
            let cells: Vec<Option<String>> =
                serde_json::from_str(row.try_get::<&str>("row")?.context("a row has no text")?)?;
            let count = row.try_get::<&str>("n")?.context("a row has no count")?;
            rows.insert(cells, count.parse::<u64>()?);
        }
        tables.insert(name.raw().to_string(), Rows { columns, rows });
    }
    // The ledger's own tables are not managed: their allocator differs between
    // a replay and a bootstrap by construction. Every other user sequence is
    // compared, so an allocator reached only through a function default is
    // never omitted.
    let sql = "SELECT n.nspname::text AS schema, c.relname::text AS name, \
        tn.nspname::text AS owner_schema, t.relname::text AS owner_table, \
        a.attname::text AS owner_column \
        FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
        LEFT JOIN pg_catalog.pg_depend d ON d.objid = c.oid \
          AND d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass \
          AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass \
          AND d.deptype IN ('a', 'i') \
        LEFT JOIN pg_catalog.pg_class t ON t.oid = d.refobjid \
        LEFT JOIN pg_catalog.pg_namespace tn ON tn.oid = t.relnamespace \
        LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = t.oid AND a.attnum = d.refobjsubid \
        WHERE c.relkind = 'S' AND n.nspname !~ '^pg_' AND n.nspname <> 'information_schema' \
        ORDER BY 1, 2";
    let mut allocators = BTreeMap::new();
    for row in conn.query(sql).await? {
        let text = |column: &str| -> anyhow::Result<Option<String>> {
            Ok(row.try_get::<&str>(column)?.map(str::to_owned))
        };
        let (owner_schema, owner_table) = (text("owner_schema")?, text("owner_table")?);
        if owner_schema.as_deref() == Some(pbps_pg::state::LEDGER_SCHEMA)
            && [
                pbps_db::ledger::STATE_TABLE_NAME,
                pbps_db::ledger::LOCK_TABLE_NAME,
            ]
            .contains(&owner_table.as_deref().unwrap_or_default())
        {
            continue;
        }
        let name = pbps_model::TableName::new(
            text("schema")?.context("a sequence has no schema")?,
            text("name")?.context("a sequence has no name")?,
        );
        let shown = name.shown().to_string();
        let used_by = match (owner_schema, owner_table, text("owner_column")?) {
            (Some(schema), Some(table), Some(column)) => Some(format!("{schema}.{table}.{column}")),
            _ => None,
        };
        let literal = table(&name)?.replace('\'', "''");
        let next = conn
            .query(&format!(
                "SELECT pg_catalog.nextval('{literal}'::pg_catalog.regclass)::pg_catalog.text AS next"
            ))
            .await
            .with_context(|| format!("the next value of sequence {shown} cannot be established"))?;
        let next = next
            .first()
            .and_then(|r| r.try_get::<&str>("next").transpose())
            .transpose()?
            .with_context(|| format!("sequence {shown} gave no next value"))?
            .to_owned();
        insert(
            &mut allocators,
            Allocator {
                name: name.raw().to_string(),
                used_by,
                next,
            },
        );
    }
    Ok(Snapshot { tables, allocators })
}

/// The PostgreSQL identifier quoting every statement here goes through.
pub(super) fn quoted(name: &str) -> anyhow::Result<String> {
    Ok(pbps_pg::Postgres::new().quote_ident(name)?)
}
pub(super) fn table(name: &TableName) -> anyhow::Result<String> {
    Ok(format!("{}.{}", quoted(&name.schema)?, quoted(&name.name)?))
}

async fn revoke_fixture_routines(session: &Session<'_>) -> anyhow::Result<()> {
    let mut conn = session.admin().await?;
    let fixture = &session.roles[1];
    // Temporary runner grants are not declaration grants; leave the ordinary
    // rebuild ACL guard intact and refresh these only before fixture SQL.
    let sql = format!(
        concat!(
            "SELECT DISTINCT n.nspname AS schema FROM pg_catalog.pg_proc p ",
            "JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace ",
            "CROSS JOIN LATERAL pg_catalog.aclexplode(p.proacl) a ",
            "WHERE a.grantee='{fixture}'::pg_catalog.regrole ",
            "AND a.privilege_type='EXECUTE'"
        ),
        fixture = fixture
    );
    for row in conn.query(&sql).await? {
        let namespace = quoted(
            row.try_get::<&str>("schema")?
                .context("missing routine namespace")?,
        )?;
        conn.execute(&format!(
            "REVOKE EXECUTE ON ALL ROUTINES IN SCHEMA {namespace} FROM {fixture}"
        ))
        .await?;
    }
    Ok(())
}

async fn grants(session: &Session<'_>, schema: &Schema) -> anyhow::Result<()> {
    let mut conn = session.admin().await?;
    let [_, fixture, assertion] = &session.roles;
    // Check every permanent namespace, including those with no managed table.
    // Engine identity, not a pg_temp name-prefix guess, excludes temp helpers.
    let sql = format!(
        concat!(
            "SELECT pg_catalog.has_database_privilege('{fixture}', current_database(), 'CREATE') ",
            "OR pg_catalog.has_database_privilege('{assertion}', current_database(), 'CREATE') ",
            "OR EXISTS (SELECT 1 FROM pg_catalog.pg_namespace n ",
            "WHERE n.oid <> pg_catalog.pg_my_temp_schema() ",
            "AND NOT pg_catalog.pg_is_other_temp_schema(n.oid) AND (",
            "pg_catalog.has_schema_privilege('{fixture}', n.oid, 'CREATE') OR ",
            "pg_catalog.has_schema_privilege('{assertion}', n.oid, 'CREATE'))) AS unsafe"
        ),
        fixture = fixture,
        assertion = assertion
    );
    let rows = conn.query(&sql).await?;
    let permanent_create = rows
        .first()
        .context("missing CREATE privilege result")?
        .try_get::<bool>("unsafe")?;
    ensure!(
        permanent_create == Some(false),
        "fixture/assertion privileges permit permanent CREATE; remove broad grants, including any PUBLIC grant"
    );
    for id in schema.modules.keys() {
        let pbps_model::ModuleId::Routine(routine) = id else {
            continue;
        };
        let namespace = quoted(&routine.name.schema)?;
        let signature = format!(
            "{}.{}({})",
            namespace,
            quoted(&routine.name.name)?,
            routine
                .args
                .iter()
                .map(|a| a.raw())
                .collect::<Vec<_>>()
                .join(",")
        );
        // Declared routines are trusted schema code (ADR-0019 §3/§6).
        // Ordinary writers must execute invoker callees even when PostgreSQL
        // records no dependency for a string/PLpgSQL body or a domain default.
        // This changes neither PUBLIC nor the assertion identity's grants.
        conn.execute(&format!(
            concat!(
                "GRANT USAGE ON SCHEMA {namespace} TO {fixture}; ",
                "GRANT EXECUTE ON ROUTINE {signature} TO {fixture}"
            ),
            namespace = namespace,
            signature = signature,
            fixture = fixture
        ))
        .await?;
    }
    for (name, declared) in &schema.tables {
        let t = table(name)?;
        let namespace = quoted(&name.schema)?;
        let sql = format!(
            concat!(
                "GRANT USAGE ON SCHEMA {namespace} TO {fixture}, {assertion}; ",
                "REVOKE ALL ON {t} FROM {fixture}, {assertion}; ",
                "GRANT SELECT ON {t} TO {fixture}, {assertion}"
            ),
            namespace = namespace,
            t = t,
            fixture = fixture,
            assertion = assertion
        );
        conn.execute(&sql).await?;
        if declared.data.is_none() {
            conn.execute(&format!("GRANT INSERT, UPDATE, DELETE ON {t} TO {fixture}"))
                .await?;
            // Ownership and default dependencies name only this table's allocators.
            let literal = t.replace('\'', "''");
            let sql = format!(
                concat!(
                    "SELECT DISTINCT n.nspname AS schema, s.relname AS name ",
                    "FROM pg_catalog.pg_class s JOIN pg_catalog.pg_namespace n ON n.oid=s.relnamespace ",
                    "WHERE s.relkind='S' AND (EXISTS (SELECT 1 FROM pg_catalog.pg_depend d ",
                    "WHERE d.classid='pg_class'::regclass AND d.objid=s.oid ",
                    "AND d.refobjid='{relation}'::regclass AND d.deptype IN ('a','i')) ",
                    "OR EXISTS (SELECT 1 FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_attrdef a ",
                    "ON d.classid='pg_attrdef'::regclass AND d.objid=a.oid ",
                    "WHERE a.adrelid='{relation}'::regclass AND d.refclassid='pg_class'::regclass ",
                    "AND d.refobjid=s.oid))"
                ),
                relation = literal
            );
            let rows = conn.query(&sql).await?;
            for r in rows {
                let sequence = TableName::new(
                    r.try_get::<&str>("schema")?
                        .context("allocator schema missing")?,
                    r.try_get::<&str>("name")?
                        .context("allocator name missing")?,
                );
                conn.execute(&format!(
                    "GRANT USAGE ON SEQUENCE {} TO {fixture}",
                    table(&sequence)?
                ))
                .await?;
            }
        }
        let literal = t.replace('\'', "''");
        // Both identities use one effective test; data: controls fixture DML.
        let sql = format!(
            concat!(
                "SELECT EXISTS (SELECT 1 FROM (VALUES ('{assertion}',true), ",
                "('{fixture}',{reference})) actor(principal,deny_write) ",
                "WHERE actor.deny_write AND (pg_catalog.has_table_privilege(",
                "actor.principal::pg_catalog.name,'{relation}', ",
                "'INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER') OR ",
                "pg_catalog.has_any_column_privilege(",
                "actor.principal::pg_catalog.name,'{relation}', ",
                "'INSERT,UPDATE,REFERENCES'))) AS unsafe"
            ),
            relation = literal,
            reference = declared.data.is_some(),
            fixture = fixture,
            assertion = assertion,
        );
        let rows = conn.query(&sql).await?;
        ensure!(
            rows.first()
                .context("missing privilege result")?
                .try_get::<bool>("unsafe")?
                == Some(false),
            "the effective SQL privileges for {} exceed the fixture/assertion boundary; remove broad grants, including any PUBLIC grant",
            name.shown()
        );
    }
    Ok(())
}

async fn fixtures(session: &Session<'_>) -> anyhow::Result<Conn> {
    Ok(Conn::connect(Driver::Postgres, &session.fixture).await?)
}

async fn run_fixture(conn: &mut Conn, sql: &str, timeout_ms: u128) -> anyhow::Result<()> {
    // PostgreSQL uses i32 milliseconds; zero would disable the limit.
    conn.execute(&format!("SET statement_timeout = '{timeout_ms}ms'"))
        .await?;
    conn.execute(sql).await?;
    // A successful file can still lose its writes when the fixture
    // connection closes. Reuse the driver-safe probe (DEC-2670.1).
    anyhow::ensure!(
        !pbps_pg::catalog::in_transaction(conn).await?,
        "the fixture left a transaction open; COMMIT or ROLLBACK inside it, because \
             closing its session would discard the writes silently"
    );
    Ok(())
}

async fn assertion(session: &Session<'_>, sql: &str, timeout: Duration) -> AssertionResult {
    pbps_pg::assertions::run(&session.assertion, sql, timeout).await
}
