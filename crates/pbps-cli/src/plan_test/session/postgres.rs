//! The PostgreSQL half of [`TestEngine`]: the original provisioning, the
//! administrator connection, the incoming-hop preparation and the comparison
//! snapshot, moved here unchanged from `session.rs` and `snapshot.rs` (#2798).
//! What a fixture and an assertion may do is still `session.rs`'s own.
use super::{
    Conn, Driver, HopContext, Provisioned, Session, SessionFuture, TestEngine, TestScratch,
    prerequisites, quoted, table,
};
use crate::{
    db::Target,
    plan_test::snapshot::{Allocator, Rows, Snapshot, insert},
};
use anyhow::{Context as _, ensure};
use pbps_model::Schema;
use std::collections::BTreeMap;

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
