//! Bound-expression retention, independent of resolver provisioning (DEC-2354.1).
use crate::{Postgres, emit, resolver::capture};
use pbps_db::Conn;
use pbps_model::{ChangeSet, ExpressionTarget, RetainedExpression, Schema, Table, TableName};
use std::collections::{BTreeMap, BTreeSet};

type Answer<T> = Result<T, String>;
fn quoted(table: &TableName) -> Answer<String> {
    emit::qualified(table).map_err(|e| e.to_string())
}
fn literal(text: &str) -> String {
    emit::value_literal(text)
}

struct Observed {
    oid: i64,
    bound: BoundExpression,
    expression: String,
}
async fn observed(conn: &mut Conn, target: &ExpressionTarget) -> Answer<Observed> {
    let table = target.table();
    let (catalog, tree, relation, filter) = match target {
        ExpressionTarget::Check { name, .. } => (
            "pg_constraint",
            "conbin",
            "conrelid",
            format!(
                "c.conname={} AND c.contype='c' AND c.convalidated AND c.conislocal AND c.coninhcount=0 AND c.conparentid=0",
                literal(name.raw())
            ),
        ),
        ExpressionTarget::ColumnDefault { column, .. } => (
            "pg_attrdef",
            "adbin",
            "adrelid",
            format!(
                "EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a WHERE a.attrelid=c.adrelid AND a.attnum=c.adnum AND a.attname={} AND NOT a.attisdropped AND a.attgenerated='' AND a.attidentity='')",
                literal(column.name.raw())
            ),
        ),
    };
    let rows = conn.query(&format!(
        "SELECT c.oid::bigint AS oid, c.{tree}::text AS tree, pg_catalog.pg_get_expr(c.{tree},c.{relation},false) AS expression
         FROM pg_catalog.{catalog} c JOIN pg_catalog.pg_class t ON t.oid=c.{relation}
         WHERE c.{relation}={}::pg_catalog.regclass AND {filter}
           AND t.relkind='r' AND NOT t.relispartition", literal(&quoted(table)?)
    )).await.map_err(|_| format!("cannot read retained {}", target.shown()))?;
    let row = rows.first().filter(|_| rows.len() == 1).ok_or_else(|| {
        format!(
            "retained {} is not an ordinary local expression",
            target.shown()
        )
    })?;
    Ok(Observed {
        oid: row
            .try_get::<i64>("oid")
            .ok()
            .flatten()
            .ok_or("unreadable retained expression identity")?,
        expression: row
            .try_get::<&str>("expression")
            .ok()
            .flatten()
            .ok_or("unreadable retained expression expression")?
            .to_owned(),
        bound: BoundExpression {
            table: table.clone(),
            tree: row
                .try_get::<&str>("tree")
                .ok()
                .flatten()
                .ok_or("unreadable retained expression tree")?
                .to_owned(),
            columns: column_map(conn, &quoted(table)?).await?,
        },
    })
}

