//! Typed creation parts for a new table, shared by ordinary planning,
//! resolver planning and scratch (DEC-1503.1). This module emits no SQL.

use std::collections::BTreeSet;

use super::{Constraint, Schedule, Source};
use pbps_dialect::Dialect;
use pbps_model::resolver::OrderReason;
use pbps_model::{
    AddedRole, Change, ChangeSet, IdsFile, ModuleDeps, ModuleId, ModuleKind, PlannedChange,
    Provenance, RiskClass, Schema, ValueSource,
};

pub const RULE: &str = "generated-new-table";

#[allow(clippy::wildcard_enum_match_arm)]
pub fn functions(changes: &[PlannedChange]) -> Vec<ModuleId> {
    changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::CreateModule { id, module } | Change::AlterModule { id, module }
                if module.kind == ModuleKind::Function =>
            {
                Some(id.clone())
            }
            _ => None,
        })
        .collect()
}

pub fn is_part(p: &PlannedChange) -> bool {
    p.provenance
        .as_ref()
        .is_some_and(|origin| origin.rule == RULE)
}

pub fn parts(
    planned: &PlannedChange,
    ids: &IdsFile,
    functions: &[ModuleId],
    dialect: &dyn Dialect,
) -> Result<Option<Vec<PlannedChange>>, String> {
    let Some(parts) = dialect
        .generated_table_parts(&planned.change, ids, functions)
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    Ok(Some(
        parts
            .into_iter()
            .map(|change| {
                let mut part = planned.clone();
                // These finish CREATE on a new relation, before its seed
                // rows. Reclassifying them as standing-table alterations
                // would invent approvals that CREATE did not need
                // (DEC-1503.3).
                // CREATE builds its indexes transactionally on an empty
                // relation. Splitting must not turn an inherited hint into
                // concurrent DDL (SPEC 9.3.2).
                if matches!(change, Change::AddIndex { .. }) {
                    part.strategy = Default::default();
                }
                part.change = change;
                part.provenance = Some(Provenance {
                    rule: RULE,
                    role: AddedRole::SplitPart,
                    source: planned.change.subject(),
                    reason: "the relation exists before its generated expression binds".into(),
                });
                part
            })
            .collect(),
    ))
}

/// Derive creation-part risks from typed ownership, never diagnostic provenance.
/// Generated-column installations identify this family after serialization;
/// an earlier CREATE of that identity proves its context (DEC-1503.3).
pub fn risks(cs: &ChangeSet, ids: &IdsFile, dialect: &dyn Dialect) -> Vec<BTreeSet<RiskClass>> {
    use pbps_model::plan_names::{At, Holder, PlanNames};

    let names = cs
        .changes
        .iter()
        .any(|p| matches!(p.change, Change::CreateTable { .. }))
        .then(|| PlanNames::of(&cs.changes, ids).ok())
        .flatten();
    let installations: BTreeSet<_> = cs
        .changes
        .iter()
        .enumerate()
        .filter_map(|(at, p)| {
            let installation = matches!(p.change, Change::AlterColumnExpression { .. })
                || matches!(&p.change, Change::AddColumn { column, .. }
                if matches!(column.value_source, ValueSource::Generated(_)));
            if !installation {
                return None;
            }
            match names.as_ref()?.holder(p.change.table()?, At::Before(at)) {
                Holder::Tracked(owner) => Some(owner.clone()),
                Holder::Unmanaged | Holder::Vacant => None,
            }
        })
        .collect();
    cs.changes
        .iter()
        .enumerate()
        .map(|(at, part)| {
            let source = || {
                if !matches!(
                    &part.change,
                    Change::AddColumn { .. }
                        | Change::AlterColumnExpression { .. }
                        | Change::AlterColumnDefault { from: None, .. }
                        | Change::SetPrimaryKey {
                            from: None,
                            to: Some(_),
                            ..
                        }
                        | Change::AddUnique { .. }
                        | Change::AddForeignKey { .. }
                        | Change::AddCheck { .. }
                        | Change::AddIndex { .. }
                        | Change::SetReplicaIdentity { .. }
                ) {
                    return None;
                }
                let Holder::Tracked(owner) =
                    names.as_ref()?.holder(part.change.table()?, At::Before(at))
                else {
                    return None;
                };
                if !installations.contains(owner) {
                    return None;
                }
                cs.changes[..at].iter().find_map(|p| {
                    matches!(&p.change, Change::CreateTable {
                        uid, table, beside_default: None, ..
                    } if uid == owner && table.partition_of.is_none())
                    .then_some(&p.change)
                })
            };
            dialect.change_risks(source().unwrap_or(&part.change))
        })
        .collect()
}

pub fn expand(cs: &mut ChangeSet, ids: &IdsFile, dialect: &dyn Dialect) -> Result<usize, String> {
    let functions = functions(&cs.changes);
    let mut expanded = Vec::new();
    let mut added = 0;
    for p in &cs.changes {
        match parts(p, ids, &functions, dialect)? {
            Some(parts) => {
                added += parts.len() - 1;
                expanded.extend(parts);
            }
            None => expanded.push(p.clone()),
        }
    }
    cs.changes = expanded;
    Ok(added)
}

