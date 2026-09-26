//! A managed table's own column-level grants can cover a
//! [`Needed::ManagedTable`] `SELECT` gap the object-level answer alone does
//! not see (issue #392).
//!
//! The only way to lack that `SELECT` is an owner who revoked it from
//! themselves, and such an owner can grant it back column by column. Measured
//! on 18.6: an owner of `app.t (id, label, n)` revokes `SELECT` and then
//! grants `SELECT (id, label, n)`. `has_table_privilege` still answers `false`,
//! `has_column_privilege` answers `true` for every column, and the pre-flight
//! probe shapes all run as that owner: `count(*)`, a `WHERE n IS NULL` count,
//! a `GROUP BY` duplicate count and `SELECT *`. One column short, the probe
//! that reads it is `permission denied`.
//!
//! `doctor` never sees a plan, so it cannot know which columns a probe will
//! read. It asks about **every** column the catalog has instead, the rescue
//! `pbps_mssql::doctor`'s `Columns::Catalog` already makes. That can only turn
//! a false gap into a true "ready": an object-level grant answers every column
//! anyway, and a table one column short keeps its gap.

use std::collections::BTreeMap;

use pbps_db::{Conn, DbError, Param};
use pbps_model::ObjectName;

use super::{MANAGED_KINDS, TableRights, text};

const SELECT: &str = "SELECT";

/// Fills in [`TableRights::columns`] with `SELECT` for every column of every
/// managed table whose object-level answer lacks it.
///
/// Reached only where it can change the answer: a table already covered at
/// object scope needs no column query.
pub(super) async fn fill(
    conn: &mut Conn,
    tables: &mut BTreeMap<ObjectName, TableRights>,
) -> Result<(), DbError> {
    let kinds = MANAGED_KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!(
        "SELECT a.attname AS column_name,
                pg_catalog.has_column_privilege(c.oid, a.attnum, 'SELECT') AS held
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
           JOIN pg_catalog.pg_attribute a
             ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
          WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ({kinds})"
    );
    for (object, rights) in tables.iter_mut() {
        if rights.privileges.contains(SELECT) {
            continue;
        }
        let rows = conn
            .query_with(
                &query,
                &[Param::Str(&object.schema), Param::Str(&object.name)],
            )
            .await?;
        for row in &rows {
            let column = text(row, "column_name")?;
            // NULL, from a column or table that vanished between the two
            // reads, is "not held", never skipped: `covered` counts every
            // column this read returned.
            let held = row.try_get::<bool>("held")?.unwrap_or(false);
            let entry = rights.columns.entry(column).or_default();
            if held {
                entry.insert(SELECT.to_owned());
            }
        }
    }
    Ok(())
}

/// Whether `rights` covers the managed-table `SELECT`: at object scope, or
/// on every one of the columns [`fill`] read.
///
/// No columns read is not covered. A table with no column still needs the
/// object-level grant for `count(*)`, and a table this module never asked
/// about has nothing here to vouch for it.
pub(super) fn covered(rights: &TableRights) -> bool {
    rights.privileges.contains(SELECT)
        || (!rights.columns.is_empty() && rights.columns.values().all(|held| held.contains(SELECT)))
}
