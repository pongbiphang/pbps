//! Changes to a partitioned table the engine does not carry through its partitions, and why they are refused.

use super::super::*;
use super::adoption::referencing_key;

pub(in super::super) fn refuse_partition_changes(
    base: Side<'_>,
    declared: Side<'_>,
    index: &Indexes<'_>,
    hints: &Hints,
    dialect: &(impl Dialect + ?Sized),
    changes: &[Change],
    errs: &mut Vec<DiffError>,
) {
    let partitioned = |schema: &Schema, name: &TableName| {
        schema
            .tables
            .get(name)
            .is_some_and(|t| t.partition_by.is_some() || t.partition_of.is_some())
    };
    let label_renames = crate::enum_labels::label_renames(changes);
    let mut refused: BTreeMap<TableName, Vec<String>> = BTreeMap::new();
    // The tables this plan attaches under each parent or detaches from it.
    let mut moving: BTreeMap<TableName, Vec<TableName>> = BTreeMap::new();
    let mut changes_meet_moves: BTreeSet<TableName> = BTreeSet::new();
    let mut refuse = |table: &TableName, what: String| {
        let list = refused.entry(table.clone()).or_default();
        if !list.contains(&what) {
            list.push(what);
        }
    };
    // A standing parent this plan renames (#1690), by its declared name and
    // by its base one. Its partitions follow it under the engine's plain
    // `RENAME`, and its own changes in the plan name it as declared.
    let standing_parent = |schema: &Schema, name: &TableName| {
        schema
            .tables
            .get(name)
            .is_some_and(|t| t.partition_by.is_some() && t.partition_of.is_none())
    };
    let renamed_parents: BTreeMap<&TableName, &TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::Table(TableChange::RenameTable { from, to, .. }) = c
                && standing_parent(base.schema, from)
                && standing_parent(declared.schema, to)
            {
                Some((to, from))
            } else {
                None
            }
        })
        .collect();
    let base_name = |declared_name: &TableName| -> TableName {
        renamed_parents
            .get(declared_name)
            .map_or_else(|| declared_name.clone(), |&from| from.clone())
    };
    let declared_parent = |base_name: &TableName| -> TableName {
        renamed_parents
            .iter()
            .find(|(_, from)| **from == base_name)
            .map_or_else(|| base_name.clone(), |(to, _)| (*to).clone())
    };
    // A difference in the key or the bound itself, which no change carries.
    for (uid, declared_name) in &declared.ids.tables {
        let Some(base_name) = base.ids.tables.get(uid) else {
            continue;
        };
        if let (Some(b), Some(d)) = (
            base.schema.tables.get(base_name),
            declared.schema.tables.get(declared_name),
        ) {
            // A partition declared as an ordinary table is detached (#1544),
            // and an ordinary table declared as a partition attached (#1545):
            // each is its own change, or its own refusal.
            let detached =
                b.partition_of.is_some() && d.partition_of.is_none() && d.partition_by.is_none();
            let attached = b.partition_of.is_none() && d.partition_of.is_some();
            let parent = if detached {
                b.partition_of.as_ref()
            } else if attached {
                d.partition_of.as_ref()
            } else {
                None
            };
            // Under the name the plan leaves the parent: a detach's is the
            // base's, brought forward; an attach's is already declared.
            if let Some(of) = parent {
                let parent = if detached {
                    declared_parent(&of.parent)
                } else {
                    of.parent.clone()
                };
                moving
                    .entry(parent)
                    .or_default()
                    .push(declared_name.clone());
            }
            // A partition's parent as declared: the base's brought forward
            // through the plan's renames, one it renames being the same
            // parent under its new name (#1690). The declared side is already
            // under the names the plan leaves; bringing it forward too would
            // move a name another rename has taken (#1751 review).
            let where_ = |t: &Table, forward: bool| {
                t.partition_of.as_ref().map(|of| {
                    (
                        if forward {
                            declared_parent(&of.parent)
                        } else {
                            of.parent.clone()
                        },
                        // The base's labels, as the plan's renames leave them
                        // (DEC-2346.1).
                        match base.schema.tables.get(&of.parent) {
                            Some(parent) if forward => crate::enum_labels::forwarded_bound(
                                &of.bound,
                                parent,
                                (&TypeResolver::new(base.ids, declared.ids), TypeSide::Base),
                                &label_renames,
                            ),
                            _ => of.bound.clone(),
                        },
                    )
                })
            };
            // A key column renamed is the same key: the engine renames it in
            // the key as it does in the table (measured on 16 and 18, #1687).
            // Another tool's partitions on one side and pbps's on the other
            // (#1533). No statement makes the switch, and a plan records only
            // what its changes do, so the setting is recorded as an adoption
            // is (#1170): by a baseline once the declarations say so. A
            // partition pbps holds is dropped or detached first, since
            // handing it over would leave the declarations nothing to say
            // about it.
            let external = |t: &Table| t.partition_by.as_ref().map(|by| by.external);
            if let (Some(was), Some(is)) = (external(b), external(d))
                && was != is
            {
                let held: Vec<String> = base
                    .schema
                    .tables
                    .iter()
                    .filter(|(_, t)| {
                        t.partition_of
                            .as_ref()
                            .is_some_and(|of| &of.parent == base_name)
                    })
                    .map(|(n, _)| n.shown().to_string())
                    .collect();
                if !is {
                    refuse(
                        declared_name,
                        "declare its external partitions; adopt them as partitions made by hand \
                         are, with `pbps pull` and `pbps baseline --reason …`"
                            .to_owned(),
                    );
                } else if !held.is_empty() {
                    refuse(
                        declared_name,
                        format!(
                            "make its partitions external while pbps holds {}; drop or detach \
                             them in a plan of their own first",
                            held.join(", ")
                        ),
                    );
                } else {
                    refuse(
                        declared_name,
                        "make its partitions external, which no statement does; record it with \
                         `pbps baseline --reason …` once the declarations say so"
                            .to_owned(),
                    );
                }
                continue;
            }
            let renamed = |by: &pbps_model::PartitionBy| {
                let names = renamed_columns(index, base_name, declared_name);
                pbps_model::PartitionBy {
                    external: by.external,
                    columns: by
                        .columns
                        .iter()
                        .map(|c| names.get(c).cloned().unwrap_or_else(|| c.clone()))
                        .collect(),
                    strategy: by.strategy,
                }
            };
            let changes_key = b.partition_by.as_ref().map(renamed) != d.partition_by;
            if (changes_key || where_(b, true) != where_(d, false)) && !detached && !attached {
                refuse(
                    declared_name,
                    if changes_key {
                        "change its partitioning (partition-key rebuild support: issue #2298)"
                    } else {
                        "change its partitioning"
                    }
                    .to_owned(),
                );
            }
        }
    }
    // As for a temporal table: the changes split out of a table this plan
    // creates are the creation; a drop or a rename is asked of the side it
    // acts on.
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
    let attaching: BTreeSet<&TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::Partition(PartitionChange::AttachPartition { table, .. }) = c {
                Some(table)
            } else {
                None
            }
        })
        .collect();
    // A partition this plan drops under its standing parent: the foreign
    // keys of its own are dropped before it, as any table's are (#1632
    // review).
    let dropping: BTreeSet<&TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::Table(TableChange::DropTable {
                name,
                detach_from: Some(_),
                ..
            }) = c
            {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    // The names the plan takes from a column, by a drop or a rename away,
    // and the name a change gives a column, by an addition or a rename into.
    // A partition dropped under its parent is a transition too: its rows
    // still stand when the pre-flight probes the parent's column (#1692
    // review).
    for change in changes {
        if let Change::Table(TableChange::DropTable {
            name,
            detach_from: Some(parent),
            ..
        }) = change
        {
            moving
                .entry(declared_parent(parent))
                .or_default()
                .push(name.clone());
        }
    }
    for change in changes {
        // A partition is created with its parent, or under a parent that
        // already stands (#1171); either way it is a creation, and whether
        // the parent's rows let it be is the connected probe's question.
        if matches!(change, Change::Table(TableChange::CreateTable { .. })) {
            continue;
        }
        // A partition is dropped, detached first, while its parent stands
        // (#1171), or detached and kept (#1544). Its parent's own drop is
        // still refused below.
        if matches!(
            change,
            Change::Table(TableChange::DropTable {
                detach_from: Some(_),
                ..
            }) | Change::Partition(PartitionChange::DetachPartition { .. })
                | Change::Partition(PartitionChange::AttachPartition { .. })
        ) {
            continue;
        }
        // A standing partition's own: its indexes and checks, which the
        // reader never confuses with its parent's clones (#1577), its
        // defaults and NOT NULLs, its storage parameters and its persistence
        // (#1581), and its foreign keys, a clone of the parent's being one
        // with a `conparentid` the reader never reads (#1632). A clone is its
        // parent's to change (#1546).
        let own = matches!(
            change,
            Change::Index(IndexChange::AddIndex { .. })
                | Change::Index(IndexChange::DropIndex { .. })
                | Change::Constraint(ConstraintChange::AddCheck { .. })
                | Change::Constraint(ConstraintChange::DropCheck { .. })
                | Change::Constraint(ConstraintChange::AddForeignKey { .. })
                | Change::Constraint(ConstraintChange::DropForeignKey { .. })
                // Its own exclusion, as its own index (DEC-2448.7): a drop and an add, or a
                // parameter edit of the index behind it, which `ALTER INDEX` takes on a leaf.
                | Change::Constraint(ConstraintChange::SetExclusion { .. })
                | Change::Index(IndexChange::SetIndexStorageParameters {
                    target: IndexPart::Exclusion(_),
                    ..
                })
                | Change::Partition(PartitionChange::SetPartitionDefault { .. })
                | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
                | Change::Policy(_)
                | Change::TableProperty(TablePropertyChange::SetStorageParameters { .. })
                | Change::TableProperty(TablePropertyChange::SetTablePersistence { .. })
        );
        let standing = |schema: &Schema, name: &TableName| {
            schema
                .tables
                .get(name)
                .is_some_and(|t| t.partition_of.is_some() && t.partition_by.is_none())
        };
        // A table this plan attaches is a partition from the attach on, and
        // its own are brought to the declaration after it (#1545).
        if own
            && change.table().is_some_and(|t| {
                (standing(base.schema, t) || attaching.contains(t)) && standing(declared.schema, t)
            })
        {
            continue;
        }
        if let Change::Constraint(ConstraintChange::DropForeignKey { table, .. }) = change
            && standing(base.schema, table)
            && dropping.contains(table)
        {
            continue;
        }
        // Before the attach, still the ordinary table's: a key or unique
        // constraint whose index a parent's index might take (#1642 review,
        // DEC-1671.1). On a standing partition it is the parent's clone. The
        // exclusion dropped there for the same reason is among a partition's
        // own, above.
        if matches!(
            change,
            Change::Constraint(ConstraintChange::DropUnique { .. })
                | Change::Constraint(ConstraintChange::SetPrimaryKey { to: None, .. })
        ) && change.table().is_some_and(|t| attaching.contains(t))
        {
            continue;
        }
        // A standing parent's column (#1687): the engine recurses each of
        // these into every partition, and the partitions' own defaults and
        // NOT NULLs follow in `diff_partition_columns`. A key column is never
        // dropped or retyped (measured on 16 and 18), and an identity column
        // on a partitioned table is not read back yet (#1681).
        let parent_standing = |schema: &Schema, name: &TableName| {
            if std::ptr::eq(schema, base.schema) {
                standing_parent(schema, &base_name(name))
            } else {
                standing_parent(schema, name)
            }
        };
        // A standing parent's indexes (#1688), keys, checks and foreign keys
        // (#1689): the engine recurses each into every partition, a clone on
        // each that the reader leaves to the parent (DEC-1577.1). A new name
        // is a drop and an add, as on any table.
        if let Change::Index(IndexChange::AddIndex { table, .. })
        | Change::Index(IndexChange::DropIndex { table, .. })
        | Change::Constraint(ConstraintChange::SetPrimaryKey { table, .. })
        | Change::Constraint(ConstraintChange::AddUnique { table, .. })
        | Change::Constraint(ConstraintChange::DropUnique { table, .. })
        | Change::Constraint(ConstraintChange::AddCheck { table, .. })
        | Change::Constraint(ConstraintChange::DropCheck { table, .. })
        | Change::Constraint(ConstraintChange::AddForeignKey { table, .. })
        | Change::Constraint(ConstraintChange::DropForeignKey { table, .. })
        | Change::Constraint(ConstraintChange::SetExclusion { table, .. }) = change
            && parent_standing(base.schema, table)
            && parent_standing(declared.schema, table)
        {
            if let Some(tables) = moving.get(table) {
                // The same two plans as a column change (DEC-1687.1): a
                // detach's shape and a unique index's probe each read the
                // tree on one side of the other (#1737 review).
                let tables: Vec<String> =
                    tables.iter().map(|name| name.shown().to_string()).collect();
                changes_meet_moves.insert(table.clone());
                refuse(
                    table,
                    format!(
                        "{} while this plan attaches, detaches or drops {}; change the parent and \
                         the partitions in separate plans (support in one plan: issue #2296)",
                        change_in_words(change),
                        tables.join(", ")
                    ),
                );
            } else if let Some(why) =
                refuse_parent_index(table, change, base, declared, hints, changes)
            {
                refuse(table, why);
            }
            continue;
        }
        if let Some(column) = parent_column(change)
            && let Some(table) = change.table()
            && parent_standing(base.schema, table)
            && parent_standing(declared.schema, table)
        {
            let key = |schema: &Schema| {
                let name = if std::ptr::eq(schema, base.schema) {
                    base_name(table)
                } else {
                    table.clone()
                };
                schema
                    .tables
                    .get(&name)
                    .and_then(|t| t.partition_by.as_ref())
                    .is_some_and(|by| by.columns.iter().any(|c| c == column))
            };
            // A drop names the base column and a retype the declared one, so
            // each is asked of its own side's key: across one plan a name
            // can leave the key's column for another (#1692 review).
            if (matches!(change, Change::Column(ColumnChange::DropColumn { .. }))
                && key(base.schema))
                || (matches!(change, Change::Column(ColumnChange::AlterColumnType { .. }))
                    && key(declared.schema))
            {
                refuse(
                    table,
                    format!(
                        "{}, which is in its partition key (table rebuild support: issue #2298)",
                        change_in_words(change)
                    ),
                );
            } else if let Change::Column(ColumnChange::AddColumn { column: c, .. }) = change
                && (match &c.value_source {
                    ValueSource::Identity(_) => true,
                    ValueSource::None
                    | ValueSource::Default(_)
                    | ValueSource::Sequence(_)
                    | ValueSource::Generated(_) => false,
                })
            {
                refuse(
                    table,
                    format!(
                        "{}, an identity column, which a partitioned table's partitions are not \
                         read back with yet (#1681)",
                        change_in_words(change)
                    ),
                );
            } else if let Some(tables) = moving.get(table)
                && !dialect.partition_meetings()
                && !matches!(
                    change,
                    Change::Column(ColumnChange::SetColumnDeprecated { .. })
                )
            {
                // The capability's canonical symbolic schedule proves this
                // cut; engines without that contract keep the guard (DEC-2296.12).
                // An attach or a detach meets the parent's columns as they
                // stand at that statement, and the table on its other side
                // holds them as declared at the other end of the plan: each
                // column change would need its own place against each
                // transition (#1692 review). Two plans keep each simple.
                let tables: Vec<String> =
                    tables.iter().map(|name| name.shown().to_string()).collect();
                changes_meet_moves.insert(table.clone());
                refuse(
                    table,
                    format!(
                        "{} while this plan attaches, detaches or drops {}; change the columns and the \
                         partitions in separate plans (support in one plan: issue #2296)",
                        change_in_words(change),
                        tables.join(", ")
                    ),
                );
            }
            continue;
        }
        // A standing parent's rename (#1690): a plain `RENAME`, which its
        // partitions follow. A partition attached, detached or dropped under
        // it in the same plan is named on one side or the other of it.
        if let Change::Table(TableChange::RenameTable { from, to, .. }) = change
            && renamed_parents.get(to) == Some(&from)
        {
            let tables: Vec<String> = moving
                .get(to)
                .into_iter()
                .flatten()
                .map(|name| name.shown().to_string())
                .collect();
            if !tables.is_empty() && !dialect.partition_meetings() {
                changes_meet_moves.insert(to.clone());
                refuse(
                    to,
                    format!(
                        "{} while this plan attaches, detaches or drops {}; rename it and change \
                         the partitions in separate plans (support in one plan: issue #2296)",
                        change_in_words(change),
                        tables.join(", ")
                    ),
                );
            }
            continue;
        }
        let ends_a_table = matches!(
            change,
            Change::Table(TableChange::DropTable { .. })
                | Change::Table(TableChange::RenameTable { .. })
        );
        if !ends_a_table && change.table().is_some_and(|t| created.contains(t)) {
            continue;
        }
        let asked: Vec<&TableName> =
            if let Change::Table(TableChange::DropTable { name, .. }) = change {
                vec![name]
                    .into_iter()
                    .filter(|n| partitioned(base.schema, n))
                    .collect()
            } else if let Change::Table(TableChange::RenameTable { from, to, .. }) = change {
                [(from, base.schema), (to, declared.schema)]
                    .into_iter()
                    .filter(|(n, side)| partitioned(side, n))
                    .map(|(n, _)| n)
                    .collect()
            } else {
                change
                    .table()
                    .into_iter()
                    .filter(|n| partitioned(base.schema, n) || partitioned(declared.schema, n))
                    .collect()
            };
        for table in asked {
            refuse(table, change_in_words(change));
        }
    }
    // A detach refused above for its parent's column changes is not also a
    // shape the detach cannot take: its declaration holds the columns as the
    // refused changes leave them.
    errs.retain(|e| {
        !matches!(e, DiffError::DetachedShape { parent, .. } if changes_meet_moves.contains(parent))
    });
    errs.extend(
        refused
            .into_iter()
            .map(|(table, what)| DiffError::PartitionedTableChange { table, what }),
    );
}

