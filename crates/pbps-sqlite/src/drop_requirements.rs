//! A guarded drop's row requirement, asked of the rows (DEC-2400.2, DEC-2950.16).
//!
//! A plan that drops a table can carry the human's requirement that its rows hold
//! a predicate. The check runs inside the apply's transaction, before the drop,
//! and counts the rows for which the predicate is not true: a NULL answer is a
//! row that violates it, as `IS NOT TRUE` makes it on the other engines (asked
//! here with a `CASE`, whose words no column can take). A missing
//! or unreadable answer never permits the removal.

use std::collections::BTreeSet;

use pbps_db::DbError;
use pbps_db::sqlite::{AuthEvent, Connection};
use pbps_dialect::drop_requirements::check_count;
use pbps_model::escape::Escaped;
use pbps_model::{ColumnName, DropGuard, TableName};

use crate::emit::{closed, qualified};
use crate::pragma::{self, table_list, table_xinfo};
use crate::{ident, lexis, validate};

/// The functions a requirement may call, by the name SQLite's authorizer
/// reports. `like` and `glob` are how it reports the `LIKE` and `GLOB`
/// operators. The rest are the built-in scalar functions whose answer depends
/// on their arguments alone, the counterparts of the list SQL Server's
/// requirements take: each was measured to prepare under the authorizer and to
/// report the columns it reads.
///
/// Not here, on purpose: anything that reads a clock or a random source
/// (`random`, `date`, `datetime`, `strftime`, `CURRENT_TIMESTAMP`), an
/// aggregate or window function (a requirement is asked of one row), `min` and
/// `max` (aggregates when given one argument, and the authorizer does not say
/// how many they were given), and whatever loads or runs code
/// (`load_extension`). `floor` and `ceil` are not here either: the bundled
/// library is built without the math functions (measured: it has no such
/// function), and a name is listed when it has been seen to work. A name this
/// connection has not been given, such as a
/// function an application registers, is refused with the rest.
const FUNCTIONS: &[&str] = &[
    "like",
    "glob",
    "coalesce",
    "ifnull",
    "nullif",
    "iif",
    "length",
    "lower",
    "upper",
    "trim",
    "ltrim",
    "rtrim",
    "substr",
    "substring",
    "instr",
    "replace",
    "concat",
    "abs",
    "round",
    "typeof",
];

