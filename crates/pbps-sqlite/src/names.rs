//! Names a declaration may take, and the one check no trait method can make
//! (#2950).
//!
//! SQLite holds two names as one when they differ only in ASCII case, so a
//! project that declares `Parent` and `parent` declares one object twice. The
//! shared name checks compare names exactly, and the trait's one switch for
//! this (`compares_names_exactly`) can say only "exact" or "unknown, check
//! skipped", neither of which is SQLite's rule, which is known offline. This
//! free function is the check; whoever wires the engine calls it beside the
//! shared ones (route steps 4 to 6 of #2798 decide whether the trait grows a
//! way to say it).

use std::collections::BTreeMap;

use pbps_model::Schema;

use crate::ident;

/// The two tables this tool keeps its ledger in (SPEC 8.1). Compared without
/// regard to ASCII case, because SQLite would. No table, view or index may
/// take either name: the three share one namespace (`validate`).
pub const LEDGER_TABLES: [&str; 2] = ["__pbps_state", "__pbps_lock"];

/// Whether `name` is one of the ledger's tables, or one SQLite holds as one.
#[must_use]
pub fn is_ledger(name: &str) -> bool {
    LEDGER_TABLES.iter().any(|ours| ident::same(ours, name))
}

/// The start of the name a table holds between the two statements of a rename
/// that changes only its ASCII case (`emit`). No declared table, view or index
/// may start with it, so no object this tool manages can hold the name the
/// rename passes through.
pub const RENAME_PREFIX: &str = "__pbps_rename_";

/// Whether `name` starts with [`RENAME_PREFIX`], ASCII case ignored as SQLite
/// would.
#[must_use]
pub fn is_intermediate(name: &str) -> bool {
    name.get(..RENAME_PREFIX.len())
        .is_some_and(|start| ident::same(start, RENAME_PREFIX))
}

