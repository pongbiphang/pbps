//! Attaching a table as a partition, and a standing partition's own defaults and NOT NULLs.

use super::super::constraints::may_adopt;
use super::super::exclusions::may_adopt_exclusion;
use super::super::*;

/// A table being attached as declared (#1545): itself as a partition the
/// moment the attach has run, holding only what the engine leaves it of its
/// own, and its columns without a default where the parent's has one.
pub(in super::super) struct Attached {
    pub(in super::super) table: Table,
    pub(in super::super) defaultless: Vec<ColumnName>,
    /// The table's indexes dropped before the attach: every one a parent's
    /// index might take as its clone, but for a certain one left to it.
    pub(in super::super) displaced: Vec<pbps_model::ObjectLocalName>,
    /// The table's own exclusions a parent's might take, left out of `table`.
    /// The caller drops each before the attach, or keeps it to be refused.
    pub(in super::super) exclusions: Vec<pbps_model::ObjectLocalName>,
    /// The table's key and unique constraints dropped before the attach,
    /// whose index an index of the parent's might take.
    pub(in super::super) released: Vec<Change>,
}

/// What the ordinary table `table` is once attached to `of.parent`, or what
/// keeps it from being attached into a tree the model holds (#1545,
/// DEC-1545.1). Measured on 16 and 18:
///
/// - its columns must be its parent's, in its parent's order, with the same
///   types, collations, identities and generations, and NOT NULL wherever
///   the parent's are, or the engine refuses the attach; the order is the
///   reader's, which leaves a tree out over a partition in another;
/// - each of the parent's checks must be on it under the same name, and
///   becomes the inherited copy; the rest of its checks stay its own;
/// - an index, a key or a foreign key matching one of the parent's is
///   adopted as its clone, and the engine builds whichever is missing; a key
///   or a foreign key of its own would stay its own, which a partition does
///   not hold yet, while an index of its own stays as one;
/// - which index or exclusion the engine adopts is not predicted: each of the
///   table's that a parent's might take, by the loose [`may_adopt`] and
///   [`may_adopt_exclusion`], is dropped before the attach, but for a lone
///   certain index (DEC-1671.1);
/// - its defaults stay, and none of the parent's is given to a column
///   without one.
#[allow(clippy::too_many_arguments)]
pub(in super::super) fn attached(
    base: Side<'_>,
    base_name: &TableName,
    declared_name: &TableName,
    table: &Table,
    declared: &Table,
    of: &pbps_model::PartitionOf,
    dialect: &(impl Dialect + ?Sized),
    read_back: Option<&Schema>,
    boundary: Option<(&TableName, bool, &Table)>,
) -> Result<Attached, Vec<String>> {
    let mut what = Vec::new();
    let parent_name = boundary.map_or(&of.parent, |(name, _, _)| name);
    let pending_columns = boundary.is_some_and(|(_, pending, _)| pending);
    let goal_parent = boundary.map(|(_, _, goal)| goal);
    let Some(parent) = base
        .schema
        .tables
        .get(parent_name)
        .filter(|p| p.partition_by.is_some() && p.partition_of.is_none())
    else {
        return Err(vec![format!(
            "its parent {} is not a partitioned table before this plan; create the partition \
             with its parent, or attach the table in a later plan",
            of.parent.shown()
        )]);
    };
    if base_name != declared_name {
        what.push(format!(
            "it is renamed from {base_name} in the same plan; rename it in one plan and attach it \
             in another",
            base_name = base_name.shown()
        ));
    }
    if table.partition_by.is_some() {
        what.push("it is partitioned itself".to_owned());
    }
    // Types in the engine's spelling, as `diff_columns` compares them. A
    // default and NOT NULL may be its own, and are compared below.
    let columns = |t: &Table, is_parent: bool| -> Vec<(ColumnName, pbps_model::Column)> {
        t.columns
            .iter()
            .map(|(name, c)| {
                let ty = dialect
                    .normalize_type(&c.ty)
                    .unwrap_or_else(|_| c.ty.clone());
                // A deprecation is an annotation the catalog does not
                // hold, as a description is.
                let column = pbps_model::Column {
                    ty,
                    nullable: true,
                    value_source: match &c.value_source {
                        ValueSource::Default(_) | ValueSource::None => ValueSource::None,
                        // A partition takes its parent's identity. Its own is
                        // refused separately below (DEC-1681.2).
                        ValueSource::Identity(_) => ValueSource::None,
                        // The parent's serial is a default the partition
                        // copies, not a sequence the attached table owns.
                        // An ordinary table's own sequence still differs.
                        ValueSource::Sequence(_) if is_parent => ValueSource::None,
                        ValueSource::Sequence(sequence) => ValueSource::Sequence(sequence.clone()),
                        ValueSource::Generated(generated) => {
                            ValueSource::Generated(generated.clone())
                        }
                    },
                    description: None,
                    deprecated: None,
                    ..c.clone()
                };
                (name.clone(), column)
            })
            .collect()
    };
    // The engine matches a check and a generation expression on what they
    // parse to, which the recorded texts do not show: `n>0` and `n > 0` are
    // one. A connected plan has the engine's own spelling of both standing
    // objects, under the overlay of the recorded texts, and compares that;
    // the recorded texts stay the shape's. Offline there is no such spelling,
    // so the texts are left out and the two are matched by name and kind:
    // an offline plan is never applied (SPEC §7.3), and refusing there would
    // also keep it from writing the identities the connected plan needs
    // (#1642 review).
    let spelled = |t: &Table, name: &TableName| -> Table {
        let mut t = t.clone();
        let read = read_back.map(|r| r.tables.get(name));
        for (check, spec) in &mut t.checks {
            match read {
                Some(read) => {
                    if let Some(engine) = read.and_then(|r| r.checks.get(check)) {
                        spec.expression = engine.expression.clone();
                    }
                }
                None => spec.expression.clear(),
            }
        }
        for (column, spec) in &mut t.columns {
            let generated = match &mut spec.value_source {
                ValueSource::Generated(generated) => generated,
                ValueSource::None
                | ValueSource::Default(_)
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_) => continue,
            };
            match read {
                Some(read) => {
                    if let Some(engine) = read.and_then(|r| r.columns.get(column)).and_then(|c| {
                        match &c.value_source {
                            ValueSource::Generated(value) => Some(value),
                            ValueSource::None
                            | ValueSource::Default(_)
                            | ValueSource::Identity(_)
                            | ValueSource::Sequence(_) => None,
                        }
                    }) {
                        generated.expression = engine.expression.clone();
                    }
                }
                None => generated.expression.clear(),
            }
        }
        t
    };
    let (table_spelled, parent_spelled) = (spelled(table, base_name), spelled(parent, parent_name));
    let (mine, theirs) = (
        columns(&table_spelled, false),
        columns(&parent_spelled, true),
    );
    let names = |c: &[(ColumnName, pbps_model::Column)]| {
        c.iter()
            .map(|(n, _)| n.shown().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    // The names themselves, not their joined spelling, which two lists can
    // share when a name holds `, ` (#1642 review).
    if !pending_columns
        && !mine
            .iter()
            .map(|(n, _)| n)
            .eq(theirs.iter().map(|(n, _)| n))
    {
        what.push(format!(
            "its columns are ({}), and its parent's are ({}), in that order",
            names(&mine),
            names(&theirs)
        ));
    } else if !pending_columns {
        for ((name, a), (_, b)) in mine.iter().zip(&theirs) {
            if a != b {
                what.push(format!(
                    "its column `{name}` is not its parent's in type, collation or \
                     generation",
                    name = name.shown(),
                ));
            } else if (match &a.value_source {
                ValueSource::Generated(_) => true,
                ValueSource::None
                | ValueSource::Default(_)
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_) => false,
            }) && base_name.schema != of.parent.schema
            {
                // The engine does not compare generation expressions as it
                // attaches, and keeps the table's: in another schema the same
                // text may call another function, and each partition would
                // then compute the column its own way (measured on 18, #1642
                // review).
                what.push(format!(
                    "its column `{name}` is generated, and in another schema than its parent \
                     the same expression may call another function; move the table to its \
                     parent's schema first",
                    name = name.shown(),
                ));
            }
        }
    }
    if pending_columns
        && (base_name.schema != parent_name.schema || base_name.schema != of.parent.schema)
        && table
            .columns
            .values()
            .any(|c| matches!(c.value_source, ValueSource::Generated(_)))
    {
        what.push(
            "a generated column is in another schema; move the table to its parent's schema first"
                .into(),
        );
    }
    let mut own_columns = BTreeMap::new();
    let mut defaultless = Vec::new();
    for (name, c) in &table.columns {
        if matches!(c.value_source, ValueSource::Identity(_)) {
            what.push(format!(
                "its column `{name}` has its own identity; drop the identity in an earlier plan \
                 before attaching it",
                name = name.shown(),
            ));
        }
        let p = parent.columns.get(name);
        if p.is_none() && !pending_columns {
            continue;
        }
        if !pending_columns && c.nullable && p.is_some_and(|p| !p.nullable) {
            what.push(format!(
                "its column `{name}` is nullable, and its parent's is NOT NULL",
                name = name.shown(),
            ));
        }
        let own = pbps_model::PartitionColumn {
            // The same text as the parent's is the parent's, as the reader
            // reads it back, but only in the parent's schema: elsewhere an
            // unqualified name in it may be another schema's object, so it
            // is the table's own, and taking the parent's back sets the
            // parent's under the parent's path (DEC-1581.1).
            default: (match &c.value_source {
                ValueSource::Default(value) => Some(value.clone()),
                ValueSource::None
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_)
                | ValueSource::Generated(_) => None,
            })
            .filter(|d| {
                pending_columns
                    || base_name.schema != of.parent.schema
                    || Some(d)
                        != (match p.map(|p| &p.value_source) {
                            Some(ValueSource::Default(value)) => Some(value),
                            None
                            | Some(
                                ValueSource::None
                                | ValueSource::Identity(_)
                                | ValueSource::Sequence(_)
                                | ValueSource::Generated(_),
                            ) => None,
                        })
            }),
            // This is the managed comparison template, not an ATTACH result.
            // The symbolic merge independently proves the actual local flag.
            not_null: !c.nullable
                && (if pending_columns {
                    goal_parent
                        .and_then(|p| p.columns.get(name))
                        .is_some_and(|p| p.nullable)
                } else {
                    p.is_some_and(|p| p.nullable)
                }),
        };
        if (match &c.value_source {
            ValueSource::Default(_) => false,
            ValueSource::None
            | ValueSource::Identity(_)
            | ValueSource::Sequence(_)
            | ValueSource::Generated(_) => true,
        }) && (match (if pending_columns {
            goal_parent.and_then(|p| p.columns.get(name))
        } else {
            p
        })
        .map(|p| &p.value_source)
        {
            Some(ValueSource::Default(_) | ValueSource::Sequence(_)) => true,
            None
            | Some(ValueSource::None | ValueSource::Identity(_) | ValueSource::Generated(_)) => {
                false
            }
        }) {
            defaultless.push(name.clone());
        }
        if own != pbps_model::PartitionColumn::default() {
            own_columns.insert(name.clone(), own);
        }
    }
    match (&table.primary_key, &parent.primary_key) {
        (None, _) => {}
        (Some(own), Some(p)) if own.columns == p.columns => {}
        (Some(_), _) => what.push("its primary key is not its parent's".to_owned()),
    }
    // One to one: a second match would stay its own.
    fn unmatched<'a, V: PartialEq>(
        own: &'a BTreeMap<pbps_model::ObjectLocalName, V>,
        parent: &BTreeMap<pbps_model::ObjectLocalName, V>,
    ) -> Vec<&'a pbps_model::ObjectLocalName> {
        let mut free: Vec<&V> = parent.values().collect();
        own.iter()
            .filter(|(_, d)| match free.iter().position(|p| p == d) {
                Some(at) => {
                    free.remove(at);
                    false
                }
                None => true,
            })
            .map(|(n, _)| n)
            .collect()
    }
    // By definition, storage parameters aside: measured on 16 and 18, the
    // engine adopts a key or an index whatever its own, which it keeps, and a
    // clone's are not held.
    let unique =
        |t: &Table| -> BTreeMap<pbps_model::ObjectLocalName, pbps_model::UniqueConstraint> {
            t.unique
                .iter()
                .map(|(n, u)| {
                    let mut u = u.clone();
                    u.storage_parameters.clear();
                    (n.clone(), u)
                })
                .collect()
        };
    // An index as the engine matches it to adopt it, which the certain case
    // below asks: storage parameters, a key's class options and sort order
    // aside, measured on 16 and 18 (`DESC` and `NULLS FIRST` adopted, another
    // collation, operator class, `INCLUDE` or method not). A BRIN key's
    // `values_per_range` of 32 under a parent's 16 is adopted all the same,
    // and the clone keeps its 32 (DEC-2449.2), so the options are not part of
    // the match.
    let comparable = |index: &Index| -> Index {
        let mut index = index.clone();
        index.storage_parameters.clear();
        for c in &mut index.columns {
            c.descending = false;
            c.opclass_parameters.clear();
        }
        index
    };
    for name in unmatched(&unique(table), &unique(parent)) {
        what.push(format!(
            "its unique constraint `{name}` is not its parent's, and a partition holds none of \
             its own yet",
            name = name.shown(),
        ));
    }
    // Measured on 16 and 18: one equal to a parent's is taken as the
    // parent's clone, and every other stays the partition's own, under its
    // name (#1632). The plan then compares those with what the partition
    // declares of its own, as for any table. Validation keeps the partition
    // from declaring one equal to a parent's, so none of those is its own.
    let own_foreign_keys: BTreeMap<pbps_model::ObjectLocalName, pbps_model::ForeignKey> = table
        .foreign_keys
        .iter()
        .filter(|(_, fk)| !parent.foreign_keys.values().any(|p| p == *fk))
        .map(|(name, fk)| (name.clone(), fk.clone()))
        .collect();
    let mut checks = table.checks.clone();
    for (name, p) in &parent_spelled.checks {
        checks.remove(name);
        if table_spelled.checks.get(name) != Some(p) {
            what.push(format!(
                "its parent's check `{name}` is not on it under that name, which the engine \
                 requires",
                name = name.shown(),
            ));
        }
    }
    // Which of several matches the engine adopts is the first by oid, and
    // whether two expressions or predicates are one is its parse trees' to
    // say: neither is in the model. So nothing is predicted. Every index of the
    // table that one of the parent's might take (`may_adopt`, wider than the
    // engine's comparison) is dropped before the attach, the engine builds the
    // clone, and what the declaration keeps is added after it. One that no
    // index of the parent's might take stays the table's own (DEC-1671.1).
    //
    // The parent's definition is the one it holds at the attach: as the plan
    // found it and, where a column rename meets the attach (DEC-2296.12), as it
    // leaves it.
    let parents_indexes: Vec<Vec<&Index>> = parent
        .indexes
        .iter()
        .map(|(name, was)| {
            let now = goal_parent
                .and_then(|goal| goal.indexes.get(name))
                .filter(|now| comparable(now) != comparable(was));
            std::iter::once(was).chain(now).collect()
        })
        .collect();
    let takes = |theirs: &[&Index], own: &Index| theirs.iter().any(|p| may_adopt(p, own, dialect));
    // The index a key or unique constraint stands on. A parent's plain index
    // adopts a matching one whether or not a constraint stands on it, while a
    // parent's key or unique constraint adopts only one that has one. Measured
    // on 16 and 18, a parent's `UNIQUE INDEX (a)` made before its `UNIQUE (a)`
    // takes the index of the table's `UNIQUE (a)`, the constraint stays the
    // table's own, and the parent's constraint is built a clone beside it: a
    // tree the reader refuses (#1642 review).
    let backing = |columns: &[ColumnName]| Index {
        columns: columns
            .iter()
            .map(|c| pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Column(c.clone()),
                descending: false,
                opclass: None,
                collation: None,
                opclass_parameters: Default::default(),
            })
            .collect(),
        include: Vec::new(),
        unique: true,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
        filegroup: None,
    };
    let own_keys: Vec<Index> = table
        .primary_key
        .iter()
        .map(|key| backing(&key.columns))
        .chain(table.unique.values().map(|u| backing(&u.columns)))
        .collect();
    let candidates: Vec<Vec<&pbps_model::ObjectLocalName>> = parents_indexes
        .iter()
        .map(|theirs| {
            table
                .indexes
                .iter()
                .filter(|(_, own)| takes(theirs, own))
                .map(|(n, _)| n)
                .collect()
        })
        .collect();
    let mut left: BTreeSet<&pbps_model::ObjectLocalName> = BTreeSet::new();
    let mut displaced: BTreeSet<&pbps_model::ObjectLocalName> = BTreeSet::new();
    for (theirs, mine) in parents_indexes.iter().zip(&candidates) {
        // The one case with nothing to predict, kept as an adoption: a lone
        // candidate for this index and for no other, equal to it, with no text
        // for the engine to parse, and no constraint's index beside it. An
        // adopted index keeps its name as the clone, so one whose name the
        // declaration gives an index of its own is dropped too (#1642 review).
        let certain = match (theirs.as_slice(), mine.as_slice()) {
            ([theirs], [name]) => table
                .indexes
                .get(*name)
                .is_some_and(|own| {
                    comparable(own) == comparable(theirs)
                        && own.filter.is_none()
                        && own.columns.iter().all(|c| c.key.column().is_some())
                        && candidates.iter().filter(|c| c.contains(name)).count() == 1
                        && !declared.indexes.contains_key(name.raw())
                        && !own_keys.iter().any(|key| may_adopt(theirs, key, dialect))
                })
                .then_some(*name),
            _ => None,
        };
        match certain {
            Some(name) => {
                left.insert(name);
            }
            None => displaced.extend(mine),
        }
    }
    let indexes = table
        .indexes
        .iter()
        .filter(|(n, _)| !left.contains(n) && !displaced.contains(n))
        .map(|(n, i)| (n.clone(), i.clone()))
        .collect();
    let displaced = displaced.into_iter().cloned().collect();
    // An own exclusion the parent's might take is made its clone by `ATTACH`,
    // keeping its name, and then goes with the parent's (measured on 18.6,
    // DEC-2448.7), so it is dropped first like an index (DEC-1671.1).
    let exclusions: Vec<pbps_model::ObjectLocalName> = table
        .exclude
        .iter()
        .filter(|(_, own)| {
            parent.exclude.iter().any(|(name, was)| {
                may_adopt_exclusion(was, own, dialect)
                    || goal_parent
                        .and_then(|goal| goal.exclude.get(name))
                        .is_some_and(|now| may_adopt_exclusion(now, own, dialect))
            })
        })
        .map(|(n, _)| n.clone())
        .collect();
    let exclude = table
        .exclude
        .iter()
        .filter(|(n, _)| !exclusions.contains(n))
        .map(|(n, x)| (n.clone(), x.clone()))
        .collect();
    let mut released = Vec::new();
    let contested = |columns: &[ColumnName]| {
        let own = backing(columns);
        parents_indexes.iter().any(|theirs| takes(theirs, &own))
    };
    if let Some(key) = &table.primary_key
        && contested(&key.columns)
    {
        released.push(Change::Constraint(ConstraintChange::SetPrimaryKey {
            table: declared_name.clone(),
            from: Some(key.clone()),
            to: None,
            nonclustered: false,
        }));
    }
    for (name, u) in &table.unique {
        if contested(&u.columns) {
            released.push(Change::Constraint(ConstraintChange::DropUnique {
                table: declared_name.clone(),
                name: name.clone(),
            }));
        }
    }
    // A foreign key the engine takes for the parent's clone keeps its own
    // name (measured on 16 and 18), so a check or foreign key the partition
    // declares of its own under that name would meet it as `already exists`.
    // And of two equal to one of the parent's, the engine takes one by an
    // order the model does not hold (measured on 16 and 18: `a_fk`, created
    // after `z_fk`, was taken), and the other stays local under a name this
    // plan cannot tell. Each such one is dropped before the attach,
    // leaving the engine to clone the parent's under the parent's name, which
    // no own name takes (#1632 review).
    let twins = |fk: &pbps_model::ForeignKey| {
        table
            .foreign_keys
            .values()
            .filter(|other| *other == fk)
            .count()
            > 1
    };
    for (name, fk) in &table.foreign_keys {
        if !own_foreign_keys.contains_key(name)
            && (twins(fk)
                || declared.foreign_keys.contains_key(name)
                || declared.checks.contains_key(name))
        {
            released.push(Change::Constraint(ConstraintChange::DropForeignKey {
                table: declared_name.clone(),
                name: name.clone(),
            }));
        }
    }
    if table.replica_identity.is_some() {
        what.push(
            "it has a replica identity of its own, which a partition does not hold yet".to_owned(),
        );
    }
    if table.data.is_some() {
        what.push("it declares `data:`, which a partition does not".to_owned());
    }
    // Neither engine's other settings: SQL Server's, which a partition never
    // has, since the dialect holds no partitions.
    let rest = Table {
        description: None,
        columns: Default::default(),
        primary_key: None,
        unique: Default::default(),
        foreign_keys: Default::default(),
        checks: Default::default(),
        indexes: Default::default(),
        exclude: Default::default(),
        replica_identity: None,
        storage_parameters: Default::default(),
        unlogged: false,
        partition_by: None,
        partition_of: None,
        data: None,
        ..table.clone()
    };
    if rest != Table::default() {
        what.push("it holds a setting a partition does not".to_owned());
    }
    // Held by the schema rather than the table: what reaches it from
    // elsewhere would then reach a partition, which the model does not hold.
    // A trigger on it does not: a module on the partition, it stays its own,
    // or is dropped before the attach (class 0), and validation refuses one
    // the declarations keep under a name its parent's row triggers clone
    // onto it (DEC-1632.3).
    for (other, t) in &base.schema.tables {
        for (name, fk) in &t.foreign_keys {
            if &fk.references_table == base_name {
                what.push(format!(
                    "the foreign key `{name}` of {other} references it, and nothing references a \
                     partition yet",
                    other = other.shown(),
                    name = name.shown(),
                ));
            }
        }
    }
    if !what.is_empty() {
        return Err(what);
    }
    Ok(Attached {
        table: Table {
            checks,
            indexes,
            exclude,
            foreign_keys: own_foreign_keys,
            storage_parameters: table.storage_parameters.clone(),
            unlogged: table.unlogged,
            partition_of: Some(pbps_model::PartitionOf {
                parent: of.parent.clone(),
                bound: of.bound.clone(),
                columns: own_columns,
            }),
            ..Table::default()
        },
        defaultless,
        displaced,
        exclusions,
        released,
    })
}

