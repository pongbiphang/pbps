//! Bound-expression retention, independent of resolver provisioning (DEC-2354.1).
use crate::{Postgres, emit, resolver::capture};
use pbps_db::Conn;
use pbps_model::{ChangeSet, ExpressionTarget, RetainedExpression, Schema, Table, TableName};
use std::collections::BTreeSet;

type Answer<T> = Result<T, String>;
fn quoted(table: &TableName) -> Answer<String> {
    emit::qualified(table).map_err(|e| e.to_string())
}
fn literal(text: &str) -> String {
    emit::value_literal(text)
}

struct Observed {
    oid: i64,
    tree: String,
    expression: String,
}
async fn observed(conn: &mut Conn, target: &ExpressionTarget) -> Answer<Observed> {
    let ExpressionTarget::Check { table, name, .. } = target;
    let rows = conn
        .query(&format!(
            "SELECT c.oid::bigint AS oid, c.conbin::text AS tree, pg_catalog.pg_get_expr(c.conbin,c.conrelid,false) AS expression
         FROM pg_catalog.pg_constraint c JOIN pg_catalog.pg_class t ON t.oid=c.conrelid
         WHERE c.conrelid={}::pg_catalog.regclass AND c.conname={}
           AND c.contype='c' AND c.convalidated AND c.conislocal AND c.coninhcount=0
           AND c.conparentid=0 AND t.relkind='r' AND NOT t.relispartition
           AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                           WHERE a.attrelid=t.oid AND a.attisdropped)",
            literal(&quoted(table)?),
            literal(name.raw())
        ))
        .await
        .map_err(|_| "cannot read the retained CHECK".to_owned())?;
    let row = rows
        .first()
        .filter(|_| rows.len() == 1)
        .ok_or("the retained CHECK is not a validated local CHECK")?;
    Ok(Observed {
        oid: row
            .try_get::<i64>("oid")
            .ok()
            .flatten()
            .ok_or("unreadable CHECK identity")?,
        expression: row
            .try_get::<&str>("expression")
            .ok()
            .flatten()
            .ok_or("unreadable CHECK expression")?
            .to_owned(),
        tree: row
            .try_get::<&str>("tree")
            .ok()
            .flatten()
            .ok_or("unreadable CHECK tree")?
            .to_owned(),
    })
}

/// Planning is read-only on the target. A missing capability selects the rebuild.
pub async fn available(conn: &mut Conn, expressions: &[RetainedExpression]) -> bool {
    let result = async {
        let rows = conn
            .query("SELECT pg_catalog.has_database_privilege(current_database(),'TEMP') AND pg_catalog.has_column_privilege('pg_catalog.pg_constraint','conbin','SELECT') AS ok")
            .await
            .ok()?;
        if !rows.first()?.try_get::<bool>("ok").ok().flatten()? {
            return None;
        }
        for e in expressions {
            let ExpressionTarget::Check { table, .. } = &e.target;
            observed(conn, &e.target).await.ok()?;
            let sql = format!(
                "SELECT pg_catalog.has_table_privilege({}::pg_catalog.regclass,'SELECT') AS ok",
                literal(&quoted(table).ok()?)
            );
            let rows = conn.query(&sql).await.ok()?;
            if !rows.first()?.try_get::<bool>("ok").ok().flatten()? {
                return None;
            }
        }
        Some(())
    }
    .await;
    result.is_some()
}

pub struct Carrier(String);

async fn execute(conn: &mut Conn, pg: &Postgres, context: &TableName, sql: &str) -> Answer<()> {
    let statement = emit::scoped(pg, &context.schema, sql).map_err(|e| e.to_string())?;
    conn.execute(&statement.sql)
        .await
        .map_err(|_| "the retained expression could not be bound".into())
}
pub async fn create_carrier(
    conn: &mut Conn,
    pg: &Postgres,
    context: &TableName,
    shape: &Table,
    index: usize,
    temporary: bool,
) -> Answer<Carrier> {
    let columns = shape
        .columns
        .iter()
        .map(|(name, c)| {
            if c.collation.is_some() {
                return Err("column collation is unsupported".to_owned());
            }
            Ok(format!(
                "{} {}",
                crate::quote(name.raw()).map_err(|e| e.to_string())?,
                emit::sql_type(&c.ty).map_err(|e| e.to_string())?
            ))
        })
        .collect::<Answer<Vec<_>>>()?;
    let carrier = Carrier(format!(
        "{}.pbps_retained_{index}",
        if temporary { "pg_temp" } else { "public" }
    ));
    execute(
        conn,
        pg,
        context,
        &format!(
            "CREATE {} TABLE {} ({});",
            if temporary { "TEMP" } else { "" },
            carrier.0,
            columns.join(", ")
        ),
    )
    .await?;
    Ok(carrier)
}
pub async fn bind(
    conn: &mut Conn,
    pg: &Postgres,
    context: &TableName,
    expression: &str,
    carrier: &Carrier,
) -> Answer<String> {
    execute(
        conn,
        pg,
        context,
        &format!(
            "ALTER TABLE {} ADD CONSTRAINT pbps_proof CHECK ({expression}) NOT VALID;",
            carrier.0
        ),
    )
    .await?;
    let rows = conn.query(&format!("SELECT conbin::text AS tree FROM pg_catalog.pg_constraint WHERE conrelid={}::pg_catalog.regclass",literal(&carrier.0)))
        .await.map_err(|_| "cannot read the bound expression".to_owned())?;
    let tree = rows
        .first()
        .filter(|_| rows.len() == 1)
        .and_then(|r| r.try_get::<&str>("tree").ok().flatten())
        .ok_or("the carrier must hold exactly one CHECK")?
        .to_owned();
    execute(
        conn,
        pg,
        context,
        &format!("ALTER TABLE {} DROP CONSTRAINT pbps_proof;", carrier.0),
    )
    .await?;
    Ok(tree)
}
pub async fn equal(conn: &mut Conn, left: &str, right: &str) -> Answer<bool> {
    let major = crate::input_path::server_major(conn)
        .await
        .map_err(|_| "cannot read the server version")?;
    capture::expressions_equal(left, right, major)
}

