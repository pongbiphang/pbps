//! The managed set: which of a database's tables pbps is answerable for
//! (SPEC §8.2).
//!
//! # Why the scope is drawn at all
//!
//! pbps has to be able to share a database with tooling that was there first —
//! that is the precondition for gradual adoption. So a table nobody declared is
//! not drift; it is somebody else's table. Comparing the whole database instead
//! would make the drift check fire on the first run in every real estate, and a
//! check that always fires is a check nobody reads.
//!
//! # Why the identity file draws it, not the declarations
//!
//! [`crate::diff`] matches by uid and never looks at a table the identity file
//! does not name, so scoping by the ids file is scoping by exactly what the
//! comparison can see. Using the declarations instead would let the two
//! disagree, and the difference would show up as a plan that creates a table
//! that already exists.

use std::collections::BTreeSet;

use pbps_model::{IdsFile, Schema, TableName};

/// A live schema cut down to the managed set, with what fell outside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scoped {
    /// Only the tables pbps manages. This is what drift and `plan --db` compare.
    pub schema: Schema,

    /// Tables the database has that the identity file does not name. Left
    /// untouched — pbps will neither change nor drop them.
    pub unmanaged: Vec<TableName>,

    /// Tables the identity file names that the database does not have.
    ///
    /// Reported rather than quietly treated as "nothing to compare": against a
    /// recorded state this is a table someone dropped by hand, and against a
    /// fresh baseline it means the ids file and this environment describe
    /// different databases.
    pub missing: Vec<TableName>,
}

/// Cuts a live schema down to the tables the identity file names.
pub fn scope(schema: &Schema, ids: &IdsFile) -> Scoped {
    let managed: BTreeSet<&TableName> = ids.tables.values().collect();

    let mut scoped = Schema::default();
    let mut unmanaged = Vec::new();
    for (name, table) in &schema.tables {
        if managed.contains(name) {
            scoped.tables.insert(name.clone(), table.clone());
        } else {
            unmanaged.push(name.clone());
        }
    }

    let missing = managed
        .into_iter()
        .filter(|n| !schema.tables.contains_key(*n))
        .cloned()
        .collect();

    Scoped {
        schema: scoped,
        unmanaged,
        missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use pbps_model::{Column, ColumnType, Table, Uid};

    fn t(s: &str) -> TableName {
        s.parse().unwrap()
    }

    fn schema(names: &[&str]) -> Schema {
        let mut s = Schema::default();
        for n in names {
            let mut columns = IndexMap::new();
            columns.insert(
                "id".to_string(),
                Column::new("int".parse::<ColumnType>().unwrap()),
            );
            s.tables.insert(
                t(n),
                Table {
                    columns,
                    ..Default::default()
                },
            );
        }
        s
    }

    fn ids(entries: &[(&str, &str)]) -> IdsFile {
        let mut ids = IdsFile::default();
        for (uid, name) in entries {
            ids.tables.insert(uid.parse::<Uid>().unwrap(), t(name));
        }
        ids
    }

    #[test]
    fn only_the_declared_tables_are_kept() {
        let scoped = scope(
            &schema(&["dbo.customer", "dbo.order", "dbo.legacy_audit"]),
            &ids(&[("t_aaaaaa", "dbo.customer"), ("t_bbbbbb", "dbo.order")]),
        );
        assert_eq!(
            scoped.schema.tables.keys().collect::<Vec<_>>(),
            vec![&t("dbo.customer"), &t("dbo.order")]
        );
        assert_eq!(scoped.unmanaged, vec![t("dbo.legacy_audit")]);
        assert!(scoped.missing.is_empty());
    }

    /// Somebody else's table is not drift. Without this the check would fire on
    /// the first run in every database that has any history.
    #[test]
    fn an_unmanaged_table_does_not_change_the_managed_state() {
        let ids = ids(&[("t_aaaaaa", "dbo.customer")]);
        assert_eq!(
            scope(&schema(&["dbo.customer"]), &ids).schema,
            scope(&schema(&["dbo.customer", "dbo.other_tool"]), &ids).schema,
        );
    }

    /// A declared table the database does not have must be named, not silently
    /// skipped: it is either a hand-dropped table or an ids file describing a
    /// different database.
    #[test]
    fn a_declared_table_missing_from_the_database_is_reported() {
        let scoped = scope(
            &schema(&["dbo.customer"]),
            &ids(&[("t_aaaaaa", "dbo.customer"), ("t_bbbbbb", "dbo.order")]),
        );
        assert_eq!(scoped.missing, vec![t("dbo.order")]);
        assert!(scoped.unmanaged.is_empty());
    }

    #[test]
    fn an_empty_identity_file_manages_nothing() {
        let scoped = scope(&schema(&["dbo.customer"]), &IdsFile::default());
        assert!(scoped.schema.tables.is_empty());
        assert_eq!(scoped.unmanaged, vec![t("dbo.customer")]);
    }
}
