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
use std::collections::{BTreeMap, BTreeSet};

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

/// The tables to ask the catalog about, under the names it holds them by now.
///
/// A plan names a table by what it leaves it as, and the catalog still holds a
/// table the plan renames under its old name: asking for the new one finds
/// nothing, and a partitioned parent's exclusion would pass the version gate
/// and fail `0A000` mid-apply. So a renamed table is asked for as its `from`.
/// A table the plan creates is not asked at all (`created` maps each to whether
/// it is partitioned): the catalog's table of that name, if any, is one the plan
/// replaces, and the plan alone says what the new one is. A name that is only
/// the `from` of a rename is the old table's, and no table the plan leaves there
/// is a standing one.
#[must_use]
pub fn asked(
    adding: &BTreeSet<TableName>,
    renames: &[(TableName, TableName)],
    created: &BTreeMap<TableName, bool>,
) -> BTreeSet<TableName> {
    adding
        .iter()
        .filter(|table| !created.contains_key(*table))
        .filter_map(|table| match renames.iter().find(|(_, to)| to == table) {
            Some((from, _)) => Some(from.clone()),
            None if renames.iter().any(|(from, _)| from == table) => None,
            None => Some(table.clone()),
        })
        .collect()
}

/// The partitioned tables of the plan's final names: the `standing` answer to
/// [`asked`], carried through the plan's renames, and the tables it creates as
/// partitioned.
#[must_use]
pub fn partitioned(
    standing: &BTreeSet<TableName>,
    renames: &[(TableName, TableName)],
    created: &BTreeMap<TableName, bool>,
) -> BTreeSet<TableName> {
    let mut found: BTreeSet<TableName> = standing
        .iter()
        .map(|table| {
            renames
                .iter()
                .find(|(from, _)| from == table)
                .map_or_else(|| table.clone(), |(_, to)| to.clone())
        })
        .collect();
    found.extend(
        created
            .iter()
            .filter(|(_, partitioned)| **partitioned)
            .map(|(table, _)| table.clone()),
    );
    found
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

    fn rename(from: &str, to: &str) -> (TableName, TableName) {
        (TableName::new("app", from), TableName::new("app", to))
    }

    #[test]
    fn a_renamed_table_is_asked_under_its_old_name_and_counted_under_its_new_one() {
        let renames = [rename("ev", "ev2")];
        let created = BTreeMap::new();
        let asked = asked(&set(&["ev2", "o"]), &renames, &created);
        assert_eq!(asked, set(&["ev", "o"]));
        // The catalog says `ev` is partitioned: the plan's `ev2` is.
        assert_eq!(
            partitioned(&set(&["ev"]), &renames, &created),
            set(&["ev2"])
        );
        // It says nothing for `ev`, which is not partitioned: nothing is counted.
        assert!(partitioned(&set(&[]), &renames, &created).is_empty());
    }

    #[test]
    fn a_rename_the_plan_adds_no_exclusion_to_is_not_asked() {
        let renames = [rename("ev", "ev2")];
        let created = BTreeMap::new();
        assert_eq!(asked(&set(&["o"]), &renames, &created), set(&["o"]));
        assert!(asked(&set(&[]), &renames, &created).is_empty());
    }

    #[test]
    fn a_name_the_plan_takes_over_is_not_a_standing_table() {
        // `a` is renamed away and a new `a` is created, and `n` replaces a
        // standing table: the plan alone says what each is.
        let renames = [rename("a", "b")];
        let created = BTreeMap::from([
            (TableName::new("app", "a"), false),
            (TableName::new("app", "n"), true),
        ]);
        assert!(asked(&set(&["a", "n"]), &renames, &created).is_empty());
        assert_eq!(partitioned(&set(&[]), &renames, &created), set(&["n"]));
        // A name that is only the `from` of a rename, with nothing created, is
        // the old table's and is not asked.
        assert!(asked(&set(&["a"]), &renames, &BTreeMap::new()).is_empty());
    }
}
