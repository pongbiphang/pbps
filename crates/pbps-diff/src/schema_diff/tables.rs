//! The tables phase: the tables only one side holds, and each table both sides hold, compared and attached or detached.

use super::*;

/// The DEFAULT partition a new range or list partition is made beside, under
/// the name the plan leaves it (#1547, DEC-1547.1, #1632): one the base holds
/// as the parent's DEFAULT and the declarations keep as that parent's. Its
/// rows in the new range or list are moved into the partition before it is attached, so the
/// plan is the same whether it holds any or not. A parent the plan creates
/// has none, and neither has a DEFAULT the plan drops, detaches or makes:
/// one leaving goes in class 6, before the partition is created, and a new
/// one starts empty. The name is the declared one because renames run in
/// class 1, before every create.
fn standing_default(
    base: Side<'_>,
    declared: Side<'_>,
    of: &pbps_model::PartitionOf,
) -> Option<TableName> {
    // A HASH parent has no DEFAULT (#1632).
    if !matches!(
        of.bound,
        pbps_model::PartitionBound::Range { .. } | pbps_model::PartitionBound::List { .. }
    ) {
        return None;
    }
    let parent = base.ids.tables.get(declared.ids.table_uid(&of.parent)?)?;
    let is_default_of = |side: Side<'_>, name: &TableName, parent: &TableName| {
        side.schema
            .tables
            .get(name)
            .and_then(|t| t.partition_of.as_ref())
            .is_some_and(|o| {
                &o.parent == parent && matches!(o.bound, pbps_model::PartitionBound::Default)
            })
    };
    declared.ids.tables.iter().find_map(|(uid, name)| {
        let was = base.ids.tables.get(uid)?;
        (is_default_of(declared, name, &of.parent) && is_default_of(base, was, parent))
            .then(|| name.clone())
    })
}

/// What the tables phase leaves for the passes after it.
pub(super) struct TableFacts<'a> {
    /// Both identity files, indexed: the partition refusals and the sort facts read it.
    pub(super) index: Indexes<'a>,
    /// The table and column renames of this plan: the dependents of a retyped
    /// column, the foreign keys onto a renamed table and the rename order read them.
    pub(super) renames: Renames,
    /// The base tables that hold an exclusion, as this plan's renames leave
    /// them: the exclusion hazards read them.
    pub(super) standing: BTreeMap<TableName, Table>,
    /// The own indexes, keys and exclusions an attach drops because a parent's
    /// might take them: the plan says why (DEC-1671.3).
    pub(super) loose: Vec<Loose>,
}

