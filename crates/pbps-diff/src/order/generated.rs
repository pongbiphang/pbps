//! Typed creation parts for a new table, shared by ordinary planning,
//! resolver planning and scratch (DEC-1503.1). This module emits no SQL.

use std::collections::{BTreeMap, BTreeSet};

use super::{Constraint, Schedule, Source};
use pbps_dialect::Dialect;
use pbps_model::TableName;
use pbps_model::resolver::OrderReason;
use pbps_model::{
    AddedRole, Change, ChangeSet, IdsFile, Module, ModuleDeps, ModuleId, ModuleKind, PlannedChange,
    Provenance, RiskClass, Schema, ValueSource,
};

pub const RULE: &str = "generated-new-table";

/// Structural uses, separate from the lexical references in `texts`.
/// Empty-table context is lost by row operations and membership changes,
/// not by column DDL that can only rewrite rows already present (DEC-1503.6).
#[derive(Debug, Default, PartialEq, Eq)]
struct TableUse<'a> {
    complete: Vec<&'a TableName>,
    writes: Vec<&'a TableName>,
    seed: Option<&'a TableName>,
}

fn table_use(change: &Change) -> TableUse<'_> {
    let complete = |table| TableUse {
        complete: vec![table],
        ..Default::default()
    };
    match change {
        Change::BuildPartitionedIndex(r) => TableUse {
            complete: std::iter::once(&r.parent.name)
                .chain(r.clones.iter().map(|c| &c.leaf.name))
                .collect(),
            ..Default::default()
        },
Change::RebuildPartitionedTable(_) => TableUse::default(),
Change::CopyTableRows(c) => TableUse { complete: std::iter::once(&c.target.name).chain(c.target_partitions.iter().map(|p| &p.name)).collect(), writes: std::iter::once(&c.target.name).chain(c.target_partitions.iter().map(|p| &p.name)).collect(), seed: None },
Change::AnalyzeTableTree(a) => TableUse { complete: std::iter::once(&a.parent.name).chain(a.partitions.iter().map(|p| &p.name)).collect(), ..Default::default() },

        Change::CreateTable {
            name,
            table,
            beside_default,
            ..
        } => TableUse {
            complete: table.partition_of.iter().map(|of| &of.parent).collect(),
            writes: beside_default
                .iter()
                .flat_map(|source| {
                    std::iter::once(name)
                        .chain(std::iter::once(source))
                        .chain(table.partition_of.iter().map(|of| &of.parent))
                })
                .collect(),
            seed: None,
        },
        Change::AttachPartition { table, parent, .. }
        | Change::DetachPartition { table, parent, .. } => TableUse {
            complete: vec![table, parent],
            writes: vec![table, parent],
            seed: None,
        },
        Change::DropTable {
            name, detach_from, ..
        } => TableUse {
            writes: std::iter::once(name).chain(detach_from).collect(),
            ..Default::default()
        },
        Change::InsertRow { table, .. }
        | Change::UpdateRow { table, .. }
        | Change::DeleteRow { table, .. } => TableUse {
            complete: vec![table],
            writes: vec![table],
            seed: Some(table),
        },
        Change::AddForeignKey {
            table, constraint, ..
        } => TableUse {
            complete: vec![table, &constraint.references_table],
            ..Default::default()
        },
        Change::AddColumn { table, .. }
        | Change::RenameColumn { table, .. }
        | Change::AddComputedColumn { table, .. }
        | Change::DropComputedColumn { table, .. }
        | Change::SetPartitionDefault { table, .. }
        | Change::SetPartitionNotNull { table, .. }
        | Change::SetPrimaryKey { table, .. }
        | Change::AddUnique { table, .. }
        | Change::DropUnique { table, .. }
        | Change::DropForeignKey { table, .. }
        | Change::AddCheck { table, .. }
        | Change::DropCheck { table, .. }
        | Change::AddIndex { table, .. }
        | Change::DropIndex { table, .. }
        | Change::CreatePolicy { table, .. }
        | Change::SetRowSecurity { table, .. } => complete(table),
        Change::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Retype { column, .. },
            ..
        }
        | Change::DropColumn { column, .. }
        | Change::AlterColumnType { column, .. }
        | Change::AlterColumnNullability { column, .. }
        | Change::AlterColumnDefault { column, .. }
        | Change::AlterColumnExpression { column, .. }
        | Change::AlterColumnNumbering { column, .. }
        | Change::SetColumnDeprecated { column, .. } => complete(&column.table),
        Change::CreateModule { id, .. } | Change::AlterModule { id, .. } => TableUse {
            // Installing a trigger binds its table but does not run its body.
            // Invoking it is row work, ordered through the DML arm above.
            complete: match id {
                ModuleId::Trigger { on, .. } => vec![on],
                ModuleId::Named(_) | ModuleId::Routine(_) => Vec::new(),
            },
            ..Default::default()
        },
        // These need only an existing name or change recorded metadata.
        Change::RenameTable { .. }
        | Change::SetReplicaIdentity { .. }
        | Change::SetStorageParameters { .. }
        | Change::SetTablePersistence { .. }
        | Change::SetIndexStorageParameters { .. }
        | Change::SetDataMode { .. }
        | Change::SetEnsureKeys { .. }
        // Removing a policy binds no new expression or column: only its
        // existing owner/name is needed, not a generated installation's prefix.
        | Change::DropPolicy { .. }
        | Change::DropModule { .. }
        | Change::AddEnumValue { .. }
        | Change::AlterDomain { .. }
        | Change::RenameEnumValue { .. }
        | Change::EnumRebuild { .. }
        | Change::CreateType { .. }
        | Change::DropType { .. }
        | Change::RenameType { .. }
        | Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. }
        | Change::PublicExecution { .. } => TableUse::default(),
    }
}

