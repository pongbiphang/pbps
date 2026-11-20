//! The catalog reader (item 4 of #2950): `sqlite_schema` and the `pragma_*`
//! functions for structure, then a tokenizer for what exists only in the stored
//! `CREATE` text (CHECK, partial index predicates, constraint names, view
//! bodies). Text it does not fully recognise is refused, never compared.
//!
//! The same shape as the other engines' readers: [`read_raw`] runs the queries
//! and decides nothing, [`assemble`] is pure and decides everything, and
//! [`introspect`] reads inside one read transaction and returns the
//! [`Pulled`] every connected command consumes. It is synchronous on the
//! crate's own connection; the wiring runs it on a blocking thread.
//!
//! Only `main` is read. SQLite's own tables (`sqlite_*`), the ledger's, the
//! shadow tables of a virtual table and the connection's `temp` and attached
//! databases are not the file's schema. A NULL is read as absent and a value of
//! another type as an error, and an object the catalog lists but cannot be
//! read is an error: an unreadable or hidden object never reads as nothing
//! there. A view's columns are never read (`pragma_table_xinfo` of a view that
//! names what is gone fails, and a rollback after it can leave the schema
//! cache unreliable).

use pbps_db::DbError;
use pbps_db::catalog::Pulled;
use pbps_db::sqlite::{Connection, Version};
use pbps_model::escape::Escaped;

mod assemble;
pub(crate) mod facts;
#[cfg(test)]
mod tests;
pub(crate) mod text;

pub(crate) use assemble::assemble;
use facts::Facts;

use crate::{READ, ident, names};

/// The first release with `pragma_table_list`, which the reader needs.
const NEEDED: Version = Version::new(3, 37, 0);

/// A table and everything the pragmas say about it.
pub(crate) struct RawTable {
    pub name: String,
    pub sql: String,
    pub facts: Facts,
}

/// An index made with `CREATE INDEX`, by the table it is on.
pub(crate) struct RawIndex {
    pub name: String,
    pub table: String,
    pub sql: String,
}

pub(crate) struct RawView {
    pub name: String,
    pub sql: String,
}

pub(crate) struct RawTrigger {
    pub name: String,
    pub table: String,
}

/// Everything one read found, with no decision made.
#[derive(Default)]
pub(crate) struct RawCatalog {
    pub tables: Vec<RawTable>,
    pub virtuals: Vec<String>,
    pub indexes: Vec<RawIndex>,
    pub views: Vec<RawView>,
    pub triggers: Vec<RawTrigger>,
    /// Tables named like the middle step of a case-only rename.
    pub intermediates: Vec<String>,
    pub attached: Vec<String>,
    /// What the connection holds outside `main`: `(schema, kind, name)`, for the
    /// attached databases' tables and views and for the `temp` schema's objects.
    pub elsewhere: Vec<(String, String, String)>,
}

fn unreadable(what: &str, name: &str) -> DbError {
    DbError::BadRow(format!(
        "{what} `{}` is listed by the catalog and has no stored text to read",
        Escaped(name)
    ))
}

/// `FROM pragma_table_list` resolves to a user table or view of that name first,
/// in `main`, `temp` or an attached database (measured), and answers its rows
/// as the catalog's with no error; the statement form is never shadowed. The
/// reader cannot tell the two apart, so such an object refuses the read.
fn refuse_shadowed_pragma_functions(c: &Connection) -> Result<(), DbError> {
    let mut schemas = vec!["main".to_owned(), "temp".to_owned()];
    for r in c.query("PRAGMA database_list", &[])? {
        let name = r.text(1)?;
        if name != "main" && name != "temp" {
            schemas.push(name.to_owned());
        }
    }
    let mut shadows = Vec::new();
    for schema in schemas {
        let sql = format!(
            "SELECT name FROM \"{}\".sqlite_schema WHERE type IN ('table', 'view')",
            schema.replace('"', "\"\"")
        );
        for r in c.query(&sql, &[])? {
            let name = r.text(0)?;
            if name
                .as_bytes()
                .get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case(b"pragma_"))
            {
                shadows.push(format!("`{}` in `{}`", Escaped(name), Escaped(&schema)));
            }
        }
    }
    if shadows.is_empty() {
        return Ok(());
    }
    Err(DbError::Refused(format!(
        "a table or view named like a pragma function ({}) takes its place in a query, so the catalog functions of that name cannot be told from it; rename or drop it",
        shadows.join(", ")
    )))
}

