//! What a retyped or re-keyed column drags with it: foreign keys, indexes and the tightening splits.

use super::*;

/// A standing FK is bound to one backing index, not merely to columns that
/// another surviving key also covers. Offline the binding is unknown, so make
/// every affected managed FK explicit in the reviewed plan. Connected guards
/// use actual catalog dependencies to refuse an unplanned or external removal.
pub(super) fn recreate_referenced_foreign_keys(
    base: Side<'_>,
    declared: Side<'_>,
    renames: &Renames,
    changes: &mut Vec<Change>,
) {
    if !changes.iter().any(|c| {
        matches!(
            c,
            Change::Constraint(ConstraintChange::SetPrimaryKey { from: Some(_), .. })
                | Change::Constraint(ConstraintChange::DropUnique { .. })
                | Change::Index(IndexChange::DropIndex { .. })
        )
    }) {
        return;
    }
    let aligned: BTreeMap<_, _> = base
        .ids
        .tables
        .iter()
        .filter_map(|(uid, old_name)| {
            let new_name = declared.ids.tables.get(uid)?;
            let table = base.schema.tables.get(old_name)?;
            Some((new_name.clone(), renames.apply(table, old_name)))
        })
        .collect();
    let mut removed = BTreeSet::new();
    for change in changes.iter() {
        let (table, columns) = match change {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => continue,
            // No foreign key references an exclusion constraint (measured:
            // 42830), so its drop frees no key (DEC-2448.3).
            Change::Constraint(ConstraintChange::SetExclusion { .. }) => continue,
            Change::Constraint(ConstraintChange::SetPrimaryKey {
                table,
                from: Some(key),
                ..
            }) => (table, key.columns.iter().cloned().collect::<BTreeSet<_>>()),
            Change::Constraint(ConstraintChange::DropUnique { table, name }) => {
                let Some(key) = aligned.get(table).and_then(|t| t.unique.get(name)) else {
                    continue;
                };
                (table, key.columns.iter().cloned().collect::<BTreeSet<_>>())
            }
            Change::Index(IndexChange::DropIndex { table, name }) => {
                // Only a unique index over plain columns can back a foreign
                // key: the engine refuses one over an expression (DEC-1169.2).
                let Some(columns) = aligned
                    .get(table)
                    .and_then(|t| t.indexes.get(name))
                    .filter(|i| i.unique && i.filter.is_none())
                    .and_then(|i| i.column_keys())
                else {
                    continue;
                };
                (table, columns.into_iter().collect::<BTreeSet<_>>())
            }
            Change::Table(_)
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(_)
            | Change::TableProperty(_)
            | Change::Constraint(ConstraintChange::SetPrimaryKey { .. })
            | Change::Constraint(ConstraintChange::AddUnique { .. })
            | Change::Constraint(ConstraintChange::AddForeignKey { .. })
            | Change::Constraint(ConstraintChange::DropForeignKey { .. })
            | Change::Constraint(ConstraintChange::AddCheck { .. })
            | Change::Constraint(ConstraintChange::DropCheck { .. })
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Row(_)
            | Change::Module(_)
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_) => continue,
        };
        // PostgreSQL can bind a permutation of a composite candidate key.
        // SQL Server requires its order; a set conservatively covers both.
        removed.insert((table.clone(), columns));
    }
    for (table, before) in &aligned {
        let Some(after) = declared.schema.tables.get(table) else {
            continue;
        };
        for (name, fk) in &before.foreign_keys {
            let Some(wanted) = after.foreign_keys.get(name) else {
                continue;
            };
            if !removed.contains(&(fk.references_table.clone(), fk.references_columns.iter().cloned().collect()))
                || changes.iter().any(|c| matches!(c, Change::Constraint(ConstraintChange::DropForeignKey { table: on, name: n }) if on == table && n == name)) {
                continue;
            }
            changes.push(Change::Constraint(ConstraintChange::DropForeignKey {
                table: table.clone(),
                name: name.clone(),
            }));
            changes.push(Change::Constraint(ConstraintChange::AddForeignKey {
                table: table.clone(),
                name: name.clone(),
                constraint: Box::new(wanted.clone()),
            }));
        }
    }
}

