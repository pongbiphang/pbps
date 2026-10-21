use crate::order::{Constraint, Source};
use pbps_model::operation::{Origin, OwnerError, Owners};
use pbps_model::part_effects::{DefaultChange, Effect, Part as DependencyPart, PartEffect};
use pbps_model::resolver::{OrderReason, Surface, SurfaceResolution};
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

/// Whether a step installs an expression a routine may be called from.
#[allow(clippy::wildcard_enum_match_arm)]
fn installs_expression(c: &Change) -> bool {
    match c {
        Change::AlterColumnDefault { to, .. } => to.is_some(),
        Change::SetPartitionDefault { to, fallback, .. } => to.is_some() || fallback.is_some(),
        Change::AddCheck { .. } | Change::AddIndex { .. } => true,
        _ => false,
    }
}

/// The constraints only the resolver's evidence states, over the prepared
/// steps: a surface's bindings, and what the principal must hold when the
/// steps that make them run. Everything else the effects and the name slots
/// order (DEC-1968.2).
#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn constraints(
    changes: &ChangeSet,
    origins: &[Origin],
    table_owners: &dyn Owners,
    observations: &[SurfaceResolution],
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
) -> BTreeSet<Constraint> {
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
        .enumerate()
        .map(|(at, (c, facts))| {
            let mut surfaces = provides(facts);
            match c {
                Change::CreateTable {
                    uid, name, table, ..
                } if desired_names.table_uid(name) == Some(uid) => {
                    // A placeholder creates an attrdef, but not its desired
                    // binding. That observation belongs to the installation
                    // from the same logical CREATE (DEC-1503.5).
                    for (column, spec) in &table.columns {
                        let ValueSource::Generated(current) = &spec.value_source else {
                            continue;
                        };
                        let reference = name.column(column);
                        let Some(ValueSource::Generated(wanted)) = desired
                            .schema
                            .tables
                            .get(name)
                            .and_then(|t| t.columns.get(column))
                            .map(|c| &c.value_source)
                        else {
                            continue;
                        };
                        if current != wanted && steps.iter().zip(origins).any(|(step, origin)| {
                            matches!(step, Change::AlterColumnExpression { uid, column, from, to }
                                if column == &reference
                                    && desired_names.column_uid(column) == Some(uid)
                                    && from == &current.expression && to == &wanted.expression
                                    && origin.source() == origins[at].source())
                        }) {
                            surfaces.remove(&Surface::Default(reference));
                        }
                    }
                }
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
    let mut edges = BTreeSet::new();
    let mut state = |before, after, source, reason| {
        edges.insert(Constraint {
            before,
            after,
            source,
            reason,
        });
    };

    // What the executing principal must hold when a step runs is not an
    // effect: a grant restored before the binding DDL and the rows that call
    // what it grants (`Execution` in DEC-1954.7). The evidence names the
    // surfaces those steps make, so it is stated here with the bindings.
    for (i, change) in steps.iter().enumerate() {
        let schema_grant = matches!(
            change,
            Change::Grant {
                target: GrantTarget::Schema(_),
                ..
            } | Change::Revoke {
                target: GrantTarget::Schema(_),
                ..
            }
        );
        let target = match change {
            Change::Grant { target, .. } => Some(target.clone()),
            Change::PublicExecution { routine, .. } => Some(GrantTarget::Routine(routine.clone())),
            _ => None,
        };
        if !schema_grant && target.is_none() {
            continue;
        }
        for (j, other) in steps.iter().enumerate() {
            if i == j {
                continue;
            }
            if schema_grant && (!made[j].is_empty() || row(other)) {
                state(i, j, Source::Execution, OrderReason::Authorization);
            }
            if target.is_some() && row(other) {
                state(i, j, Source::Execution, OrderReason::Authorization);
            }
            if let Some(target) = &target
                && (installs_expression(other) || writes_attrdef(other))
                && (observations.iter().any(|o| {
                    made[j].contains(&o.surface)
                        && o.desired.as_ref().is_some_and(|d| {
                            d.managed_inputs
                                .iter()
                                .any(|input| grant_matches(target, input))
                        })
                }) || (matches!(change, Change::Grant { .. })
                    && !steps.iter().any(|c| creates_target(c, target))))
            {
                state(i, j, Source::Execution, OrderReason::Authorization);
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
        if let Some(current) = &observation.current {
            for input in &current.managed_inputs {
                for (i, facts) in parts.iter().enumerate() {
                    if super::prepare::invalidates(facts, input) {
                        for &drop in &removes {
                            if drop != i {
                                state(drop, i, Source::Binding, OrderReason::Binding);
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
                            state(i, create, Source::Binding, OrderReason::Binding);
                        }
                    }
                }
            }
        }
    }
    edges
}

fn row(c: &Change) -> bool {
    matches!(
        c,
        Change::InsertRow { .. } | Change::UpdateRow { .. } | Change::DeleteRow { .. }
    )
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
        GrantTarget::Sequence(column) => {
            matches!(c, Change::CreateTable { name, .. } | Change::RenameTable { to: name, .. } if name == &column.table)
                || matches!(c, Change::RenameColumn { table, to, .. } if table == &column.table && to == &column.name)
                || matches!(c, Change::AddColumn { table, name, .. } if table == &column.table && name == &column.name)
                || matches!(c, Change::AlterColumnNumbering { column: changed, .. } if changed == column)
        }
        GrantTarget::Schema(_) => false,
    }
}

#[allow(clippy::wildcard_enum_match_arm)]
fn grant_matches(target: &GrantTarget, surface: &Surface) -> bool {
    match (target, surface) {
        (GrantTarget::Object(a), Surface::Table(b)) => a == b,
        (GrantTarget::Sequence(c), Surface::Table(b)) => &c.table == b,
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

#[test]
fn sequence_grants_follow_the_existing_table_and_column_rename_dependencies() {
    let target = "sequence::app.new.new_id".parse().unwrap();
    let table_rename = Change::RenameTable {
        uid: "t_aaaaaa".parse().unwrap(),
        from: "app.old".parse().unwrap(),
        to: "app.new".parse().unwrap(),
        defaults: Vec::new(),
    };
    let column_rename = Change::RenameColumn {
        uid: "c_aaaaaa".parse().unwrap(),
        table: "app.new".parse().unwrap(),
        from: "old_id".into(),
        to: "new_id".into(),
        table_was: None,
    };
    assert!(creates_target(&table_rename, &target));
    assert!(creates_target(&column_rename, &target));
    assert!(!creates_target(
        &column_rename,
        &"sequence::app.new.other".parse().unwrap()
    ));
    assert!(!creates_target(
        &table_rename,
        &"sequence::app.unrelated.new_id".parse().unwrap()
    ));
    assert!(grant_matches(
        &target,
        &Surface::Table("app.new".parse().unwrap())
    ));
    assert!(!grant_matches(
        &target,
        &Surface::Table("app.other".parse().unwrap())
    ));
}