/// Runs the queries. Sorted by name, whatever order the engine answers in.
pub(crate) fn read_raw(c: &Connection) -> Result<RawCatalog, DbError> {
    let version = c.version()?;
    if version < NEEDED {
        return Err(DbError::Refused(format!(
            "reading a SQLite catalog needs {NEEDED} or later (`pragma_table_list`); this library is {version}"
        )));
    }
    refuse_shadowed_pragma_functions(c)?;
    let mut raw = RawCatalog::default();
    // The stored text of every object, by kind and name. A NULL is an object
    // with no text (an automatic index); it is kept apart from an empty one.
    let mut stored: Vec<(String, String, String, Option<String>)> = Vec::new();
    for r in c.query(
        "SELECT type, name, tbl_name, sql IS NULL, COALESCE(sql, '') FROM sqlite_schema ORDER BY type, name",
        &[],
    )? {
        let sql = (r.integer(3)? == 0).then(|| r.text(4).map(str::to_owned)).transpose()?;
        stored.push((r.text(0)?.to_owned(), r.text(1)?.to_owned(), r.text(2)?.to_owned(), sql));
    }
    let text_of = |kind: &str, name: &str| {
        stored
            .iter()
            .find(|(k, n, _, _)| k == kind && n == name)
            .and_then(|(_, _, _, sql)| sql.clone())
    };
    let mut listed: Vec<String> = Vec::new();
    for r in c.query(
        "SELECT name, type FROM pragma_table_list WHERE schema = 'main' ORDER BY name",
        &[],
    )? {
        let (name, kind) = (r.text(0)?.to_owned(), r.text(1)?.to_owned());
        listed.push(name.clone());
        // `sqlite_*` is SQLite's, and a shadow table belongs to its virtual one.
        // Only a table can be the ledger's: a view under one of its names is
        // the file's own, and is said so by the assembly.
        if ident::has_reserved_prefix(&name)
            || kind == "shadow"
            || (kind == "table" && names::is_ledger(&name))
        {
            continue;
        }
        match kind.as_str() {
            "table" if names::is_intermediate(&name) => raw.intermediates.push(name),
            "table" => {
                let sql = text_of("table", &name).ok_or_else(|| unreadable("table", &name))?;
                // Listed a moment ago in this snapshot and not there now cannot
                // be told from a table with no columns, so it is an error.
                let Some(facts) = facts::read(c, &name)? else {
                    return Err(DbError::BadRow(format!(
                        "the table `{}` is listed by the catalog and then not found",
                        Escaped(&name)
                    )));
                };
                raw.tables.push(RawTable { name, sql, facts });
            }
            "virtual" => raw.virtuals.push(name),
            "view" => {
                let sql = text_of("view", &name).ok_or_else(|| unreadable("view", &name))?;
                raw.views.push(RawView { name, sql });
            }
            other => {
                return Err(DbError::BadRow(format!(
                    "`{}` is listed with the kind `{}`, which this reader does not know",
                    Escaped(&name),
                    Escaped(other)
                )));
            }
        }
    }
    for (kind, name, table, sql) in &stored {
        match kind.as_str() {
            "table" | "view" if !ident::has_reserved_prefix(name) && !listed.contains(name) => {
                return Err(DbError::BadRow(format!(
                    "`{}` is in the schema table and is not listed as a table or a view",
                    Escaped(name)
                )));
            }
            "index" if !ident::has_reserved_prefix(table) && !names::is_ledger(table) => {
                if let Some(sql) = sql {
                    raw.indexes.push(RawIndex {
                        name: name.clone(),
                        table: table.clone(),
                        sql: sql.clone(),
                    });
                }
            }
            "trigger" => raw.triggers.push(RawTrigger {
                name: name.clone(),
                table: table.clone(),
            }),
            _ => {}
        }
    }
    for r in c.query(
        "SELECT name FROM pragma_database_list WHERE name NOT IN ('main', 'temp') ORDER BY name",
        &[],
    )? {
        raw.attached.push(r.text(0)?.to_owned());
    }
    // Not the file's schema, and said by name rather than left unsaid. Indexes
    // and triggers are listed for `temp` only: `pragma_table_list` has none, and
    // an attached database's would need a catalog query per database.
    for r in c.query(
        "SELECT schema, type, name FROM pragma_table_list WHERE schema <> 'main' ORDER BY schema, name",
        &[],
    )? {
        let (schema, kind, name) = (r.text(0)?, r.text(1)?, r.text(2)?);
        if kind != "shadow" && !ident::has_reserved_prefix(name) {
            raw.elsewhere
                .push((schema.to_owned(), kind.to_owned(), name.to_owned()));
        }
    }
    for r in c.query(
        "SELECT type, name FROM sqlite_temp_schema WHERE type IN ('index', 'trigger') AND sql IS NOT NULL ORDER BY type, name",
        &[],
    )? {
        raw.elsewhere
            .push(("temp".to_owned(), r.text(0)?.to_owned(), r.text(1)?.to_owned()));
    }
    Ok(raw)
}

/// Reads the schema inside a transaction the caller has open (the closing read
/// of an apply), so the read and what the apply wrote are one snapshot.
pub fn introspect_within_transaction(c: &Connection) -> Result<Pulled, DbError> {
    if !c.in_transaction() {
        return Err(DbError::Refused(
            "a catalog read inside a transaction was asked for outside one".into(),
        ));
    }
    Ok(assemble(&read_raw(c)?))
}

/// Reads the schema in one read transaction: the catalog queries see one
/// snapshot, and the read takes no write lock.
pub fn introspect(c: &mut Connection) -> Result<Pulled, DbError> {
    c.transaction(&READ, |tx| Ok(assemble(&read_raw(tx)?)))
}
