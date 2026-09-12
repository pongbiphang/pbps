//! A referenced target's own column-level grants can cover a
//! [`Needed::Referenced`] gap the object-level answer alone does not see
//! (issue #215).
//!
//! PostgreSQL grants `SELECT` and `REFERENCES` per column, and a declared
//! foreign key needs them only on the columns it names — not on the whole of
//! somebody else's table. Measured on 18.6, with
//! `GRANT SELECT (id), REFERENCES (id) ON shared.parent`:
//!
//! ```text
//! has_table_privilege ('shared.parent',      'REFERENCES') -> f    has_table_privilege(..., 'SELECT') -> f
//! has_column_privilege('shared.parent','id', 'REFERENCES') -> t    has_column_privilege(...,  'SELECT') -> t
//! CREATE TABLE app.child (id integer PRIMARY KEY, pid integer REFERENCES shared.parent(id))  -> CREATE TABLE
//! ```
//!
//! So a role granted exactly what the key uses deploys, and the object-scope
//! question alone reported two gaps against it. This module asks the
//! narrower question — column by column, for exactly the columns a declared
//! key names — only where the object answer already said no (DECISIONS 440).

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError, Param, doctor::ReferencedColumns};
use pbps_model::ObjectName;

use super::{Needed, REFERENCED_KINDS, REQUIRED, TableRights, text};

/// Fills in [`TableRights::columns`] for every referenced target whose
/// object-level answer does not already cover a [`Needed::Referenced`]
/// permission, over exactly the columns a declared key names there.
///
/// Reached only where it can change the answer: a target already covered at
/// object scope needs no column query, and a target no key names any column
/// on — unreachable in practice, since a foreign key always names at least
/// one — is left alone rather than read as vacuously covered.
pub(super) async fn fill(
    conn: &mut Conn,
    referenced_objects: &mut BTreeMap<ObjectName, TableRights>,
    referenced_columns: &ReferencedColumns,
) -> Result<(), DbError> {
    let perms: Vec<&'static str> = REQUIRED
        .iter()
        .filter(|r| matches!(r.needed, Needed::Referenced))
        .map(|r| r.name)
        .collect();
    for (object, rights) in referenced_objects.iter_mut() {
        let Some(columns) = referenced_columns.get(object) else {
            continue;
        };
        if columns.is_empty() {
            continue;
        }
        for permission in &perms {
            // The column question is reached only for a permission the object
            // answer did not already grant — the same ordering `pbps-mssql`
            // keeps its own column fallback under, and for the same reason:
            // an object-level grant already answers every column, so asking
            // again would cost a round trip for an answer this loop already
            // has.
            if rights.privileges.contains(*permission) {
                continue;
            }
            let held = column_privileges(conn, object, permission, columns).await?;
            // Every wanted column gets an entry, held or not: `missing`
            // reads "every wanted column covers this permission" off the
            // key set of this map, and a column silently left out would read
            // as vacuously covered rather than as the gap it is.
            for column in columns {
                let entry = rights.columns.entry(column.clone()).or_default();
                if held.get(column).copied().unwrap_or(false) {
                    entry.insert((*permission).to_owned());
                }
            }
        }
    }
    Ok(())
}

/// Whether `permission` is held, per column of `columns`, on `object`.
///
/// The attribute is joined by **number**, not asked by name:
/// `has_column_privilege(oid, name, privilege)` raises `column "x" of
/// relation "y" does not exist` for a name the table does not carry, and a
/// declared key's referenced columns are not validated against a table this
/// project does not manage — a stale or mistyped name is realistic here in a
/// way it is not for `pbps-pg`'s own tables. Joined by `attnum` instead
/// (measured on 18.6): a `LEFT JOIN` miss leaves the attribute `NULL`, and
/// `has_column_privilege(oid, NULL::smallint, ...)` answers `NULL` rather
/// than raising — read as "not held", the same rule this module follows for
/// every other NULL the server returns.
async fn column_privileges(
    conn: &mut Conn,
    object: &ObjectName,
    permission: &str,
    columns: &BTreeSet<String>,
) -> Result<BTreeMap<String, bool>, DbError> {
    let values = (0..columns.len())
        .map(|i| format!("(${}::text)", i + 4))
        .collect::<Vec<_>>()
        .join(", ");
    // The same two kinds `read_tables` asks about for a referenced target: an
    // ordinary table or the partitioned one a key may point at (DECISIONS
    // 295).
    let kinds = REFERENCED_KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!(
        "WITH obj AS (
            SELECT c.oid FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ({kinds})
         )
         SELECT wanted.column_name,
                pg_catalog.has_column_privilege(obj.oid, a.attnum, $3) AS held
           FROM (VALUES {values}) AS wanted(column_name)
           CROSS JOIN obj
           LEFT JOIN pg_catalog.pg_attribute a
             ON a.attrelid = obj.oid AND a.attname = wanted.column_name
            AND a.attnum > 0 AND NOT a.attisdropped"
    );
    let mut params = vec![
        Param::Str(&object.schema),
        Param::Str(&object.name),
        Param::Str(permission),
    ];
    params.extend(columns.iter().map(|c| Param::Str(c)));
    let mut out = BTreeMap::new();
    for row in conn.query_with(&query, &params).await? {
        let column = text(&row, "column_name")?;
        // NULL — no such column, or the object itself vanished between the
        // caller's own read and this one — is read as "not held", not
        // skipped: a column this loop asked about and got no clear answer for
        // must not silently drop out of the "every wanted column" count
        // `missing` relies on.
        let held = row.try_get::<bool>("held")?.unwrap_or(false);
        out.insert(column, held);
    }
    Ok(out)
}