/// Why a standing parent's index (#1688), key or check change (#1689) cannot
/// be planned, if it cannot. Each is measured on 16.15 and 18.6:
/// - `CONCURRENTLY` is refused on a partitioned table, building or dropping,
///   so `strategy: online` cannot be honoured, and leaving it off would lock
///   every partition the operator asked to keep writable;
/// - a unique index lacking a partition-key column is refused by the engine,
///   "unique constraint on partitioned table must include all partitioning
///   columns";
/// - a new index or exclusion adopts a partition's own that matches it
///   (DEC-1577.1), which then becomes the parent's clone, and the partition's
///   own is gone from every later read. A standing partition's is rebuilt
///   around the addition (`adoption.rs`, DEC-1671.1); one the plan creates the
///   partition with, or for an exclusion adds, is refused by name, as is a
///   standing own index a foreign key references, which is not rebuilt;
/// - a primary key or unique constraint lacking a partition-key column is
///   refused by the engine as a unique index is;
/// - a new check absorbs a partition's own check of its name.
///
/// `strategy: online` asks only of an index: a key, a check and a foreign
/// key drop the hint on every table, a partitioned one included (`emit.rs`).
fn refuse_parent_index(
    table: &TableName,
    change: &Change,
    base: Side<'_>,
    declared: Side<'_>,
    hints: &Hints,
    changes: &[Change],
) -> Option<String> {
    if matches!(
        change,
        Change::Index(IndexChange::AddIndex { .. }) | Change::Index(IndexChange::DropIndex { .. })
    ) && hints.strategies.get(table).is_some_and(|s| s.online)
    {
        return Some(format!(
            "{} with `strategy: online`, which PostgreSQL cannot build or drop concurrently on a \
             partitioned table; remove the table's `strategy: online` for this plan",
            change_in_words(change)
        ));
    }
    let parent = declared.schema.tables.get(table)?;
    let partitions = || {
        declared.schema.tables.iter().filter(|(_, t)| {
            t.partition_of
                .as_ref()
                .is_some_and(|of| of.parent == *table)
        })
    };
    // The partition as the base held it, if it did: one the plan creates has none.
    let held_before = |partition: &TableName| {
        declared
            .ids
            .tables
            .iter()
            .find(|(_, name)| *name == partition)
            .and_then(|(uid, _)| base.ids.tables.get(uid))
            .and_then(|name| base.schema.tables.get(name))
    };
    // A key holds every partition-key column, or the engine refuses it, as
    // it does a unique index (#1689).
    let missing_key = |columns: &[ColumnName]| {
        parent.partition_by.as_ref().and_then(|by| {
            by.columns
                .iter()
                .find(|key| !columns.contains(key))
                .cloned()
        })
    };
    let without_key = |missing: ColumnName| {
        format!(
            "{} without the partition key column `{missing}`, which PostgreSQL refuses on a \
             partitioned table; add `{missing}` to its columns",
            change_in_words(change),
            missing = missing.shown(),
        )
    };
    if let Change::Constraint(ConstraintChange::SetPrimaryKey { to: Some(pk), .. }) = change {
        return missing_key(&pk.columns).map(without_key);
    }
    if let Change::Constraint(ConstraintChange::AddUnique { constraint, .. }) = change {
        return missing_key(&constraint.columns).map(without_key);
    }
    // A parent's new exclusion (PostgreSQL 18 only; DEC-2448.6): the engine requires
    // every partition-key column as a plain key with an equality, and takes a
    // partition's own exclusion of the same shape as the parent's clone, which the
    // parent's drop then removes. A drop has nothing to ask.
    if let Change::Constraint(ConstraintChange::SetExclusion {
        name,
        to: Some(exclusion),
        ..
    }) = change
    {
        if let Some(missing) = parent.partition_by.as_ref().and_then(|by| {
            by.columns
                .iter()
                .find(|key| !holds_key_with_equality(exclusion, key))
                .cloned()
        }) {
            return Some(format!(
                "add exclusion constraint `{name}` without the partition key column `{missing}` \
                 as a plain key excluded with `=`, which PostgreSQL refuses on a partitioned \
                 table; add `{missing}` WITH `=` to its keys",
                missing = missing.shown(),
                name = name.shown(),
            ));
        }
        // An own the plan adds that the parent's would adopt, on a partition it creates, adds
        // it to or redefines it on: the declaration rule (`validate_partition`, DEC-2448.7)
        // already refuses an own of the parent's shape, because the state it declares is one
        // the engine reaches only by order, and the order within one plan is #2992's. A
        // standing own the plan leaves as it was is rebuilt around the parent's instead
        // (DEC-1671.1), which runs after this and so is not seen here.
        let adds = |partition: &TableName, own: &pbps_model::ObjectLocalName| {
            changes.iter().any(|c| {
                matches!(c, Change::Constraint(ConstraintChange::SetExclusion {
                    table, name, to: Some(_), ..
                }) if table == partition && name == own)
            })
        };
        let adopted: Vec<String> = partitions()
            .flat_map(|(partition, t)| {
                t.exclude
                    .iter()
                    .filter(move |(own, x)| adds(partition, own) && adopts_exclusion(exclusion, x))
                    .map(move |(own, _)| {
                        format!(
                            "{partition}'s `{own}`",
                            partition = partition.shown(),
                            own = own.shown(),
                        )
                    })
            })
            .collect();
        return (!adopted.is_empty()).then(|| {
            format!(
                "add exclusion constraint `{name}`, which can take {} as the partition's copy \
                 of it and leave the partition without its own; add the partition's own \
                 exclusion in a later plan",
                adopted.join(", "),
                name = name.shown(),
            )
        });
    }
    // A partition's own check under the parent's new check's name is
    // absorbed into it, measured on 16 and 18: the same expression makes it
    // the parent's clone, which the parent's later drop removes, and another
    // is refused by the engine. Refused by name (leon, 2026-10-08), the
    // remedy being to drop or rename the partition's own first.
    if let Change::Constraint(ConstraintChange::AddCheck { name, .. }) = change {
        let held: Vec<String> = partitions()
            .filter(|(_, t)| t.checks.contains_key(name.raw()))
            .map(|(partition, _)| partition.shown().to_string())
            .collect();
        return (!held.is_empty()).then(|| {
            format!(
                "add check `{name}`, a name {} holds as its own check, which the parent's would \
                 absorb; drop or rename the partition's own check in an earlier plan",
                held.join(", "),
                name = name.shown(),
            )
        });
    }
    let Change::Index(IndexChange::AddIndex { name, index, .. }) = change else {
        return None;
    };
    if index.unique
        && let Some(by) = &parent.partition_by
        && let Some(missing) = by
            .columns
            .iter()
            .find(|key| !index.columns.iter().any(|c| c.key.column() == Some(*key)))
    {
        return Some(format!(
            "add unique index `{name}` without the partition key column `{missing}`, which \
             PostgreSQL refuses on a partitioned table; add `{missing}` to its columns",
            missing = missing.shown(),
            name = name.shown(),
        ));
    }
    // A standing own a foreign key references is not rebuilt, since the key
    // would then be bound to the parent's clone. Where the parent's would adopt
    // it, nothing keeps it the partition's own, so the add is refused, naming
    // the key (DEC-1671.1).
    let referenced: Vec<String> = partitions()
        .filter_map(|(partition, t)| Some((partition, t, held_before(partition)?)))
        .flat_map(|(partition, t, before)| {
            t.indexes
                .iter()
                .filter(move |(own, x)| {
                    before.indexes.contains_key(own.raw())
                        && !changes.iter().any(|c| {
                            matches!(c, Change::Index(IndexChange::DropIndex { table, name })
                                if table == partition && name == *own)
                        })
                        && adopts(index, x)
                })
                .filter_map(move |(own, x)| {
                    let (from, key) = referencing_key(declared.schema, partition, x)?;
                    Some(format!(
                        "{partition}'s `{own}`, which the foreign key `{key}` of {from} references",
                        partition = partition.shown(),
                        own = own.shown(),
                        key = key.shown(),
                        from = from.shown(),
                    ))
                })
        })
        .collect();
    if !referenced.is_empty() {
        return Some(format!(
            "add index `{name}`, which can take as the partition's copy of it {}; the own index \
             cannot be rebuilt around it, since the key would then reference the copy, so remove \
             that foreign key first",
            referenced.join(", and "),
            name = name.shown(),
        ));
    }
    // An own index of a partition the plan creates, which is built with the
    // partition and so before its parent's. A standing partition's own is
    // otherwise rebuilt around the parent's (DEC-1671.1), and one the plan adds
    // to it is built after the parent's, and is its own.
    let adopted: Vec<String> = partitions()
        .filter(|(partition, _)| held_before(partition).is_none())
        .flat_map(|(partition, t)| {
            t.indexes
                .iter()
                .filter(|(_, own)| adopts(index, own))
                .map(move |(own, _)| {
                    format!(
                        "{partition}'s `{own}`",
                        partition = partition.shown(),
                        own = own.shown(),
                    )
                })
        })
        .collect();
    (!adopted.is_empty()).then(|| {
        format!(
            "add index `{name}`, which can take {} as the partition's copy of it and leave the \
             partition without its own; add the partition's own index in a later plan",
            adopted.join(", "),
            name = name.shown(),
        )
    })
}

/// The column a parent's column change names, for the kinds the engine
/// recurses into every partition (#1687): its name before a rename, as the
/// partition key holds it.
// The complement is every change that names no column of a table.
#[allow(clippy::wildcard_enum_match_arm)]
pub(in super::super) fn parent_column(change: &Change) -> Option<&ColumnName> {
    match change {
        Change::Column(ColumnChange::AddColumn { name, .. }) => Some(name),
        Change::Column(ColumnChange::RenameColumn { from, .. }) => Some(from),
        Change::TypeValue(TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Retype { column, .. },
            ..
        })
        | Change::Column(ColumnChange::DropColumn { column, .. })
        | Change::Column(ColumnChange::AlterColumnType { column, .. })
        | Change::Column(ColumnChange::AlterColumnNullability { column, .. })
        | Change::Column(ColumnChange::AlterColumnDefault { column, .. })
        | Change::Column(ColumnChange::AlterColumnNumbering { column, .. })
        | Change::Column(ColumnChange::SetColumnDeprecated { column, .. }) => Some(&column.name),
        _ => None,
    }
}
