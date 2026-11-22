//! Computed columns, and the refusals for changes that reach them.

use super::*;

/// The column a change renames, drops, retypes or tightens or relaxes on
/// `table`, under the name its expression would use, and what the change
/// does to it. Every one is an `ALTER COLUMN` or an `sp_rename` the engine
/// refuses on a computed column's input (#1174).
fn input_change<'a>(change: &'a Change, table: &TableName) -> Option<(&'a str, &'static str)> {
    if let Change::Column(ColumnChange::RenameColumn { table: t, from, .. }) = change
        && t == table
    {
        return Some((from.raw(), "renames"));
    }
    if let Change::Column(ColumnChange::DropColumn { column, .. }) = change
        && column.table == *table
    {
        return Some((column.name.raw(), "drops"));
    }
    if let Change::Column(ColumnChange::AlterColumnType { column, .. }) = change
        && column.table == *table
    {
        return Some((column.name.raw(), "retypes or recollates"));
    }
    if let Change::Column(ColumnChange::AlterColumnNullability { column, .. }) = change
        && column.table == *table
    {
        return Some((column.name.raw(), "changes the nullability of"));
    }
    None
}

/// The plans a standing SQL Server computed column cannot be part of, each
/// refused by name (DEC-1174.1):
///
/// - a rename, drop, retype or nullability change of a column its expression
///   may read: 15336 and 4922, measured on 17.0;
/// - an alter or drop of a module it may call: 3729, even without
///   SCHEMABINDING;
/// - and, for one this plan adds, a module this plan creates, which the
///   ordering puts after it (class 14 after class 9).
///
/// "May" by [`Dialect::may_name`], which over-approximates. A computed column
/// this plan drops, or changes and so drops and re-adds, is out of the way
/// before any of these and is not standing.
pub(super) fn refuse_temporal_changes(
    base: Side<'_>,
    declared: Side<'_>,
    changes: &[Change],
    errs: &mut Vec<DiffError>,
) {
    let temporal = |schema: &Schema, name: &TableName| {
        schema
            .tables
            .get(name)
            .is_some_and(|t| t.system_time.is_some())
    };
    let mut refused: BTreeMap<TableName, Vec<String>> = BTreeMap::new();
    let mut refuse = |table: &TableName, what: String| {
        let list = refused.entry(table.clone()).or_default();
        if !list.contains(&what) {
            list.push(what);
        }
    };
    // A difference in `system_time` itself, which no change carries: without
    // this it would plan nothing and record the declaration as applied.
    for (uid, declared_name) in &declared.ids.tables {
        let Some(base_name) = base.ids.tables.get(uid) else {
            continue;
        };
        if let (Some(b), Some(d)) = (
            base.schema.tables.get(base_name),
            declared.schema.tables.get(declared_name),
        ) && b.system_time != d.system_time
        {
            refuse(declared_name, "change its `system_time`".to_owned());
        }
    }
    // A table this plan creates is created with every change it splits out
    // of the `CREATE` (its foreign keys, #1501 review): those are the
    // creation, not a change to a table that stands. By the change, not by
    // the base: a new table may take the name of one the plan renames or
    // drops. Whatever drops or renames a table at that name is still asked
    // about below, so a temporal table dropped and recreated under its own
    // name is still refused.
    let created: BTreeSet<&TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::Table(TableChange::CreateTable { name, .. }) = c {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    // A grant or a trigger on the table is not a change to the pair, and
    // `table()` names neither; a rename is reached by both of its names.
    for change in changes {
        let ends_a_table = matches!(
            change,
            Change::Table(TableChange::DropTable { .. })
                | Change::Table(TableChange::RenameTable { .. })
        );
        if matches!(change, Change::Table(TableChange::CreateTable { .. }))
            || (!ends_a_table && change.table().is_some_and(|t| created.contains(t)))
        {
            continue;
        }
        // A nullable column the engine adds to the history beside the table,
        // NULL in every row of both, with versioning left on (DEC-1177.1,
        // measured on 17.0). A period column is declared through
        // `system_time`, whose difference is refused above.
        if let Change::Column(ColumnChange::AddColumn { column, .. }) = change
            && column.nullable
        {
            continue;
        }
        // A drop or a rename acts on the table the base has at that name, and
        // a rename brings it to the declared one: asked of those sides, so the
        // ordinary table a new temporal one replaces may still go.
        let asked: Vec<&TableName> =
            if let Change::Table(TableChange::DropTable { name, .. }) = change {
                vec![name]
                    .into_iter()
                    .filter(|n| temporal(base.schema, n))
                    .collect()
            } else if let Change::Table(TableChange::RenameTable { from, to, .. }) = change {
                [(from, base.schema), (to, declared.schema)]
                    .into_iter()
                    .filter(|(n, side)| temporal(side, n))
                    .map(|(n, _)| n)
                    .collect()
            } else {
                change
                    .table()
                    .into_iter()
                    .filter(|n| temporal(base.schema, n) || temporal(declared.schema, n))
                    .collect()
            };
        // The one kind admitted is refused only for its nullability: say so.
        let what = if matches!(change, Change::Column(ColumnChange::AddColumn { .. })) {
            "add a NOT NULL column".to_owned()
        } else {
            change_in_words(change)
        };
        for table in asked {
            refuse(table, what.clone());
        }
    }
    errs.extend(
        refused
            .into_iter()
            .map(|(table, what)| DiffError::TemporalTableChange { table, what }),
    );
}

pub(super) fn refuse_computed_dependencies(
    base: &Schema,
    declared: &Schema,
    dialect: &(impl Dialect + ?Sized),
    screen: Screen,
    changes: &[Change],
    errs: &mut Vec<DiffError>,
) {
    // A computed column calls functions and nothing else: a view or a
    // procedure that shares a name it uses is never what it calls.
    let function = |id: &ModuleId| {
        [base, declared].iter().any(|s| {
            s.modules
                .get(id)
                .is_some_and(|m| m.kind == pbps_model::ModuleKind::Function)
        })
    };
    let dropped = |table: &TableName, name: &ColumnName| {
        changes.iter().any(|c| {
            matches!(c, Change::Column(ColumnChange::DropComputedColumn { table: t, name: n, .. })
                if t == table && n == name)
        })
    };
    // Added by an `ADD`, or with its table by a `CREATE TABLE`: both come
    // before a module this plan creates (class 7 and 9, before 14).
    let added = |table: &TableName, name: &ColumnName| {
        changes.iter().any(|c| {
            matches!(c, Change::Column(ColumnChange::AddComputedColumn { table: t, name: n, .. })
                if t == table && n == name)
                || matches!(c, Change::Table(TableChange::CreateTable { name: t, table: created, .. })
                    if t == table && created.computed.contains_key(name))
        })
    };
    // A schema-bound module over a computed column the plan drops is the
    // catalog's to name (`sys.sql_modules.is_schema_bound`, DEC-1431.1): the
    // word in a module's text is no proof of the clause (#1439).
    for (table_name, table) in &declared.tables {
        for (name, computed) in &table.computed {
            // Standing: there before the plan and there throughout. One the
            // plan adds, new or again, comes at (9, 3), after every input
            // change, and one it drops is gone at (2, 4), before them.
            let standing = !dropped(table_name, name) && !added(table_name, name);
            // A connected plan's catalog edges judge a standing column, by
            // the database's collation (#1460).
            let screened = standing && screen == Screen::Text;
            let at = table_name.column(name);
            for change in changes {
                if screened
                    && let Some((column, what)) = input_change(change, table_name)
                    && dialect.may_name(&computed.expression, column)
                {
                    errs.push(DiffError::ComputedInputChanged {
                        computed: at.clone(),
                        column: column.into(),
                        change: what,
                    });
                }
                let module = match change {
                    Change::Partition(PartitionChange::RebuildPartitionedTable(_))
                    | Change::Index(IndexChange::BuildPartitionedIndex(_))
                    | Change::Partition(PartitionChange::CopyTableRows(_))
                    | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => None,
                    // Standing, or re-added: either way it calls the module
                    // when the module changes. One only dropped is out of the
                    // way first, its module's drop moved after it.
                    Change::Module(ModuleChange::AlterModule { id, .. })
                        if screened || added(table_name, name) =>
                    {
                        Some((id, "alters"))
                    }
                    Change::Module(ModuleChange::DropModule { id, .. })
                        if screened || added(table_name, name) =>
                    {
                        Some((id, "drops"))
                    }
                    Change::Module(ModuleChange::CreateModule { id, .. })
                        if added(table_name, name) =>
                    {
                        Some((id, "creates"))
                    }
                    Change::Table(_)
                    | Change::Partition(PartitionChange::DetachPartition { .. })
                    | Change::Partition(PartitionChange::AttachPartition { .. })
                    | Change::Column(_)
                    | Change::TableProperty(_)
                    | Change::Constraint(_)
                    | Change::Index(IndexChange::SetIndexStorageParameters { .. })
                    | Change::Partition(PartitionChange::SetPartitionDefault { .. })
                    | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
                    | Change::Policy(_)
                    | Change::Index(IndexChange::AddIndex { .. })
                    | Change::Index(IndexChange::DropIndex { .. })
                    | Change::Row(_)
                    | Change::Module(_)
                    | Change::TypeValue(_)
                    | Change::Role(_)
                    | Change::Type(_)
                    | Change::Search(_)
                    | Change::Extension(_)
                    | Change::Permission(_) => None,
                };
                if let Some((id, what)) = module
                    && function(id)
                {
                    let qualified = id.object_name();
                    let name = qualified.name.clone();
                    if dialect.may_name_qualified(
                        &computed.expression,
                        &qualified.schema,
                        &qualified.name,
                    ) {
                        errs.push(DiffError::ComputedFunctionChanged {
                            computed: at.clone(),
                            function: name,
                            change: what,
                        });
                    }
                }
            }
        }
    }
}

/// A table's computed columns (#1174, DEC-1174.1). Neither the expression nor
/// the persistence changes in place, so a column that differs is dropped and
/// added again, which `recreate_retyped_dependents` then rebuilds the indexes
/// and checks over. With no uid, a renamed one is the same pair under two
/// names, which the engine performs as well.
pub(super) fn diff_computed(
    name: &TableName,
    base: &Table,
    declared: &Table,
    changes: &mut Vec<Change>,
) {
    // `declares`, not equality: a read-back holds `not_null` wherever the
    // engine reports the column not nullable, which it also does for an
    // expression that can never be NULL.
    let kept = |wanted: Option<&pbps_model::ComputedColumn>,
                was: Option<&pbps_model::ComputedColumn>| matches!((wanted, was), (Some(w), Some(b)) if w.declares(b));
    for (column, was) in &base.computed {
        if !kept(declared.computed.get(column), Some(was)) {
            changes.push(Change::Column(ColumnChange::DropComputedColumn {
                table: name.clone(),
                name: column.clone(),
                computed: was.clone(),
            }));
        }
    }
    for (column, wanted) in &declared.computed {
        if !kept(Some(wanted), base.computed.get(column)) {
            changes.push(Change::Column(ColumnChange::AddComputedColumn {
                table: name.clone(),
                name: column.clone(),
                computed: wanted.clone(),
            }));
        }
    }
}