/// Structural consumers use the same rule for a split or a retained CREATE.
/// Lexical expression and module references are handled by their callers.
pub fn needs_complete(change: &Change, table: &TableName) -> bool {
    table_use(change).complete.contains(&table)
}

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

type ModuleStep<'a> = (usize, &'a ModuleId, &'a Module);

#[allow(clippy::wildcard_enum_match_arm)]
fn modules(changes: &[PlannedChange]) -> Vec<ModuleStep<'_>> {
    changes
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match &p.change {
            Change::CreateModule { id, module } | Change::AlterModule { id, module } => {
                Some((i, id, module.as_ref()))
            }
            _ => None,
        })
        .collect()
}

/// Keep selection and settlement on exactly the same relation, including
/// declared dependencies and the existing same-name exclusions (DEC-1503.5).
fn module_dependencies(
    modules: &[ModuleStep<'_>],
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
) -> Vec<(usize, usize)> {
    let mut edges = Vec::new();
    for (i, id, module) in modules {
        for (j, provider, _) in modules {
            if i != j
                && id.name() != provider.name()
                && (dialect.generation_may_call(&module.definition, provider.name())
                    || deps.get(*id).is_some_and(|needs| needs.contains(*provider)))
            {
                edges.push((*j, *i));
            }
        }
    }
    edges
}

pub fn is_part(p: &PlannedChange) -> bool {
    p.provenance
        .as_ref()
        .is_some_and(|origin| origin.rule == RULE)
}

#[allow(clippy::wildcard_enum_match_arm)]
pub fn parts(
    planned: &PlannedChange,
    ids: &IdsFile,
    changes: &[PlannedChange],
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
) -> Result<Option<Vec<PlannedChange>>, String> {
    let functions = functions(changes);
    // A reader can be behind any number of callees. Walk the very edges
    // settlement uses rather than predicting a different call graph here.
    let table_reader = if let Change::CreateTable { name, table, .. } = &planned.change {
        let modules = modules(changes);
        let edges = module_dependencies(&modules, deps, dialect);
        let mut pending: Vec<_> = modules
            .iter()
            .filter(|(_, id, module)| {
                module.kind == ModuleKind::Function
                    && table.columns.values().any(|c| {
                        matches!(&c.value_source,
                ValueSource::Generated(g) if dialect.generation_may_call(&g.expression, id.name()))
                    })
            })
            .map(|(i, _, _)| *i)
            .collect();
        let mut reached = BTreeSet::new();
        while let Some(at) = pending.pop() {
            if reached.insert(at) {
                pending.extend(
                    edges
                        .iter()
                        .filter(|(_, consumer)| *consumer == at)
                        .map(|(provider, _)| *provider),
                );
            }
        }
        modules.iter().any(|(i, _, module)| {
            reached.contains(i)
                && dialect.generation_names_table(&module.definition, name)
                && !dialect.generation_table_name_only(&module.definition, name)
        })
    } else {
        false
    };
    let Some(parts) = dialect
        .generated_table_parts(&planned.change, ids, &functions, table_reader)
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    Ok(Some(
        parts
            .into_iter()
            .map(|change| {
                let mut part = planned.clone();
                // The final typed order re-derives these provisional risks:
                // only parts before the owner's first write finish an empty
                // CREATE (DEC-1503.4). Lowering cannot certify a later position.
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

/// Derive contextual risks from typed ownership, never diagnostic provenance.
/// An earlier CREATE proves generated-part context (DEC-1503.3); policy
/// replacements pair by table identity and policy name (DEC-1173.1).
pub fn risks(
    cs: &ChangeSet,
    ids: &IdsFile,
    dialect: &(impl Dialect + ?Sized),
) -> Vec<BTreeSet<RiskClass>> {
    use pbps_model::plan_names::{At, Holder, PlanNames};

    let names = cs
        .changes
        .iter()
        .any(|p| {
            matches!(
                p.change,
                Change::CreateTable { .. }
                    | Change::DropPolicy { .. }
                    | Change::CreatePolicy { .. }
            )
        })
        .then(|| PlanNames::of(&cs.changes, ids).ok())
        .flatten();
    // Every replacement is unknown in both directions, even a mode flip
    // whose two intrinsic risks happen to agree. Table identity, not spelling,
    // keeps this pairing correct across renames and reused names (DEC-1173.1).
    let mut dropped = BTreeSet::new();
    let mut installed = BTreeSet::new();
    let mut policy_keys = BTreeMap::new();
    for (at, p) in cs.changes.iter().enumerate() {
        let (Change::DropPolicy { table, name, .. }
        | Change::CreatePolicy {
            table,
            name,
            origin: pbps_model::RowSecurityOrigin::Standing,
            ..
        }) = &p.change
        else {
            continue;
        };
        let is_drop = matches!(&p.change, Change::DropPolicy { .. });
        if let Some(names) = &names
            && let Holder::Tracked(uid) = names.holder(table, At::Before(at))
        {
            let key = (uid.clone(), name.clone());
            policy_keys.insert(at, key.clone());
            if is_drop {
                dropped.insert(key);
            } else {
                installed.insert(key);
            }
        }
    }
    let replacements: BTreeSet<_> = dropped.intersection(&installed).cloned().collect();
    let mut first_write = BTreeMap::new();
    let mut parents = BTreeMap::new();
    for (at, p) in cs.changes.iter().enumerate() {
        let Some(names) = &names else { continue };
        // A row written through a partition is also a row of every ancestor.
        // Resolve membership by UID so a rename cannot restore emptiness.
        let membership = if let Change::CreateTable { uid, table, .. } = &p.change {
            table.partition_of.as_ref().map(|of| (uid, &of.parent))
        } else if let Change::AttachPartition { uid, parent, .. } = &p.change {
            Some((uid, parent))
        } else {
            None
        };
        if let Some((child, parent)) = membership
            && let Holder::Tracked(parent) = names.holder(parent, At::Before(at))
        {
            parents.insert(child.clone(), parent.clone());
        }
        for table in table_use(&p.change).writes {
            // A move from DEFAULT writes the newly created child too.
            let boundary = if matches!(&p.change, Change::CreateTable { name, .. } if name == table)
            {
                At::After(at)
            } else {
                At::Before(at)
            };
            if let Holder::Tracked(owner) = names.holder(table, boundary) {
                let mut owner = owner;
                let mut visited = BTreeSet::new();
                while visited.insert(owner.clone()) {
                    first_write.entry(owner.clone()).or_insert(at);
                    let Some(parent) = parents.get(owner) else {
                        break;
                    };
                    owner = parent;
                }
            }
        }
        if let Change::DetachPartition { uid, .. } | Change::DropTable { uid, .. } = &p.change {
            parents.remove(uid);
        }
    }
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
                Holder::Tracked(owner) => Some((owner.clone(), at)),
                Holder::Unmanaged | Holder::Vacant => None,
            }
        })
        .collect();
    // Every installation must precede every write. One safe installation
    // cannot certify a later recomputation on the same table (DEC-1503.4).
    let unsafe_owners: BTreeSet<_> = installations
        .iter()
        .filter(|(owner, at)| first_write.get(owner).is_some_and(|write| write < at))
        .map(|(owner, _)| owner.clone())
        .collect();
    let installations: BTreeSet<_> = installations
        .into_iter()
        .map(|(owner, _)| owner)
        .filter(|owner| !unsafe_owners.contains(owner))
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
                if !installations.contains(owner)
                    || first_write.get(owner).is_some_and(|write| *write < at)
                {
                    return None;
                }
                cs.changes[..at].iter().find_map(|p| {
                    matches!(&p.change, Change::CreateTable {
                        uid, table, beside_default: None, ..
                    } if uid == owner && table.partition_of.is_none())
                    .then_some(&p.change)
                })
            };
            let mut risks = dialect.change_risks(source().unwrap_or(&part.change));
            if policy_keys
                .get(&at)
                .is_some_and(|key| replacements.contains(key))
            {
                risks.extend([RiskClass::Revoke, RiskClass::RowSecurity]);
            }
            risks
        })
        .collect()
}

