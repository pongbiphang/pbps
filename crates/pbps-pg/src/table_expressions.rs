//! The admission predicate at the column-level expressions a plan introduces on
//! a table: CHECK, DEFAULT and generated columns (#2563, DEC-2563.4).
//!
//! Before this, a table's expressions were only change-detected (DEC-319.1): a
//! function any role owned ran as the deployer. Each change that carries one is
//! bound here, in a transaction that is rolled back, the way a requirement is:
//! a temporary table with the table's columns and the one expression, whose
//! stored tree names every routine it calls. The routines are judged by the one
//! predicate ([`crate::input_path::judge_trees`]), so what a human reviews and
//! pins is what the engine would have run.
//!
//! The same function serves three moments. `plan` calls it to learn which
//! routines the review admits, so the plan can show and pin them; apply calls it
//! before the pre-flight probes, which evaluate these expressions over the
//! stored rows; and apply calls it again before each statement that introduces
//! an expression, when everything the plan created earlier exists. That last
//! call is what judges a routine or a type the same plan creates: the earlier
//! two cannot bind it, and say so as a note rather than refuse.

use pbps_db::{Conn, Param};
use pbps_model::{Change, TableName, ValueSource};

use crate::admission::Admission;
use crate::input_path::{Part, Use, judge_trees};
use crate::pins::Within;

const TEMP: &str = "pbps_table_expressions";

/// The placeholder name of the column an existing column's new expression is
/// bound as; [`name_bound_columns`] replaces it before anything runs.
const BIND: &str = "pbps_bound";

/// When the expressions are judged, which decides what an unbindable one means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// Plan time and the pre-flight: objects the plan creates do not exist yet,
    /// so an expression that does not bind is a note, judged later.
    Early,
    /// Just before the statement that introduces them: whatever the plan
    /// created earlier exists, so an expression that does not bind is refused
    /// rather than run unjudged.
    Statement,
}

/// The statement shape of one change's expressions.
struct Spec {
    /// What a refusal calls them: `the CHECK `positive` of app.t`.
    what: String,
    /// The table they belong to, as a refusal shows it.
    table: String,
    /// The schema whose path the statement that carries them runs on.
    schema: String,
    /// The table the columns are copied from, or `None` for a table the plan
    /// creates, whose columns are all listed.
    like: Option<TableName>,
    /// Name in the temporary table, the name to show, the type, the value.
    columns: Vec<Column>,
    checks: Vec<(String, String)>,
}

struct Column {
    bound: String,
    shown: String,
    ty: String,
    value: Option<Value>,
}

enum Value {
    Default(String),
    Generated(String),
}

/// Whether `change` introduces an expression this module judges.
#[must_use]
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "the changes that carry no table expression this module judges are every other one; indexes, policies and partition keys are the next slice"
)]
pub fn carries(change: &Change) -> bool {
    match change {
        Change::CreateTable { table, .. } => {
            table.partition_of.is_none()
                && (!table.checks.is_empty()
                    || table.columns.values().any(|c| has_value(&c.value_source)))
        }
        Change::AddColumn { column, .. } => has_value(&column.value_source),
        Change::AlterColumnDefault { to, .. } => to.is_some(),
        Change::AlterColumnExpression { .. } | Change::AddCheck { .. } => true,
        _ => false,
    }
}

fn has_value(source: &ValueSource) -> bool {
    matches!(source, ValueSource::Default(_) | ValueSource::Generated(_))
}

/// Judges the expressions `change` introduces. `Ok` carries the notes of the
/// expressions that cannot be bound yet and are judged at apply; `Err` is a
/// refusal, with the fix.
pub async fn vet_change(
    conn: &mut Conn,
    admission: &Admission,
    within: Within,
    moment: Moment,
    change: &Change,
) -> Result<Vec<String>, String> {
    if !carries(change) {
        return Ok(Vec::new());
    }
    if within == Within::CallersTransaction {
        return vet_in_callers_transaction(conn, admission, moment, change).await;
    }
    let framing = crate::input_path::framing(within, TEMP);
    conn.begin(&framing).await.map_err(|_| {
        "the expressions of a table could not be vetted: no transaction to bind them in".to_owned()
    })?;
    let result = vet_bound(conn, admission, moment, change).await;
    let closed = conn.rollback(&framing).await;
    let notes = result?;
    closed.map_err(|_| {
        "the expressions of a table could not be vetted: its transaction did not close".to_owned()
    })?;
    Ok(notes)
}

