use pbps_model::resolver::{OrderEdge, OrderReason, Surface, SurfaceResolution};
use pbps_model::{Change, ChangeSet, GrantTarget, TableName};
use std::collections::BTreeSet;

// This is a projection onto the binding-bearing effects, not an alternative
// exhaustive definition of Change. All other changes retain ordinary order.
#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn provides(c: &Change) -> BTreeSet<Surface> {
    match c {
        Change::CreateTable { name, table, .. } => std::iter::once(Surface::Table(name.clone()))
            .chain(
                table
                    .columns
                    .keys()
                    .map(|n| Surface::Column(name.column(n))),
            )
            .collect(),
        Change::AddColumn {
            table,
            name,
            column,
            ..
        } => {
            let mut set = BTreeSet::from([Surface::Column(table.column(name))]);
            if column.default.is_some() {
                set.insert(Surface::Default(table.column(name)));
            }
            set
        }
        Change::AlterColumnDefault {
            column,
            to: Some(_),
            ..
        } => BTreeSet::from([Surface::Default(column.clone())]),
        Change::CreateModule { id, .. } | Change::AlterModule { id, .. } => {
            BTreeSet::from([Surface::Module(id.clone())])
        }
        Change::AddCheck { table, name, .. } => BTreeSet::from([Surface::Check {
            table: table.clone(),
            name: name.clone(),
        }]),
        Change::AddIndex { table, name, .. } => BTreeSet::from([Surface::Index {
            table: table.clone(),
            name: name.clone(),
        }]),
        _ => BTreeSet::new(),
    }
}

#[allow(clippy::wildcard_enum_match_arm)]
fn release(c: &Change, surface: &Surface) -> bool {
    match (c, surface) {
        (Change::DropModule { id, .. }, Surface::Module(m)) => id == m,
        (
            Change::AlterColumnDefault {
                column, to: None, ..
            },
            Surface::Default(r),
        ) => column == r,
        (Change::DropColumn { column, .. }, Surface::Default(r)) => column == r,
        (Change::DropCheck { table, name }, Surface::Check { table: t, name: n })
        | (Change::DropIndex { table, name }, Surface::Index { table: t, name: n }) => {
            table == t && name == n
        }
        (Change::DropTable { name, .. }, surface) => table_of(surface).is_some_and(|t| t == name),
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
        Change::AddCheck { .. } | Change::AddIndex { .. } => Some(true),
        Change::DropCheck { .. } | Change::DropIndex { .. } => Some(false),
        _ => None,
    }
}

#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn constraints(
    changes: &ChangeSet,
    observations: &[SurfaceResolution],
    annotations: &pbps_model::ModuleDeps,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
) -> BTreeSet<OrderEdge> {
    let steps: Vec<_> = changes.changes.iter().map(|p| &p.change).collect();
    let made: Vec<_> = steps
        .iter()
        .map(|c| {
            let mut surfaces = provides(c);
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
                .get(&desired.ids.resolved_in(table, base.ids))
                .and_then(|t| t.foreign_keys.get(name))
                .map(|f| (f.references_table.clone(), false)),
            _ => None,
        };
        if let Some((table, adding)) = reference {
            let final_name = base.ids.resolved_in(&table, desired.ids);
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

    for (i, change) in steps.iter().enumerate() {
        for (j, other) in steps.iter().enumerate() {
            if i == j {
                continue;
            }
            if expression(change).is_some()
                && let Change::RenameTable { to, .. } = other
                && change.table() == Some(to)
            {
                edge(j, i, OrderReason::Identity);
            }
            if let (Some(t), Some(u), Some(install)) =
                (change.table(), other.table(), expression(change))
                && t == u
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
                    if install && matches!(change, Change::AlterColumnDefault { .. }) {
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
                && (expression(other) == Some(true)
                    || matches!(other, Change::AddColumn { column, .. } if column.default.is_some()))
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
    for observation in observations {
        let removes: Vec<_> = steps
            .iter()
            .enumerate()
            .filter(|(_, c)| release(c, &observation.surface))
            .map(|(i, _)| i)
            .collect();
        let creates: Vec<_> = made
            .iter()
            .enumerate()
            .filter(|(_, s)| s.contains(&observation.surface))
            .map(|(i, _)| i)
            .collect();
        for &drop in &removes {
            for &create in &creates {
                edge(drop, create, OrderReason::Restoration);
            }
        }
        if let Some(current) = &observation.current {
            for input in &current.managed_inputs {
                for (i, change) in steps.iter().enumerate() {
                    if super::prepare::invalidates(change, input) {
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
                        for &create in &creates {
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