fn texts(change: &Change) -> Vec<&str> {
    fn column(c: &pbps_model::Column) -> Option<&str> {
        match &c.value_source {
            ValueSource::Default(t) => Some(t.as_str()),
            ValueSource::Generated(g) => Some(g.expression.as_str()),
            ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => None,
        }
    }
    match change {
        Change::CreateTable { table, .. } => table
            .columns
            .values()
            .filter_map(column)
            .chain(table.checks.values().map(|c| c.expression.as_str()))
            .chain(
                table
                    .partition_of
                    .iter()
                    .flat_map(|of| of.columns.values())
                    .filter_map(|c| c.default.as_deref()),
            )
            .chain(table.indexes.values().flat_map(|ix| {
                ix.filter
                    .as_deref()
                    .into_iter()
                    .chain(ix.columns.iter().filter_map(|c| c.key.expression()))
            }))
            .collect(),
        Change::AddColumn { column: c, .. } => column(c).into_iter().collect(),
        Change::AlterColumnExpression { to, .. } => vec![to],
        Change::AlterColumnDefault { to, .. } | Change::SetPartitionDefault { to, .. } => {
            to.as_deref().into_iter().collect()
        }
        Change::AddCheck { constraint, .. } => vec![&constraint.expression],
        Change::AddIndex { index, .. } => index
            .filter
            .as_deref()
            .into_iter()
            .chain(index.columns.iter().filter_map(|c| c.key.expression()))
            .collect(),
        Change::DropTable { .. }
        | Change::DetachPartition { .. }
        | Change::AttachPartition { .. }
        | Change::RenameTable { .. }
        | Change::DropColumn { .. }
        | Change::RenameColumn { .. }
        | Change::AlterColumnType { .. }
        | Change::AlterColumnNullability { .. }
        | Change::AlterColumnNumbering { .. }
        | Change::AddComputedColumn { .. }
        | Change::DropComputedColumn { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetReplicaIdentity { .. }
        | Change::SetStorageParameters { .. }
        | Change::SetTablePersistence { .. }
        | Change::SetPartitionNotNull { .. }
        | Change::SetIndexStorageParameters { .. }
        | Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::DropUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::DropForeignKey { .. }
        | Change::DropCheck { .. }
        | Change::DropIndex { .. }
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::SetEnsureKeys { .. }
        | Change::CreateModule { .. }
        | Change::AlterModule { .. }
        | Change::DropModule { .. }
        | Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. }
        | Change::PublicExecution { .. } => Vec::new(),
    }
}