/// The tables phase of `diff_partial_rebuilding`: the tables only the base
/// holds are dropped, the tables only the declarations hold are created, and
/// each table both sides hold is renamed, compared and attached or detached.
///
/// It takes the lists of `changes` and `errs` built so far and hands them back,
/// not lists of its own to append afterwards: the order of both is part of the
/// result, and the loop over the surviving tables reads `changes` as `&changes`
/// and lends both as `&mut changes` and `&mut errs`, which a `&mut Vec`
/// parameter would not allow. It takes the resolver by value, as no pass after
/// it reads it. It returns the facts the passes after it read.
pub(super) fn diff_tables<'a>(
    base: Side<'a>,
    declared: Side<'a>,
    dialect: &(impl Dialect + ?Sized),
    types: TypeResolver<'_>,
    read_back: Option<&Schema>,
    mut changes: Vec<Change>,
    mut errs: Vec<DiffError>,
) -> (TableFacts<'a>, Vec<Change>, Vec<DiffError>) {
    let base_tables = &base.ids.tables;
    let declared_tables = &declared.ids.tables;
    let index = Indexes {
        base: base.ids.index(),
        declared: declared.ids.index(),
    };

    // One UID-derived address/template map, shared by every moving boundary.
    // Pending is a pure comparison; diff_columns still runs once in its usual place.
    let parent_shapes: BTreeMap<_, _> = base_tables
        .iter()
        .filter_map(|(uid, old_name)| {
            if !dialect.partition_meetings() {
                return None;
            }
            let new_name = declared_tables.get(uid)?;
            let old = base.schema.tables.get(old_name)?;
            let new = declared.schema.tables.get(new_name)?;
            let pending = old.partition_by.is_some()
                && old.partition_of.is_none()
                && new.partition_by.is_some()
                && new.partition_of.is_none()
                && (old_name != new_name || parent_shape_changes(old, new));
            Some((uid, (old_name, new_name, new, pending)))
        })
        .collect();
    let final_parent = |name: &TableName| {
        base.ids
            .table_uid(name)
            .and_then(|uid| parent_shapes.get(uid))
            .map_or_else(|| name.clone(), |(_, name, _, _)| (*name).clone())
    };

    // Table present only in the base: dropped. Its foreign keys are dropped
    // first as separate changes — two dropped tables that reference each other
    // would otherwise fail or succeed depending on which DROP TABLE runs first.
    for (uid, name) in base_tables {
        if !declared_tables.contains_key(uid) {
            if let Some(t) = base.schema.tables.get(name) {
                for fk_name in t.foreign_keys.keys() {
                    changes.push(Change::Constraint(ConstraintChange::DropForeignKey {
                        table: name.clone(),
                        name: fk_name.clone(),
                    }));
                }
            }
            changes.push(Change::Table(TableChange::DropTable {
                uid: uid.clone(),
                name: name.clone(),
                // A partition is detached from its parent before it goes
                // (#1171, DEC-1171.1).
                detach_from: base
                    .schema
                    .tables
                    .get(name)
                    .and_then(|t| t.partition_of.as_ref())
                    .map(|of| final_parent(&of.parent)),
            }));
        }
    }

    // Table present only in the declarations: created. Foreign keys are split
    // out of the CREATE into their own changes, because they sort after every
    // CreateTable — a new table's FK may reference another new table, and
    // within one ordering class the order of creation is not meaningful.
    for (uid, name) in declared_tables {
        if !base_tables.contains_key(uid)
            && let Some(t) = declared.schema.tables.get(name)
        {
            let mut table = t.clone();
            for (fk_name, fk) in std::mem::take(&mut table.foreign_keys) {
                changes.push(Change::Constraint(ConstraintChange::AddForeignKey {
                    table: name.clone(),
                    name: fk_name,
                    constraint: Box::new(fk),
                }));
            }
            // Rows are split out of the CREATE for the same reason the
            // foreign keys above are: they are separate statements in a
            // separate ordering class, and a `CreateTable` carrying rows would
            // give the emitter a second place to produce DML.
            match (&table.data, table.primary_key.as_ref()) {
                (Some(data), Some(pk)) if pk.columns.len() == 1 => {
                    let identity_key = table.columns.get(&pk.columns[0]).is_some_and(|c| match &c
                        .value_source
                    {
                        ValueSource::Identity(_) => true,
                        // A `serial` key is refused by its dialect (#1631).
                        ValueSource::None
                        | ValueSource::Default(_)
                        | ValueSource::Sequence(_)
                        | ValueSource::Generated(_) => false,
                    });
                    for (key, row) in &data.rows {
                        let (defaults, types) = omitted_defaults(&table, pk.columns[0].raw(), row);
                        changes.push(Change::Row(RowChange::InsertRow {
                            table: name.clone(),
                            key_column: pk.columns[0].clone(),
                            identity_key,
                            key: key.clone(),
                            row: row.clone(),
                            defaults,
                            types,
                        }));
                    }
                }
                (Some(_), _) => errs.push(DiffError::DataWithoutKey {
                    table: name.clone(),
                }),
                (None, _) => {}
            }
            let beside_default = table
                .partition_of
                .as_ref()
                .and_then(|of| standing_default(base, declared, of));
            // What a human allowed it to take (#1788): read from the
            // declared side, where `pbps move-rows` writes it.
            let moves_at_most = beside_default
                .as_ref()
                .and_then(|_| declared.ids.moves.get(uid))
                .map(|m| m.max_rows);
            for (policy_name, policy) in &table.row_policies {
                changes.push(Change::Policy(PolicyChange::CreatePolicy {
                    table: name.clone(),
                    name: policy_name.clone(),
                    policy: policy.clone(),
                    origin: pbps_model::RowSecurityOrigin::Created,
                }));
            }
            if !table.row_security.is_default() {
                changes.push(Change::Policy(PolicyChange::SetRowSecurity {
                    uid: uid.clone(),
                    table: name.clone(),
                    from: Default::default(),
                    to: table.row_security,
                    origin: pbps_model::RowSecurityOrigin::Created,
                }));
            }
            // An exclusion is added by its own change, after the table's rows
            // (DEC-2448.3): the creation payload never carries one, so no
            // emitter can create the table and lose it.
            let exclusions = std::mem::take(&mut table.exclude);
            changes.push(Change::Table(TableChange::CreateTable {
                uid: uid.clone(),
                name: name.clone(),
                table: Box::new(table),
                beside_default,
                moves_at_most,
            }));
            for (exclusion, constraint) in exclusions {
                changes.push(Change::Constraint(ConstraintChange::SetExclusion {
                    table: name.clone(),
                    name: exclusion,
                    from: None,
                    to: Some(Box::new(constraint)),
                    rehearsal: None,
                }));
            }
        }
    }

    // Table present on both sides: possibly renamed, and its contents must be
    // compared.
    let renames = renames_of(base, declared);
    let mut standing: BTreeMap<TableName, Table> = BTreeMap::new();
    let mut loose = Vec::new();
    for (name, table) in &declared.schema.tables {
        if table.unlogged {
            continue;
        }
        for (key, fk) in &table.foreign_keys {
            if declared
                .schema
                .tables
                .get(&fk.references_table)
                .is_some_and(|t| t.unlogged)
            {
                errs.push(DiffError::PermanentReferencesUnlogged {
                    table: name.clone(),
                    key: key.clone(),
                    target: fk.references_table.clone(),
                });
            }
            // Through a partitioned table, which the engine lets pass: the
            // key reaches each of its partitions (#1580).
            for (partition, _) in declared.schema.tables.iter().filter(|(_, t)| {
                t.unlogged
                    && t.partition_of
                        .as_ref()
                        .is_some_and(|of| of.parent == fk.references_table)
            }) {
                errs.push(DiffError::PermanentReferencesUnloggedPartition {
                    table: name.clone(),
                    key: key.clone(),
                    target: fk.references_table.clone(),
                    partition: partition.clone(),
                });
            }
        }
    }
    let ctx = TableCtx {
        base,
        declared,
        dialect,
        types: &types,
        index: &index,
        read_back,
        renames: &renames,
        parent_shapes: &parent_shapes,
        final_parent: &final_parent,
        base_tables,
    };
    for (uid, declared_name) in declared_tables {
        diff_surviving_table(
            &ctx,
            uid,
            declared_name,
            &mut standing,
            &mut loose,
            &mut changes,
            &mut errs,
        );
    }
    (
        TableFacts {
            index,
            renames,
            standing,
            loose,
        },
        changes,
        errs,
    )
}