/// A foreign key that reads the same by name on both sides can still point at
/// two tables: across skipped revisions, the table it referenced is dropped and
/// another is renamed into that name (DEC-536.1). The standing key blocks the
/// drop, and the engine never binds it to the new occupant, so the plan drops
/// and re-adds it visibly. Identity is compared by uid, read from the baseline
/// as spelled there, not through the plan's renames.
pub(super) fn rebind_foreign_keys_to_a_new_occupant(
    base: Side<'_>,
    declared: Side<'_>,
    changes: &mut Vec<Change>,
) {
    let base_uid: BTreeMap<&TableName, &pbps_model::Uid> = base
        .ids
        .tables
        .iter()
        .map(|(uid, name)| (name, uid))
        .collect();
    let declared_uid: BTreeMap<&TableName, &pbps_model::Uid> = declared
        .ids
        .tables
        .iter()
        .map(|(uid, name)| (name, uid))
        .collect();
    let mut pairs = Vec::new();
    for (uid, old_name) in &base.ids.tables {
        let (Some(new_name), Some(before)) = (
            declared.ids.tables.get(uid),
            base.schema.tables.get(old_name),
        ) else {
            continue;
        };
        let Some(after) = declared.schema.tables.get(new_name) else {
            continue;
        };
        for (name, fk) in &before.foreign_keys {
            let Some(wanted) = after.foreign_keys.get(name) else {
                continue;
            };
            // Already replaced when the diff added it back. Matched by the add,
            // not the drop: a dropped table this plan's rename takes the name
            // of emits a drop of its own key at the same address, and on
            // PostgreSQL that key can share this one's name.
            if base_uid.get(&fk.references_table) == declared_uid.get(&wanted.references_table)
                || changes.iter().any(|c| {
                    matches!(c, Change::Constraint(ConstraintChange::AddForeignKey { table, name: n, .. })
                        if table == new_name && n == name)
                })
            {
                continue;
            }
            pairs.push(Change::Constraint(ConstraintChange::DropForeignKey {
                table: new_name.clone(),
                name: name.clone(),
            }));
            pairs.push(Change::Constraint(ConstraintChange::AddForeignKey {
                table: new_name.clone(),
                name: name.clone(),
                constraint: Box::new(wanted.clone()),
            }));
        }
    }
    changes.extend(pairs);
}

/// A NOT NULL a parent with external partitions drops (#1533) reaches every
/// partition, but on 18 one a partition set of its own outlives it
/// (DEC-1687.1). A declared partition is brought back by its own change after
/// the parent's; an external one is listed nowhere, so the parent's change
/// carries the clearing itself (DEC-1533.1, #1833 review).
pub(super) fn clear_external_partitions_after_loosening(declared: &Schema, changes: &mut [Change]) {
    let external = |table: &TableName| {
        declared
            .tables
            .get(table)
            .and_then(|t| t.partition_by.as_ref())
            .is_some_and(|by| by.external)
    };
    for change in changes {
        if let Change::Column(ColumnChange::AlterColumnNullability {
            column,
            to_nullable: true,
            through_external_partitions,
            ..
        })
        | Change::Column(ColumnChange::AlterColumnType {
            column,
            from_nullable: false,
            to_nullable: true,
            through_external_partitions,
            ..
        }) = change
        {
            *through_external_partitions = external(&column.table);
        }
    }
}

/// A tightening folded into a retype comes out of it when the plan writes or
/// deletes rows of its table. The retype keeps the column nullable, and an
/// `AlterColumnNullability` tightens it after the rows, which may be what
/// fills or removes its NULLs (#1367). The same split DEC-1168.1 makes for a
/// recomputed column, for the same reason: the check has to meet the values
/// the plan leaves, not the ones it found.
///
/// Not on a column the plan puts an identity on in a table it inserts rows
/// into: the column must be NOT NULL before the identity, and the identity
/// on before the rows (#1930 review).
pub(super) fn split_tightenings_from_retypes(changes: &mut Vec<Change>) {
    let rows = tables_with_row_changes(changes);
    let identified = identities_put_on(changes.iter());
    let inserted = tables_with_inserts(changes.iter());
    let late = numbering_after_deletes(changes.iter());
    let mut split = Vec::new();
    for change in changes.iter_mut() {
        if let Change::Column(ColumnChange::AlterColumnType {
            uid,
            column,
            to,
            from_nullable: true,
            to_nullable,
            to_collation,
            ..
        }) = change
            && !*to_nullable
            && rows.contains(&column.table)
            && !(identified.contains(column)
                && inserted.contains(&column.table)
                && !late.contains(&column.table))
        {
            *to_nullable = true;
            split.push(Change::Column(ColumnChange::AlterColumnNullability {
                uid: uid.clone(),
                column: column.clone(),
                ty: to.clone(),
                to_nullable: false,
                through_external_partitions: false,
                collation: to_collation.clone(),
            }));
        }
    }
    changes.extend(split);
}

