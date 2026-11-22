//! The catalog pragmas, asked as statements (#2994, DEC-2994.1).
//!
//! SQLite answers each of them twice: as a statement (`PRAGMA main.table_list`)
//! and as a table-valued function (`FROM pragma_table_list`). A name in `FROM`
//! is an ordinary table or view first, in `main`, `temp` or an attached
//! database, so an object named like the function answers in its place, its rows
//! taken for the catalog's and no error raised (measured). Nothing a database
//! holds changes what the statement answers, so every catalog pragma this crate
//! reads is asked here, and no database is refused for its names.
//!
//! What the statement cannot be asked is done by the caller:
//!
//! - **No `WHERE` and no `ORDER BY`.** Rows come in the engine's own order
//!   (measured: `table_list` in no order of names, `foreign_key_list` by `id`
//!   ascending), so a caller filters them and orders them with [`sorted`]. A
//!   name is ordered as `BINARY` orders it, which is the order of a Rust string.
//! - **Columns by position**, named by the constants below. A row with fewer
//!   columns, or a value of another type, is an error of [`Record`]'s and never
//!   a default.
//! - **The schema is always written.** `PRAGMA table_xinfo("t")` answers for a
//!   `TEMP` table `t` before the file's (measured).
//! - **The name is written into the statement**: a pragma takes no bound
//!   parameter. It is quoted as an identifier, which the pragma reads back
//!   whatever the name holds (measured: quotes, keywords, digits, a line feed,
//!   the empty name), and compared as SQLite compares names, in ASCII case
//!   folding.
//! - **A pragma the library does not know answers no rows and no error**
//!   (measured). The two asked with no argument always have a row (`main`'s
//!   `sqlite_schema`; the database `main`), so none from them is an error and
//!   never an empty catalog.

use pbps_db::DbError;
use pbps_db::sqlite::{Connection, Record};
use pbps_model::escape::Escaped;

use crate::ident;

/// `schema, name, type, ncol, wr, strict`.
pub(crate) mod table_list {
    pub(crate) const SCHEMA: usize = 0;
    pub(crate) const NAME: usize = 1;
    /// `table`, `view`, `virtual` or `shadow`.
    pub(crate) const KIND: usize = 2;
    pub(crate) const WITHOUT_ROWID: usize = 4;
    pub(crate) const STRICT: usize = 5;
}

/// `cid, name, type, notnull, dflt_value, pk, hidden`.
pub(crate) mod table_xinfo {
    pub(crate) const CID: usize = 0;
    pub(crate) const NAME: usize = 1;
    pub(crate) const TYPE: usize = 2;
    pub(crate) const NOT_NULL: usize = 3;
    /// NULL for a column with no default.
    pub(crate) const DEFAULT: usize = 4;
    pub(crate) const PK: usize = 5;
    pub(crate) const HIDDEN: usize = 6;
}

/// `seq, name, unique, origin, partial`.
pub(crate) mod index_list {
    pub(crate) const NAME: usize = 1;
    pub(crate) const UNIQUE: usize = 2;
    pub(crate) const ORIGIN: usize = 3;
    pub(crate) const PARTIAL: usize = 4;
}

/// `seqno, cid, name, desc, coll, key`.
pub(crate) mod index_xinfo {
    pub(crate) const SEQNO: usize = 0;
    /// NULL for an expression and for the rowid.
    pub(crate) const NAME: usize = 2;
    pub(crate) const DESC: usize = 3;
    pub(crate) const COLLATION: usize = 4;
    /// 1 for a key column, 0 for one the index only carries.
    pub(crate) const KEY: usize = 5;
}

/// `id, seq, table, from, to, on_update, on_delete, match`.
pub(crate) mod foreign_key_list {
    pub(crate) const ID: usize = 0;
    pub(crate) const SEQ: usize = 1;
    pub(crate) const TABLE: usize = 2;
    pub(crate) const FROM: usize = 3;
    /// NULL where the clause names no parent column.
    pub(crate) const TO: usize = 4;
    pub(crate) const ON_UPDATE: usize = 5;
    pub(crate) const ON_DELETE: usize = 6;
}

