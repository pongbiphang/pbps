//! What the shared scheduler needs to know about SQLite's own refusals (#2950).
//!
//! Measured on 3.54.1: while any view of the schema is invalid (it names a
//! table or column that is gone), `DROP COLUMN`, `RENAME COLUMN` and `RENAME TO`
//! fail, on any table, whether or not they touch what the view reads; the
//! other forms do not. A view is invalid from the statement that removes what
//! it reads until the one that makes it again, and a view's alteration is the
//! last class of a plan, so a view that reads a dropped table or column would
//! stand invalid across every statement in between.
//!
//! The answer is the one SQL Server's schema binding gets: such a view is
//! dropped before the change and created again after it, and the shared classes
//! (module drops first, creates last) do the bracketing. This module says
//! *which* views, by a scan of their text with SQLite's own lexis, and says it
//! to both callers, the differ (through `schema_bound_teardown`) and the order
//! check (through `engine_effects`), so the plan the first writes is the plan
//! the second verifies. The scan over-approximates: a false yes tears a view
//! down that needed no tearing; a false no leaves the engine to refuse the
//! apply, inside the plan's transaction.
//!

use std::collections::BTreeSet;

use pbps_model::operation::{
    Effects, Operation, OwnerError, Owners, Requirement, Snapshot, module_key,
};
use pbps_model::{Change, ColumnChange, Module, ModuleChange, ModuleId, ModuleKind, Schema};

use crate::dialect::Sqlite;
use crate::{ident, lexis};

/// Whether `text` may name `name` (ASCII case ignored, non-ASCII exact).
pub(crate) fn may_name(text: &str, name: &str) -> bool {
    lexis::names(text, name, ident::same)
}

fn views(schema: &Schema) -> impl Iterator<Item = (&ModuleId, &Module)> {
    schema
        .modules
        .iter()
        .filter(|(_, module)| module.kind == ModuleKind::View)
}