/// Binds inside the apply's own transaction, with no savepoint: measured on
/// PostgreSQL 18, a savepoint taken after an `ALTER DOMAIN` of that
/// transaction makes a later `CREATE TABLE` fail with "cannot start a
/// subtransaction when there are unprocessed inval messages" (XX000). A bind
/// that fails here is a refusal, which ends the apply and rolls the whole
/// transaction back, so only a bind that succeeds has to clean up after
/// itself: the temporary table, and the `search_path` it set.
async fn vet_in_callers_transaction(
    conn: &mut Conn,
    admission: &Admission,
    moment: Moment,
    change: &Change,
) -> Result<Vec<String>, String> {
    let unclean = || {
        "the expressions of a table could not be vetted: its binding table could not be removed"
            .to_owned()
    };
    let rows = conn
        .query("SELECT pg_catalog.current_setting('search_path') AS path")
        .await
        .map_err(|_| unclean())?;
    let path = rows
        .first()
        .and_then(|r| r.try_get::<&str>("path").ok().flatten())
        .map(str::to_owned)
        .ok_or_else(unclean)?;
    let notes = vet_bound(conn, admission, moment, change).await?;
    conn.execute(&format!("DROP TABLE IF EXISTS pg_temp.{TEMP}"))
        .await
        .map_err(|_| unclean())?;
    conn.execute_with(
        "SELECT pg_catalog.set_config('search_path', $1::pg_catalog.text, true)",
        &[pbps_db::Param::Str(&path)],
    )
    .await
    .map_err(|_| unclean())?;
    Ok(notes)
}