/// Problems among the tables, views and indexes a schema declares: they share
/// one namespace per database, and two names there that differ only in ASCII
/// case are one name. Names equal letter for letter are the shared checks'
/// (`check_module_names`, `check_index_names`), and are not repeated here.
/// Case is kept as written; nothing is folded in the declarations.
///
/// Columns of one table are checked with the table (`validate_table`), and
/// triggers have a namespace of their own and are refused anyway.
#[must_use]
pub fn check_collisions(schema: &Schema) -> Vec<String> {
    let mut seen: BTreeMap<String, Vec<(&'static str, String, String)>> = BTreeMap::new();
    let mut note = |kind: &'static str, name: &str, shown: String| {
        seen.entry(ident::fold(name).into_owned())
            .or_default()
            .push((kind, name.to_owned(), shown));
    };
    for (table, def) in &schema.tables {
        note("table", &table.name, table.shown().to_string());
        for index in def.indexes.keys() {
            note("index", index.raw(), index.shown().to_string());
        }
    }
    for (id, module) in &schema.modules {
        if module.kind == pbps_model::ModuleKind::View {
            let name = id.object_name();
            note("view", &name.name, name.shown().to_string());
        }
    }
    let mut problems = Vec::new();
    for group in seen.values() {
        let spelled: std::collections::BTreeSet<&str> =
            group.iter().map(|(_, raw, _)| raw.as_str()).collect();
        if spelled.len() > 1 {
            let listed: Vec<String> = group
                .iter()
                .map(|(kind, _, shown)| ["the ", kind, " `", shown, "`"].concat())
                .collect();
            problems.push(format!(
                "{} are one name to sqlite, which holds names that differ only in ASCII case as \
                 one and keeps tables, views and indexes in a single namespace; rename one",
                listed.join(", ")
            ));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::accepts;
    use pbps_model::{
        Column, Module, ModuleId, ModuleKind, ModuleOptions, ObjectName, Table, TableName,
    };

    fn table_with_index(index: &str) -> Table {
        let mut table = Table::default();
        table
            .columns
            .insert("c".into(), Column::new("int".parse().expect("type")));
        table.indexes.insert(
            index.into(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Column("c".into()),
                    descending: false,
                    opclass: None,
                    collation: None,
                    opclass_parameters: Default::default(),
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: pbps_model::IndexMethod::Btree,
                storage_parameters: Default::default(),
                filegroup: None,
            },
        );
        table
    }

    fn view(name: &str) -> (ModuleId, Module) {
        (
            ModuleId::Named(ObjectName::new("main", name)),
            Module {
                kind: ModuleKind::View,
                description: None,
                options: ModuleOptions::default(),
                definition: "SELECT 1".into(),
            },
        )
    }

    fn schema(tables: &[(&str, Table)], views: &[&str]) -> Schema {
        let mut schema = Schema::default();
        for (name, table) in tables {
            schema
                .tables
                .insert(TableName::new("main", *name), table.clone());
        }
        for name in views {
            let (id, module) = view(name);
            schema.modules.insert(id, module);
        }
        schema
    }

    /// Each case is built as DDL too, and the engine and the check agree.
    #[test]
    fn names_that_differ_only_in_ascii_case_are_reported_exactly_when_the_engine_collides() {
        let t = || Table::default();
        let cases: Vec<(&str, Schema, &str)> = vec![
            (
                "two tables",
                schema(&[("Parent", t()), ("parent", t())], &[]),
                "CREATE TABLE \"Parent\"(c); CREATE TABLE \"parent\"(c)",
            ),
            (
                "table and view",
                schema(&[("T", t())], &["t"]),
                "CREATE TABLE \"T\"(c); CREATE VIEW \"t\" AS SELECT 1",
            ),
            (
                "two views",
                schema(&[], &["V", "v"]),
                "CREATE VIEW \"V\" AS SELECT 1; CREATE VIEW \"v\" AS SELECT 1",
            ),
            (
                "table and index",
                schema(&[("T", t()), ("u", table_with_index("t"))], &[]),
                "CREATE TABLE \"T\"(c); CREATE TABLE u(c); CREATE INDEX \"t\" ON u(c)",
            ),
            (
                "two indexes",
                schema(
                    &[("a", table_with_index("I")), ("b", table_with_index("i"))],
                    &[],
                ),
                "CREATE TABLE a(c); CREATE TABLE b(c); CREATE INDEX \"I\" ON a(c); CREATE INDEX \"i\" ON b(c)",
            ),
        ];
        for (what, schema, ddl) in cases {
            assert!(!accepts(ddl), "the engine refuses {what}");
            assert_eq!(
                check_collisions(&schema).len(),
                1,
                "{what}: {:?}",
                check_collisions(&schema)
            );
        }
        // Not one name: other spellings, non-ASCII case, and exact repeats
        // (the shared checks' business, not repeated here).
        let apart: Vec<(&str, Schema, &str)> = vec![
            (
                "different names",
                schema(&[("a", t()), ("b", t())], &["v"]),
                "CREATE TABLE a(c); CREATE TABLE b(c); CREATE VIEW v AS SELECT 1",
            ),
            (
                "non-ASCII case",
                schema(&[("é", t()), ("É", t())], &[]),
                "CREATE TABLE \"é\"(c); CREATE TABLE \"É\"(c)",
            ),
            (
                "the same index name on two tables",
                schema(
                    &[("a", table_with_index("i")), ("b", table_with_index("i"))],
                    &[],
                ),
                "CREATE TABLE a(c); CREATE TABLE b(c); CREATE INDEX i ON a(c)",
            ),
        ];
        for (what, schema, ddl) in apart {
            assert!(accepts(ddl), "the engine takes {what}");
            assert!(
                check_collisions(&schema).is_empty(),
                "{what}: {:?}",
                check_collisions(&schema)
            );
        }
    }

    #[test]
    fn the_message_names_each_object_and_keeps_the_case_as_written() {
        let t = Table::default();
        let problems = check_collisions(&schema(&[("Parent", t.clone()), ("parent", t)], &[]));
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].contains("`main.Parent`") && problems[0].contains("`main.parent`"),
            "{}",
            problems[0]
        );
    }

    #[test]
    fn the_ledger_names_are_held_as_the_engine_holds_them() {
        for name in ["__pbps_state", "__pbps_lock", "__PBPS_STATE", "__Pbps_Lock"] {
            assert!(is_ledger(name), "{name}");
        }
        for name in ["__pbps_rename_", "__pbps_rename_x", "__PBPS_Rename_x"] {
            assert!(is_intermediate(name) && !is_ledger(name), "{name}");
        }
        for name in [
            "__pbps_rename",
            "_pbps_rename_x",
            "x__pbps_rename_",
            "é__pbps_rename_",
            "",
        ] {
            assert!(!is_intermediate(name), "{name}");
        }
        for name in ["__pbps_statements", "pbps_state", "__pbps_state_", "é"] {
            assert!(!is_ledger(name), "{name}");
        }
        assert!(
            accepts("CREATE TABLE __pbps_state(a)")
                && !accepts("CREATE TABLE __pbps_state(a); CREATE TABLE __PBPS_STATE(a)")
        );
    }
}