/// The views of `base` that `change` alone leaves invalid.
fn blocked_by(base: &Schema, change: &Change) -> BTreeSet<ModuleId> {
    let reading = |table: &str, column: Option<&str>| -> BTreeSet<ModuleId> {
        views(base)
            .filter(|(_, view)| {
                may_name(&view.definition, table)
                    && column.is_none_or(|name| may_name(&view.definition, name))
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    match change {
        Change::Column(ColumnChange::DropColumn { column, .. }) => {
            reading(&column.table.name, Some(column.name.raw()))
        }
        Change::Table(pbps_model::TableChange::DropTable { name, .. }) => reading(&name.name, None),
        Change::Module(ModuleChange::DropModule { id, .. }) => id
            .referenced_name()
            .map(|name| {
                views(base)
                    .filter(|(reader, view)| {
                        *reader != id && may_name(&view.definition, &name.name)
                    })
                    .map(|(reader, _)| reader.clone())
                    .collect()
            })
            .unwrap_or_default(),
        // Every other change leaves a valid view valid: a rename rewrites the
        // views that read the old name, an added column or index is read by no
        // view, and a view's alteration is its own drop and create, which
        // nothing runs between.
        Change::Table(_)
        | Change::TableProperty(_)
        | Change::Partition(_)
        | Change::Column(_)
        | Change::Constraint(_)
        | Change::Index(_)
        | Change::Policy(_)
        | Change::Row(_)
        | Change::Module(_)
        | Change::Permission(_)
        | Change::TypeValue(_)
        | Change::Role(_)
        | Change::Type(_)
        | Change::Extension(_)
        | Change::Search(_) => BTreeSet::new(),
    }
}

/// `torn`, and every view that reads one of them: a view cannot stand while
/// what it reads is away.
fn close(base: &Schema, torn: &mut BTreeSet<ModuleId>) {
    loop {
        let readers: BTreeSet<ModuleId> = views(base)
            .filter(|(id, view)| {
                !torn.contains(*id)
                    && torn.iter().any(|gone| {
                        gone.referenced_name()
                            .is_some_and(|name| may_name(&view.definition, &name.name))
                    })
            })
            .map(|(id, _)| id.clone())
            .collect();
        if readers.is_empty() {
            return;
        }
        torn.extend(readers);
    }
}

/// The views `change` leaves invalid, with the views that read them.
fn torn_down_by(base: &Schema, change: &Change) -> BTreeSet<ModuleId> {
    let mut torn = blocked_by(base, change);
    close(base, &mut torn);
    torn
}

/// What the differ rebuilds: every view the changes leave invalid that the
/// declaration keeps. A view the plan drops or creates needs nothing more.
pub(crate) fn teardown(base: &Schema, declared: &Schema, changes: &[Change]) -> BTreeSet<ModuleId> {
    let mut torn: BTreeSet<ModuleId> = changes
        .iter()
        .flat_map(|change| blocked_by(base, change))
        .collect();
    close(base, &mut torn);
    torn.into_iter()
        .filter(|id| base.modules.contains_key(id) && declared.modules.contains_key(id))
        .collect()
}

/// What only SQLite knows about a step: the checks and filtered indexes that
/// name a column stand in the way of dropping it, as do the views that read
/// it.
pub(crate) fn effects(
    dialect: &Sqlite,
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
) -> Result<Effects, OwnerError> {
    // A rename does not block: SQLite rewrites the checks and indexes that
    // name what it renames.
    let mut fx =
        pbps_dialect::engine_effects::text_dependents(dialect, op, owners, before, after, false)?;
    for view in torn_down_by(before.schema, op.change()) {
        fx.requires.push(Requirement::Absent(module_key(&view)));
    }
    Ok(fx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Fresh;

    /// A text that names a column, as the engine reads it: `DROP COLUMN` is
    /// refused while a view or a check of the table does. The scan must say yes
    /// to every one the engine refuses (a false no is an apply that fails), and
    /// may say yes to a few it does not (a false yes tears a view down).
    #[test]
    fn the_scan_names_every_column_the_engine_will_not_let_go() {
        // (a view body over t(a, b), whether the engine refuses to drop b)
        let bodies: &[(&str, bool)] = &[
            ("SELECT b FROM t", true),
            ("SELECT B FROM t", true),
            ("SELECT `b` FROM t", true),
            ("SELECT [b] FROM t", true),
            ("SELECT t.b FROM t", true),
            ("SELECT a FROM t WHERE b > 0", true),
            ("SELECT a /* x */ , b FROM t", true),
            // The comment ends at its first `*/`: what follows is code.
            ("SELECT a /* /* */ , b FROM t", true),
            ("SELECT a FROM t ORDER BY b", true),
            ("SELECT a FROM t -- b\n WHERE a > b", true),
            ("SELECT * FROM t", false),
            ("SELECT a FROM t", false),
            ("SELECT a, 'b' FROM t", false),
            ("SELECT a /* b */ FROM t", false),
            ("SELECT a FROM t -- b", false),
            ("SELECT a AS bb FROM t", false),
            ("SELECT a AS b_ FROM t", false),
            ("SELECT a AS \"bb\" FROM t", false),
        ];
        for (body, refused) in bodies {
            check_body(body, *refused, *refused);
        }
        // The one the scan over-approximates on purpose: once `b` is gone SQLite
        // reads a double-quoted `"b"` as the string it falls back to, so the view
        // survives the drop, and the scan still counts it a name.
        check_body("SELECT \"b\" FROM t", false, true);
    }

    fn check_body(body: &str, refused: bool, scan_says: bool) {
        let mut db = Fresh::memory();
        let c = db.connection();
        c.execute_batch("CREATE TABLE t(a int, b int)")
            .expect("table");
        c.execute_batch(&["CREATE VIEW v AS ", body].concat())
            .unwrap_or_else(|e| panic!("{body}: {e}"));
        let engine_refuses = c.execute_batch("ALTER TABLE t DROP COLUMN b").is_err();
        let scan = may_name(body, "b");
        assert_eq!(engine_refuses, refused, "the engine's answer for {body}");
        assert!(scan || !engine_refuses, "a false no for {body}");
        assert_eq!(scan, scan_says, "{body}");
    }

    #[test]
    fn the_scan_reads_names_as_sqlite_folds_them() {
        // ASCII case folds; non-ASCII does not; a longer name is another name.
        assert!(may_name("select B from t", "b"));
        assert!(!may_name("select É from t", "é"));
        assert!(may_name("select é from t", "é"));
        assert!(!may_name("select bb from t", "b"));
        assert!(!may_name("select b$ from t", "b"));
        // A name with a doubled quote is one name.
        assert!(may_name("select \"a\"\"b\" from t", "a\"b"));
        assert!(!may_name("select \"a\"\"b\" from t", "a"));
        assert!(!may_name("", "b"));
    }

    fn schema_of(views: &[(&str, &str)]) -> Schema {
        let mut s = Schema::default();
        for (name, body) in views {
            s.modules.insert(
                ModuleId::Named(pbps_model::ObjectName::new("main", *name)),
                Module {
                    kind: ModuleKind::View,
                    description: None,
                    options: Default::default(),
                    definition: (*body).to_owned(),
                },
            );
        }
        s
    }

    fn id(name: &str) -> ModuleId {
        ModuleId::Named(pbps_model::ObjectName::new("main", name))
    }

    fn drop_column(table: &str, column: &str) -> Change {
        Change::Column(ColumnChange::DropColumn {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, column, 0),
            column: pbps_model::TableName::new("main", table).column(column),
        })
    }

    #[test]
    fn a_view_is_torn_down_by_the_column_table_or_view_it_reads_and_by_nothing_else() {
        let base = schema_of(&[
            ("reads_b", "SELECT b FROM t"),
            ("reads_a", "SELECT a FROM t"),
            ("other_table", "SELECT b FROM u"),
            ("star", "SELECT * FROM t"),
            ("over_view", "SELECT * FROM reads_b"),
        ]);
        let torn = torn_down_by(&base, &drop_column("t", "b"));
        // `other_table` names `b` but not `t`: no view of it is torn.
        assert_eq!(torn, [id("reads_b"), id("over_view")].into_iter().collect());
        let dropped_table = Change::Table(pbps_model::TableChange::DropTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "t", 0),
            name: pbps_model::TableName::new("main", "t"),
            detach_from: None,
        });
        let torn = torn_down_by(&base, &dropped_table);
        assert_eq!(
            torn,
            [id("reads_b"), id("reads_a"), id("star"), id("over_view")]
                .into_iter()
                .collect()
        );
        // A rename, an added column and an index change tear nothing.
        let rename = Change::Column(ColumnChange::RenameColumn {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "b", 0),
            table: pbps_model::TableName::new("main", "t"),
            from: "b".into(),
            to: "c".into(),
            table_was: None,
        });
        assert!(torn_down_by(&base, &rename).is_empty());
    }

    #[test]
    fn a_view_that_reads_a_dropped_view_is_torn_down_with_it() {
        let base = schema_of(&[
            ("v1", "SELECT a FROM t"),
            ("v2", "SELECT * FROM v1"),
            ("v3", "SELECT a FROM t"),
        ]);
        let dropped = Change::Module(ModuleChange::DropModule {
            id: id("v1"),
            kind: ModuleKind::View,
        });
        assert_eq!(
            torn_down_by(&base, &dropped),
            [id("v2")].into_iter().collect()
        );
    }

    #[test]
    fn only_a_view_both_sides_keep_is_rebuilt_by_the_differ() {
        let base = schema_of(&[("kept", "SELECT b FROM t"), ("gone", "SELECT b FROM t")]);
        let declared = schema_of(&[("kept", "SELECT a FROM t"), ("new", "SELECT a FROM t")]);
        let torn = teardown(&base, &declared, &[drop_column("t", "b")]);
        assert_eq!(torn, [id("kept")].into_iter().collect());
    }
}
