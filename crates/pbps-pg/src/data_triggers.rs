//! Reference-data writes must not invoke unapproved trigger code as the deployer.
//!
//! The table lock is part of the execution guard, not a post-write drift check:
//! TRIGGER privilege permits CREATE OR REPLACE even for an existing trigger.
//! It cannot change a trigger while the writer holds ROW EXCLUSIVE (DECISIONS 445).

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError};
use pbps_dialect::{RowOperation, RowWrite};
use pbps_model::{ModuleId, ModuleKind, Schema, TableName};

/// Valid only inside the transaction that authenticated and locked the tables.
/// Oids survive a table rename; names alone would also bless a replacement.
#[derive(Default)]
pub struct Guard {
    approved: BTreeMap<i64, i64>,
}

/// Authenticate existing triggers before any plan statement changes names.
/// New tables have no baseline trigger allowance. A trigger the plan drops is
/// not allowed: it must actually be gone by the time a row statement runs.
pub async fn prepare(
    conn: &mut Conn,
    writes: &[RowWrite],
    baseline: &Schema,
    dropped: &BTreeSet<ModuleId>,
) -> Result<Guard, DbError> {
    let mut writes = writes.to_vec();
    writes.sort_by(|a, b| a.table.cmp(&b.table).then(a.operation.cmp(&b.operation)));
    writes.dedup();
    let mut guard = Guard::default();
    for write in writes {
        for trigger in read_locked(conn, &write).await? {
            let id = ModuleId::Trigger {
                on: trigger.table.clone(),
                name: trigger.name.clone(),
            };
            if dropped.contains(&id) {
                continue;
            }
            let matches_record = baseline.modules.get(&id).is_some_and(|m| {
                m.kind == ModuleKind::Trigger
                    && crate::introspect::after_the_name(&trigger.definition, "CREATE TRIGGER ")
                        == Some(m.definition.as_str())
            });
            if !matches_record || !trigger.trusted_owner {
                return Err(refused(&trigger.table, &trigger.name));
            }
            guard.approved.insert(trigger.oid, trigger.function_oid);
        }
    }
    Ok(guard)
}

/// Recheck immediately before each row statement, including on a newly created
/// or renamed table. The lock stays held through that statement's transaction.
pub async fn check(conn: &mut Conn, write: &RowWrite, guard: &Guard) -> Result<(), DbError> {
    for trigger in read_locked(conn, write).await? {
        if guard.approved.get(&trigger.oid) != Some(&trigger.function_oid) || !trigger.trusted_owner
        {
            return Err(refused(&trigger.table, &trigger.name));
        }
    }
    Ok(())
}

fn event(operation: &RowOperation) -> i16 {
    match operation {
        RowOperation::Insert => 4,
        RowOperation::Update { .. } => 16,
        RowOperation::Delete => 8,
    }
}

struct Trigger {
    oid: i64,
    table: TableName,
    function_oid: i64,
    name: String,
    definition: String,
    trusted_owner: bool,
}

