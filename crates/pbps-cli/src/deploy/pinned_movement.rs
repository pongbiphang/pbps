//! Refusing a plan whose pinned objects moved between planning and applying.

use super::*;

/// Refuses a state that moved while the plan was running, and not by the plan.
///
/// `apply` reads the baseline before the statements and reads the state back
/// after them, and it is the second read it records (SPEC §8.2, DECISIONS
/// 147). Between the two, another session can change something this plan
/// never mentions — revoke a grant the declarations still hold, edit a
/// declared row of a table the plan does not touch — and the read-back takes
/// it in as if the plan had produced it. `apply` then reports success,
/// `verify` is clean against the newly blessed change, and only the next
/// connected plan proposes the declaration back. The pinned checksum does not
/// reach it: it is read before the statements, and this happens after.
///
/// Locking every managed object for the length of an apply is not on offer,
/// and moving the checksum later only moves the window. What is exact is the
/// half of the question the tool can answer with no dialect knowledge at all:
/// **for every object this plan does not touch, the state after is the state
/// before.** The objects it does touch are held by the plan's own
/// preconditions and postconditions (132, 136, 143) and by the locks its own
/// statements take (DECISIONS 150).
///
/// Both ends of a rename count as touched, since the same object is one name
/// before and another after; a created object is absent on one side and a
/// dropped one on the other, and both are named by the change that does it.
///
/// A touched *table* is not exempt down to its rows, though — only down to
/// the rows this plan names. An `AFTER` trigger on a declared table reaches
/// the table's *other* rows from inside the very statement that writes the
/// one the plan asked for, and the statement's own postcondition speaks for
/// that row alone (132, 136, 143). Exempting the whole table let the trigger's
/// collateral write be recorded as the plan's result (DECISIONS 153).
///
/// The table's *shape* stays exempt where the plan names it: a plan that
/// alters a column is meant to change the table, and a concurrent DDL on the
/// same table has to wait for the schema lock this plan's own statements
/// hold. Rows are what a trigger can move while the apply is running.
/// How much of the plan has run by the time the comparison is made.
///
/// The two halves of this check need different amounts of it. Asking whether
/// something the plan does *not* touch moved is fair at any point; asking
/// whether the plan got what it wanted is only fair once every statement has
/// run, and putting the whole plan's postconditions to a staged checkpoint
/// demanded changes that had not executed yet — the run then stopped at its
/// first checkpoint (DECISIONS 161).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Settled {
    /// Every statement of the plan has run.
    Whole,
    /// Every statement has run, *and* the read was taken from the plan's
    /// scope alone — no recorded row said how to spell a cell at its default,
    /// so one the engine confirmed at its default is omitted, and one that is
    /// there is not at it. That is the one read a cell the plan leaves to its
    /// default can be held to (DECISIONS 191). A checkpoint read is spelled
    /// against the checkpoint before it and cannot say the same.
    Closing,
    /// Some of them have. Only movement is comparable.
    SoFar,
}

impl Settled {
    /// Whether the plan's postconditions can be asked.
    pub(super) fn whole(self) -> bool {
        self != Settled::SoFar
    }
}

