use pbps_model::operation::{Origin, OwnerError, Owners};
use pbps_model::part_effects::{DefaultChange, Effect, Part as DependencyPart, PartEffect};
use pbps_model::resolver::{OrderEdge, OrderReason, Surface, SurfaceResolution};
use pbps_model::{Change, ChangeSet, GrantTarget, TableName, Uid, ValueSource};
use std::collections::BTreeSet;

// Model facts classify every Change kind. This consumer selects the binding
// surfaces it provides; scheduling and containment remain graph policy.
#[allow(clippy::wildcard_enum_match_arm)]
fn provides(parts: &[PartEffect<'_>]) -> BTreeSet<Surface> {
    parts
        .iter()
        .filter_map(|fact| match (&fact.part, fact.effect) {
            (DependencyPart::Table(table), Effect::Created) => {
                Some(Surface::Table((*table).clone()))
            }
            (DependencyPart::Column(column), Effect::Created) => {
                Some(Surface::Column(column.clone()))
            }
            (
                DependencyPart::Default { column, .. } | DependencyPart::Generated(column),
                Effect::Created | Effect::Rewritten,
            ) => Some(Surface::Default(column.clone())),
            (DependencyPart::Module { id, .. }, Effect::Created | Effect::Rewritten) => {
                Some(Surface::Module((*id).clone()))
            }
            (
                DependencyPart::Named {
                    table,
                    kind: pbps_model::Part::Check,
                    name,
                },
                Effect::Created,
            ) => Some(Surface::Check {
                table: (*table).clone(),
                name: (*name).to_owned(),
            }),
            (
                DependencyPart::Named {
                    table,
                    kind: pbps_model::Part::Index,
                    name,
                },
                Effect::Created,
            ) => Some(Surface::Index {
                table: (*table).clone(),
                name: (*name).to_owned(),
            }),
            _ => None,
        })
        .collect()
}

// Ordinary default rewrites have already been split by prepare. A partition
// can release its own binding while retaining an inherited default; neither
// question is simply whether the effective expression disappears (DEC-1773.1).
#[allow(clippy::wildcard_enum_match_arm)]
fn release(parts: &[PartEffect<'_>], surface: &Surface) -> bool {
    parts
        .iter()
        .any(|fact| match (&fact.part, fact.effect, surface) {
            (DependencyPart::Module { id, .. }, Effect::Removed, Surface::Module(module)) => {
                *id == module
            }
            (
                DependencyPart::Default {
                    column,
                    change: DefaultChange::Column { to: None, .. },
                },
                _,
                Surface::Default(target),
            )
            | (
                DependencyPart::Default {
                    column,
                    change:
                        DefaultChange::Partition {
                            from: Some(_),
                            to: None,
                            ..
                        },
                },
                _,
                Surface::Default(target),
            )
            | (DependencyPart::Column(column), Effect::Removed, Surface::Default(target))
            | (DependencyPart::Generated(column), Effect::Rewritten, Surface::Default(target)) => {
                column == target
            }
            (
                DependencyPart::Named {
                    table,
                    kind: pbps_model::Part::Check,
                    name,
                },
                Effect::Removed,
                Surface::Check {
                    table: target,
                    name: target_name,
                },
            )
            | (
                DependencyPart::Named {
                    table,
                    kind: pbps_model::Part::Index,
                    name,
                },
                Effect::Removed,
                Surface::Index {
                    table: target,
                    name: target_name,
                },
            ) => *table == target && *name == target_name,
            (DependencyPart::Table(table), Effect::Removed, surface) => {
                table_of(surface).is_some_and(|target| *table == target)
            }
            _ => false,
        })
}

/// Whether a step writes a column's `pg_attrdef` expression in its own DDL:
/// an added column's default or generation expression, a new table's
/// generation expression, or a rewritten one. A generated column computes
/// its rows as the step runs, so a routine it calls must be executable then.
#[allow(clippy::wildcard_enum_match_arm)]
fn writes_attrdef(c: &Change) -> bool {
    match c {
        Change::AddColumn { column, .. } => match column.value_source {
            ValueSource::Default(_) | ValueSource::Generated(_) => true,
            ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => false,
        },
        Change::CreateTable { table, .. } => table.columns.values().any(|c| match c.value_source {
            ValueSource::Generated(_) => true,
            ValueSource::None
            | ValueSource::Default(_)
            | ValueSource::Identity(_)
            | ValueSource::Sequence(_) => false,
        }),
        Change::AlterColumnExpression { .. } => true,
        _ => false,
    }
}

fn table_of(s: &Surface) -> Option<&TableName> {
    match s {
        Surface::Table(t) | Surface::Check { table: t, .. } | Surface::Index { table: t, .. } => {
            Some(t)
        }
        Surface::Column(c) | Surface::Default(c) => Some(&c.table),
        Surface::Namespace(_) | Surface::Module(_) => None,
    }
}

#[allow(clippy::wildcard_enum_match_arm)]
fn expression(c: &Change) -> Option<bool> {
    match c {
        Change::AlterColumnDefault { to, .. } => Some(to.is_some()),
        // A partition's default is installed or released as a column's is,
        // so the rebuild's pair takes the same order (#1610).
        Change::SetPartitionDefault { to, fallback, .. } => {
            Some(to.is_some() || fallback.is_some())
        }
        Change::AddCheck { .. } | Change::AddIndex { .. } => Some(true),
        Change::DropCheck { .. } | Change::DropIndex { .. } => Some(false),
        _ => None,
    }
}

/// [`constraints`] over a plan that is its own logical plan, each step
/// whole: the shape a test builds by hand.
#[cfg(test)]
pub(super) fn constraints_of_logical(
    changes: &ChangeSet,
    observations: &[SurfaceResolution],
    annotations: &pbps_model::ModuleDeps,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
) -> BTreeSet<OrderEdge> {
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, desired.ids)
        .expect("a hand-built test plan is an executable order");
    let owners = pbps_model::operation::PlanOwners::new(names, base.ids, desired.ids);
    let origins: Vec<_> = (0..changes.changes.len()).map(Origin::Whole).collect();
    constraints(
        changes,
        changes.changes.len(),
        &origins,
        &owners,
        observations,
        annotations,
        base,
        desired,
    )
}

#[allow(clippy::wildcard_enum_match_arm, clippy::too_many_arguments)]
pub(super) fn constraints(
    changes: &ChangeSet,
    ordinary: usize,
    origins: &[Origin],
    table_owners: &dyn Owners,
    observations: &[SurfaceResolution],
    annotations: &pbps_model::ModuleDeps,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
) -> BTreeSet<OrderEdge> {
    // Asked once per step and observation: indexed once (#1870).
    let base_names = base.ids.index();
    let desired_names = desired.ids.index();
    let steps: Vec<_> = changes.changes.iter().map(|p| &p.change).collect();
    // These are the prepared steps, after splitting and expansion. Borrow
    // this slice locally so facts cannot outlive a change of its order.
    let parts: Vec<_> = steps.iter().map(|c| super::part_effects(c)).collect();
    let made: Vec<_> = steps
        .iter()
        .zip(&parts)
        .map(|(c, facts)| {
            let mut surfaces = provides(facts);
            match c {
                Change::RenameTable { to, .. } => {
                    surfaces.insert(Surface::Table(to.clone()));
                    if let Some(table) = desired.schema.tables.get(to) {
                        surfaces
                            .extend(table.columns.keys().map(|n| Surface::Column(to.column(n))));
                    }
                }
                Change::RenameColumn { table, to, .. } => {
                    surfaces.insert(Surface::Column(table.column(to)));
                }
                Change::AlterColumnType { column, .. } => {
                    surfaces.insert(Surface::Column(column.clone()));
                }
                _ => {}
            }
            surfaces
        })
        .collect();
    // The table each step changes, by recorded UID, read from where the step
    // came from: the ordinary plan's names in force at its position, a
    // teardown's base names, a restoration's final ones (DEC-1498.1). The
    // graph keeps no history of its own: the logical plan is ordered and
    // indexed once, and these steps may be split, appended or reordered
    // (#1871). A dropped table's name taken by a renamed one is two tables.
    // A name no managed table holds there leaves the owner unknown, which
    // keeps every edge a shared name would draw.
    debug_assert_eq!(origins.len(), steps.len());
    let owners: Vec<Option<Uid>> = steps
        .iter()
        .zip(origins)
        .map(|(c, origin)| match c {
            Change::CreateTable { uid, .. }
            | Change::DropTable { uid, .. }
            | Change::RenameTable { uid, .. } => Some(uid.clone()),
            other => other
                .table()
                .and_then(|t| match table_owners.table(t, origin.spelling()) {
                    Ok(uid) => Some(uid),
                    Err(OwnerError::Unmanaged { .. } | OwnerError::Vacant { .. }) => None,
                    Err(e @ OwnerError::Outside { .. }) => {
                        unreachable!("a prepared step outside its logical plan: {e}")
                    }
                }),
        })
        .collect();
    // Same name is not enough when both UIDs are known: only a false edge
    // between two tables that share a name in turn is ever dropped.
    let same_table = |i: usize, j: usize| match (&owners[i], &owners[j]) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    let mut edges = BTreeSet::new();
    let mut edge = |before, after, reason| {
        edges.insert(OrderEdge {
            before,
            after,
            reason,
        });
    };

    // Retain ordinary ordering only where there is an actual structural,
    // identity or data relation. A global chain would invent a cycle when an
    // ADD COLUMN DEFAULT needs a routine over a later, independent table.
    for (i, change) in steps.iter().enumerate() {
        for (j, other) in steps.iter().enumerate().skip(i + 1) {
            if expression(change).is_none()
                && expression(other).is_none()
                && change.objects().any(|a| other.objects().any(|b| a == b))
            {
                edge(i, j, OrderReason::Identity);
            }
            if row(change) && row(other) {
                edge(i, j, OrderReason::Data);
            }
            if role_identity(change).is_some() && role_identity(change) == role_identity(other) {
                edge(i, j, OrderReason::Identity);
            }
            // A view and a table can occupy the same engine namespace. The
            // ordinary differ has already decided whether a drop precedes a
            // creation; binding edges do not erase that identity constraint.
            if matches!(change, Change::DropModule { .. } | Change::DropTable { .. })
                && matches!(
                    other,
                    Change::CreateModule { .. } | Change::CreateTable { .. }
                )
                && change.object() == other.object()
            {
                edge(i, j, OrderReason::Identity);
            }
        }
    }
    for (i, change) in steps.iter().enumerate() {
        let reference = match change {
            Change::AddForeignKey { constraint, .. } => {
                Some((constraint.references_table.clone(), true))
            }
            Change::DropForeignKey { table, name } => base
                .schema
                .tables
                .get(&desired_names.resolved_in(table, base.ids))
                .and_then(|t| t.foreign_keys.get(name))
                .map(|f| (f.references_table.clone(), false)),
            _ => None,
        };
        if let Some((table, adding)) = reference {
            let final_name = base_names.resolved_in(&table, desired.ids);
            for (j, other) in steps.iter().enumerate() {
                if i != j
                    && other
                        .objects()
                        .any(|t| t == &table || (!adding && t == &final_name))
                    && (adding || !matches!(other, Change::RenameTable { .. }))
                    && expression(other).is_none()
                    && !matches!(
                        other,
                        Change::AddForeignKey { .. } | Change::DropForeignKey { .. }
                    )
                {
                    if adding {
                        edge(j, i, OrderReason::Structural);
                    } else {
                        edge(i, j, OrderReason::Structural);
                    }
                }
            }
        }
    }

    // A partitioned parent's new index before each partition's own: built
    // after one of its shape, it would take it as its clone (DEC-1688.1). The
    // ordinary plan's (13, 0) rank is no edge, and an index the parent waits
    // for a column to build could otherwise follow the partition's (#1737
    // review).
    for (i, change) in steps.iter().enumerate() {
        let Change::AddIndex { table: parent, .. } = change else {
            continue;
        };
        for (j, other) in steps.iter().enumerate() {
            if let Change::AddIndex { table, .. } = other
                && desired
                    .schema
                    .tables
                    .get(table)
                    .and_then(|t| t.partition_of.as_ref())
                    .is_some_and(|of| of.parent == *parent)
            {
                edge(i, j, OrderReason::Structural);
            }
        }
    }

    // A parent's default change before its partitions' own on that column:
    // the engine recurses the parent's `SET DEFAULT` or `DROP DEFAULT` into
    // every partition, over the partition's own default, so the partition's
    // must come after it to stay (DEC-1687.1, docs/ORDERING.md). The ordinary
    // plan's (9, 4) rank is no edge here (#1610). Only what sets the
    // partition's default waits: its release has nothing to keep, and when
    // one routine both defaults call is rebuilt, the release must precede
    // the routine's drop, which precedes the parent's restoration (#1610
    // review).
    for (i, change) in steps.iter().enumerate() {
        let Change::AlterColumnDefault { column, .. } = change else {
            continue;
        };
        for (j, other) in steps.iter().enumerate() {
            if let Change::SetPartitionDefault {
                parent,
                column: c,
                to,
                fallback,
                ..
            } = other
                && (to.is_some() || fallback.is_some())
                && *parent == column.table
                && *c == column.name
            {
                edge(i, j, OrderReason::Structural);
            }
        }
    }

    // A partition's own default spelled by a column name its parent's
    // rename moves. A partition declares no column, so the parent's rename is
    // the one that moves the name, and the rules below that read a column's
    // own rename by its UID do not see it (#1610 review). Only a release the
    // resolver appended is spelled by the base name, so it goes before the
    // parent's rename away from that name; every other change, the ordinary
    // plan's included, is spelled by the name the column has once the
    // renames have run, so it goes after the rename into that name. The
    // parent is matched by its table UID, which a rename of the parent
    // itself does not change.
    for (i, change) in steps.iter().enumerate() {
        let Change::SetPartitionDefault {
            parent,
            column,
            to,
            fallback,
            ..
        } = change
        else {
            continue;
        };
        let base_spelled = i >= ordinary && to.is_none() && fallback.is_none();
        let parent_uid = if base_spelled {
            base_names.table_uid(parent)
        } else {
            desired_names
                .table_uid(parent)
                .or_else(|| base_names.table_uid(parent))
        };
        if parent_uid.is_none() {
            continue;
        }
        for (j, other) in steps.iter().enumerate() {
            if let Change::RenameColumn { from, to, .. } = other
                && owners[j].as_ref() == parent_uid
            {
                if base_spelled && from == column {
                    edge(i, j, OrderReason::Identity);
                } else if !base_spelled && to == column {
                    edge(j, i, OrderReason::Identity);
                }
            }
        }
    }

    // A default change spelled by its own column's final name: where the
    // plan renames that column, by its UID, into the name the change uses.
    let own_rename: Vec<Option<usize>> = steps
        .iter()
        .map(|c| match c {
            Change::AlterColumnDefault { uid, column, .. } => steps.iter().position(|r| {
                matches!(r, Change::RenameColumn { uid: renamed, table, to, .. }
                    if renamed == uid && *table == column.table && *to == column.name)
            }),
            _ => None,
        })
        .collect();
    for (i, change) in steps.iter().enumerate() {
        for (j, other) in steps.iter().enumerate() {
            if i == j {
                continue;
            }
            if expression(change).is_some()
                && let Change::RenameTable { to, .. } = other
                && change.table() == Some(to)
                && same_table(i, j)
            {
                edge(j, i, OrderReason::Identity);
            }
            // A default spelled by its column's final name follows every
            // column rename of its table: the name is the one the column has
            // once they have all run, as an expression change follows its
            // table's rename and as the ordinary plan orders `RenameColumn`
            // before later changes naming the column (docs/ORDERING.md;
            // #1292). Whose rename frees that name is not read from names: a
            // chain may fold case, as the differ's rename order does. The
            // removal-first rule below would address the name before the
            // column holds it. A teardown spelled by the base name keeps it.
            let renamed_into = own_rename[i].is_some()
                && matches!(other, Change::RenameColumn { table, .. }
                    if change.table() == Some(table))
                && same_table(i, j);
            // What the differ placed ahead of that rename, such as a drop
            // that frees the name, runs before the name exists, so it does
            // not wait for a removal that needs the name.
            let ahead_of_the_name = own_rename[i].is_some_and(|rename| j < rename);
            if renamed_into {
                edge(j, i, OrderReason::Identity);
            }
            if let (Some(t), Some(u), Some(install)) =
                (change.table(), other.table(), expression(change))
                && t == u
                && same_table(i, j)
                && !renamed_into
                && !ahead_of_the_name
                && expression(other).is_none()
                // A replica identity follows the index it names and goes
                // ahead of the old identity's index's drop, as the differ
                // ordered it (DEC-1444.1); this rule would put it before
                // every add and after every drop of its table's indexes.
                && !matches!(other, Change::SetReplicaIdentity { .. })
            {
                if row(other) {
                    // A DEFAULT is consumed by INSERT/UPDATE. CHECKs and
                    // indexes keep the differ's data-before-validation order.
                    if install
                        && matches!(
                            change,
                            Change::AlterColumnDefault { .. } | Change::SetPartitionDefault { .. }
                        )
                    {
                        edge(i, j, OrderReason::Data);
                    } else if install {
                        edge(j, i, OrderReason::Data);
                    } else {
                        edge(i, j, OrderReason::Data);
                    }
                } else if install {
                    edge(j, i, OrderReason::Structural);
                } else if !matches!(other, Change::CreateTable { .. }) {
                    edge(i, j, OrderReason::Structural);
                }
            }
            if let (Some(false), Some(true)) = (expression(change), expression(other))
                && change.table() == other.table()
                && same_table(i, j)
            {
                edge(i, j, OrderReason::Restoration);
            }
            // Grant targets and principals must exist. Restore grants before
            // row operations can evaluate a routine in its new incarnation.
            if let Change::Grant { role, target, .. } | Change::Revoke { role, target, .. } = other
                && (creates_role(change, role) || creates_target(change, target))
            {
                edge(i, j, OrderReason::Authorization);
            }
            if matches!(
                change,
                Change::Grant {
                    target: GrantTarget::Schema(_),
                    ..
                } | Change::Revoke {
                    target: GrantTarget::Schema(_),
                    ..
                }
            ) && (!made[j].is_empty() || row(other))
            {
                edge(i, j, OrderReason::Authorization);
            }
            if matches!(
                change,
                Change::Grant { .. } | Change::PublicExecution { .. }
            ) && row(other)
            {
                edge(i, j, OrderReason::Authorization);
            }
            let target = match change {
                Change::Grant { target, .. } => Some(target.clone()),
                Change::PublicExecution { routine, .. } => {
                    Some(GrantTarget::Routine(routine.clone()))
                }
                _ => None,
            };
            if let Some(target) = target
                && (expression(other) == Some(true) || writes_attrdef(other))
                && (observations.iter().any(|o| {
                    made[j].contains(&o.surface)
                        && o.desired.as_ref().is_some_and(|d| {
                            d.managed_inputs
                                .iter()
                                .any(|input| grant_matches(&target, input))
                        })
                }) || (matches!(change, Change::Grant { .. })
                    && !steps.iter().any(|c| creates_target(c, &target))))
            {
                edge(i, j, OrderReason::Authorization);
            }
            if let Change::PublicExecution { routine, .. } = other
                && change
                    .module_id()
                    .is_some_and(|id| id == &pbps_model::ModuleId::Routine(routine.clone()))
                && matches!(change, Change::CreateModule { .. })
            {
                edge(i, j, OrderReason::Restoration);
            }
        }
    }
    for (dependent, inputs) in annotations {
        for input in inputs {
            for (i, change) in steps.iter().enumerate() {
                for (j, other) in steps.iter().enumerate() {
                    if change.module_id() == Some(input) && other.module_id() == Some(dependent) {
                        if matches!(
                            change,
                            Change::CreateModule { .. } | Change::AlterModule { .. }
                        ) && matches!(
                            other,
                            Change::CreateModule { .. } | Change::AlterModule { .. }
                        ) {
                            edge(i, j, OrderReason::Structural);
                        }
                        if matches!(change, Change::DropModule { .. })
                            && matches!(other, Change::DropModule { .. })
                        {
                            edge(j, i, OrderReason::Structural);
                        }
                    }
                }
            }
        }
    }
    // An observation names its opening record by the base spelling when the
    // base holds the surface, and otherwise by the final one, as coverage
    // does. A step releases or makes it only if it changes that table, by
    // UID: a dropped table and the one that takes its name are two owners,
    // and the renamed table's removal is spelled by its final name
    // (DEC-1498.1).
    let opening = super::prepare::surfaces(base.schema);
    let owned = |i: usize, owner: Option<&Uid>| match (&owners[i], owner) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    for observation in observations {
        let surface = &observation.surface;
        let table = table_of(surface);
        let base_owner = table.and_then(|t| {
            if opening.contains(surface) {
                base_names.table_uid(t)
            } else {
                desired_names.table_uid(t)
            }
        });
        let final_spelling = super::prepare::forward(surface, &base_names, desired.ids);
        let removes: Vec<_> = steps
            .iter()
            .enumerate()
            .filter(|&(i, _)| {
                owned(i, base_owner)
                    && (release(&parts[i], surface)
                        || (&final_spelling != surface
                            && owners[i].is_some()
                            && base_owner.is_some()
                            && release(&parts[i], &final_spelling)))
            })
            .map(|(i, _)| i)
            .collect();
        let desired_owner = table.and_then(|t| desired_names.table_uid(t));
        let creates: Vec<_> = made
            .iter()
            .enumerate()
            .filter(|&(i, s)| s.contains(surface) && owned(i, desired_owner))
            .map(|(i, _)| i)
            .collect();
        for &drop in &removes {
            // A rewritten expression both lets go of its old binding and
            // installs its new one: no step precedes itself.
            for &create in creates.iter().filter(|&&create| create != drop) {
                edge(drop, create, OrderReason::Restoration);
            }
        }
        if let Some(current) = &observation.current {
            for input in &current.managed_inputs {
                for (i, facts) in parts.iter().enumerate() {
                    if super::prepare::invalidates(facts, input) {
                        for &drop in &removes {
                            if drop != i {
                                edge(drop, i, OrderReason::Binding);
                            }
                        }
                    }
                }
            }
        }
        if let Some(desired) = &observation.desired {
            for input in &desired.managed_inputs {
                for (i, surfaces) in made.iter().enumerate() {
                    if surfaces.contains(input) {
                        // A table's generation expression reads columns the
                        // same CREATE TABLE makes: no step precedes itself.
                        for &create in creates.iter().filter(|&&create| create != i) {
                            edge(i, create, OrderReason::Binding);
                        }
                    }
                }
            }
        }
    }
    edges
}

fn role_identity(c: &Change) -> Option<&pbps_model::Uid> {
    if let Change::CreateRole { uid, .. }
    | Change::DropRole { uid, .. }
    | Change::RenameRole { uid, .. } = c
    {
        Some(uid)
    } else {
        None
    }
}
fn row(c: &Change) -> bool {
    matches!(
        c,
        Change::InsertRow { .. } | Change::UpdateRow { .. } | Change::DeleteRow { .. }
    )
}
fn creates_role(c: &Change, role: &str) -> bool {
    matches!(c, Change::CreateRole { name, .. } if name == role)
        || matches!(c, Change::RenameRole { to, .. } if to == role)
}
#[allow(clippy::wildcard_enum_match_arm)]
fn creates_target(c: &Change, target: &GrantTarget) -> bool {
    match target {
        GrantTarget::Object(object) => match c {
            Change::CreateTable { name, .. } => object == name,
            Change::RenameTable { to, .. } => object == to,
            Change::CreateModule { id, .. } => {
                id.schema() == object.schema && id.name() == object.name
            }
            _ => false,
        },
        GrantTarget::Routine(routine) => {
            matches!(c, Change::CreateModule { id, .. } if id == &pbps_model::ModuleId::Routine(routine.clone()))
        }
        GrantTarget::Schema(_) => false,
    }
}

#[allow(clippy::wildcard_enum_match_arm)]
fn grant_matches(target: &GrantTarget, surface: &Surface) -> bool {
    match (target, surface) {
        (GrantTarget::Object(a), Surface::Table(b)) => a == b,
        (GrantTarget::Object(a), Surface::Module(pbps_model::ModuleId::Named(b))) => a == b,
        (GrantTarget::Routine(a), Surface::Module(pbps_model::ModuleId::Routine(b))) => a == b,
        _ => false,
    }
}

#[test]
fn a_prepared_default_rewrite_releases_then_provides_its_binding() {
    let column = "app.t.n".parse::<pbps_model::ColumnRef>().unwrap();
    let change = |from: Option<&str>, to: Option<&str>| Change::AlterColumnDefault {
        uid: "c_aaaaaa".parse().unwrap(),
        column: column.clone(),
        from: from.map(str::to_owned),
        to: to.map(str::to_owned),
    };
    let target = Surface::Default(column.clone());
    let rewrite = change(Some("app.old()"), Some("app.new()"));
    assert!(
        !release(&rewrite.part_effects(), &target),
        "release consumes the prepared split, not the unsplit edit"
    );
    assert!(provides(&rewrite.part_effects()).contains(&target));
    let drop = change(Some("app.old()"), None);
    let create = change(None, Some("app.new()"));
    assert!(release(&drop.part_effects(), &target));
    assert!(!provides(&drop.part_effects()).contains(&target));
    assert!(provides(&create.part_effects()).contains(&target));
    assert!(!release(&create.part_effects(), &target));
}