async fn vet_bound(
    conn: &mut Conn,
    admission: &Admission,
    moment: Moment,
    change: &Change,
) -> Result<Vec<String>, String> {
    let zone = conn
        .session_zone()
        .cloned()
        .unwrap_or_else(pbps_model::SessionZone::utc);
    conn.execute(&crate::session_pins(&zone)).await.map_err(|_| {
        "the expressions of a table could not be vetted: its session settings could not be pinned"
            .to_owned()
    })?;
    // A failure to bind is a note before the statement and a refusal at it.
    let unbound = |what: &str, why: &str| match moment {
        Moment::Early => Ok(vec![note(what, why)]),
        Moment::Statement => Err(format!(
            "{what} could not be bound just before they run (SQLSTATE {why}), so they cannot be \
             judged; nothing has been changed"
        )),
    };
    let mut spec = match spec(conn, change).await {
        Ok(Some(spec)) => spec,
        Ok(None) => return Ok(Vec::new()),
        Err(why) => return unbound(&describe(change), &why),
    };
    if let Some(table) = &spec.like {
        match column_names(conn, table).await {
            Ok(taken) => name_bound_columns(&mut spec, taken),
            Err(why) => return unbound(&spec.what, &why),
        }
    }
    // Bound on the path the real statement runs on, or an unqualified name
    // would resolve differently here than where it is created (`SET LOCAL`
    // ends with the transaction or savepoint that is rolled back).
    let path = crate::emit::write_path(&crate::Postgres::new(), &spec.schema)
        .map_err(|e| e.to_string())?;
    conn.execute(&format!("SET LOCAL search_path = {path}"))
        .await
        .map_err(|_| {
            format!(
                "{} could not be vetted: its search_path could not be set",
                spec.what
            )
        })?;
    // The extended protocol refuses more than one command, so an expression that
    // closes its clause and adds a statement is refused as one that does not
    // bind, never run before it is judged (as for a requirement, DEC-1635.4).
    if let Err(e) = conn.execute_with(&create_statement(&spec)?, &[]).await {
        let code = e
            .server_error_code()
            .unwrap_or_else(|| "unknown".to_owned());
        return unbound(&spec.what, &code);
    }
    let unreadable = || {
        format!(
            "{} could not be vetted: the catalog could not be read",
            spec.what
        )
    };
    let like_columns = match &spec.like {
        Some(table) => crate::input_path::column_count(conn, table)
            .await
            .map_err(|_| unreadable())?,
        None => 0,
    };
    let expected = [
        like_columns + count(spec.columns.len()),
        count(spec.checks.len()),
        count(spec.columns.iter().filter(|c| c.value.is_some()).count()),
    ];
    let shape = conn
        .query(&format!(
            "SELECT
               (SELECT count(*) FROM pg_catalog.pg_attribute a
                 WHERE a.attrelid OPERATOR(pg_catalog.=) r.oid
                   AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped)::pg_catalog.int4 AS columns,
               (SELECT count(*) FROM pg_catalog.pg_constraint c
                 WHERE c.conrelid OPERATOR(pg_catalog.=) r.oid
                   AND c.contype OPERATOR(pg_catalog.<>) 'n')::pg_catalog.int4 AS constraints,
               (SELECT count(*) FROM pg_catalog.pg_attrdef d
                 WHERE d.adrelid OPERATOR(pg_catalog.=) r.oid)::pg_catalog.int4 AS defaults
               FROM pg_catalog.pg_class r
              WHERE r.oid OPERATOR(pg_catalog.=) 'pg_temp.{TEMP}'::pg_catalog.regclass"
        ))
        .await
        .map_err(|_| unreadable())?;
    let counts: Option<Vec<i32>> = shape.first().and_then(|row| {
        ["columns", "constraints", "defaults"]
            .iter()
            .map(|name| row.try_get::<i32>(name).ok().flatten())
            .collect()
    });
    if counts.as_deref() != Some(&expected) {
        return Err(format!(
            "{} are more than the expressions declared: one closes its clause and adds to the \
             statement, and each may be the one expression and nothing else; nothing has been changed",
            spec.what
        ));
    }
    let rows = conn
        .query(&format!(
            "SELECT 'check' AS kind, c.conname::pg_catalog.text AS name, c.conbin::pg_catalog.text AS tree
               FROM pg_catalog.pg_constraint c
              WHERE c.conrelid OPERATOR(pg_catalog.=) 'pg_temp.{TEMP}'::pg_catalog.regclass
                AND c.contype OPERATOR(pg_catalog.=) 'c'
             UNION ALL
             SELECT CASE WHEN a.attgenerated OPERATOR(pg_catalog.<>) '' THEN 'generated' ELSE 'default' END,
                    a.attname::pg_catalog.text, d.adbin::pg_catalog.text
               FROM pg_catalog.pg_attrdef d
               JOIN pg_catalog.pg_attribute a
                 ON a.attrelid OPERATOR(pg_catalog.=) d.adrelid AND a.attnum OPERATOR(pg_catalog.=) d.adnum
              WHERE d.adrelid OPERATOR(pg_catalog.=) 'pg_temp.{TEMP}'::pg_catalog.regclass"
        ))
        .await
        .map_err(|_| unreadable())?;
    let mut trees = Vec::new();
    for row in &rows {
        let (Some(kind), Some(name), Some(tree)) = (
            row.try_get::<&str>("kind").ok().flatten(),
            row.try_get::<&str>("name").ok().flatten(),
            row.try_get::<&str>("tree").ok().flatten(),
        ) else {
            return Err(unreadable());
        };
        let label = match kind {
            "check" => format!("the CHECK `{name}` of {}", spec.table),
            "generated" => format!(
                "the generated expression of {}.{}",
                spec.table,
                shown_column(&spec, name)
            ),
            _ => format!(
                "the DEFAULT of {}.{}",
                spec.table,
                shown_column(&spec, name)
            ),
        };
        trees.push((
            Use {
                ty: label,
                part: Part::TableExpression,
            },
            tree.to_owned(),
        ));
    }
    let what = spec.what.clone();
    judge_trees(conn, admission, trees, false, &|why: &str| {
        format!("{what} {why}")
    })
    .await?;
    Ok(Vec::new())
}

fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn shown_column<'a>(spec: &'a Spec, bound: &'a str) -> &'a str {
    spec.columns
        .iter()
        .find(|c| c.bound == bound)
        .map_or(bound, |c| c.shown.as_str())
}