/// A foreign key a type change would leave between two types the engine cannot
/// join is named here, at plan time, where the apply would otherwise take the key
/// down for the change and be refused when it put it back (#2359, DEC-2359.1).
pub(super) fn refuse_unjoinable_foreign_keys(
    declared: &Schema,
    dialect: &(impl Dialect + ?Sized),
    changes: &[Change],
    errs: &mut Vec<DiffError>,
) {
    for key in crate::foreign_keys::unjoinable(changes, declared, dialect) {
        let (referencing, referencing_type) = &key.referencing;
        let (referenced, referenced_type) = &key.referenced;
        let next = match (key.referencing_retyped, key.referenced_retyped) {
            (true, true) => format!(
                "Declare types for {} and {} that a key can join, or remove the foreign key.",
                referencing.shown(),
                referenced.shown()
            ),
            (changed_is_referencing, _) => {
                let (changed, other) = if changed_is_referencing {
                    (referencing, referenced)
                } else {
                    (referenced, referencing)
                };
                let record = if dialect.records_conversions() {
                    format!(
                        " (when the engine cannot make that change by itself, record its \
                         conversion with `pbps convert {} --using -`)",
                        other.shown()
                    )
                } else {
                    String::new()
                };
                format!(
                    "Change {other} to a type a key can join with it in the same plan{record}, \
                     or declare {changed} with the type it has today, or remove the foreign key.",
                    other = other.shown(),
                    changed = changed.shown(),
                )
            }
        };
        errs.push(DiffError::ForeignKeyTypesDiffer {
            table: key.table.clone(),
            constraint: key.constraint.clone(),
            referencing: referencing.clone(),
            referenced: referenced.clone(),
            referencing_type: referencing_type.raw().to_string(),
            referenced_type: referenced_type.raw().to_string(),
            next,
        });
    }
}