/// `seq, name, file`.
pub(crate) mod database_list {
    pub(crate) const NAME: usize = 1;
}

/// A table or index name as a pragma's argument. [`ident::quote`] refuses the
/// empty name, which SQLite accepts for a table and the pragma reads (measured),
/// so it is written out here: an object of that name must not be skipped. A NUL
/// ends the statement, so a name with one is an error and never a question about
/// some other object.
fn argument(name: &str) -> Result<String, DbError> {
    if name.is_empty() {
        return Ok("\"\"".to_owned());
    }
    ident::quote(name).map_err(|_| {
        DbError::BadRow(format!(
            "the name `{}` cannot be written into a statement",
            Escaped(name)
        ))
    })
}

/// A pragma asked with no argument, which always has a row to answer.
fn listing(c: &Connection, pragma: &str) -> Result<Vec<Record>, DbError> {
    answered(pragma, c.query(&format!("PRAGMA {pragma}"), &[])?)
}

fn answered(pragma: &str, rows: Vec<Record>) -> Result<Vec<Record>, DbError> {
    if rows.is_empty() {
        return Err(DbError::BadRow(format!(
            "`PRAGMA {pragma}` answered no row, which is a pragma this library does not know and not an empty database"
        )));
    }
    Ok(rows)
}

/// What `main` says about one object; no row when it holds none of that name.
fn about(c: &Connection, pragma: &str, name: &str) -> Result<Vec<Record>, DbError> {
    c.query(&format!("PRAGMA main.{pragma}({})", argument(name)?), &[])
}

/// Every table and view of `main`, `sqlite_schema` and the shadow tables
/// included ([`table_list`]).
pub(crate) fn tables(c: &Connection) -> Result<Vec<Record>, DbError> {
    listing(c, "main.table_list")
}

/// Every table and view of every schema on the connection ([`table_list`]).
pub(crate) fn tables_everywhere(c: &Connection) -> Result<Vec<Record>, DbError> {
    listing(c, "table_list")
}

/// The connection's databases, `main` and `temp` included ([`database_list`]).
pub(crate) fn databases(c: &Connection) -> Result<Vec<Record>, DbError> {
    listing(c, "database_list")
}

/// The [`table_list`] row of one table or view of `main`.
pub(crate) fn table(c: &Connection, name: &str) -> Result<Vec<Record>, DbError> {
    about(c, "table_list", name)
}

/// The columns of a table or view of `main`, hidden and generated ones
/// included ([`table_xinfo`]).
pub(crate) fn columns(c: &Connection, table: &str) -> Result<Vec<Record>, DbError> {
    about(c, "table_xinfo", table)
}

/// The indexes of a table of `main` ([`index_list`]).
pub(crate) fn indexes(c: &Connection, table: &str) -> Result<Vec<Record>, DbError> {
    about(c, "index_list", table)
}

/// The columns of an index of `main`, key and carried ([`index_xinfo`]).
pub(crate) fn index_columns(c: &Connection, index: &str) -> Result<Vec<Record>, DbError> {
    about(c, "index_xinfo", index)
}

/// The foreign keys of a table of `main`, a row for each column
/// ([`foreign_key_list`]).
pub(crate) fn foreign_keys(c: &Connection, table: &str) -> Result<Vec<Record>, DbError> {
    about(c, "foreign_key_list", table)
}

/// `rows` in the order of `key`, which stands for the `ORDER BY` the statement
/// form does not take. Rows of one key keep the engine's order.
pub(crate) fn sorted<K: Ord>(
    rows: Vec<Record>,
    key: impl Fn(&Record) -> Result<K, DbError>,
) -> Result<Vec<Record>, DbError> {
    let mut keyed = rows
        .into_iter()
        .map(|row| key(&row).map(|k| (k, row)))
        .collect::<Result<Vec<_>, _>>()?;
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(keyed.into_iter().map(|(_, row)| row).collect())
}

/// [`sorted`] by a text column.
pub(crate) fn by_name(rows: Vec<Record>, column: usize) -> Result<Vec<Record>, DbError> {
    sorted(rows, |row| row.text(column).map(str::to_owned))
}

#[cfg(test)]
mod tests;