fn note(what: &str, why: &str) -> String {
    format!(
        "{what} cannot be bound at plan time (SQLSTATE {why}), so they are judged at apply only: \
         they use a type or routine this plan creates, or a column it adds"
    )
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "the changes that carry no table expression this module judges are every other one; indexes, policies and partition keys are the next slice"
)]
fn describe(change: &Change) -> String {
    match change {
        Change::CreateTable { name, .. } => {
            format!("the expressions of the new table {}", name.shown())
        }
        Change::AddColumn { table, name, .. } => {
            format!("the expression of {}.{}", table.shown(), name.shown())
        }
        Change::AlterColumnDefault { column, .. }
        | Change::AlterColumnExpression { column, .. } => {
            format!(
                "the expression of {}.{}",
                column.table.shown(),
                column.name.shown()
            )
        }
        Change::AddCheck { table, name, .. } => {
            format!("the CHECK `{}` of {}", name.shown(), table.shown())
        }
        _ => "the expressions of a table".to_owned(),
    }
}

/// The statement that builds the binding table. Nothing in it is evaluated: the
/// table has no row, and a default, a check and a generated expression run on
/// one.
fn create_statement(spec: &Spec) -> Result<String, String> {
    let quoted = |name: &str| crate::quote(name).map_err(|e| e.to_string());
    // Each expression closes on its own line: one ending in a `--` comment
    // would otherwise swallow the parenthesis that ends it.
    let mut parts = Vec::new();
    if let Some(table) = &spec.like {
        parts.push(format!(
            "LIKE {} EXCLUDING ALL",
            crate::emit::qualified(table).map_err(|e| e.to_string())?
        ));
    }
    for c in &spec.columns {
        let value = match &c.value {
            Some(Value::Default(e)) => format!(" DEFAULT ({e}\n)"),
            Some(Value::Generated(e)) => format!(" GENERATED ALWAYS AS ({e}\n) STORED"),
            None => String::new(),
        };
        parts.push(format!("{} {}{value}", quoted(&c.bound)?, c.ty));
    }
    for (name, expression) in &spec.checks {
        parts.push(format!(
            "CONSTRAINT {} CHECK ({expression}\n)",
            quoted(name)?
        ));
    }
    Ok(format!(
        "CREATE TEMPORARY TABLE pg_temp.{TEMP} ({})",
        parts.join(", ")
    ))
}

/// The names of the columns `table` has today, dropped ones excluded.
async fn column_names(
    conn: &mut Conn,
    table: &TableName,
) -> Result<std::collections::BTreeSet<String>, String> {
    let qualified = crate::emit::qualified(table).map_err(|e| e.to_string())?;
    let rows = conn
        .query_with(
            "SELECT a.attname::pg_catalog.text AS name
               FROM pg_catalog.pg_attribute a
              WHERE a.attrelid OPERATOR(pg_catalog.=) $1::pg_catalog.text::pg_catalog.regclass
                AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped",
            &[Param::Str(&qualified)],
        )
        .await
        .map_err(|e| {
            e.server_error_code()
                .unwrap_or_else(|| "unknown".to_owned())
        })?;
    Ok(rows
        .iter()
        .filter_map(|r| r.try_get::<&str>("name").ok().flatten().map(str::to_owned))
        .collect())
}

/// Gives every bound column a name the copied columns do not have. The copy
/// (`LIKE`) brings the table's own columns, so a fixed helper name, or the
/// name of a column the plan drops and adds back, would make the temporary
/// table fail to build and refuse a valid plan. The name is derived from the
/// column set itself (`pbps_bound`, then `pbps_bound_1`, ...), so it cannot
/// collide: no declared column can have a name this search skipped. The other
/// helpers cannot collide by construction: the temporary table lives in the
/// session's own `pg_temp`, `LIKE ... EXCLUDING ALL` copies no constraint or
/// default, and no alias is used.
fn name_bound_columns(spec: &mut Spec, mut taken: std::collections::BTreeSet<String>) {
    for column in &mut spec.columns {
        if column.bound == BIND || taken.contains(&column.bound) {
            let mut n = 0_usize;
            let mut name = BIND.to_owned();
            while taken.contains(&name) {
                n += 1;
                name = format!("{BIND}_{n}");
            }
            column.bound = name;
        }
        taken.insert(column.bound.clone());
    }
}