/// The read-only facts the loop over the tables both sides hold shares across
/// its iterations; `diff_surviving_table` reads them.
struct TableCtx<'a, D: Dialect + ?Sized> {
    base: Side<'a>,
    declared: Side<'a>,
    dialect: &'a D,
    types: &'a TypeResolver<'a>,
    index: &'a Indexes<'a>,
    read_back: Option<&'a Schema>,
    renames: &'a Renames,
    parent_shapes: &'a BTreeMap<&'a Uid, (&'a TableName, &'a TableName, &'a Table, bool)>,
    final_parent: &'a dyn Fn(&TableName) -> TableName,
    base_tables: &'a BTreeMap<Uid, TableName>,
}

/// One table both sides hold: renamed, then detached, attached or compared
/// with its declaration. It pushes into the caller's lists in place, as the
/// loop it came from did: the order of both is part of the result, and the
/// attach and detach arms stop early where the table needs no more.
#[allow(clippy::needless_borrow)] // The body moved as it was; its borrows were of owned locals.
fn diff_surviving_table<D: Dialect + ?Sized>(
    ctx: &TableCtx<'_, D>,
    uid: &Uid,
    declared_name: &TableName,
    standing: &mut BTreeMap<TableName, Table>,
    loose: &mut Vec<Loose>,
    mut changes: &mut Vec<Change>,
    mut errs: &mut Vec<DiffError>,
) {
    let TableCtx {
        base,
        declared,
        dialect,
        types,
        index,
        read_back,
        renames,
        parent_shapes,
        final_parent,
        base_tables,
    } = *ctx;
    let Some(base_name) = base_tables.get(uid) else {
        return;
    };
    if base_name != declared_name {
        let defaults = base
            .schema
            .tables
            .get(base_name)
            .map(|t| {
                t.columns
                    .iter()
                    .filter(|(_, c)| match &c.value_source {
                        ValueSource::Default(_) => true,
                        ValueSource::None
                        | ValueSource::Identity(_)
                        | ValueSource::Sequence(_)
                        | ValueSource::Generated(_) => false,
                    })
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();
        changes.push(Change::Table(TableChange::RenameTable {
            uid: uid.clone(),
            from: base_name.clone(),
            to: declared_name.clone(),
            defaults,
        }));
    }
    let (Some(base_table), Some(declared_table)) = (
        base.schema.tables.get(base_name),
        declared.schema.tables.get(declared_name),
    ) else {
        return;
    };
    // Only a retained identity needs explicit policy teardown. Dropping
    // the owner removes its policies, while a replacement UID is created
    // through the ordinary new-table path (DEC-1173.2).
    for name in base_table
        .row_policies
        .keys()
        .chain(declared_table.row_policies.keys())
        .collect::<BTreeSet<_>>()
    {
        let before = base_table.row_policies.get(name);
        let after = declared_table.row_policies.get(name);
        if before == after {
            continue;
        }
        if let Some(policy) = before {
            changes.push(Change::Policy(PolicyChange::DropPolicy {
                table: base_name.clone(),
                name: name.clone(),
                policy: policy.clone(),
            }));
        }
        if let Some(policy) = after {
            changes.push(Change::Policy(PolicyChange::CreatePolicy {
                table: declared_name.clone(),
                name: name.clone(),
                policy: policy.clone(),
                origin: pbps_model::RowSecurityOrigin::Standing,
            }));
        }
    }
    if base_table.row_security != declared_table.row_security {
        changes.push(Change::Policy(PolicyChange::SetRowSecurity {
            uid: uid.clone(),
            table: declared_name.clone(),
            from: base_table.row_security,
            to: declared_table.row_security,
            origin: pbps_model::RowSecurityOrigin::Standing,
        }));
    }
    // A partition declared as an ordinary table is detached, and that is
    // all this plan does to it (#1544, DEC-1544.1): its base holds no
    // columns of its own, so a column-by-column diff would read every one
    // as new.
    if let Some(of) = &base_table.partition_of
        && declared_table.partition_of.is_none()
        && declared_table.partition_by.is_none()
    {
        match base
            .schema
            .tables
            .get(&of.parent)
            .ok_or_else(|| {
                vec![format!(
                    "its parent {} is not in the base",
                    of.parent.shown()
                )]
            })
            // The parent as this plan leaves it: a foreign key to a
            // table renamed in the same plan is declared under the new
            // name, as `diff_constraints` reads it below. And the
            // partition as well, whose own foreign keys are matched by
            // name and definition (#1632).
            .and_then(|parent| {
                detached_names(
                    &renames.apply(parent, &of.parent),
                    &renames.apply(base_table, base_name),
                    declared_table,
                    dialect,
                    base.ids
                        .table_uid(&of.parent)
                        .and_then(|uid| parent_shapes.get(uid))
                        .is_some_and(|(_, _, _, pending)| *pending),
                )
            }) {
            Ok(names) => changes.push(Change::Partition(PartitionChange::DetachPartition {
                uid: uid.clone(),
                table: declared_name.clone(),
                parent: final_parent(&of.parent),
                names,
                shape: Box::new(declared_table.clone()),
            })),
            Err(what) => errs.push(DiffError::DetachedShape {
                table: declared_name.clone(),
                parent: of.parent.clone(),
                what,
            }),
        }
        return;
    }

    // An ordinary table declared as a partition is attached, and keeps
    // its rows (#1545, DEC-1545.1). Its columns become its parent's, so
    // they are not compared; what it holds of its own once attached is
    // compared with the declaration as a standing partition's is, and
    // brought to it by the same changes after the attach.
    if base_table.partition_of.is_none()
        && let Some(of) = &declared_table.partition_of
    {
        match attached(
            base,
            base_name,
            declared_name,
            base_table,
            declared_table,
            of,
            dialect,
            read_back,
            declared
                .ids
                .table_uid(&of.parent)
                .and_then(|uid| parent_shapes.get(uid))
                .map(|(old, _, goal, pending)| (*old, *pending, *goal)),
        ) {
            Ok(Attached {
                mut table,
                defaultless,
                displaced,
                exclusions,
                released,
                by,
            }) => {
                // Its own exclusions' retirements, as the base held them: asked
                // of the table's original definition, as for a standing table
                // (DEC-2448.7).
                let retired = |key: &pbps_model::ObjectLocalName| {
                    base_table
                        .exclude
                        .get(key)
                        .is_some_and(|held| declared.ids.exclusion_retired(uid, key, held))
                };
                // Each drop the parent's might take is marked as it is pushed,
                // for the plan to say why (DEC-1671.3).
                let mut mark = |drop: &Change,
                                name: Option<&pbps_model::ObjectLocalName>,
                                exclusions: bool,
                                kept: Option<bool>| {
                    if let Some(by) = by.get(&name.cloned()) {
                        loose.extend(Loose::attached(
                            drop,
                            declared_name,
                            name,
                            by,
                            exclusions,
                            kept,
                        ));
                    }
                };
                for change in released {
                    if let Change::Constraint(ConstraintChange::DropUnique { name, .. }) = &change {
                        mark(&change, Some(name), false, None);
                    } else if let Change::Constraint(ConstraintChange::SetPrimaryKey {
                        from: Some(key),
                        ..
                    }) = &change
                    {
                        mark(&change, key.name.as_ref(), false, None);
                    }
                    changes.push(change);
                }
                let held = renames.apply(base_table, base_name);
                // Gone before the attach, so the engine adopts at most the
                // certain index left for one of its parent's and builds the
                // rest; one the declaration keeps as its own is added after.
                for name in displaced {
                    let kept = declared_table
                        .indexes
                        .get(&name)
                        .map(|now| held.indexes.get(&name) == Some(now));
                    let drop = Change::Index(IndexChange::DropIndex {
                        table: declared_name.clone(),
                        name: name.clone(),
                    });
                    mark(&drop, Some(&name), false, kept);
                    changes.push(drop);
                }
                // The same for an exclusion the parent's might take
                // (DEC-1671.1). One the declarations keep is added back after
                // the attach by `diff_exclusions` below, as one they retire is
                // dropped here; any other is left on the attached table, so that
                // same pass refuses the drop for want of a recorded reason.
                for name in exclusions {
                    let Some(own) = held.exclude.get(&name) else {
                        continue;
                    };
                    if declared_table.exclude.contains_key(&name) || retired(&name) {
                        let kept = declared_table.exclude.get(&name).map(|now| now == own);
                        let drop = Change::Constraint(ConstraintChange::SetExclusion {
                            table: declared_name.clone(),
                            name: name.clone(),
                            from: Some(Box::new(own.clone())),
                            to: None,
                            rehearsal: None,
                        });
                        mark(&drop, Some(&name), true, kept);
                        changes.push(drop);
                    } else if let Some(own) = base_table.exclude.get(&name) {
                        table.exclude.insert(name, own.clone());
                    }
                }
                changes.push(Change::Partition(PartitionChange::AttachPartition {
                    uid: uid.clone(),
                    table: declared_name.clone(),
                    parent: of.parent.clone(),
                    bound: of.bound.clone(),
                    key: dialect.attach_scan_key(
                        uid,
                        &declared
                            .schema
                            .tables
                            .get(&of.parent)
                            .and_then(|parent| {
                                let by = parent.partition_by.as_ref()?;
                                by.columns
                                    .iter()
                                    .map(|c| Some((c.clone(), parent.columns.get(c)?.ty.clone())))
                                    .collect::<Option<Vec<_>>>()
                            })
                            .unwrap_or_default(),
                        &declared.schema.types,
                    ),
                    shape: Box::new(declared_table.clone()),
                    rehearsal: None,
                }));
                // Compared as this plan leaves them: an own foreign key
                // to a table renamed in the same plan is declared under
                // the new name (#1632). Matched to the parent's above on
                // the base side, where the engine matches by relation.
                diff_constraints(
                    declared_name,
                    &renames.apply(&table, base_name),
                    declared_table,
                    &mut changes,
                    &mut errs,
                );
                // Its own exclusions: those the parent's cannot take, as the
                // base held them, and the ones dropped above, added back.
                diff_exclusions(
                    declared_name,
                    &renames.apply(&table, base_name),
                    declared_table,
                    &retired,
                    &mut changes,
                    &mut errs,
                );
                diff_storage_parameters(uid, declared_name, &table, declared_table, &mut changes);
                if table.unlogged != declared_table.unlogged {
                    changes.push(Change::TableProperty(
                        TablePropertyChange::SetTablePersistence {
                            uid: uid.clone(),
                            table: declared_name.clone(),
                            unlogged: declared_table.unlogged,
                        },
                    ));
                }
                diff_partition_columns(
                    uid,
                    declared_name,
                    &table,
                    declared_table,
                    base,
                    declared,
                    &index,
                    dialect,
                    &mut changes,
                    &mut errs,
                );
                // A column without a default where its parent's has one:
                // the attach gives it none, which no declaration can say
                // and the reader does not hold, so it takes its parent's
                // unless it declares its own, which the comparison above
                // sets (measured on 16 and 18).
                let parent = declared.schema.tables.get(&of.parent);
                for column in defaultless {
                    let own = of.columns.get(&column).and_then(|c| c.default.as_ref());
                    let fallback = partition_default(
                        dialect,
                        &of.parent,
                        &column,
                        parent.and_then(|p| p.columns.get(&column)),
                        base.schema
                            .tables
                            .get(&of.parent)
                            .and_then(|p| p.columns.get(&column)),
                        &mut errs,
                    );
                    if let (None, Some(fallback)) = (own, fallback) {
                        changes.push(Change::Partition(PartitionChange::SetPartitionDefault {
                            uid: uid.clone(),
                            table: declared_name.clone(),
                            parent: of.parent.clone(),
                            column,
                            from: None,
                            to: None,
                            fallback: Some(fallback),
                        }));
                    }
                }
            }
            Err(what) => errs.push(DiffError::AttachedShape {
                table: declared_name.clone(),
                parent: of.parent.clone(),
                what,
            }),
        }
        return;
    }

    diff_columns(
        &index,
        &types,
        base_name,
        declared_name,
        base_table,
        declared_table,
        dialect,
        &declared.ids.conversions,
        &mut changes,
        &mut errs,
    );
    // Asked of the whole change list below, not of this loop's own: an enum
    // or domain rebuild retypes a column before this loop runs.
    if !base_table.exclude.is_empty() {
        standing.insert(
            declared_name.clone(),
            renames.apply(base_table, base_name).into_owned(),
        );
    }
    diff_filegroups(declared_name, base_table, declared_table, &mut errs);
    diff_constraints(
        declared_name,
        &renames.apply(base_table, base_name),
        declared_table,
        &mut changes,
        &mut errs,
    );
    // A retirement holds the definition as the baseline wrote it, and the
    // table compared below has had this plan's column renames applied, so
    // authority is asked of the original: a plan that renames a key column
    // and drops the constraint is a valid one, and no rerun of `pbps
    // drop-exclusion` could record the renamed text.
    let retired = |key: &pbps_model::ObjectLocalName| {
        base_table
            .exclude
            .get(key)
            .is_some_and(|held| declared.ids.exclusion_retired(uid, key, held))
    };
    diff_exclusions(
        declared_name,
        &renames.apply(base_table, base_name),
        declared_table,
        &retired,
        &mut changes,
        &mut errs,
    );
    diff_computed(declared_name, base_table, declared_table, &mut changes);
    diff_storage_parameters(uid, declared_name, base_table, declared_table, &mut changes);
    if base_table.unlogged != declared_table.unlogged {
        changes.push(Change::TableProperty(
            TablePropertyChange::SetTablePersistence {
                uid: uid.clone(),
                table: declared_name.clone(),
                unlogged: declared_table.unlogged,
            },
        ));
    }
    diff_partition_columns(
        uid,
        declared_name,
        base_table,
        declared_table,
        base,
        declared,
        &index,
        dialect,
        &mut changes,
        &mut errs,
    );
    // Rows are compared by column *name* on each side, and a rename in
    // this same plan means the two sides know one column by two names.
    // The uid is what says they are the same column.
    let base_cols = columns_of(&index.base, base_name);
    let base_name_of: BTreeMap<ColumnName, ColumnName> = columns_of(&index.declared, declared_name)
        .iter()
        .filter_map(|(uid, d)| base_cols.get(uid).map(|b| (d.name.clone(), b.name.clone())))
        .collect();
    // The observed rows as this plan's label renames leave them: the
    // engine renames a label in the rows that hold it (DEC-2346.2).
    let forwarded = crate::enum_labels::forwarded_rows(
        base_table,
        (&TypeResolver::new(base.ids, declared.ids), TypeSide::Base),
        &crate::enum_labels::label_renames(&*changes),
    );
    diff_data(
        declared_name,
        forwarded.as_ref().unwrap_or(base_table),
        declared_table,
        &base_name_of,
        &mut changes,
        &mut errs,
    );
}