/// Constraints on the parts, not a second ordering algorithm. The caller
/// includes these in the schedule it already settles.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn constraints(
    cs: &ChangeSet,
    declared: &Schema,
    deps: &ModuleDeps,
    dialect: &dyn Dialect,
) -> Vec<Constraint> {
    let mut out = Schedule::new(cs.changes.len());
    let edge = |out: &mut Schedule, a, b| {
        if a != b {
            out.edge(a, b, Source::Lexical, OrderReason::Structural);
        }
    };
    let modules: Vec<_> = cs
        .changes
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match &p.change {
            Change::CreateModule { id, module } | Change::AlterModule { id, module } => {
                Some((i, id, module))
            }
            _ => None,
        })
        .collect();
    for (first, p) in cs.changes.iter().enumerate().filter(|(_, p)| is_part(p)) {
        let Change::CreateTable {
            name,
            table: prefix,
            ..
        } = &p.change
        else {
            continue;
        };
        let Some(full) = declared.tables.get(name) else {
            continue;
        };
        let own: Vec<_> = cs
            .changes
            .iter()
            .enumerate()
            .filter(|(_, p)| is_part(p) && p.change.table() == Some(name))
            .collect();
        let suffix: Vec<_> = full
            .columns
            .keys()
            .filter(|c| !prefix.columns.contains_key(*c))
            .collect();
        let mut previous = first;
        for column in full.columns.keys() {
            for (i, p) in &own {
                if matches!(&p.change, Change::AddColumn { name, .. } if name == column)
                    || matches!(&p.change, Change::AlterColumnExpression { column: c, .. } if &c.name == column)
                {
                    edge(&mut out, previous, *i);
                    previous = *i;
                }
            }
        }
        let complete = previous;
        let called: Vec<_> = modules
            .iter()
            .filter(|(_, id, _)| {
                own.iter().any(|(_, p)| {
                    let expression = match &p.change {
                        Change::AddColumn { column, .. } => match &column.value_source {
                            ValueSource::Generated(g) => Some(g.expression.as_str()),
                            _ => None,
                        },
                        Change::AlterColumnExpression { to, .. } => Some(to.as_str()),
                        _ => None,
                    };
                    expression.is_some_and(|t| dialect.generation_may_call(t, id.name()))
                })
            })
            .collect();
        for (i, p) in &own {
            edge(&mut out, first, *i);
            if !matches!(
                &p.change,
                Change::CreateTable { .. }
                    | Change::AddColumn { .. }
                    | Change::AlterColumnExpression { .. }
            ) {
                edge(&mut out, complete, *i);
            }
            if let Change::SetReplicaIdentity {
                to: Some(target), ..
            } = &p.change
            {
                for (index, part) in &own {
                    let provides = match (target, &part.change) {
                        (
                            pbps_model::ReplicaIdentity::Index(target),
                            Change::AddIndex { name, .. },
                        )
                        | (
                            pbps_model::ReplicaIdentity::Unique(target),
                            Change::AddUnique { name, .. },
                        ) => name == target,
                        (
                            pbps_model::ReplicaIdentity::PrimaryKey,
                            Change::SetPrimaryKey { to: Some(_), .. },
                        ) => true,
                        _ => false,
                    };
                    if provides {
                        edge(&mut out, *index, *i);
                    }
                }
            }
            for (function, id, _) in &modules {
                if texts(&p.change)
                    .iter()
                    .any(|t| dialect.generation_may_call(t, id.name()))
                {
                    edge(&mut out, *function, *i);
                }
            }
        }
        for (i, p) in cs.changes.iter().enumerate() {
            if matches!(&p.change, Change::InsertRow { table, .. } | Change::UpdateRow { table, .. } if table == name)
            {
                edge(&mut out, complete, i);
                for (part, p) in &own {
                    match &p.change {
                        Change::AddColumn { .. }
                        | Change::AlterColumnExpression { .. }
                        | Change::AlterColumnDefault { .. } => edge(&mut out, *part, i),
                        Change::CreateTable { .. } => {}
                        // Newly installed validators judge the final seed values,
                        // as they do in the ordinary plan's data-before-key order.
                        _ => edge(&mut out, i, *part),
                    }
                }
                // Stored functions may precede the seed rows. Newly created
                // triggers must keep the existing rows-before-trigger rule.
                for (j, _, module) in &modules {
                    if module.kind == ModuleKind::Trigger {
                        edge(&mut out, i, *j);
                    }
                }
            }
            if texts(&p.change)
                .iter()
                .any(|t| dialect.generation_may_call(t, &name.name))
            {
                edge(&mut out, first, i);
            }
            if matches!(&p.change, Change::CreateTable { table, .. } if table.partition_of.as_ref().is_some_and(|of| &of.parent == name))
            {
                edge(&mut out, complete, i);
            }
        }
        for (i, p) in cs.changes.iter().enumerate() {
            if let Change::AddForeignKey {
                table, constraint, ..
            } = &p.change
            {
                if table == name || &constraint.references_table == name {
                    edge(&mut out, complete, i);
                }
                if &constraint.references_table == name {
                    for (key, part) in &own {
                        let columns = match &part.change {
                            Change::SetPrimaryKey { to: Some(key), .. } => Some(&key.columns),
                            Change::AddUnique { constraint, .. } => Some(&constraint.columns),
                            _ => None,
                        };
                        if columns == Some(&constraint.references_columns) {
                            edge(&mut out, *key, i);
                        }
                    }
                }
            }
        }
        for (i, id, module) in &modules {
            if !dialect.generation_may_call(&module.definition, &name.name) {
                continue;
            }
            if called.iter().any(|(_, called, _)| *called == *id) {
                edge(&mut out, first, *i);
                if suffix
                    .iter()
                    .any(|c| dialect.may_name(&module.definition, c.raw()))
                    || !suffix.is_empty()
                        && dialect.generation_needs_whole_row(&module.definition, name)
                {
                    edge(&mut out, complete, *i);
                }
            } else {
                edge(&mut out, complete, *i);
            }
        }
    }
    if out.constraints().is_empty() {
        return Vec::new();
    }
    // A function moved past a held table reader must not pass a module it
    // calls. Keep the same lexical and declared prerequisites as the differ.
    for (i, id, module) in &modules {
        for (j, provider, _) in &modules {
            if i != j
                && id.name() != provider.name()
                && (dialect.generation_may_call(&module.definition, provider.name())
                    || deps.get(*id).is_some_and(|needs| needs.contains(*provider)))
            {
                edge(&mut out, *j, *i);
            }
        }
    }
    out.constraints().iter().cloned().collect()
}

pub fn settle(
    cs: &mut ChangeSet,
    declared: &Schema,
    deps: &ModuleDeps,
    dialect: &dyn Dialect,
) -> Result<(), String> {
    let edges = constraints(cs, declared, deps, dialect);
    if edges.is_empty() {
        return Ok(());
    }
    let mut schedule = Schedule::new(cs.changes.len());
    for c in edges {
        schedule.edge(c.before, c.after, c.source, c.reason);
    }
    let settled = schedule.settle(cs.clone()).map_err(|r| r.describe(cs))?;
    *cs = settled.changes;
    Ok(())
}