/// Whether two types are one type to the dialect.
///
/// SQL Server fills in a type's defaulted arguments, so a declared `decimal`
/// is stored `decimal(18,0)`, `char` as `char(1)`, `float` as `float(53)` and
/// `nvarchar` as `nvarchar(1)`; compared raw, a valid apply is refused.
/// `normalize_type` expands exactly those. A type it cannot normalize is one
/// nothing here can say anything about, and gets no answer rather than a wrong
/// one (DECISIONS 186).
pub(super) fn same_type(
    dialect: &dyn pbps_dialect::Dialect,
    a: &pbps_model::ColumnType,
    b: &pbps_model::ColumnType,
) -> bool {
    match (dialect.normalize_type(a), dialect.normalize_type(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => true,
    }
}

/// Whether an index read back is the one declared. Structure only: the
/// filter's text is the engine's to rewrite, and whether there is one at all
/// decides which rows the index covers, and is not (DECISIONS 185). The
/// access method is read back exactly, and a GIN index rebuilt as a B-tree
/// answers other queries (DEC-1169.1).
pub(super) fn index_as_declared(declared: &pbps_model::Index, now: &pbps_model::Index) -> bool {
    declared.columns.len() == now.columns.len()
        && declared
            .columns
            .iter()
            .zip(&now.columns)
            .all(|(d, n)| key_as_declared(d, n))
        && declared.include == now.include
        && declared.unique == now.unique
        && declared.filter.is_some() == now.filter.is_some()
        && declared.method == now.method
        // Both canonical (#1442), so compared outright: another session's
        // `ALTER INDEX … SET` after the creation is not this plan's (#1483
        // review).
        && declared.storage_parameters == now.storage_parameters
}

/// Whether an exclusion constraint read back is the one declared (DEC-2448.3):
/// the method, timing, payload, storage parameters, and each key's direction,
/// class and operator, with an expression by being one, as an index key is, and
/// the predicate by being there or not.
pub(super) fn exclusion_as_declared(
    declared: &pbps_model::ExclusionConstraint,
    now: &pbps_model::ExclusionConstraint,
) -> bool {
    declared.method == now.method
        && declared.timing == now.timing
        && declared.include == now.include
        && declared.filter.is_some() == now.filter.is_some()
        && declared.storage_parameters == now.storage_parameters
        && declared.keys.len() == now.keys.len()
        && declared.keys.iter().zip(&now.keys).all(|(d, n)| {
            key_as_declared(&d.key, &n.key)
                && d.key.collation == n.key.collation
                && d.operator == n.operator
        })
}

/// Whether an index key read back is the one declared: a column by its name,
/// and an expression by being one — its text is the engine's to respell, as a
/// filter's is (DEC-1169.2) — each with the same direction and class.
pub(super) fn key_as_declared(
    declared: &pbps_model::IndexColumn,
    now: &pbps_model::IndexColumn,
) -> bool {
    declared.descending == now.descending
        && declared.opclass == now.opclass
        && match (&declared.key, &now.key) {
            (pbps_model::IndexKey::Column(a), pbps_model::IndexKey::Column(b)) => a == b,
            (pbps_model::IndexKey::Expression(_), pbps_model::IndexKey::Expression(_)) => true,
            (pbps_model::IndexKey::Column(_), pbps_model::IndexKey::Expression(_))
            | (pbps_model::IndexKey::Expression(_), pbps_model::IndexKey::Column(_)) => false,
        }
}

/// Whether a primary key read back is the one declared. The name only where
/// the declaration gives one: `name: None` leaves it to the database, and the
/// engine's generated `PK__t__3213E83F` is not movement.
pub(super) fn primary_key_as_declared(
    declared: &pbps_model::PrimaryKey,
    now: &pbps_model::PrimaryKey,
) -> bool {
    declared.columns == now.columns
        && !declared
            .name
            .as_ref()
            .is_some_and(|n| Some(n) != now.name.as_ref())
        // As an index's (#1442, #1483 review).
        && declared.storage_parameters == now.storage_parameters
}

/// `schema` with each owned sequence's name set to the one it has
/// ([`pbps_dialect::Dialect::owned_sequence_name`]), so that two reads compare
/// the sequence and not the name its column's or table's name would generate.
pub(super) fn sequences_named<'a>(
    dialect: &dyn pbps_dialect::Dialect,
    schema: &'a Schema,
) -> std::borrow::Cow<'a, Schema> {
    let any = schema.tables.values().any(|t| {
        t.columns
            .values()
            .any(|c| matches!(c.value_source, pbps_model::ValueSource::Sequence(_)))
    });
    if !any {
        return std::borrow::Cow::Borrowed(schema);
    }
    let mut out = schema.clone();
    for (name, table) in &mut out.tables {
        for (column, spec) in &mut table.columns {
            if let pbps_model::ValueSource::Sequence(sequence) = &mut spec.value_source {
                sequence.name = dialect.owned_sequence_name(name, column.raw(), sequence);
            }
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
pub(super) fn refuse_unplanned_movement(
    dialect: &dyn pbps_dialect::Dialect,
    changes: &pbps_model::ChangeSet,
    before: &Schema,
    after: &Schema,
    label: &str,
    settled: Settled,
) -> anyhow::Result<()> {
    let ids = guard::fixture_ids(changes, before);
    let plan = guard::Plan { changes, ids: &ids };
    let read = match settled {
        // Final-shape fixtures use the transactional plan-scoped read.
        // Staged terminal-checkpoint policy is tested through staged_movement.
        Settled::Whole | Settled::Closing => guard::Read::transactional(plan),
        Settled::SoFar => guard::Staged::new(plan)?.read(StagedRead::Checkpoint {
            completed: 1,
            total: 2,
        }),
    };
    refuse_pinned_movement(dialect, read, before, after, label)
}

#[cfg(test)]
pub(super) fn staged_comparison(
    dialect: &dyn pbps_dialect::Dialect,
    changes: &pbps_model::ChangeSet,
    before: &Schema,
    after: &Schema,
    label: &str,
    settled: Settled,
) -> anyhow::Result<()> {
    let read = match settled {
        Settled::SoFar => StagedRead::Checkpoint {
            completed: 1,
            total: 2,
        },
        Settled::Whole => StagedRead::Checkpoint {
            completed: 2,
            total: 2,
        },
        Settled::Closing => StagedRead::Closing { total: 2 },
    };
    staged_movement(dialect, changes, before, after, label, None, read)
}

#[cfg(test)]
pub(super) fn staged_movement(
    dialect: &dyn pbps_dialect::Dialect,
    changes: &pbps_model::ChangeSet,
    before: &Schema,
    after: &Schema,
    label: &str,
    environment: Option<&str>,
    read: StagedRead,
) -> anyhow::Result<()> {
    let ids = guard::fixture_ids(changes, before);
    staged_pinned_movement(
        dialect,
        guard::Staged::new(guard::Plan { changes, ids: &ids })?,
        before,
        after,
        label,
        environment,
        read,
        None,
        None,
    )
}

/// Compares each kind the plan can change, by its own rule below. It never reads
/// `Schema::search`, deliberately (DEC-2855.1): a text search object has no plan, so
/// nothing this plan promised can move one, and what the read-back holds of one is not what
/// an apply records either (the previous record is carried forward,
/// `Schema::as_recorded_after_apply`). A search object changed during the run is therefore
/// neither blessed by the record nor a movement of the plan's; `verify` reports it against
/// the carried record. A kind that gains plans gains its rule here when it leaves
/// `NameOnly`.
#[allow(clippy::wildcard_enum_match_arm)] // Type deltas are checked apart from table/module effects.
pub(super) fn refuse_pinned_movement(
    dialect: &dyn pbps_dialect::Dialect,
    read: guard::Read<'_>,
    before: &Schema,
    after: &Schema,
    label: &str,
) -> anyhow::Result<()> {
    let verified = read.retained();
    let defaults = read.domain_defaults();
    let (plan, settled) = read.parts();
    let changes = plan.changes;
    let mut named_before = sequences_named(dialect, before);
    use pbps_diff::identity::type_refs::{TypeResolver, TypeSide, after_changes, before_changes};
    let base_ids = before_changes(plan.ids, changes);
    let physical_types = match defaults {
        Some(defaults) => std::borrow::Cow::Borrowed(defaults),
        None => std::borrow::Cow::Owned(pbps_pg::type_refs::declared::completed(
            before, &base_ids, changes,
        )),
    };
    let realized = |uid, definition: &pbps_model::ManagedType| {
        let mut expected = definition.clone();
        if let (
            pbps_model::ManagedType::Domain { default, .. },
            Some(pbps_model::ManagedType::Domain {
                default: physical, ..
            }),
        ) = (&mut expected, physical_types.get(uid))
        {
            *default = physical.clone();
        }
        expected
    };
    let desired_ids = after_changes(&base_ids, changes);
    let resolver = TypeResolver::new(&base_ids, &desired_ids);
    let renames = pbps_pg::type_refs::renames::TypeRenames::resolved(&base_ids, &desired_ids);
    let mut named_after = sequences_named(dialect, after);
    if !renames.0.is_empty() {
        renames.schema(named_before.to_mut());
        pbps_pg::type_refs::renames::TypeRenames::default().schema(named_after.to_mut());
    }
    if let Some(verified) = verified {
        verified.comparison(named_before.to_mut(), named_after.as_ref());
    }
    let after = named_after.as_ref();
    let before = named_before.as_ref();
    let mut expected_types = BTreeMap::new();
    for step in &changes.changes {
        match &step.change {
            pbps_model::Change::AddEnumValue {
                uid, definition, ..
            }
            | pbps_model::Change::AlterDomain {
                uid,
                to: definition,
                ..
            }
            | pbps_model::Change::RenameEnumValue {
                uid, definition, ..
            }
            | pbps_model::Change::CreateType {
                uid, definition, ..
            }
            | pbps_model::Change::RenameType {
                uid, definition, ..
            } => {
                if let Some(name) = resolver.name(TypeSide::Desired, uid) {
                    expected_types.insert(name.clone(), Some(realized(uid, definition)));
                }
            }
            // The replacement may stand anywhere between its creation and the
            // swap, and no longer after it; the swap leaves the final labels
            // under the type's own name (DEC-1635.7).
            pbps_model::Change::EnumRebuild { uid, tmp, step, .. } => {
                expected_types.entry(tmp.clone()).or_insert(None);
                if let pbps_model::EnumRebuildStep::Swap { definition, .. } = step
                    && let Some(name) = resolver.name(TypeSide::Desired, uid)
                {
                    expected_types.insert(name.clone(), Some(realized(uid, definition)));
                }
            }
            _ => {}
        }
    }
    // Retired names and intermediate addresses may be reused by another uid.
    // Only the final identity map decides which names must be absent.
    for step in &changes.changes {
        let old = match &step.change {
            pbps_model::Change::DropType { name, .. } => Some(name.clone()),
            pbps_model::Change::RenameType { from, to, .. } => {
                if let Ok(intermediate) = pbps_model::TypeRef::new(to.schema(), from.name()) {
                    expected_types.entry(intermediate).or_insert(None);
                }
                Some(from.clone())
            }
            _ => None,
        };
        if let Some(name) = old {
            expected_types.entry(name).or_insert(None);
        }
    }
    // Both reads are already scoped to held identities. A plan admits only
    // creates (DEC-2447.1, DEC-2447.6): each package it installs may appear
    // between its statement and the close, in exactly the pinned definition,
    // and every other package keeps its exact definition throughout.
    let installs: BTreeMap<&pbps_model::ObjectLocalName, &pbps_model::Extension> = changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            pbps_model::Change::CreateExtension {
                name, definition, ..
            } => Some((name, definition)),
            _ => None,
        })
        .collect();
    // The packages a drop removes: standing until its statement, absent from
    // it on, and in exactly the pinned definition while they stand.
    let removals: BTreeMap<&pbps_model::ObjectLocalName, &pbps_model::Extension> = changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            pbps_model::Change::DropExtension {
                name, definition, ..
            } => Some((name, definition)),
            _ => None,
        })
        .collect();
    // The packages an update or a move changes: at one of the definitions its
    // chain passes through while the plan runs, and at the end of it nowhere
    // else (DEC-2447.10, DEC-2447.11).
    let mut updates: BTreeMap<
        &pbps_model::ObjectLocalName,
        (Vec<&pbps_model::Extension>, &pbps_model::Extension),
    > = BTreeMap::new();
    for p in &changes.changes {
        if let pbps_model::Change::UpdateExtension { name, from, to, .. } = &p.change {
            let entry = updates.entry(name).or_insert((Vec::new(), to));
            entry.0.extend([from, to]);
        }
    }
    for p in &changes.changes {
        if let pbps_model::Change::SetExtensionSchema { name, from, to, .. } = &p.change {
            let entry = updates.entry(name).or_insert((Vec::new(), to));
            entry.0.extend([from, to]);
            entry.1 = to;
        }
    }
    let extensions: BTreeSet<_> = before
        .extensions
        .keys()
        .chain(after.extensions.keys())
        .chain(installs.keys().copied())
        .chain(removals.keys().copied())
        .chain(updates.keys().copied())
        .collect();
    for name in extensions {
        let (was, now) = (before.extensions.get(name), after.extensions.get(name));
        if let Some((passes, to)) = updates.get(name) {
            let stands =
                |held: Option<&pbps_model::Extension>| held.is_some_and(|d| passes.contains(&d));
            if !stands(was)
                || !stands(now)
                || (now != Some(*to) && !matches!(settled, Settled::SoFar))
            {
                bail!(
                    "extension `{}` does not match what its plan promised during {label}",
                    name.shown()
                )
            }
            continue;
        }
        if let Some(pinned) = removals.get(name) {
            if was.is_some_and(|was| was != *pinned)
                || now.is_some_and(|now| now != *pinned)
                || (now.is_some() && !matches!(settled, Settled::SoFar))
            {
                bail!(
                    "extension `{}` does not match what its plan promised during {label}",
                    name.shown()
                )
            }
            continue;
        }
        match installs.get(name) {
            None if was != now => bail!("extension `{}` changed during {label}", name.shown()),
            // A resumed run starts from a checkpoint that may already hold
            // the package, so `was` may be the pinned definition too.
            Some(wrote)
                if was.is_some_and(|was| was != *wrote)
                    || now.is_some_and(|now| now != *wrote)
                    || (now.is_none() && !matches!(settled, Settled::SoFar)) =>
            {
                bail!(
                    "extension `{}` does not match what its plan promised during {label}",
                    name.shown()
                )
            }
            _ => {}
        }
    }
    // A managed domain over a type a moved package provides reads back with
    // its base in the new schema and no change of the plan on it: the same
    // carrying as for a column of the table (DEC-2447.12).
    let package_moves = pbps_diff::package_moves(changes);
    let mut types_before = before.types.clone();
    pbps_diff::forward_domain_moves(&mut types_before, &after.types, &package_moves);
    let types: BTreeSet<_> = types_before.keys().chain(after.types.keys()).collect();
    for name in types {
        if types_before.get(name) != after.types.get(name) && !expected_types.contains_key(name) {
            bail!("type `{}` changed during {label}", name.shown());
        }
    }
    if !matches!(settled, Settled::SoFar) {
        for (name, expected) in &expected_types {
            let matches = match (expected, after.types.get(name)) {
                (Some(wrote), Some(now)) => {
                    pbps_pg::type_refs::declared::matches_declaration(wrote, now)
                }
                (None, None) => true,
                _ => false,
            };
            if !matches {
                bail!(
                    "type `{}` does not match what its plan promised during {label}",
                    name.shown()
                );
            }
        }
    }
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, plan.ids)?;
    // Each owned sequence under the name it has, before anything below maps
    // a column or table across a rename: a held name is `None` where it is
    // the generated one, and a rename changes that without touching the
    // sequence (#1631).
    let (before, after) = (&named_before, &named_after);
    let mut objects: BTreeSet<&TableName> = BTreeSet::new();
    // Modules are counted in their own set, under their own identity: a
    // module is no longer a name in the tables namespace (ADR-0009 §1), and
    // asking whether the plan touched `app.f` would exempt every overload of
    // it from the comparison below.
    let mut touched_modules: BTreeSet<&pbps_model::ModuleId> = BTreeSet::new();
    let mut roles: BTreeSet<&pbps_model::RoleName> = BTreeSet::new();
    for p in &changes.changes {
        objects.extend(p.change.objects());
        touched_modules.extend(p.change.module_id());
        roles.extend(p.change.roles());
    }
    let table_changes = guard::TableChanges::of(changes, &names);
    let column_changes = guard::ColumnChanges::of(changes, &names);
    let part_changes = guard::PartChanges::of(changes, &names);
    let created = guard::CreatedChanges::of(changes, &names, &table_changes, &part_changes);
    let implied = table_changes.implied(before, after, &objects);
    objects.extend(implied.names());

    // Row scope remains keyed by each statement's table spelling. The table
    // expectations below carry that scope to the surviving identity.
    let mut written: BTreeMap<&TableName, BTreeSet<&pbps_model::RowKey>> = BTreeMap::new();
    // The permissions this plan moves, keyed by the role it moves them on and
    // the target they sit on. A role can be both granted and revoked on one
    // target in one plan, so the sets are unioned rather than replaced.
    // What this plan leaves each role holding on each target it touches: the
    // permissions it adds and the ones it takes away, kept apart because the
    // comparison below reconstructs the set rather than excusing it.
    type Permissions = BTreeSet<pbps_model::Permission>;
    let mut granting: BTreeMap<(pbps_model::RoleName, pbps_model::GrantTarget), Permissions> =
        BTreeMap::new();
    let mut revoking: BTreeMap<(pbps_model::RoleName, pbps_model::GrantTarget), Permissions> =
        BTreeMap::new();
    // The columns this plan changes, under the table they are on, and the
    // objects it removes outright. Both are things the plan does to a
    // *container* that show up somewhere else without any change of its own
    // saying so: a renamed column re-keys every row of its table, and a
    // dropped table takes its grants with it (DECISIONS 158).
    let mut columns: BTreeMap<TableName, BTreeSet<pbps_model::ColumnName>> = BTreeMap::new();
    let mut gone: BTreeSet<pbps_model::Dropped> = BTreeSet::new();
    for p in &changes.changes {
        if let Some((table, key, _)) = p.change.row() {
            written.entry(table).or_default().insert(key);
        }
        if let Some((role, target, permissions)) = p.change.grant() {
            let (side, permissions) = match permissions {
                pbps_model::PermissionChange::Granted(p) => (&mut granting, p),
                pbps_model::PermissionChange::Revoked(p) => (&mut revoking, p),
            };
            let one = pbps_model::Role {
                grants: [(target.clone(), permissions.clone())].into(),
                ..Default::default()
            };
            // Statements carrying the marker operate on the sequences present
            // when they run; grants follow creations, revokes precede drops.
            let grant_tables = if matches!(p.change, pbps_model::Change::Grant { .. }) {
                &after.tables
            } else {
                &before.tables
            };
            for (target, permissions) in pbps_model::role::effective_grants(&one, grant_tables) {
                side.entry((role.clone(), target))
                    .or_default()
                    .extend(permissions);
            }
        }
        for column in p.change.columns() {
            columns.entry(column.table).or_default().insert(column.name);
        }
        if let Some(dropped) = p.change.drops() {
            gone.insert(dropped);
        }
    }

    let tables = guard::Tables::of(&names, before, after, &column_changes);
    let undo_before = &tables.undo_before;
    let undo_after = &tables.undo_after;
    let partition_hands = &column_changes.partition_handoffs;

    let mut moved = Vec::new();
    let named = |n: &TableName| objects.contains(n);
    // A child's foreign key follows the parent it references through
    // `sp_rename` — the referenced table and its columns are both rewritten by
    // the engine — so the child's shape moves with no change of this plan on
    // it and nothing to excuse it with.
    // The engine renames a label where a partition's bound holds it, so a
    // standing partition's bound follows this plan's label renames without a
    // change of its own (DEC-2346.1).
    let label_renames =
        pbps_diff::enum_labels::label_renames(changes.changes.iter().map(|p| &p.change));
    // The same for a column of a type a moved package provides: the type keeps
    // its OID and its name, and only its schema follows the package, so a table
    // this plan does not touch reads back with the new schema in its column
    // types and no change of the plan on it (DEC-2447.12).
    let label_forwarded = |table: &pbps_model::Table, now: Option<&pbps_model::Table>| {
        let mut table = table.clone();
        if let Some(now) = now {
            pbps_diff::forward_schema_moves(&mut table, now, &package_moves);
        }
        if let Some(of) = &mut table.partition_of
            && let Some(parent) = before.tables.get(&of.parent)
        {
            of.bound = pbps_diff::enum_labels::forwarded_bound(
                &of.bound,
                parent,
                (&resolver, TypeSide::Desired),
                &label_renames,
            );
        }
        // And so do its rows (DEC-2346.2).
        pbps_diff::enum_labels::forwarded_rows(
            &table,
            (&resolver, TypeSide::Desired),
            &label_renames,
        )
        .unwrap_or(table)
    };
    compare(
        "",
        &before.tables,
        &after.tables,
        named,
        |name: &TableName, was: &pbps_model::Table, now: &pbps_model::Table| {
            let read = undo_after.apply(now, name);
            label_forwarded(&undo_before.apply(was, name), Some(&read)) == *read
        },
        &mut moved,
        |n| n.shown().to_string(),
    );
    guard::implied(&implied, after, &mut moved);
    // Every touched table, under the name it ends with — including the ones
    // this plan creates, which have no `before` entry to be found under. A
    // loop over the baseline alone never visited a new table, so an
    // undeclared row that arrived in one (a DDL trigger, or another session
    // between a staged `CREATE TABLE` and its checkpoint) was recorded into an
    // `exact` snapshot and read as clean ever after (DECISIONS 163). A
    // created table's baseline is simply no rows, which is what it had.
    let carried: BTreeSet<&TableName> = before
        .tables
        .keys()
        .filter(|name| !tables.retired(name))
        .map(|name| tables.final_name(name))
        .collect();
    let no_rows = pbps_model::TableData {
        mode: pbps_model::DataMode::Exact,
        rows: Default::default(),
    };
    // The rows the engine renames a label in follow this plan's label renames
    // without a change of their own: the baseline's keys and cells are brought
    // forward as the differ brought them (DEC-2346.2).
    let forwarded_rows: BTreeMap<&TableName, pbps_model::TableData> = before
        .tables
        .iter()
        .filter_map(|(name, was)| {
            let forwarded = pbps_diff::enum_labels::forwarded_rows(
                was,
                (&resolver, TypeSide::Desired),
                &label_renames,
            )?;
            Some((name, forwarded.data?))
        })
        .collect();
    let touched_tables = before
        .tables
        .iter()
        .filter(|(name, _)| !tables.retired(name))
        .map(|(name, was)| {
            (
                name,
                forwarded_rows
                    .get(name)
                    .or_else(|| guard::Fields::of(was).data),
            )
        })
        .chain(
            after
                .tables
                .keys()
                .filter(|name| named(name) && !carried.contains(*name))
                .map(|name| (name, Some(&no_rows))),
        );
    for (name, was_rows) in touched_tables {
        if !named(name) {
            // Already compared whole, rows included.
            continue;
        }
        let now_name = tables.final_name(name);
        // The table's *shape*, entry by entry, over everything this plan does
        // not move. Its own columns and constraints are compared between two
        // read-backs — never against the declaration — so nothing here depends
        // on predicting the engine's stored form, which is why the whole table
        // could stop being exempt (DECISIONS 166). What the plan's own
        // alterations achieved is checked once every statement has run,
        // below: against the plan's own values, in the fields the catalog
        // reads back unchanged (DECISIONS 189).
        //
        // At every read, not only the settled one. Everything this plan will
        // move is excluded below whether or not its statement has run, so
        // nothing here needs the plan to be finished — and gating it on the
        // last read meant a change that landed before an earlier checkpoint
        // went into `previous`, after which the final comparison measured the
        // contaminated shape against itself (DECISIONS 167).
        // A table this plan creates has no `before` to be compared against, and
        // `CreateTable` names no column and no part of its own, so nothing
        // answered for its shape: a DDL trigger, or another session between a
        // staged `CREATE TABLE` and its checkpoint, could add a column or an
        // index to it and have that recorded as this plan's result. 163 gave
        // such a table a synthetic baseline of *no rows* for the same reason;
        // this is the other half of it (DECISIONS 181).
        //
        // Keep asking after the table enters the checkpoint: its constraints
        // and indexes can be separate later statements, replaced by a trigger
        // before their first read. Comparing only with that checkpoint would
        // certify the replacement against itself (decision 458). Comparable
        // structure is held to the declaration; engine-rewritten expressions
        // still use presence, as SPEC 7.6 requires.
        // Only at a read taken while statements remain: once every one has
        // run, an attached table that reads as an ordinary one is movement.
        let not_yet_attached = !settled.whole()
            && created.get(now_name).is_some_and(|table| table.attached())
            && after
                .tables
                .get(now_name)
                .is_some_and(|t| t.partition_of.is_none());
        if let (Some(declared), Some(now)) = (created.get(now_name), after.tables.get(now_name))
            && !not_yet_attached
        {
            declared.check(
                dialect,
                now_name,
                &guard::Fields::of(now),
                settled,
                &mut moved,
            );
        }
        // Held to its declared shape above: a detached table's partition held
        // nothing to compare it with, and an attached table's columns are its
        // parent's now (#1545).
        if created.get(now_name).is_some_and(|table| table.detached())
            || (created.get(now_name).is_some_and(|table| table.attached()) && !not_yet_attached)
        {
            continue;
        }
        if let (Some(was), Some(now)) = (before.tables.get(name), after.tables.get(now_name))
            && carried.contains(now_name)
        {
            // A rename carries the constraints that name the column or the
            // table with it — a primary key, a unique, an index's key and
            // `INCLUDE` columns, a child's `references_table` — so no change
            // of this plan restates them and there is nothing here to excuse
            // them with. The before side is brought forward to the spelling
            // the rename leaves, exactly as the differ does when it decides
            // not to emit the restatement in the first place. What may be
            // rewritten and what may not is `Renames::apply`'s business.
            // Both reads back in the spelling that precedes this plan, so
            // the constraints its renames carry compare equal without any
            // change of its own restating them.
            //
            // Both, not the later one alone: a staged run's closing check
            // compares two checkpoints, and by then the earlier of them
            // already carries the rename. Undoing is idempotent on a name the
            // plan does not rename, so a read from either side of a statement
            // lands in the same spelling.
            let was = &undo_before.apply(was, name);
            let was = &label_forwarded(was, after.tables.get(now_name));
            let now = &undo_after.apply(now, now_name);
            let expected_columns = column_changes.expect(now_name, was, now);
            let was_fields = guard::Fields::of(was);
            let now_fields = guard::Fields::of(now);
            guard::columns(&expected_columns, &now_fields, now_name, &mut moved);
            let pending = (settled != Settled::Closing)
                .then(|| created.get(now_name).map(|created| created.table()))
                .flatten();
            part_changes.kept(now_name, &was_fields, &now_fields, pending, &mut moved);
            let key_handoff = partition_hands.get(now_name);
            let scalar_expected =
                table_changes.expect(name, now_name, was, settled.whole(), key_handoff);
            let placed = if key_handoff.is_some() {
                after.tables.get(now_name).unwrap_or(now)
            } else {
                now
            };
            guard::table(
                &scalar_expected,
                &now_fields,
                &guard::Fields::of(placed),
                now_name,
                &mut moved,
            );
            let parent = was.partition_of.as_ref().and_then(|of| {
                use pbps_model::plan_names::{At, Holder};
                match names.holder(&of.parent, At::Start) {
                    Holder::Tracked(uid) => names.table_at(uid, At::End),
                    Holder::Unmanaged => Some(&of.parent),
                    Holder::Vacant => None,
                }
            });
            let own_handoff = parent.and_then(|parent| partition_hands.get(parent));
            let own_expected = table_changes.own(
                now_name,
                was,
                settled.whole(),
                guard::OwnNames {
                    parent,
                    handoff: own_handoff,
                    undo: undo_after,
                    index: &names,
                },
            );
            let own_now = if own_handoff.is_some() {
                after.tables.get(now_name).unwrap_or(now)
            } else {
                now
            };
            guard::own(
                &own_expected,
                &guard::Fields::of(own_now),
                now_name,
                &mut moved,
            );
            guard::system_time(&scalar_expected, &now_fields, now_name, &mut moved);
            part_changes.storage(
                now_name,
                &was_fields,
                &now_fields,
                settled.whole(),
                &mut moved,
            );
            part_changes.layout(now_name, &was_fields, &now_fields, &mut moved);
        }
        // Dropped, or outside the row scope on one side: there is no pair of
        // row sets to compare, and "absent" is not "empty".
        let (Some(was_rows), Some(now_rows)) = (
            was_rows,
            after
                .tables
                .get(now_name)
                .and_then(|t| guard::Fields::of(t).data),
        ) else {
            continue;
        };
        let empty = BTreeSet::new();
        let plans = written.get(now_name).unwrap_or(&empty);
        // A row is compared on the columns this plan leaves alone. One it
        // renames is under one name in the baseline and another in the
        // read-back; one it adds is in neither; one it retypes reads back in a
        // different rendering. All three are the plan's own doing, and
        // comparing the whole row called every one of them somebody else's
        // (DECISIONS 158).
        let no_columns = BTreeSet::new();
        let touched_columns = columns.get(now_name).unwrap_or(&no_columns);
        // A free function rather than a closure: the borrow of the row has to
        // outlive the call, and a closure cannot say so.
        fn held<'a>(
            row: &'a pbps_model::Row,
            skip: &BTreeSet<pbps_model::ColumnName>,
        ) -> BTreeMap<&'a pbps_model::ColumnName, &'a pbps_model::Value> {
            row.0
                .iter()
                .filter(|(column, _)| !skip.contains(*column))
                .collect()
        }
        for (key, row) in &was_rows.rows {
            if plans.contains(key) {
                continue;
            }
            match now_rows.rows.get(key) {
                Some(after_row)
                    if held(after_row, touched_columns) == held(row, touched_columns) => {}
                Some(_) => moved.push(format!(
                    "{now_name} row `{key}` is not what the plan was approved over, \
                     and no change of this plan writes it",
                    now_name = now_name.shown()
                )),
                None => moved.push(format!(
                    "{now_name} row `{key}` is gone, and no change of this plan deletes it",
                    now_name = now_name.shown()
                )),
            }
        }
        for key in now_rows.rows.keys() {
            if plans.contains(key) || was_rows.rows.contains_key(key) {
                continue;
            }
            moved.push(format!(
                "{now_name} row `{key}` is there, and no change of this plan inserts it",
                now_name = now_name.shown()
            ));
        }
    }
    compare(
        "",
        &before.modules,
        &after.modules,
        |id: &pbps_model::ModuleId| touched_modules.contains(id),
        |_: &pbps_model::ModuleId, was: &pbps_model::Module, now: &pbps_model::Module| was == now,
        &mut moved,
        |id| id.shown().to_string(),
    );
    // A module the plan names is held to the definition the plan wrote, not
    // exempted. `CREATE OR ALTER` reports success and says nothing about what
    // is now stored, and no statement carries a postcondition for one — so a
    // session, or a DDL trigger inside the statement itself, that altered or
    // dropped the module straight afterwards was read back and recorded as
    // this plan's own result (DECISIONS 160).
    // By identity, not by change: a module that changes kind is a
    // `DropModule` *and* a `CreateModule` for one identity (`diff_modules`).
    // Checked separately the drop always failed — the create had put the
    // module back — and every replacement was refused (DECISIONS 161). The
    // plan is in `order_key` order, which puts the drop first, so the last
    // word on an identity is the net one. A trigger moved to another table is
    // no longer such a pair: it is two identities, one dropped and one
    // created, and each answers for itself.
    let mut modules_after_plan: BTreeMap<&pbps_model::ModuleId, pbps_model::ModuleAfter<'_>> =
        BTreeMap::new();
    for p in &changes.changes {
        if let Some((name, expected)) = p.change.module() {
            modules_after_plan.insert(name, expected);
        }
    }
    for (name, expected) in modules_after_plan {
        if settled == Settled::SoFar {
            break;
        }
        match (expected, after.modules.get(name)) {
            (pbps_model::ModuleAfter::Standing(wrote), Some(now))
                if dialect.module_matches_declaration(wrote, now) => {}
            (pbps_model::ModuleAfter::Standing(_), Some(_)) => moved.push(format!(
                "{name} does not hold the definition this plan wrote",
                name = name.shown()
            )),
            (pbps_model::ModuleAfter::Standing(_), None) => moved.push(format!(
                "{name} is not there, and this plan writes it",
                name = name.shown()
            )),
            (pbps_model::ModuleAfter::Gone, None) => {}
            (pbps_model::ModuleAfter::Gone, Some(_)) => moved.push(format!(
                "{name} is still there, and this plan drops it",
                name = name.shown()
            )),
        }
    }

    // The rows the plan writes, held to being there or gone once every
    // statement has run. Their *contents* are held by the statement itself
    // (132, 136, 143) — but only until it commits. In one transaction that is
    // the whole story, because the row stays locked until the commit and
    // nothing else can reach it; a staged run commits each statement, and a
    // row it inserted can be deleted before the checkpoint read
    // (DECISIONS 162).
    if settled.whole() {
        for p in &changes.changes {
            let Some((table, key, expected)) = p.change.row() else {
                continue;
            };
            // Out of the read's row scope, or the table itself is gone: the
            // table checks below answer for that, and "absent" is not "empty".
            let Some(rows) = after.tables.get(table).and_then(|t| t.data.as_ref()) else {
                continue;
            };
            match (expected, rows.rows.get(key)) {
                // Present, and holding what the plan spelled. Only the cells
                // it spells, and only where the read-back carries them: a cell
                // at its column's default is omitted from the read-back, so
                // its absence proves nothing and demanding it would refuse a
                // valid apply (DECISIONS 165).
                (pbps_model::RowAfter::Holding(cells), Some(row)) => {
                    for (column, wrote) in cells {
                        match (wrote, row.get(column.raw())) {
                            (pbps_model::CellAfter::Spelled(wrote), Some(now)) if now != wrote => {
                                moved.push(format!(
                                    "{table} row `{key}` does not hold in `{column}` what this \
                                     plan wrote",
                                    table = table.shown(),
                                    column = column.shown(),
                                ));
                            }
                            (pbps_model::CellAfter::Spelled(_), _) => {}
                            // A cell the plan leaves to its default is held
                            // there only where a present cell *means* not at
                            // it: at the closing read, whose spelling omits a
                            // confirmed default, and on a column whose default
                            // the engine confirms at all. A `NEWID()` cell
                            // comes back with its value on every read, and a
                            // checkpoint read spells a cell the way the
                            // checkpoint before it did (DECISIONS 191).
                            (pbps_model::CellAfter::AtDefault, Some(_))
                                if settled == Settled::Closing
                                    && after
                                        .tables
                                        .get(table)
                                        .and_then(|t| t.columns.get(column))
                                        .is_some_and(|c| dialect.reads_back_at_default(c)) =>
                            {
                                moved.push(format!(
                                    "{table} row `{key}` holds a value in `{column}`, and this \
                                     plan leaves it at its default",
                                    table = table.shown(),
                                    column = column.shown(),
                                ));
                            }
                            (pbps_model::CellAfter::AtDefault, _) => {}
                        }
                    }
                }
                (pbps_model::RowAfter::Holding(_), None) => moved.push(format!(
                    "{table} row `{key}` is not there, and this plan writes it",
                    table = table.shown()
                )),
                (pbps_model::RowAfter::Gone, Some(_)) => moved.push(format!(
                    "{table} row `{key}` is still there, and this plan deletes it",
                    table = table.shown()
                )),
                (pbps_model::RowAfter::Gone, None) => {}
            }
        }
    }

    tables.kept(after, &mut moved);

    // And the names the plan leaves standing or empty. Existence, for the
    // tables and roles themselves: a table's shape is answered for above,
    // entry by entry, and below for the entries this plan writes. A role
    // with no grants has nothing *but* its name, so without this a
    // `CREATE ROLE` another session undid was recorded as success
    // (DECISIONS 161).
    if settled.whole() {
        let mut expected_roles: BTreeMap<&pbps_model::RoleName, pbps_model::Presence> =
            BTreeMap::new();
        for p in &changes.changes {
            expected_roles.extend(p.change.roles_after());
        }
        tables.after(after, &mut moved);
        guard::columns_after(dialect, &column_changes, after, &mut moved);
        part_changes.after(after, &mut moved);
        for (name, expected) in expected_roles {
            match (expected, after.roles.contains_key(name)) {
                (pbps_model::Presence::Present, false) => moved.push(format!(
                    "role {name} is not there, and this plan creates it",
                    name = name.shown(),
                )),
                (pbps_model::Presence::Absent, true) => moved.push(format!(
                    "role {name} is still there, and this plan removes it",
                    name = name.shown(),
                )),
                _ => {}
            }
        }
    }
    // The baseline's grants, spelled the way the read-back will spell them.
    // Measured: SQL Server carries a grant across `sp_rename`, so a plan that
    // renames a granted table changes no permission and the differ emits no
    // grant change at all — the role is untouched. Compared by name alone,
    // the one grant read as two and every such rename was refused
    // (DECISIONS 157).
    let before_roles: BTreeMap<pbps_model::RoleName, pbps_model::Role> = before
        .roles
        .iter()
        .map(|(name, role)| {
            let effective = pbps_model::role::effective_grants(role, &before.tables);
            let grants = effective
                .iter()
                // A grant on an object this plan drops goes with it: the engine
                // removes a permission with its securable, so `diff_roles`
                // emits no `REVOKE` and there is nothing left for the read-back
                // to hold (DECISIONS 158).
                // By whole identity: dropping one overload leaves the
                // sibling's grants standing, and standing grants are compared.
                .filter(|(target, _)| !gone.iter().any(|d| d.takes(target)))
                .filter(|(target, _)| {
                    let pbps_model::GrantTarget::Sequence(column) = target else {
                        return true;
                    };
                    use pbps_model::plan_names::{At, Holder};
                    let Holder::Tracked(column_uid) = names.column_holder(column, At::Start) else {
                        return true;
                    };
                    // The baseline uses starting names; a drop or numbering
                    // change uses the names at its step. Only the UID survives
                    // both renames and another column taking the vacated name.
                    !changes.changes.iter().any(|p| {
                        if let pbps_model::Change::DropColumn { uid, .. } = &p.change {
                            return uid == column_uid;
                        }
                        if let pbps_model::Change::AlterColumnNumbering { uid, to, .. } = &p.change
                        {
                            return uid == column_uid
                                && !matches!(to, pbps_model::ValueSource::Sequence(_));
                        }
                        false
                    })
                })
                .map(|(target, held)| {
                    let target = match target {
                        pbps_model::GrantTarget::Object(object) => {
                            pbps_model::GrantTarget::Object(tables.final_name(object).clone())
                        }
                        // Only tables are renamed, and a routine is not a
                        // table — as `diff_roles` says. Where routines have a
                        // namespace of their own, `app.f(integer)` stands
                        // beside a table `app.f`, and forwarding the routine's
                        // grant through the table's rename refused the apply
                        // for a grant that had, rightly, not moved.
                        pbps_model::GrantTarget::Sequence(column) => {
                            use pbps_model::plan_names::{At, Holder};
                            let final_column = match names.column_holder(column, At::Start) {
                                Holder::Tracked(uid) => names
                                    .column_at(uid, At::End)
                                    .and_then(|(table, name)| {
                                        names
                                            .table_at(table, At::End)
                                            .map(|table| table.column(name))
                                    })
                                    .unwrap_or_else(|| column.clone()),
                                Holder::Vacant | Holder::Unmanaged => column.clone(),
                            };
                            pbps_model::GrantTarget::Sequence(final_column)
                        }
                        routine @ pbps_model::GrantTarget::Routine(_) => routine.clone(),
                        schema @ pbps_model::GrantTarget::Schema(_) => schema.clone(),
                    };
                    (target, held.clone())
                })
                .collect();
            (
                name.clone(),
                pbps_model::Role {
                    description: role.description.clone(),
                    grants,
                },
            )
        })
        .collect();
    let after_roles: BTreeMap<_, _> = after
        .roles
        .iter()
        .map(|(name, role)| {
            (
                name.clone(),
                pbps_model::Role {
                    description: role.description.clone(),
                    grants: pbps_model::role::effective_grants(role, &after.tables),
                },
            )
        })
        .collect();
    let named_role = |n: &pbps_model::RoleName| roles.contains(n);
    compare(
        "role ",
        &before_roles,
        &after_roles,
        named_role,
        |_: &pbps_model::RoleName, was: &pbps_model::Role, now: &pbps_model::Role| was == now,
        &mut moved,
        |name| name.shown().to_string(),
    );
    // And the same for a role the plan does name: exempt down to the
    // permissions it moves, and no further. A plan that adds one grant is not
    // answerable for the rest of the role's set, and nothing else in the run
    // speaks for them — the statements grant and revoke what they were asked
    // to and say nothing about what they left alone (DECISIONS 156).
    // Created roles included, with no grants before them — for the reason the
    // tables above are: a loop over the baseline never visits a name that was
    // not there, so a grant that arrived on a role this plan had just created
    // was checked by nothing at all (DECISIONS 163). Its existence alone was.
    use pbps_model::plan_names::{At, Holder};
    let carried: Vec<_> = before_roles
        .iter()
        .filter_map(|(name, was)| {
            let now_name = match names.role_holder(name, At::Start) {
                Holder::Tracked(uid) => match names.role_at(uid, At::End) {
                    Some(to) => to,
                    // The retired principal's grants are not the incoming
                    // holder's baseline. An unrefilled name stays compared:
                    // staged membership removal can leave DROP ROLE pending.
                    None if matches!(names.role_holder(name, At::End), Holder::Tracked(_)) => {
                        return None;
                    }
                    None => name,
                },
                // A staged closing baseline may already use the final name.
                // Its observed grants are still held, not reset to empty.
                Holder::Vacant | Holder::Unmanaged => name,
            };
            Some((now_name, was))
        })
        .collect();
    // Keep the pairs, not a map keyed by their destination: two observed
    // baseline names must not silently overwrite one grant comparison.
    let carried_roles: BTreeSet<&pbps_model::RoleName> =
        carried.iter().map(|(name, _)| *name).collect();
    let no_grants = pbps_model::Role {
        description: None,
        grants: BTreeMap::new(),
    };
    let touched_roles = carried.iter().copied().chain(
        after_roles
            .keys()
            .filter(|name| named_role(name) && !carried_roles.contains(*name))
            .map(|name| (name, &no_grants)),
    );
    for (now_name, was) in touched_roles {
        if !roles.contains(now_name) {
            // Already compared whole, grants included.
            continue;
        }
        // Every pair already uses its destination. A new role under a vacated
        // source name must not follow the old holder's rename (DEC-1988.1).
        let Some(now) = after_roles.get(now_name) else {
            // Created or dropped: named by the change that does it, and there
            // is no pair of grant sets to compare.
            continue;
        };
        let empty = Permissions::new();
        // Every target either side holds permissions on, *and* every target
        // this plan names. A plan that adds the first permission a role has on
        // a target puts it in neither set when the grant is reversed before
        // the read — so the one thing being checked was the one thing the loop
        // never visited (DECISIONS 162).
        let targets: BTreeSet<_> = was
            .grants
            .keys()
            .chain(now.grants.keys())
            .chain(
                granting
                    .keys()
                    .chain(revoking.keys())
                    .filter(|(role, _)| role == now_name)
                    .map(|(_, target)| target),
            )
            .collect();
        for target in targets {
            // What the plan says the role will hold here: what it held, less
            // what this plan revokes, plus what it grants. Excusing the moved
            // permissions from both sides instead left the plan's own grant
            // checked by nothing — no statement has a postcondition on a
            // permission, so a session (or a DDL trigger) that reversed it
            // straight away was recorded as the plan's result (DECISIONS 160).
            let mut expected: Permissions = was.grants.get(target).cloned().unwrap_or_default();
            let mut held = now.grants.get(target).cloned().unwrap_or_default();
            let adds = granting
                .get(&(now_name.clone(), target.clone()))
                .unwrap_or(&empty);
            let removes = revoking
                .get(&(now_name.clone(), target.clone()))
                .unwrap_or(&empty);
            match settled {
                // Every statement has run, so what the plan asked for is part
                // of the answer.
                Settled::Whole | Settled::Closing => {
                    for revoked in removes {
                        expected.remove(revoked);
                    }
                    expected.extend(adds);
                }
                // Mid-run: the grant may simply not have happened yet, so only
                // the permissions this plan does not move can be compared.
                Settled::SoFar => {
                    expected.retain(|p| !adds.contains(p) && !removes.contains(p));
                    held.retain(|p| !adds.contains(p) && !removes.contains(p));
                }
            }
            if expected != held {
                // Which permission, both ways: a revoke that did not take and a
                // grant that was taken away read the same without it (#700).
                let listed = |set: &Permissions| {
                    set.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let mut detail = Vec::new();
                // A surplus the plan revoked is a revoke that did not take;
                // any other was neither held nor granted by the plan, so
                // another session put it there, and "still" would misdescribe
                // it (#1052).
                let (survived, arrived): (Permissions, Permissions) = held
                    .difference(&expected)
                    .cloned()
                    .partition(|p| removes.contains(p));
                let surplus = listed(&survived);
                if !surplus.is_empty() {
                    detail.push(format!("it still holds {surplus}"));
                }
                let unplanned = listed(&arrived);
                if !unplanned.is_empty() {
                    detail.push(format!("it unexpectedly holds {unplanned}"));
                }
                let missing = listed(&expected.difference(&held).cloned().collect());
                if !missing.is_empty() {
                    detail.push(format!("it lacks {missing}"));
                }
                moved.push(format!(
                    "role {now_name} does not hold on {target} what this plan leaves it holding: {}",
                    detail.join("; "),
                    now_name = now_name.shown(),
                    target = target.shown(),
                ));
            }
        }
    }
    if moved.is_empty() {
        return Ok(());
    }
    // The finding and the reason, and no remedy: what to do about it depends
    // on the caller. Inside a transaction nothing has been applied and the
    // plan can simply be run again; a staged run has committed, and whether
    // a `--resume` can go on depends on which read found the change. Naming
    // the transactional remedy here put "the transaction was rolled back"
    // into every staged refusal, one line above "nothing was rolled back"
    // (DECISIONS 190).
    bail!(
        "`{label}` moved while this plan was running, and not because of it:\n  {}\n\
         The state `apply` records is the database read back, so another session's change \
         committed during the run would have been written down as this plan's own result, \
         and every later `verify` would call it clean.",
        moved.join("\n  "),
        label = pbps_model::escape::Escaped(label),
    );
}

