//! What a server needs to hold an exclusion constraint on a partitioned table
//! (DEC-2448.6): PostgreSQL 18. Version 16 refuses one with `0A000 exclusion
//! constraints are not supported on partitioned tables` (measured on 16.15, for
//! `ALTER TABLE .. ADD`, `CREATE TABLE (EXCLUDE ..)` and a B-tree one alike).
//!
//! Not a property of a declaration, which is valid against one server and
//! refused by the next, so it is asked of the connected one, before anything is
//! written: the exclusion sorts after the rows and the failing `ADD` would run on
//! a database the rest of the plan had already changed.
use pbps_db::{Conn, DbError};
use pbps_model::TableName;
use std::collections::BTreeSet;

/// `server_version_num` of the first release that has it.
pub const ARRIVED_IN: i64 = 180_000;

/// Which of `tables` the connected database holds as partitioned tables.
/// A table it does not hold is not in the answer: the plan's own new tables are
/// the caller's to name.
pub async fn standing_partitioned(
    conn: &mut Conn,
    tables: &BTreeSet<TableName>,
) -> Result<BTreeSet<TableName>, DbError> {
    let mut found = BTreeSet::new();
    for table in tables {
        let sql = format!(
            "SELECT c.relkind = 'p' AS partitioned
               FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = {} AND c.relname = {}",
            crate::emit::literal(&table.schema),
            crate::emit::literal(&table.name)
        );
        for row in conn.query(&sql).await? {
            let partitioned = row.try_get::<bool>("partitioned")?.ok_or_else(|| {
                DbError::BadRow("catalog column `partitioned` is null".to_owned())
            })?;
            if partitioned {
                found.insert(table.clone());
            }
        }
    }
    Ok(found)
}

/// One refusal per partitioned table a plan adds an exclusion to, empty from
/// PostgreSQL 18. `adding` are the tables an exclusion is added to, `partitioned`
/// those of them that are partitioned tables, standing or created by the plan.
#[must_use]
pub fn unsupported(
    server_version_num: i64,
    adding: &BTreeSet<TableName>,
    partitioned: &BTreeSet<TableName>,
) -> Vec<String> {
    if server_version_num >= ARRIVED_IN {
        return Vec::new();
    }
    adding
        .intersection(partitioned)
        .map(|table| {
            format!(
                "{table}: an exclusion constraint on a partitioned table needs PostgreSQL 18 or \
                 later, and this server is {major}.{minor}; the engine answers `exclusion \
                 constraints are not supported on partitioned tables` (0A000), measured on \
                 16.15, which is a statement that would fail after everything ordered before it \
                 had run (DEC-2448.6)",
                table = table.shown(),
                major = server_version_num / 10_000,
                minor = server_version_num % 10_000,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<TableName> {
        names.iter().map(|n| TableName::new("app", *n)).collect()
    }

    #[test]
    fn a_partitioned_table_needs_18_and_an_ordinary_one_does_not() {
        let adding = set(&["t", "o"]);
        let partitioned = set(&["t"]);
        let refused = unsupported(160_015, &adding, &partitioned);
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains("app.t") && refused[0].contains("16.15"));
        assert!(!refused[0].contains("app.o"));
        // From 18, and for a table that is not partitioned, nothing is refused.
        assert!(unsupported(180_006, &adding, &partitioned).is_empty());
        assert!(unsupported(160_015, &adding, &set(&[])).is_empty());
        // A partitioned table this plan adds nothing to is not asked.
        assert!(unsupported(160_015, &set(&["o"]), &partitioned).is_empty());
    }
}