/// Dependency maintenance is visible in the saved plan, including its ordinary
/// drop/add risks (DECISIONS 461). The emitter must not discover extra
/// cross-table work after approval. Align identities first so the selection survives simultaneous
/// table and column renames; explicit replacements already have their own pair.
pub(super) fn recreate_retyped_dependents(
    base: Side<'_>,
    declared: Side<'_>,
    renames: &Renames,
    dialect: &(impl Dialect + ?Sized),
    changes: &mut Vec<Change>,
) {
    let also = |dependents: &mut pbps_dialect::RetypeDependents,
                more: pbps_dialect::RetypeDependents| {
        dependents.keys_and_indexes |= more.keys_and_indexes;
        dependents.checks |= more.checks;
        dependents.filtered_indexes |= more.filtered_indexes;
        dependents.foreign_keys |= more.foreign_keys;
    };
    // A column can carry two of these, a retype and the tightening split out
    // of it (#1367), so the answers are merged rather than the last kept.
    let mut retyped: BTreeMap<ColumnRef, pbps_dialect::RetypeDependents> = BTreeMap::new();
    // A computed column dropped and added again under its name, or replacing
    // an ordinary column of that name, or replaced by one: what is over the
    // name has to come down first (4922, measured on 17.0) and go back after,
    // as around a retype. No foreign key is ever over a computed column
    // (validation), and a check or a filter names it in text this does not
    // parse (#1174).
    let removed = |column: &ColumnRef| -> Option<bool> {
        changes.iter().find_map(|c| {
            if let Change::Column(ColumnChange::DropComputedColumn { table, name, .. }) = c
                && *table == column.table
                && *name == column.name
            {
                Some(true)
            } else if let Change::Column(ColumnChange::DropColumn {
                column: dropped, ..
            }) = c
                && dropped == column
            {
                Some(false)
            } else {
                None
            }
        })
    };
    let recomputed: BTreeSet<ColumnRef> = changes
        .iter()
        .filter_map(|change| {
            if let Change::Column(ColumnChange::AddComputedColumn { table, name, .. }) = change {
                Some((table.column(name), true))
            } else if let Change::Column(ColumnChange::AddColumn { table, name, .. }) = change {
                Some((table.column(name), false))
            } else {
                None
            }
        })
        .filter(|(column, computed)| removed(column).is_some_and(|was| was || *computed))
        .map(|(column, _)| column)
        .collect();
    for column in recomputed {
        also(
            retyped.entry(column).or_default(),
            pbps_dialect::RetypeDependents {
                keys_and_indexes: true,
                checks: true,
                filtered_indexes: true,
                foreign_keys: false,
            },
        );
    }
    for (column, dependents) in changes.iter().filter_map(|change| match change {
        Change::Partition(PartitionChange::RebuildPartitionedTable(_))
        | Change::Index(IndexChange::BuildPartitionedIndex(_))
        | Change::Partition(PartitionChange::CopyTableRows(_))
        | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => None,
        Change::Column(ColumnChange::AlterColumnType {
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            from_collation,
            to_collation,
            ..
        }) => {
            // A nullability change in the same statement brings its own
            // blockers, which a type change alone may not have (#1363).
            let dependents = pbps_dialect::RetypeDependents::of_retype(
                dialect,
                (from, to),
                from_collation != to_collation,
                (from_nullable != to_nullable).then_some(*to_nullable),
            );
            Some((column.clone(), dependents))
        }
        Change::Column(ColumnChange::AlterColumnNullability {
            column,
            to_nullable,
            ..
        }) => Some((column.clone(), dialect.nullability_dependents(*to_nullable))),
        // A conversion by name leaves constants of the old type in checks,
        // keys and partial indexes that the replacement cannot be compared
        // with. The engine's own assignment cast of a domain's base change
        // keeps them (measured on 16 and 18, DEC-1635.9), so nothing comes
        // down for it.
        Change::TypeValue(TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Retype { column, values, .. },
            ..
        }) => Some((
            column.clone(),
            match values.as_ref() {
                pbps_model::ValueChange::Relabel { .. } => dialect.enum_rebuild_dependents(),
                pbps_model::ValueChange::Cast { .. } => Default::default(),
            },
        )),
        Change::Table(_)
        | Change::Partition(PartitionChange::DetachPartition { .. })
        | Change::Partition(PartitionChange::AttachPartition { .. })
        | Change::Column(ColumnChange::AddColumn { .. })
        | Change::Column(ColumnChange::DropColumn { .. })
        | Change::Column(ColumnChange::RenameColumn { .. })
        | Change::Column(ColumnChange::AlterColumnDefault { .. })
        | Change::Column(ColumnChange::AlterColumnExpression { .. })
        | Change::Column(ColumnChange::AlterColumnNumbering { .. })
        | Change::Column(ColumnChange::SetColumnDeprecated { .. })
        | Change::TableProperty(_)
        | Change::Constraint(_)
        | Change::Index(IndexChange::SetIndexStorageParameters { .. })
        | Change::Partition(PartitionChange::SetPartitionDefault { .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
        | Change::Policy(_)
        | Change::Index(IndexChange::AddIndex { .. })
        | Change::Column(ColumnChange::AddComputedColumn { .. })
        | Change::Column(ColumnChange::DropComputedColumn { .. })
        | Change::Index(IndexChange::DropIndex { .. })
        | Change::Row(_)
        | Change::Module(_)
        | Change::TypeValue(_)
        | Change::Role(_)
        | Change::Type(_)
        | Change::Search(_)
        | Change::Extension(_)
        | Change::Permission(_) => None,
    }) {
        let merged = retyped.entry(column).or_default();
        also(merged, dependents);
    }
    if retyped.is_empty() {
        return;
    }
    // A partition keeps its own CHECKs and filtered indexes over the columns
    // it takes from its parent, and the parent's retype reaches them.
    for (column, dependents) in retyped.clone() {
        let mut parents = vec![column.table.clone()];
        let mut reached = BTreeSet::from([column.table.clone()]);
        while let Some(parent) = parents.pop() {
            for (name, table) in &declared.schema.tables {
                if table
                    .partition_of
                    .as_ref()
                    .is_some_and(|of| of.parent == parent)
                    && reached.insert(name.clone())
                {
                    also(
                        retyped.entry(name.column(&column.name)).or_default(),
                        dependents,
                    );
                    parents.push(name.clone());
                }
            }
        }
    }
    for (uid, old_name) in &base.ids.tables {
        let Some(name) = declared.ids.tables.get(uid) else {
            continue;
        };
        let (Some(old), Some(after)) = (
            base.schema.tables.get(old_name),
            declared.schema.tables.get(name),
        ) else {
            continue;
        };
        let before = renames.apply(old, old_name);
        let key_column = |column: &ColumnName| {
            retyped
                .get(&name.column(column))
                .is_some_and(|d| d.keys_and_indexes)
        };
        let checks = retyped.iter().any(|(c, d)| &c.table == name && d.checks);
        let filters = retyped
            .iter()
            .any(|(c, d)| &c.table == name && d.filtered_indexes);
        if let (Some(key), Some(wanted)) = (&before.primary_key, &after.primary_key)
            && key.columns.iter().any(&key_column)
            && !changes
                .iter()
                .any(|c| matches!(c, Change::Constraint(ConstraintChange::SetPrimaryKey { table, .. }) if table == name))
        {
            changes.push(Change::Constraint(ConstraintChange::SetPrimaryKey {
                table: name.clone(),
                from: Some(key.clone()),
                to: None,
                nonclustered: false,
            }));
            changes.push(Change::Constraint(ConstraintChange::SetPrimaryKey {
                table: name.clone(),
                from: None,
                to: Some(wanted.clone()),
                nonclustered: !after.primary_key_is_clustered(),
            }));
        }
        // Only a constraint or index whose layout stays as it was: one whose
        // layout moves is already dropped and re-added by `diff_constraints`,
        // and a second pair here would drop it twice.
        for (n, key) in &before.unique {
            if after.unique.get(n) == Some(key)
                && before.unique_is_clustered(n.raw()) == after.unique_is_clustered(n.raw())
                && key.columns.iter().any(&key_column)
            {
                changes.push(Change::Constraint(ConstraintChange::DropUnique {
                    table: name.clone(),
                    name: n.clone(),
                }));
                changes.push(Change::Constraint(ConstraintChange::AddUnique {
                    table: name.clone(),
                    name: n.clone(),
                    constraint: key.clone(),
                    clustered: after.unique_is_clustered(n.raw()),
                }));
            }
        }
        for (n, check) in &before.checks {
            if checks && after.checks.get(n) == Some(check) {
                changes.push(Change::Constraint(ConstraintChange::DropCheck {
                    table: name.clone(),
                    name: n.clone(),
                }));
                changes.push(Change::Constraint(ConstraintChange::AddCheck {
                    table: name.clone(),
                    name: n.clone(),
                    constraint: check.clone(),
                }));
            }
        }
        for (n, index) in &before.indexes {
            if after.indexes.get(n) == Some(index)
                && before.index_is_clustered(n.raw()) == after.index_is_clustered(n.raw())
                && (index
                    .columns
                    .iter()
                    .any(|c| c.key.column().is_some_and(key_column))
                    || index.include.iter().any(&key_column)
                    // An expression names its columns inside text this does
                    // not parse, so it is treated as a filter is: rebuilt
                    // wherever the dialect rebuilds filtered indexes
                    // (DEC-1169.2).
                    || (filters
                        && (index.filter.is_some()
                            || index.columns.iter().any(|c| c.key.expression().is_some()))))
            {
                changes.push(Change::Index(IndexChange::DropIndex {
                    table: name.clone(),
                    name: n.clone(),
                }));
                changes.push(Change::Index(IndexChange::AddIndex {
                    table: name.clone(),
                    name: n.clone(),
                    index: Box::new(index.clone()),
                    clustered: after.index_is_clustered(n.raw()),
                }));
            }
        }
        for (n, fk) in &before.foreign_keys {
            let local = fk.columns.iter().map(|c| name.column(c));
            let referenced = fk
                .references_columns
                .iter()
                .map(|c| fk.references_table.column(c));
            if after.foreign_keys.get(n) == Some(fk)
                && local
                    .chain(referenced)
                    .any(|c| retyped.get(&c).is_some_and(|d| d.foreign_keys))
            {
                changes.push(Change::Constraint(ConstraintChange::DropForeignKey {
                    table: name.clone(),
                    name: n.clone(),
                }));
                changes.push(Change::Constraint(ConstraintChange::AddForeignKey {
                    table: name.clone(),
                    name: n.clone(),
                    constraint: Box::new(fk.clone()),
                }));
            }
        }
    }
}