/// Locks precede every deployment statement and survive temporary-carrier cleanup.
pub struct Held(Vec<(RetainedExpression, Observed)>);
pub async fn hold(conn: &mut Conn, changes: &ChangeSet, authored: Option<&Schema>) -> Answer<Held> {
    let mut held = Vec::new();
    let mut locked = BTreeSet::new();
    for e in pbps_model::retained::expressions(changes) {
        let ExpressionTarget::Check { table, name, .. } = &e.target;
        if let Some(authored) = authored
            && authored
                .tables
                .get(table)
                .and_then(|t| t.checks.get(name))
                .map(|c| c.expression.as_str())
                != Some(e.before.as_str())
        {
            return Err(format!(
                "retained CHECK {}.{} no longer has its approved before declaration",
                table.shown(),
                name.shown()
            ));
        }
        if locked.insert(table) {
            conn.execute(&format!(
                "LOCK TABLE {} IN ACCESS SHARE MODE;",
                quoted(table)?
            ))
            .await
            .map_err(|_| format!("cannot lock retained CHECK on {}", table.shown()))?;
        }
        held.push((e.clone(), observed(conn, &e.target).await?));
    }
    Ok(Held(held))
}

/// The only constructor performs fresh target binding; saved booleans grant nothing.
pub struct Verified(Vec<(ExpressionTarget, String)>);
impl Held {
    pub async fn verify(&self, conn: &mut Conn, pg: &Postgres, after: &Schema) -> Answer<Verified> {
        let mut verified = Vec::new();
        for (index, (e, was)) in self.0.iter().enumerate() {
            let ExpressionTarget::Check { table, name, .. } = &e.target;
            let refuse = || {
                format!(
                    "retained CHECK {}.{} does not match its approved expression; nothing was recorded",
                    table.shown(),
                    name.shown()
                )
            };
            let now = observed(conn, &e.target).await.map_err(|_| refuse())?;
            if was.oid != now.oid || was.tree != now.tree {
                return Err(refuse());
            }
            conn.execute("SAVEPOINT pbps_retained;")
                .await
                .map_err(|_| refuse())?;
            let result = async {
                let shape = after.tables.get(table).ok_or_else(refuse)?;
                let carrier = create_carrier(conn, pg, table, shape, index, true).await?;
                let wanted = bind(conn, pg, table, &e.after, &carrier).await?;
                if !equal(conn, &now.tree, &wanted).await? {
                    return Err(refuse());
                }
                Ok(())
            }
            .await;
            conn.execute("ROLLBACK TO SAVEPOINT pbps_retained; RELEASE SAVEPOINT pbps_retained;")
                .await
                .map_err(|_| refuse())?;
            result.map_err(|_| refuse())?;
            verified.push((e.target.clone(), now.expression));
        }
        Ok(Verified(verified))
    }
}
impl Verified {
    /// Only verified expressions lose byte comparison; all other fields stay pinned.
    pub fn comparison(&self, before: &mut Schema, after: &Schema) {
        for (target, expected) in &self.0 {
            let ExpressionTarget::Check { table, name, .. } = target;
            if let (Some(old), Some(new)) = (
                before
                    .tables
                    .get_mut(table)
                    .and_then(|t| t.checks.get_mut(name)),
                after.tables.get(table).and_then(|t| t.checks.get(name)),
            ) {
                let canonical = crate::type_refs::renames::TypeRenames::default();
                if canonical.expression(&new.expression) == canonical.expression(expected) {
                    old.expression = new.expression.clone();
                }
            }
        }
    }
}