/// A standing partition's own defaults and NOT NULLs (#1581, DEC-1581.1),
/// one change per column and kind. Its partitioning, when changed, is refused
/// by `refuse_partition_changes`; these are compared only where both sides
/// are a partition.
///
/// The parent's own column changes in the same plan are followed (#1687),
/// each recursed by the engine into every partition, measured on 16 and 18:
/// - a rename carries a partition's own default and NOT NULL with it;
/// - a drop takes them, and nothing is left to change;
/// - a default set or dropped on the parent overwrites the partition's own,
///   which is set again after it;
/// - a NOT NULL dropped on the parent leaves one of the partition's own on 18
///   and not on 16, so every partition is brought to its declaration after
///   it;
/// - a NOT NULL set on the parent holds every partition, so the partition's
///   own, which the declarations then leave out, is left to it.
#[allow(clippy::too_many_arguments)]
pub(in super::super) fn diff_partition_columns(
    uid: &Uid,
    name: &TableName,
    base_table: &Table,
    declared_table: &Table,
    base: Side<'_>,
    declared: Side<'_>,
    index: &Indexes<'_>,
    dialect: &(impl Dialect + ?Sized),
    changes: &mut Vec<Change>,
    errors: &mut Vec<DiffError>,
) {
    let (Some(was), Some(now)) = (&base_table.partition_of, &declared_table.partition_of) else {
        return;
    };
    let parent = declared.schema.tables.get(&now.parent);
    let base_parent = base.schema.tables.get(&was.parent);
    let was_columns = partition_columns_by_uid(&was.columns, &was.parent, &now.parent, index);
    let base_of = base_parent
        .map(|p| partition_columns_by_uid(&p.columns, &was.parent, &now.parent, index))
        .unwrap_or_default();
    let none = pbps_model::PartitionColumn::default();
    let loosened: BTreeSet<ColumnName> = parent
        .map(|p| {
            p.columns
                .iter()
                .filter(|(c, column)| {
                    column.nullable && base_of.get(*c).is_some_and(|b| !b.nullable)
                })
                .map(|(c, _)| c.clone())
                .collect()
        })
        .unwrap_or_default();
    let columns: BTreeSet<&ColumnName> = was_columns
        .keys()
        .chain(now.columns.keys())
        .chain(&loosened)
        .collect();
    for column in columns {
        let from = was_columns.get(column).copied().unwrap_or(&none);
        let to = now.columns.get(column).unwrap_or(&none);
        let theirs = parent.and_then(|p| p.columns.get(column));
        let theirs_was = base_of.get(column);
        let parent_default = |c: &pbps_model::Column| match &c.value_source {
            source @ (ValueSource::Default(_) | ValueSource::Sequence(_)) => Some(source.clone()),
            ValueSource::None | ValueSource::Identity(_) | ValueSource::Generated(_) => None,
        };
        let was_default = theirs_was.and_then(|c| parent_default(c));
        let now_default = theirs.and_then(parent_default);
        // An omitted sequence name is unknown in a declaration, not a
        // request to replace the name a read knows (DEC-1631.1).
        // A new column's own default is new too, so the comparison below
        // already restores it after AddColumn (DEC-2219.1).
        let default_moved = theirs_was.is_some()
            && if let (Some(ValueSource::Sequence(was)), Some(ValueSource::Sequence(now))) =
                (&was_default, &now_default)
            {
                let was = pbps_model::OwnedSequence {
                    name: was.name.clone().or_else(|| now.name.clone()),
                    ..was.clone()
                };
                !now.declares(&was)
            } else {
                was_default != now_default
            };
        if defaults_differ(from.default.as_ref(), to.default.as_ref())
            || (default_moved && to.default.is_some())
        {
            changes.push(Change::Partition(PartitionChange::SetPartitionDefault {
                uid: uid.clone(),
                table: name.clone(),
                parent: now.parent.clone(),
                column: column.clone(),
                from: from.default.clone(),
                to: to.default.clone(),
                // What the engine gave the partition when it was made, and
                // what a row written to it directly gets without one.
                fallback: partition_default(
                    dialect,
                    &now.parent,
                    column,
                    theirs,
                    theirs_was.copied(),
                    errors,
                ),
            }));
        }
        let tightened =
            theirs.is_some_and(|c| !c.nullable) && theirs_was.is_some_and(|b| b.nullable);
        if !tightened && (from.not_null != to.not_null || loosened.contains(column)) {
            changes.push(Change::Partition(PartitionChange::SetPartitionNotNull {
                uid: uid.clone(),
                table: name.clone(),
                column: column.clone(),
                not_null: to.not_null,
            }));
        }
    }
}