/// One kind of named thing on a table — its unique constraints, foreign keys,
/// checks or indexes — compared by definition over the names the plan leaves
/// alone.
///
/// A free function because it is generic over the value, which a closure
/// cannot be, and because comparing by name alone was the bug: a constraint
/// dropped and recreated under one name with a different body is present on
/// both sides (DECISIONS 167).
/// One namespace of the two states, compared over the names the plan leaves
/// alone. Generic over the key so tables, modules and roles get the same
/// answer from the same code: three near-identical loops is three places for
/// one of them to stop being written.
pub(super) fn compare<K, V>(
    kind: &str,
    before: &BTreeMap<K, V>,
    after: &BTreeMap<K, V>,
    touched: impl Fn(&K) -> bool,
    // Equality, supplied because one namespace's is not plain `==`: a table's
    // constraints follow this plan's renames without a change of its own, and
    // in a staged run a checkpoint falls between the renames, so both
    // spellings have to be accepted. Modules and roles pass `==`.
    unchanged: impl Fn(&K, &V, &V) -> bool,
    moved: &mut Vec<String>,
    shown: impl Fn(&K) -> String,
) where
    K: Ord,
{
    for (name, was) in before {
        if touched(name) {
            continue;
        }
        let display = shown(name);
        match after.get(name) {
            Some(now) if unchanged(name, was, now) => {}
            Some(_) => moved.push(format!(
                "{kind}{display} is not what the plan was approved over"
            )),
            None => moved.push(format!(
                "{kind}{display} is gone, and no change of this plan drops it"
            )),
        }
    }
    for name in after.keys() {
        if touched(name) || before.contains_key(name) {
            continue;
        }
        let display = shown(name);
        moved.push(format!(
            "{kind}{display} is there, and no change of this plan creates it"
        ));
    }
}