/// The type the table has for `column` today, for an expression that changes
/// an existing column's default or generation.
async fn column_type(conn: &mut Conn, column: &pbps_model::ColumnRef) -> Result<String, String> {
    let qualified = crate::emit::qualified(&column.table).map_err(|e| e.to_string())?;
    let rows = conn
        .query_with(
            "SELECT pg_catalog.format_type(a.atttypid, a.atttypmod) AS ty
               FROM pg_catalog.pg_attribute a
              WHERE a.attrelid OPERATOR(pg_catalog.=) $1::pg_catalog.text::pg_catalog.regclass
                AND a.attname OPERATOR(pg_catalog.=) $2::pg_catalog.text::pg_catalog.name
                AND NOT a.attisdropped",
            &[Param::Str(&qualified), Param::Str(column.name.raw())],
        )
        .await
        .map_err(|e| {
            e.server_error_code()
                .unwrap_or_else(|| "unknown".to_owned())
        })?;
    rows.first()
        .and_then(|r| r.try_get::<&str>("ty").ok().flatten())
        .map(str::to_owned)
        .ok_or_else(|| "42703".to_owned())
}

/// What `change` binds, or why it cannot be built here (a SQLSTATE).
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "the changes that carry no table expression this module judges are every other one; indexes, policies and partition keys are the next slice"
)]
async fn spec(conn: &mut Conn, change: &Change) -> Result<Option<Spec>, String> {
    let value = |source: &ValueSource| match source {
        ValueSource::Default(e) => Some(Value::Default(e.clone())),
        ValueSource::Generated(g) => Some(Value::Generated(g.expression.clone())),
        ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => None,
    };
    let sql_type =
        |ty: &pbps_model::ColumnType| crate::emit::sql_type(ty).map_err(|_| "0A000".to_owned());
    Ok(Some(match change {
        Change::CreateTable { name, table, .. } => Spec {
            what: describe(change),
            table: name.shown().to_string(),
            schema: name.schema.clone(),
            like: None,
            columns: table
                .columns
                .iter()
                .map(|(n, c)| {
                    Ok(Column {
                        bound: n.raw().to_owned(),
                        shown: n.raw().to_owned(),
                        ty: sql_type(&c.ty)?,
                        value: value(&c.value_source),
                    })
                })
                .collect::<Result<_, String>>()?,
            checks: table
                .checks
                .iter()
                .map(|(n, c)| (n.raw().to_owned(), c.expression.clone()))
                .collect(),
        },
        Change::AddColumn {
            table,
            name,
            column,
            ..
        } => Spec {
            what: describe(change),
            table: table.shown().to_string(),
            schema: table.schema.clone(),
            like: Some(table.clone()),
            columns: vec![Column {
                bound: name.raw().to_owned(),
                shown: name.raw().to_owned(),
                ty: sql_type(&column.ty)?,
                value: value(&column.value_source),
            }],
            checks: Vec::new(),
        },
        Change::AddCheck {
            table,
            name,
            constraint,
        } => Spec {
            what: describe(change),
            table: table.shown().to_string(),
            schema: table.schema.clone(),
            like: Some(table.clone()),
            columns: Vec::new(),
            checks: vec![(name.raw().to_owned(), constraint.expression.clone())],
        },
        Change::AlterColumnDefault { column, to, .. } => {
            let Some(expression) = to else {
                return Ok(None);
            };
            existing(conn, change, column, Value::Default(expression.clone())).await?
        }
        Change::AlterColumnExpression { column, to, .. } => {
            existing(conn, change, column, Value::Generated(to.clone())).await?
        }
        _ => return Ok(None),
    }))
}

/// An existing column given a new expression, bound as an extra column of the
/// column's type so that the copy of the table keeps the one it replaces.
async fn existing(
    conn: &mut Conn,
    change: &Change,
    column: &pbps_model::ColumnRef,
    value: Value,
) -> Result<Spec, String> {
    Ok(Spec {
        what: describe(change),
        table: column.table.shown().to_string(),
        schema: column.table.schema.clone(),
        like: Some(column.table.clone()),
        columns: vec![Column {
            bound: BIND.to_owned(),
            shown: column.name.raw().to_owned(),
            ty: column_type(conn, column).await?,
            value: Some(value),
        }],
        checks: Vec::new(),
    })
}