/// The same generated-part expansion, retaining each part's logical source.
pub fn expand(
    plan: &mut super::PlanningPlan,
    ids: &IdsFile,
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
) -> Result<usize, String> {
    let mut expanded = Vec::new();
    let mut origins = Vec::new();
    let mut added = 0;
    for (p, origin) in plan.changes.iter().zip(&plan.origins) {
        match parts(p, ids, &plan.changes, deps, dialect)? {
            Some(parts) => {
                added += parts.len() - 1;
                let part_origin = match origin.source() {
                    pbps_model::operation::Source::Change(at) => {
                        pbps_model::operation::Origin::SplitPart(at)
                    }
                    pbps_model::operation::Source::Rebinding(_) => origin.clone(),
                };
                origins.extend(std::iter::repeat_n(part_origin, parts.len()));
                expanded.extend(parts);
            }
            None => {
                expanded.push(p.clone());
                origins.push(origin.clone());
            }
        }
    }
    plan.payload.changes = expanded;
    plan.origins = origins;
    plan.invalidate();
    Ok(added)
}

fn texts(change: &Change) -> Vec<&str> {
    fn column(c: &pbps_model::Column) -> Vec<&str> {
        let expression = match &c.value_source {
            ValueSource::Default(t) => Some(t.as_str()),
            ValueSource::Generated(g) => Some(g.expression.as_str()),
            ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => None,
        };
        std::iter::once(c.ty.base.as_str())
            .chain(expression)
            .collect()
    }
    match change {
        Change::BuildPartitionedIndex(r) => match &r.definition {
            pbps_model::PartitionIndexDefinition::Index(i) => i
                .filter
                .as_deref()
                .into_iter()
                .chain(i.columns.iter().filter_map(|c| c.key.expression()))
                .collect(),
            pbps_model::PartitionIndexDefinition::PrimaryKey(_)
            | pbps_model::PartitionIndexDefinition::Unique(_) => Vec::new(),
        },
        Change::RebuildPartitionedTable(_) | Change::AnalyzeTableTree(_) => Vec::new(),
        Change::CopyTableRows(c) => c.columns.iter().map(|p| p.cast.base.as_str()).collect(),

        Change::CreateTable { table, .. } => table
            .columns
            .values()
            .flat_map(column)
            .chain(table.computed.values().map(|c| c.expression.as_str()))
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
        Change::AddColumn { column: c, .. } => column(c),
        Change::AddComputedColumn { computed, .. } => vec![&computed.expression],
        Change::AlterColumnExpression { to, .. } => vec![to],
        Change::AlterColumnDefault { to, .. } => to.as_deref().into_iter().collect(),
        Change::SetPartitionDefault { to, fallback, .. } => {
            to.as_deref().or(fallback.as_deref()).into_iter().collect()
        }
        Change::AlterColumnType { from, to, .. } => vec![&from.base, &to.base],
        Change::AddCheck { constraint, .. } => vec![&constraint.expression],
        Change::CreatePolicy { policy, .. } => policy
            .using
            .iter()
            .chain(&policy.with_check)
            .map(pbps_model::Text::raw)
            .collect(),
        Change::AddIndex { index, .. } => index
            .filter
            .as_deref()
            .into_iter()
            .chain(index.columns.iter().filter_map(|c| c.key.expression()))
            .collect(),
        Change::InsertRow {
            defaults, types, ..
        } => defaults
            .values()
            .map(String::as_str)
            .chain(types.values().map(|t| t.base.as_str()))
            .collect(),
        Change::UpdateRow {
            types,
            after_types,
            key_type,
            ..
        }
        | Change::DeleteRow {
            types,
            after_types,
            key_type,
            ..
        } => types
            .values()
            .chain(after_types.values())
            .chain(key_type.iter())
            .map(|t| t.base.as_str())
            .collect(),
        Change::DropTable { .. }
        | Change::DetachPartition { .. }
        | Change::AttachPartition { .. }
        | Change::RenameTable { .. }
        | Change::DropColumn { .. }
        | Change::RenameColumn { .. }
        | Change::AlterColumnNullability { .. }
        | Change::AlterColumnNumbering { .. }
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
        | Change::SetDataMode { .. }
        | Change::SetEnsureKeys { .. }
        | Change::DropPolicy { .. }
        | Change::SetRowSecurity { .. }
        | Change::CreateModule { .. }
        | Change::AlterModule { .. }
        | Change::DropModule { .. }
        | Change::AddEnumValue { .. }
        | Change::AlterDomain { .. }
        | Change::RenameEnumValue { .. }
        | Change::EnumRebuild { .. }
        | Change::CreateType { .. }
        | Change::DropType { .. }
        | Change::RenameType { .. }
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
/// The columns of `table` that every one of `texts` may read, for an engine
/// that can say, and `None` where any text cannot be read that way (unknown,
/// or a projection of every column), which keeps the whole-shape rule.
fn reads(
    dialect: &(impl Dialect + ?Sized),
    texts: &[&str],
    table: &TableName,
) -> Option<BTreeSet<String>> {
    let mut named = BTreeSet::new();
    for text in texts {
        named.extend(dialect.generation_column_names(text, table)?);
    }
    Some(named)
}

/// A reader of only some columns waits for the first part and for the part
/// of each late column it names, not for the columns it does not.
fn wait_for(
    dialect: &(impl Dialect + ?Sized),
    out: &mut Schedule,
    first: usize,
    own: &[(usize, &PlannedChange)],
    columns: &BTreeSet<String>,
    reader: usize,
) {
    let mut wait = |before: usize| {
        if before != reader {
            out.edge(before, reader, Source::Lexical, OrderReason::Structural);
        }
    };
    wait(first);
    for (part, p) in own {
        if let Change::AddColumn { name, .. } | Change::AddComputedColumn { name, .. } = &p.change
            && columns.contains(&dialect.generation_column_key(name.raw()))
        {
            wait(*part);
        }
    }
}

#[allow(clippy::wildcard_enum_match_arm)]
pub fn constraints(
    cs: &ChangeSet,
    declared: &Schema,
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
) -> Vec<Constraint> {
    let mut out = Schedule::new(cs.changes.len());
    let edge = |out: &mut Schedule, a, b| {
        if a != b {
            out.edge(a, b, Source::Lexical, OrderReason::Structural);
        }
    };
    let modules = modules(&cs.changes);
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
        // A computed column is added after every column before it, and in
        // declared order: the engine appends it, and its position is compared
        // as reported, so adding `d` before `c` would reproduce the target's
        // columns in another order.
        for column in full.computed.keys() {
            for (i, p) in &own {
                if matches!(&p.change, Change::AddComputedColumn { name, .. } if name == column) {
                    edge(&mut out, previous, *i);
                    previous = *i;
                }
            }
        }
        let complete = previous;
        // The prefix holds every column the table declares, computed ones
        // included, so the first part is already the complete shape.
        let whole = prefix.columns.len() == full.columns.len()
            && prefix.computed.len() == full.computed.len();
        for (i, p) in &own {
            edge(&mut out, first, *i);
            if !matches!(
                &p.change,
                Change::CreateTable { .. }
                    | Change::AddColumn { .. }
                    | Change::AddComputedColumn { .. }
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
                    .any(|t| dialect.generation_calls(t, id))
                {
                    edge(&mut out, *function, *i);
                }
            }
        }
        for (i, p) in cs.changes.iter().enumerate() {
            let uses = table_use(&p.change);
            // Creation parts have their own column/validator chain above.
            // Every other structural consumer must wait for that chain.
            if uses.complete.contains(&name) && !own.iter().any(|(part, _)| *part == i) {
                edge(&mut out, complete, i);
            }
            if uses.seed == Some(name) {
                edge(&mut out, complete, i);
                for (part, p) in &own {
                    match &p.change {
                        Change::AddColumn { .. }
                        | Change::AddComputedColumn { .. }
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
            let references: Vec<_> = texts(&p.change)
                .into_iter()
                .filter(|text| dialect.generation_names_table(text, name))
                .collect();
            if !references.is_empty() {
                let name_only = references
                    .iter()
                    .all(|text| dialect.generation_table_name_only(text, name));
                let shape = if name_only || whole { first } else { complete };
                edge(&mut out, shape, i);
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
        for (i, _, module) in &modules {
            if dialect.generation_names_table(&module.definition, name) {
                // Called readers already chose a full-shape placeholder or
                // the whole-table hold; every consumer sees that full shape.
                if let Some(columns) = reads(dialect, &[module.definition.as_str()], name) {
                    wait_for(dialect, &mut out, first, &own, &columns, *i);
                } else {
                    edge(&mut out, if whole { first } else { complete }, *i);
                }
            }
        }
    }
    if out.constraints().is_empty() {
        return Vec::new();
    }
    // A function moved past a held table reader must not pass a module it
    // calls. Keep the same lexical and declared prerequisites as the differ.
    for (provider, consumer) in module_dependencies(&modules, deps, dialect) {
        edge(&mut out, provider, consumer);
    }
    out.constraints().iter().cloned().collect()
}

/// The generated schedule consumes and returns only an owned intermediate.
pub fn settle(
    plan: &mut super::PlanningPlan,
    declared: &Schema,
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
) -> Result<(), String> {
    let edges = constraints(plan, declared, deps, dialect);
    if edges.is_empty() {
        return Ok(());
    }
    let mut schedule = Schedule::new(plan.changes.len());
    for c in edges {
        schedule.edge(c.before, c.after, c.source, c.reason);
    }
    let settled = schedule
        .settle(plan.clone())
        .map_err(|r| r.describe(plan))?;
    *plan = settled.changes;
    Ok(())
}

#[cfg(test)]
#[path = "generated_tests.rs"]
mod tests;
