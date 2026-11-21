//! A guarded drop's row requirement, asked of the rows (DEC-2400.2, DEC-2950.16).
//!
//! A plan that drops a table can carry the human's requirement that its rows hold
//! a predicate. The check runs inside the apply's transaction, before the drop,
//! and counts the rows for which the predicate is not true: a NULL answer is a
//! row that violates it, as `IS NOT TRUE` makes it on the other engines (asked
//! here with a `CASE`, whose words no column can take). A missing
//! or unreadable answer never permits the removal.

use pbps_db::sqlite::Connection;
use pbps_dialect::drop_requirements::check_count;
use pbps_model::DropGuard;
use pbps_model::escape::Escaped;

use crate::emit::{closed, qualified};
use crate::lexis;

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