/// Planning is read-only on the target. A missing capability selects the rebuild.
pub async fn available(conn: &mut Conn, expressions: &[RetainedExpression]) -> bool {
    let result = async {
        let rows = conn
            .query("SELECT pg_catalog.has_database_privilege(current_database(),'TEMP') AND pg_catalog.has_column_privilege('pg_catalog.pg_constraint','conbin','SELECT') AND pg_catalog.has_column_privilege('pg_catalog.pg_attrdef','adbin','SELECT') AS ok")
            .await
            .ok()?;
        if !rows.first()?.try_get::<bool>("ok").ok().flatten()? {
            return None;
        }
        for e in expressions {
            let table = e.target.table();
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

// Candidate admission preserves table/column UIDs. Catalog names carry those
// logical identities across independently assigned physical attribute numbers.
#[derive(PartialEq, Eq)]
pub struct BoundExpression {
    table: TableName,
    tree: String,
    columns: BTreeMap<u32, String>,
}
pub struct Carrier {
    name: String,
    table: TableName,
    columns: BTreeMap<u32, String>,
}
async fn column_map(conn: &mut Conn, relation: &str) -> Answer<BTreeMap<u32, String>> {
    let rows = conn.query(&format!(
        "SELECT attnum::bigint AS number, attname::text AS name FROM pg_catalog.pg_attribute WHERE attrelid={}::pg_catalog.regclass AND attnum>0 AND NOT attisdropped", literal(relation)
    )).await.map_err(|_| "cannot read the bound column identities")?;
    rows.iter()
        .map(|row| {
            let number = row
                .try_get::<i64>("number")
                .ok()
                .flatten()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or("unreadable bound column number")?;
            let name = row
                .try_get::<&str>("name")
                .ok()
                .flatten()
                .ok_or("unreadable bound column name")?;
            Ok((number, name.to_owned()))
        })
        .collect()
}

async fn execute(conn: &mut Conn, pg: &Postgres, context: &TableName, sql: &str) -> Answer<()> {
    let path = emit::write_path(pg, &context.schema).map_err(|e| e.to_string())?;
    let refused = |_| {
        format!(
            "the retained expression on {} could not be bound as one statement",
            context.shown()
        )
    };
    conn.execute_with(&format!("SET search_path = {path}"), &[])
        .await
        .map_err(refused)?;
    // Extended-query parsing refuses a trailing statement before executing any
    // part of the declared expression. Scratch and apply share this path (DEC-2354.3).
    let result = conn.execute_with(sql, &[]).await;
    let restored = conn.execute_with("SET search_path = ''", &[]).await;
    result.map_err(refused)?;
    restored.map_err(refused)?;
    Ok(())
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
    let name = format!(
        "{}.pbps_retained_{index}",
        if temporary { "pg_temp" } else { "public" }
    );
    execute(
        conn,
        pg,
        context,
        &format!(
            "CREATE {} TABLE {} ({});",
            if temporary { "TEMP" } else { "" },
            name,
            columns.join(", ")
        ),
    )
    .await?;
    let columns = column_map(conn, &name).await?;
    Ok(Carrier {
        name,
        table: context.clone(),
        columns,
    })
}
pub async fn bind(
    conn: &mut Conn,
    pg: &Postgres,
    target: &ExpressionTarget,
    expression: &str,
    carrier: &Carrier,
) -> Answer<BoundExpression> {
    let (install, read, remove) = match target {
        ExpressionTarget::Check { .. } => (
            format!("ADD CONSTRAINT pbps_proof CHECK ({expression}) NOT VALID"),
            format!(
                "SELECT conbin::text AS tree FROM pg_catalog.pg_constraint WHERE conrelid={}::pg_catalog.regclass",
                literal(&carrier.name)
            ),
            "DROP CONSTRAINT pbps_proof".to_owned(),
        ),
        ExpressionTarget::ColumnDefault { column, .. } => {
            let name = crate::quote(column.name.raw()).map_err(|e| e.to_string())?;
            (
                format!("ALTER COLUMN {name} SET DEFAULT ({expression})"),
                format!(
                    "SELECT d.adbin::text AS tree FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_attribute a ON a.attrelid=d.adrelid AND a.attnum=d.adnum WHERE d.adrelid={}::pg_catalog.regclass AND a.attname={}",
                    literal(&carrier.name),
                    literal(column.name.raw())
                ),
                format!("ALTER COLUMN {name} DROP DEFAULT"),
            )
        }
    };
    execute(
        conn,
        pg,
        target.table(),
        &format!("ALTER TABLE {} {install};", carrier.name),
    )
    .await?;
    let rows = conn
        .query(&read)
        .await
        .map_err(|_| "cannot read the bound expression".to_owned())?;
    let tree = rows
        .first()
        .filter(|_| rows.len() == 1)
        .and_then(|r| r.try_get::<&str>("tree").ok().flatten())
        .ok_or("the carrier must hold exactly one expression")?
        .to_owned();
    execute(
        conn,
        pg,
        target.table(),
        &format!("ALTER TABLE {} {remove};", carrier.name),
    )
    .await?;
    Ok(BoundExpression {
        table: carrier.table.clone(),
        tree,
        columns: carrier.columns.clone(),
    })
}
pub async fn equal(
    conn: &mut Conn,
    left: &BoundExpression,
    right: &BoundExpression,
) -> Answer<bool> {
    if left.table != right.table {
        return Ok(false);
    }
    let major = crate::input_path::server_major(conn)
        .await
        .map_err(|_| "cannot read the server version")?;
    capture::expressions_equal(
        &left.tree,
        &right.tree,
        major,
        &left.columns,
        &right.columns,
    )
}

/// Locks precede every deployment statement and survive temporary-carrier cleanup.
pub struct Held(Vec<(RetainedExpression, Observed)>);
pub async fn hold(conn: &mut Conn, changes: &ChangeSet, authored: Option<&Schema>) -> Answer<Held> {
    let mut held = Vec::new();
    let mut locked = BTreeSet::new();
    for e in pbps_model::retained::expressions(changes) {
        let table = e.target.table();
        if let Some(authored) = authored
            && e.target.expression(authored).map(String::as_str) != Some(e.before.as_str())
        {
            return Err(format!(
                "retained {} no longer has its approved before declaration",
                e.target.shown()
            ));
        }
        if locked.insert(table) {
            conn.execute(&format!(
                "LOCK TABLE {} IN ACCESS SHARE MODE;",
                quoted(table)?
            ))
            .await
            .map_err(|_| format!("cannot lock retained expression on {}", table.shown()))?;
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
            let table = e.target.table();
            let refuse = || {
                format!(
                    "retained {} does not match its approved expression; nothing was recorded",
                    e.target.shown()
                )
            };
            let now = observed(conn, &e.target).await.map_err(|_| refuse())?;
            if was.oid != now.oid || was.bound != now.bound {
                return Err(refuse());
            }
            conn.execute("SAVEPOINT pbps_retained;")
                .await
                .map_err(|_| refuse())?;
            let result = async {
                let shape = after.tables.get(table).ok_or_else(refuse)?;
                let carrier = create_carrier(conn, pg, table, shape, index, true).await?;
                let wanted = bind(conn, pg, &e.target, &e.after, &carrier).await?;
                if !equal(conn, &now.bound, &wanted).await? {
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
            if let (Some(old), Some(new)) =
                (target.expression_mut(before), target.expression(after))
            {
                let canonical = crate::type_refs::renames::TypeRenames::default();
                if canonical.expression(new) == canonical.expression(expected) {
                    *old = new.clone();
                }
            }
        }
    }
}