/// Vets `predicate` against `table` through SQLite's compiler and authorizer
/// without reading a row, returning the canonical names of `table`'s ordinary
/// columns that `predicate` reads.
pub fn vet(
    c: &mut Connection,
    table: &TableName,
    predicate: &str,
) -> Result<BTreeSet<ColumnName>, String> {
    let prefix = format!(
        "the row requirement `{}` of {}",
        Escaped(predicate),
        table.shown()
    );
    let refused = |why: &str| format!("{prefix} {why}");
    let trimmed = closed(predicate);
    if !lexis::has_token(&trimmed) {
        return Err(refused("is blank"));
    }
    if !lexis::is_one_expression(&trimmed) {
        return Err(refused(
            "is not one expression SQLite reads whole (unbalanced parentheses, a `;`, or a \
             comment or quote left open); a requirement that cannot be asked never permits removal",
        ));
    }
    let target = qualified(table).map_err(|error| error.to_string())?;
    let (expected, table_columns) = read_ordinary_table_columns(c, table, &refused)?;

    let sql = format!("SELECT CASE WHEN ({predicate}\n) THEN 0 ELSE 1 END FROM {target}");
    let auth_prefix = prefix.clone();
    let mut selects = 0usize;

    let audit = c
        .prepare_authorized(&sql, move |event| {
            let deny = |why: String| Err(format!("{auth_prefix} {why}"));
            match event {
                AuthEvent::Select => {
                    selects += 1;
                    if selects == 1 {
                        Ok(())
                    } else {
                        deny("contains a subquery or nested SELECT".into())
                    }
                }
                AuthEvent::Read {
                    database,
                    table: read_table,
                    accessor,
                    ..
                } => match (accessor, database.as_deref()) {
                    (Some(via), _) => deny(format!(
                        "reads through `{}` (a view or trigger)",
                        Escaped(via)
                    )),
                    (None, db) if db.is_none_or(|d| !ident::same(d, validate::SCHEMA)) => {
                        deny(format!(
                            "reads from another database (`{}`)",
                            Escaped(db.unwrap_or("unknown"))
                        ))
                    }
                    (None, _) if !ident::same(read_table, &expected) => {
                        deny(format!("reads another table (`{}`)", Escaped(read_table)))
                    }
                    (None, _) => Ok(()),
                },
                AuthEvent::Function { name }
                    if FUNCTIONS.iter().any(|f| name.eq_ignore_ascii_case(f)) =>
                {
                    Ok(())
                }
                AuthEvent::Function { name } => deny(format!("calls function `{}`", Escaped(name))),
                AuthEvent::Other(action) => deny(format!(
                    "attempts a disallowed SQLite action (`{}`)",
                    Escaped(action)
                )),
            }
        })
        .map_err(|error| {
            if let DbError::Refused(reason) = error {
                reason
            } else {
                refused(&format!(
                    "could not be prepared by SQLite (SQLite code {}); a requirement that cannot \
                     be bound never permits removal",
                    error.server_error_code().as_deref().unwrap_or("unknown")
                ))
            }
        })?;

    if audit.parameters > 0 {
        return Err(refused("contains a bound parameter"));
    }
    if !audit.events.iter().any(|e| matches!(e, AuthEvent::Select)) {
        return Err(refused(
            "did not prepare as a SELECT against the target table",
        ));
    }

    let mut columns = BTreeSet::new();
    for event in &audit.events {
        let AuthEvent::Read { column, .. } = event else {
            continue;
        };
        if column.is_empty() {
            continue;
        }
        let Some((canonical, hidden)) = table_columns.iter().find(|(n, _)| ident::same(n, column))
        else {
            return Err(refused(&format!(
                "references implicit column `{}`; use a declared column",
                Escaped(column)
            )));
        };
        if *hidden != 0 {
            let kind = ["hidden", "virtual generated", "stored generated"]
                .get(usize::try_from(*hidden - 1).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or("non-ordinary");
            return Err(refused(&format!(
                "references {kind} column `{}`; use an ordinary stored column",
                Escaped(canonical)
            )));
        }
        columns.insert(ColumnName::new(canonical.as_str()));
    }
    Ok(columns)
}

fn read_ordinary_table_columns(
    c: &Connection,
    table: &TableName,
    refused: &dyn Fn(&str) -> String,
) -> Result<(String, Vec<(String, i64)>), String> {
    let db_err = |what: &str, e: DbError| {
        refused(&format!(
            "could not be vetted because {what} (SQLite code {})",
            e.server_error_code().as_deref().unwrap_or("unknown")
        ))
    };
    let bad = |what: &str| {
        refused(&format!(
            "could not be vetted: unreadable {what} in catalog"
        ))
    };
    let version = c.version().map_err(|e| db_err("version read failed", e))?;
    if version < crate::catalog::NEEDED {
        return Err(refused(&format!(
            "could not be vetted: `PRAGMA table_list` needs {} or later (this library is {version})",
            crate::catalog::NEEDED
        )));
    }
    // Check `table_list` before preparing anything naming `table` so a virtual
    // table's module is never loaded (DEC-2950.14).
    let listed = pragma::table(c, &table.name).map_err(|e| db_err("catalog read failed", e))?;
    let Some(row) = listed.first() else {
        return Err(refused("names a table that does not exist in `main`"));
    };
    let actual = row
        .text(table_list::NAME)
        .map_err(|_| bad("table name"))?
        .to_owned();
    match row.text(table_list::KIND).map_err(|_| bad("object kind"))? {
        "view" => return Err(refused("targets a view, not a table")),
        "virtual" => return Err(refused("targets a virtual table, not an ordinary table")),
        "shadow" => {
            return Err(refused(
                "targets a virtual table's shadow table, not an ordinary table",
            ));
        }
        "table" if ident::has_reserved_prefix(&actual) => {
            return Err(refused(
                "targets an internal SQLite table, not a user table",
            ));
        }
        "table" => {}
        other => {
            return Err(refused(&format!(
                "targets `{}` of unsupported kind `{}`",
                Escaped(&actual),
                Escaped(other)
            )));
        }
    }
    let raw = pragma::columns(c, &actual).map_err(|e| db_err("column read failed", e))?;
    if raw.is_empty() {
        return Err(refused(
            "could not be vetted: listed table has no columns to read",
        ));
    }
    let cols = raw
        .into_iter()
        .map(|col| {
            Ok((
                col.text(table_xinfo::NAME)
                    .map_err(|_| bad("column name"))?
                    .to_owned(),
                col.integer(table_xinfo::HIDDEN)
                    .map_err(|_| bad("column kind"))?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((actual, cols))
}

/// `Ok` when every requirement of `guard` holds for every row of its table; the
/// refusal otherwise, naming the count and the predicate.
pub fn check(c: &Connection, guard: &DropGuard) -> Result<(), String> {
    let table = qualified(&guard.table).map_err(|error| error.to_string())?;
    for requirement in guard.requirements.values() {
        // The count is only as good as the wrapper around the predicate: an open
        // comment or quote would swallow the rest of it, and a parenthesis closed
        // early would leave it binding to something else.
        // (A `--` comment at the end is closed with a line feed first, as the
        // emitter closes one.)
        if !lexis::is_one_expression(&closed(&requirement.predicate)) {
            return Err(format!(
                "the row requirement `{}` of {} is not one expression SQLite reads whole \
                 (unbalanced parentheses, a `;`, or a comment or quote left open); a requirement \
                 that cannot be asked never permits removal",
                Escaped(&requirement.predicate),
                guard.table.shown()
            ));
        }
        // A line feed before the closing parenthesis, so a predicate that ends in a
        // `--` comment does not swallow it. One statement is prepared, so a `;`
        // inside it is refused by the library and writes nothing.
        //
        // `CASE` and not `IS NOT TRUE`: in SQLite `TRUE` is a name before it is a
        // constant, so a column named `true` takes its place (measured: with such
        // a column holding 0, a row that fails `x > 0` was counted as none). The
        // words of a `CASE` are reserved and cannot be a column's. A NULL answer
        // takes the `ELSE`, so it is still a violation.
        let sql = format!(
            "SELECT count(*) FROM {table} WHERE CASE WHEN ({}\n) THEN 0 ELSE 1 END",
            requirement.predicate
        );
        let rows = c.query(&sql, &[]).map_err(|error| {
            format!(
                "the row requirement `{}` of {} could not be evaluated (SQLite code {}); a query \
                 failure never permits removal",
                Escaped(&requirement.predicate),
                guard.table.shown(),
                error.server_error_code().as_deref().unwrap_or("unknown")
            )
        })?;
        check_count(
            rows.first().and_then(|row| row.integer(0).ok()),
            guard,
            requirement,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