/// Both an attach and an override removed later take the parent's default.
/// Keep the serial expression in the dialect, while the saved change keeps
/// the same concrete fallback text as a plain default (DEC-2218.1).
pub(in super::super) fn partition_default(
    dialect: &(impl Dialect + ?Sized),
    parent: &TableName,
    column: &ColumnName,
    declared: Option<&pbps_model::Column>,
    previous: Option<&pbps_model::Column>,
    errors: &mut Vec<DiffError>,
) -> Option<String> {
    match &declared?.value_source {
        ValueSource::Default(value) => Some(value.clone()),
        ValueSource::Sequence(sequence) => {
            let name = previous.and_then(|c| match &c.value_source {
                ValueSource::Sequence(sequence) => sequence.name.clone(),
                ValueSource::None
                | ValueSource::Default(_)
                | ValueSource::Identity(_)
                | ValueSource::Generated(_) => None,
            });
            let sequence = pbps_model::OwnedSequence {
                name: sequence.name.clone().or(name),
                ..sequence.clone()
            };
            match dialect.owned_sequence_default(parent, column.raw(), &sequence) {
                Ok(value) => Some(value),
                Err(error) => {
                    errors.push(DiffError::PartitionDefault {
                        column: parent.column(column.clone()),
                        reason: error.to_string(),
                    });
                    None
                }
            }
        }
        ValueSource::None | ValueSource::Identity(_) | ValueSource::Generated(_) => None,
    }
}