async fn read_locked(conn: &mut Conn, write: &RowWrite) -> Result<Vec<Trigger>, DbError> {
    let table = &write.table;
    let events = event(&write.operation);
    // UPDATE OF follows the SET list, including generated columns that depend
    // on it. A BEFORE ROW UPDATE trigger makes PostgreSQL include all generated
    // columns, even if that trigger is disabled; measured on PostgreSQL 18.
    let columns = match &write.operation {
        RowOperation::Update { columns } => columns
            .iter()
            .map(|name| literal(name))
            .collect::<Vec<_>>()
            .join(", "),
        RowOperation::Insert | RowOperation::Delete => String::new(),
    };
    // No ONLY: PostgreSQL DML can reach inheritance descendants. Lock and
    // inspect only descendants that this operation can reach. Statement-level
    // triggers fire on the named target, not each inheritance descendant.
    conn.execute(&format!(
        "LOCK TABLE {}.{} IN ROW EXCLUSIVE MODE",
        ident(&table.schema),
        ident(&table.name)
    ))
    .await?;
    let path = conn
        .query("SELECT pg_catalog.current_setting('search_path') AS path")
        .await?[0]
        .try_get::<&str>("path")?
        .ok_or_else(missing)?
        .to_owned();
    conn.query("SELECT pg_catalog.set_config('search_path', '', true)")
        .await?;
    let result: Result<Vec<Trigger>, DbError> = async {
        let rows = conn.query(&format!(
            "WITH RECURSIVE relations(oid) AS (
                 SELECT c.oid FROM pg_catalog.pg_class c
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = {schema} AND c.relname = {name}
                 UNION
                 SELECT i.inhrelid FROM pg_catalog.pg_inherits i
                 JOIN relations r ON r.oid = i.inhparent
                 JOIN pg_catalog.pg_class parent ON parent.oid = i.inhparent
                 WHERE {events} <> 4 OR parent.relkind = 'p'
             )
             SELECT t.oid::int8 AS oid, t.tgfoid::int8 AS function_oid,
                    n.nspname AS schema_name, c.relname AS table_name,
                    t.tgname AS name, pg_catalog.pg_get_triggerdef(t.oid) AS definition,
                    pg_catalog.pg_has_role(p.proowner, current_user::regrole::oid, 'SET') AS trusted_owner
             FROM pg_catalog.pg_trigger t
             JOIN relations r ON r.oid = t.tgrelid
             JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid
             JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE NOT t.tgisinternal AND (t.tgtype & {events}) <> 0
               AND ({events} <> 16 OR t.tgattr = ''::pg_catalog.int2vector OR EXISTS (
                   SELECT 1 FROM pg_catalog.pg_attribute a
                   WHERE a.attrelid = t.tgrelid AND a.attnum = ANY(t.tgattr)
                     AND (a.attname::text = ANY(ARRAY[{columns}]::text[]) OR
                          (a.attgenerated <> '' AND (EXISTS (
                              SELECT 1 FROM pg_catalog.pg_trigger before_update
                              WHERE before_update.tgrelid = t.tgrelid AND (before_update.tgtype & 19) = 19
                          ) OR EXISTS (
                              SELECT 1 FROM pg_catalog.pg_attrdef ad
                              JOIN pg_catalog.pg_depend d ON d.classid = 'pg_catalog.pg_attrdef'::regclass AND d.objid = ad.oid
                              JOIN pg_catalog.pg_attribute source ON source.attrelid = d.refobjid AND source.attnum = d.refobjsubid
                              WHERE ad.adrelid = a.attrelid AND ad.adnum = a.attnum
                                AND d.refclassid = 'pg_catalog.pg_class'::regclass AND d.refobjid = a.attrelid
                                AND source.attname::text = ANY(ARRAY[{columns}]::text[])
                          ))))
               ))
               AND ((n.nspname = {schema} AND c.relname = {name}) OR (t.tgtype & 1) <> 0)
               AND (t.tgenabled = 'A' OR t.tgenabled =
                    CASE WHEN pg_catalog.current_setting('session_replication_role') = 'replica'
                         THEN 'R' ELSE 'O' END)
             ORDER BY t.oid",
            schema = literal(&table.schema), name = literal(&table.name)
        )).await?;
        rows.iter().map(|r| Ok(Trigger {
            oid: r.try_get("oid")?.ok_or_else(missing)?,
            table: TableName::new(r.try_get::<&str>("schema_name")?.ok_or_else(missing)?, r.try_get::<&str>("table_name")?.ok_or_else(missing)?),
            function_oid: r.try_get("function_oid")?.ok_or_else(missing)?,
            name: r.try_get::<&str>("name")?.ok_or_else(missing)?.to_owned(),
            definition: r.try_get::<&str>("definition")?.ok_or_else(missing)?.to_owned(),
            trusted_owner: r.try_get("trusted_owner")?.ok_or_else(missing)?,
        })).collect()
    }.await;
    // On a query error the caller must roll back the transaction. On success,
    // restore its path before returning; no setting leaks into emitted SQL.
    if result.is_ok() {
        conn.query(&format!(
            "SELECT pg_catalog.set_config('search_path', {}, true)",
            literal(&path)
        ))
        .await?;
    }
    result
}

fn ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn literal(value: &str) -> String {
    format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

fn refused(table: &TableName, trigger: &str) -> DbError {
    DbError::Driver {
        code: None,
        message: format!(
            "unsafe data trigger `{trigger}` on `{table}`: reference-data writes require an unchanged recorded managed trigger whose function owner can act as the deployment role. Remove the trigger or establish that managed/trusted definition before planning again. `unmanaged: ignore` and `warn` do not authorize executing external trigger code."
        ),
    }
}

fn missing() -> DbError {
    DbError::Driver {
        code: None,
        message: "data trigger catalog read returned a missing value".to_owned(),
    }
}
