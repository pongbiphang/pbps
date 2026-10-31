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

mod inputs;

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

type ModuleStep<'a, P = usize> = (P, &'a ModuleId, &'a Module);

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

/// What a module's text names for the order. In `scratch` that is the text the
/// engine resolves when it creates the module (DEC-2389.2): two routines whose
/// bodies call each other are created in either order, and an edge for a call
/// the engine makes later would close a cycle between them that is no order's
/// fault. Otherwise the whole definition, as the ordinary route has it.
fn named_text<'a>(
    module: &'a Module,
    dialect: &(impl Dialect + ?Sized),
    scratch: bool,
) -> std::borrow::Cow<'a, str> {
    if scratch {
        dialect.module_creation_text(module)
    } else {
        std::borrow::Cow::Borrowed(module.definition.as_str())
    }
}

/// The texts of one change, with scratch's typed-position policy.
fn change_texts<'a>(
    change: &'a Change,
    dialect: &(impl Dialect + ?Sized),
    scratch: bool,
) -> Vec<&'a str> {
    gather(change, reading_types(dialect, scratch))
}

/// The type spellings scratch's guesses read: only those that name a
/// user-defined type, since a built-in spelling such as `text` binds the
/// engine's own type whatever a view or routine of that name is (DEC-2389.4).
/// Every spelling where the edges are stated, as the ordinary route reads them.
fn reading_types(dialect: &(impl Dialect + ?Sized), scratch: bool) -> Types {
    if scratch && dialect.scratch_text_edges_are_guesses() {
        Types::Named
    } else {
        Types::All
    }
}

/// Keep selection and settlement on exactly the same relation, including
/// declared dependencies and the existing same-name exclusions (DEC-1503.5).
/// Each edge is `(provider, consumer, declared)`: a declared one is stated by
/// the user, and one that is only a name matched in the consumer's text is a
/// guess (DEC-2389.4).
fn module_dependencies(
    modules: &[ModuleStep<'_>],
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
    scratch: bool,
) -> Vec<(usize, usize, bool)> {
    let mut edges: Vec<_> =
        super::annotations::modules(modules.iter().map(|(at, id, _)| (*at, *id)), deps)
            .into_iter()
            .map(|edge| (edge.before, edge.after, true))
            .collect();
    infer_module_dependencies(modules, dialect, scratch, &mut edges);
    edges
}

fn infer_module_dependencies<P: Copy + Ord>(
    modules: &[ModuleStep<'_, P>],
    dialect: &(impl Dialect + ?Sized),
    scratch: bool,
    edges: &mut Vec<(P, P, bool)>,
) {
    // Keep vector insertion order; membership must not rescan all emitted edges
    // for each module pair. P also permits counted comparisons in regression tests.
    let mut seen: BTreeSet<_> = edges.iter().map(|&(a, b, _)| (a, b)).collect();
    for (i, id, module) in modules {
        let text = named_text(module, dialect, scratch);
        for (j, provider, _) in modules {
            if i == j || id.name() == provider.name() {
                continue;
            }
            if !seen.contains(&(*j, *i)) && dialect.generation_may_call(&text, provider.name()) {
                seen.insert((*j, *i));
                edges.push((*j, *i, false));
            }
        }
    }
}

pub fn is_part(p: &PlannedChange) -> bool {
    p.provenance
        .as_ref()
        .is_some_and(|origin| origin.rule == RULE)
}

pub fn parts(
    planned: &PlannedChange,
    ids: &IdsFile,
    changes: &[PlannedChange],
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
) -> Result<Option<Vec<PlannedChange>>, String> {
    parts_in(
        planned,
        ids,
        changes,
        deps,
        dialect,
        false,
        &BTreeSet::new(),
        &Default::default(),
    )
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CreationNeed {
    Module(usize),
    Reference(pbps_model::object_ref::ObjectRef),
}

/// [`parts`], for the resolver's scratch reconstruction when `scratch`: the
/// engine splits every new table, not only one whose generation needs a
/// planned function (DEC-2389.1).
#[allow(clippy::wildcard_enum_match_arm)]
#[allow(clippy::too_many_arguments)]
fn parts_in(
    planned: &PlannedChange,
    ids: &IdsFile,
    changes: &[PlannedChange],
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
    scratch: bool,
    demanded: &BTreeSet<pbps_model::object_ref::ObjectRef>,
    prerequisites: &super::annotations::Creation,
) -> Result<Option<Vec<PlannedChange>>, String> {
    let functions = functions(changes);
    // A reader can be behind any number of callees. Walk the very edges
    // settlement uses rather than predicting a different call graph here.
    let (table_reader, required_columns) = if let Change::CreateTable { name, table, .. } =
        &planned.change
    {
        let modules = modules(changes);
        let edges = module_dependencies(&modules, deps, dialect, scratch);
        let mut providers: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (provider, consumer, _) in &edges {
            providers.entry(*consumer).or_default().push(*provider);
        }
        let mut pending: Vec<_> = modules
                .iter()
                .filter(|(_,id,module)| module.kind==ModuleKind::Function && table.columns.values().any(|c|matches!(&c.value_source,ValueSource::Generated(g) if dialect.generation_may_call(&g.expression,id.name()))))
                .map(|(at, _, _)| CreationNeed::Module(*at))
                .collect();
        pending.extend(
            prerequisites
                .references
                .iter()
                .cloned()
                .map(CreationNeed::Reference),
        );
        let by_at: BTreeMap<_, _> = modules
            .iter()
            .map(|(at, id, module)| (*at, (*id, *module)))
            .collect();
        let mut by_id: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (at, id, _) in &modules {
            by_id.entry((*id).clone()).or_default().push(*at);
        }
        let mut reached = BTreeSet::new();
        let mut required = BTreeSet::new();
        let mut reader = false;
        while let Some(need) = pending.pop() {
            if !reached.insert(need.clone()) {
                continue;
            }
            match need {
                CreationNeed::Module(at) => {
                    let (id, module) = by_at[&at];
                    pending.extend(
                        providers
                            .get(&at)
                            .into_iter()
                            .flatten()
                            .copied()
                            .map(CreationNeed::Module),
                    );
                    let reference = pbps_model::object_ref::ObjectRef {
                        root: pbps_model::object_ref::Root::Module(id.clone()),
                        part: pbps_model::object_ref::Selector::Whole,
                    };
                    // Without typed edges, retain each legacy transition's body.
                    if prerequisites
                        .dependencies
                        .get(&reference)
                        .is_some_and(|p| !p.is_empty())
                    {
                        pending.push(CreationNeed::Reference(reference));
                    }
                    let text = named_text(module, dialect, scratch);
                    reader |= dialect.generation_names_table(&text, name)
                        && !dialect.generation_table_name_only(&text, name);
                }
                CreationNeed::Reference(reference) => {
                    if let Some(column) = prerequisites.columns.get(&reference) {
                        required.insert(column.clone());
                    }
                    if let pbps_model::object_ref::Root::Module(id) = &reference.root {
                        pending.extend(
                            by_id
                                .get(id)
                                .into_iter()
                                .flatten()
                                .copied()
                                .map(CreationNeed::Module),
                        );
                    }
                    pending.extend(
                        prerequisites
                            .dependencies
                            .get(&reference)
                            .into_iter()
                            .flatten()
                            .cloned()
                            .map(CreationNeed::Reference),
                    );
                }
            }
        }
        (reader, required)
    } else {
        (false, BTreeSet::new())
    };
    let split = if !demanded.is_empty() || !required_columns.is_empty() {
        dialect.annotation_parts(
            &planned.change,
            ids,
            &functions,
            table_reader,
            pbps_dialect::creation::Request {
                scratch,
                references: demanded,
                dependencies: Some(&prerequisites.local_dependencies),
            },
            &required_columns,
        )
    } else if scratch {
        dialect.scratch_table_parts(&planned.change, ids, &functions, table_reader)
    } else {
        dialect.generated_table_parts(&planned.change, ids, &functions, table_reader)
    };
    let Some(parts) = split.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    // Scratch can carry a partition default with a placeholder column UID.
    // Annotation endpoints need its real child-table/root-column identity;
    // the existing partition operation resolves that through the shared chain.
    // Scratch consumers keep their existing carrier; Contract resolves its
    // identity when reading annotations (DEC-2469.3).
    let parts = if !scratch
        && !demanded.is_empty()
        && let Change::CreateTable {
            uid, name, table, ..
        } = &planned.change
        && let Some(of) = &table.partition_of
    {
        parts
            .into_iter()
            .map(|part| match part {
                Change::AlterColumnDefault {
                    column,
                    from: None,
                    to: Some(to),
                    ..
                } if column.table == *name => Change::SetPartitionDefault {
                    uid: uid.clone(),
                    table: name.clone(),
                    parent: of.parent.clone(),
                    column: column.name,
                    from: None,
                    to: Some(to),
                    fallback: None,
                },
                part => part,
            })
            .collect()
    } else {
        parts
    };
    if !scratch && !demanded.is_empty() && parts.len() == 1 && parts[0] == planned.change {
        return Ok(None);
    }
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

/// The same UID/membership write boundary governs empty creation effects and
/// their contextual risks. A rename or a partition write cannot restore emptiness.
fn first_writes(
    cs: &ChangeSet,
    names: Option<&pbps_model::plan_names::PlanNames>,
) -> BTreeMap<pbps_model::Uid, usize> {
    use pbps_model::plan_names::{At, Holder};
    let mut first_write = BTreeMap::new();
    let mut parents = BTreeMap::new();
    for (at, p) in cs.changes.iter().enumerate() {
        let Some(names) = names else { continue };
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
    first_write
}

fn creation_sources<'a>(
    cs: &'a ChangeSet,
    names: Option<&pbps_model::plan_names::PlanNames>,
    first_write: &BTreeMap<pbps_model::Uid, usize>,
) -> Vec<Option<&'a Change>> {
    use pbps_model::plan_names::{At, Holder};
    let mut created = BTreeMap::new();
    cs.changes
        .iter()
        .enumerate()
        .map(|(at, planned)| {
            if let Change::CreateTable {
                uid,
                table,
                beside_default: None,
                ..
            } = &planned.change
                && table.partition_of.is_none()
            {
                created.insert(uid, &planned.change);
            }
            let Holder::Tracked(owner) = names?.holder(planned.change.table()?, At::Before(at))
            else {
                return None;
            };
            if first_write.get(owner).is_some_and(|write| *write < at) {
                return None;
            }
            created.get(owner).copied()
        })
        .collect()
}

/// Column DDL before an empty root's first write has no values to rewrite.
/// Object existence and generation input boundaries remain independently required.
pub(super) fn empty_columns(cs: &ChangeSet, ids: &IdsFile) -> BTreeSet<usize> {
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, ids).ok();
    let writes = first_writes(cs, names.as_ref());
    creation_sources(cs, names.as_ref(), &writes)
        .into_iter()
        .enumerate()
        .filter_map(|(at, source)| {
            (source.is_some()
                && matches!(
                    cs.changes[at].change,
                    Change::AddColumn { .. } | Change::AlterColumnExpression { .. }
                ))
            .then_some(at)
        })
        .collect()
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
    let first_write = first_writes(cs, names.as_ref());
    let sources = creation_sources(cs, names.as_ref(), &first_write);
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
            // NOT NULL finishes a column of this empty table without granting
            // an exemption to any other generation installation.
            let completes_nullability = matches!(
                part.change,
                Change::AlterColumnNullability {
                    to_nullable: false,
                    ..
                }
            );
            let source = || {
                if !matches!(
                    &part.change,
                    Change::AddColumn { .. }
                        | Change::AlterColumnNullability {
                            to_nullable: false,
                            ..
                        }
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
                if !installations.contains(owner) && !completes_nullability {
                    return None;
                }
                sources[at]
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
    expand_in(plan, ids, deps, dialect, false)
}

/// Prepare descriptive engine facts before any existing settlement can refuse.
pub(super) fn inline_contexts(plan: &mut super::PlanningPlan, dialect: &(impl Dialect + ?Sized)) {
    if !plan.annotations.as_ref().is_some_and(|a| a.extended) {
        return;
    }
    let inline: BTreeMap<_, _> = plan
        .changes
        .iter()
        .filter_map(|p| {
            if let Change::CreateTable {
                uid, name, table, ..
            } = &p.change
            {
                dialect
                    .creation_inline(name, table)
                    .explanation()
                    .map(|reason| (uid.clone(), reason))
            } else {
                None
            }
        })
        .collect();
    if let Some(contract) = &mut plan.annotations {
        std::sync::Arc::make_mut(contract).inline.extend(inline);
    }
}

/// [`expand`], splitting as the scratch reconstruction does when `scratch`.
pub(super) fn expand_in(
    plan: &mut super::PlanningPlan,
    ids: &IdsFile,
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
    scratch: bool,
) -> Result<usize, String> {
    let mut expanded = Vec::new();
    let mut inline = BTreeMap::new();
    let mut origins = Vec::new();
    let mut added = 0;
    for (p, origin) in plan.changes.iter().zip(&plan.origins) {
        let demanded = plan
            .annotations
            .as_ref()
            .map_or_else(BTreeSet::new, |a| a.demands(&p.change, origin));
        let prerequisites = plan
            .annotations
            .as_ref()
            .map_or_else(Default::default, |a| a.prerequisites(origin));
        match parts_in(
            p,
            ids,
            &plan.changes,
            deps,
            dialect,
            scratch,
            &demanded,
            &prerequisites,
        )? {
            Some(parts) => {
                if plan.annotations.as_ref().is_some_and(|a| a.extended)
                    && let Change::CreateTable {
                        uid, name, table, ..
                    } = &p.change
                    && let Some(reason) = dialect.creation_inline(name, table).explanation()
                {
                    inline.insert(uid.clone(), reason);
                }
                added += parts.len() - 1;
                let part_origin = match origin.source() {
                    // A table that scratch leaves whole is still one part.
                    pbps_model::operation::Source::Change(_) if parts.len() == 1 => origin.clone(),
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
    if let Some(contract) = &mut plan.annotations {
        std::sync::Arc::make_mut(contract).inline.extend(inline);
    }
    plan.payload.changes = expanded;
    plan.origins = origins;
    plan.invalidate();
    Ok(added)
}

/// The texts of `change` that are expressions: a default, a generated or
/// computed expression, a check, an index expression or predicate, a policy.
/// What names an index or a routine is read here and nowhere else a type
/// spelling is (DEC-2389.4).
fn expressions(change: &Change) -> Vec<&str> {
    gather(change, Types::Omit)
}

/// Which type spellings a read of the texts of a change includes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Types {
    /// None: only the expressions.
    Omit,
    /// Those that name a user-defined type ([`named`]).
    Named,
    /// Every one, as the ordinary route reads them.
    All,
}

fn spelling(ty: &pbps_model::ColumnType, types: Types) -> Option<&str> {
    match types {
        Types::Omit => None,
        Types::Named => named(ty),
        Types::All => Some(ty.base.as_str()),
    }
}

/// The spelling of `ty` where it names something a declaration made: a type
/// qualified by its schema, which is an enum, a domain or the row type of a
/// table or view (measured on 16 and 18: absent, `42704`). A built-in spelling
/// such as `text` binds the engine's own type whatever a view or table of that
/// name is, so it names nothing here.
fn named(ty: &pbps_model::ColumnType) -> Option<&str> {
    ty.type_ref()
        .filter(|r| r.schema() != "pg_catalog")
        .map(|_| ty.base.as_str())
}

/// The texts of `change`: its expressions and the type spellings `types` says.
fn gather(change: &Change, types: Types) -> Vec<&str> {
    fn column(c: &pbps_model::Column, types: Types) -> Vec<&str> {
        let expression = match &c.value_source {
            ValueSource::Default(t) => Some(t.as_str()),
            ValueSource::Generated(g) => Some(g.expression.as_str()),
            ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => None,
        };
        spelling(&c.ty, types)
            .into_iter()
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
        Change::CopyTableRows(c) => c
            .columns
            .iter()
            .filter_map(|p| spelling(&p.cast, types))
            .collect(),

        Change::CreateTable { table, .. } => table
            .columns
            .values()
            .flat_map(|c| column(c, types))
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
        Change::AddColumn { column: c, .. } => column(c, types),
        Change::AddComputedColumn { computed, .. } => vec![&computed.expression],
        Change::AlterColumnExpression { to, .. } => vec![to],
        Change::AlterColumnDefault { to, .. } => to.as_deref().into_iter().collect(),
        Change::SetPartitionDefault { to, fallback, .. } => {
            to.as_deref().or(fallback.as_deref()).into_iter().collect()
        }
        Change::AlterColumnType { from, to, .. } => [from, to]
            .into_iter()
            .filter_map(|t| spelling(t, types))
            .collect(),
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
            defaults,
            types: row,
            ..
        } => defaults
            .values()
            .map(String::as_str)
            .chain(row.values().filter_map(|t| spelling(t, types)))
            .collect(),
        Change::UpdateRow {
            types: row,
            after_types,
            key_type,
            ..
        }
        | Change::DeleteRow {
            types: row,
            after_types,
            key_type,
            ..
        } => row
            .values()
            .chain(after_types.values())
            .chain(key_type.iter())
            .filter_map(|t| spelling(t, types))
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
    guessed: bool,
) {
    let mut wait = |before: usize| {
        if before != reader {
            if guessed {
                out.prefer(before, reader, OrderReason::Structural);
            } else {
                out.edge(before, reader, Source::Lexical, OrderReason::Structural);
            }
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

pub fn constraints(
    cs: &ChangeSet,
    declared: &Schema,
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
) -> Vec<Constraint> {
    constraints_in(cs, declared, deps, dialect, false, false)
}

/// The transient annotation contract opts into exact facet prerequisites;
/// unannotated recipes retain their existing constraints and proof bytes.
pub(super) fn plan_constraints<S>(
    plan: &super::PlanningPlan<S>,
    declared: &Schema,
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
    scratch: bool,
) -> Vec<Constraint> {
    constraints_in(
        plan,
        declared,
        deps,
        dialect,
        scratch,
        plan.annotations.as_ref().is_some_and(|a| a.extended),
    )
}

/// [`constraints`], and when `scratch` the rules only the resolver's scratch
/// reconstruction has (DEC-2389.1), over parts that every new table was split
/// into.
#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn constraints_in(
    cs: &ChangeSet,
    declared: &Schema,
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
    scratch: bool,
    demanded: bool,
) -> Vec<Constraint> {
    let mut out = Schedule::new(cs.changes.len());
    let edge = |out: &mut Schedule, a, b| {
        if a != b {
            out.edge(a, b, Source::Lexical, OrderReason::Structural);
        }
    };
    // An edge that rests only on a name matched in a text (DEC-2389.4).
    let guessed = scratch && dialect.scratch_text_edges_are_guesses();
    let guess = |out: &mut Schedule, a, b| {
        if guessed {
            if a != b {
                out.prefer(a, b, OrderReason::Structural);
            }
        } else if a != b {
            out.edge(a, b, Source::Inferred, OrderReason::Structural);
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
        let replacements: Vec<_> = own
            .iter()
            .filter_map(|(at, p)| {
                if let Change::AlterColumnExpression { column, to, .. } = &p.change {
                    Some((*at, column, to))
                } else {
                    None
                }
            })
            .collect();
        let mixed = !replacements.is_empty()
            && own
                .iter()
                .any(|(_, p)| matches!(p.change, Change::AddColumn { .. }));
        let independent = mixed || (demanded && !replacements.is_empty());
        let mut previous = first;
        for column in full.columns.keys() {
            for (i, p) in &own {
                if matches!(&p.change, Change::AddColumn { name, .. } if name == column)
                    || (!independent
                        && matches!(&p.change, Change::AlterColumnExpression { column: c, .. } if &c.name == column))
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
        for (key_at, key) in &own {
            if let Change::SetPrimaryKey {
                to: Some(primary), ..
            } = &key.change
            {
                for (at, part) in &own {
                    if matches!(&part.change, Change::AlterColumnNullability { column, to_nullable: false, .. } if primary.columns.contains(&column.name))
                    {
                        edge(&mut out, *at, *key_at);
                    }
                }
            }
        }
        // A placeholder's final definition is not a physical append. Keep
        // legacy unmixed chains byte-identical, but let a demanded recipe install
        // each expression after its actual inputs, not its declaration slot.
        let mut complete = vec![previous];
        if independent {
            for (at, column, text) in &replacements {
                for (input, p) in &own {
                    if let Change::AddColumn { name, .. } = &p.change
                        && (name == &column.name || dialect.generation_may_read_column(text, name))
                    {
                        edge(&mut out, *input, *at);
                    }
                }
                complete.push(*at);
            }
        }
        let mut providers: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
        for (at, part) in &own {
            let column = match &part.change {
                Change::AddColumn { name, .. } | Change::AddComputedColumn { name, .. } => {
                    Some(name)
                }
                _ => None,
            };
            if let Some(column) = column {
                providers.entry(column).or_default().insert(*at);
            }
        }
        let finish = |out: &mut Schedule, after| {
            for &before in &complete {
                edge(out, before, after);
            }
        };
        let inputs = |out: &mut Schedule, after, change: &Change| {
            if demanded && let Some(columns) = inputs::columns(change, name, full, dialect) {
                for column in &columns {
                    for &before in providers.get(column).into_iter().flatten() {
                        edge(out, before, after);
                    }
                }
            } else {
                finish(out, after);
            }
        };
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
                inputs(&mut out, *i, &p.change);
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
            // The engine needs a routine before the expression that calls it.
            // A name matched anywhere else (a type spelling `text` beside a
            // view `app.text`) is a guess, and so is a name that is no
            // function (DEC-2389.4).
            let read = change_texts(&p.change, dialect, scratch);
            let expressions = expressions(&p.change);
            // Call-position hints read code only; literals remain reference
            // guesses. PG trials resolve both hints (DEC-2465.1).
            let called: Vec<_> = expressions
                .iter()
                .map(|text| {
                    if guessed {
                        std::borrow::Cow::Owned(dialect.lexicon().code_only(text))
                    } else {
                        std::borrow::Cow::Borrowed(*text)
                    }
                })
                .collect();
            for (function, id, module) in &modules {
                if module.kind == ModuleKind::Function
                    && called.iter().any(|t| dialect.generation_calls(t, id))
                {
                    // A call spelling can also be a parameter or a column.
                    // PG scratch resolves the guess by trials (DEC-2465.1).
                    if guessed {
                        guess(&mut out, *function, *i);
                    } else {
                        edge(&mut out, *function, *i);
                    }
                } else if module.kind == ModuleKind::Function
                    && guessed
                    && expressions.iter().any(|t| dialect.generation_calls(t, id))
                {
                    guess(&mut out, *function, *i);
                } else if (module.kind != ModuleKind::Function || !guessed)
                    && read.iter().any(|t| dialect.generation_calls(t, id))
                {
                    // A view or table can be named as a type, a function
                    // cannot: where guesses are read exactly, a function in
                    // a type spelling is no reference at all.
                    guess(&mut out, *function, *i);
                }
            }
        }
        for (i, p) in cs.changes.iter().enumerate() {
            let uses = table_use(&p.change);
            // Creation parts have their own column/validator chain above.
            // Every other structural consumer must wait for that chain.
            if uses.complete.contains(&name)
                && !own.iter().any(|(part, _)| *part == i)
                && !(demanded && matches!(p.change, Change::AddForeignKey { .. }))
            {
                finish(&mut out, i);
            }
            if uses.seed == Some(name) {
                finish(&mut out, i);
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
            let references: Vec<_> = change_texts(&p.change, dialect, scratch)
                .into_iter()
                .filter(|text| dialect.generation_names_table(text, name))
                .collect();
            if !references.is_empty() {
                let name_only = references
                    .iter()
                    .all(|text| dialect.generation_table_name_only(text, name));
                if name_only || whole {
                    guess(&mut out, first, i);
                } else {
                    for &shape in &complete {
                        guess(&mut out, shape, i);
                    }
                }
            }
        }
        for (i, p) in cs.changes.iter().enumerate() {
            if let Change::AddForeignKey {
                table, constraint, ..
            } = &p.change
            {
                if table == name || &constraint.references_table == name {
                    edge(&mut out, first, i);
                    inputs(&mut out, i, &p.change);
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
            let text = named_text(module, dialect, scratch);
            if dialect.generation_names_table(&text, name) {
                // Called readers already chose a full-shape placeholder or
                // the whole-table hold; every consumer sees that full shape.
                if let Some(columns) = reads(dialect, &[&text], name) {
                    wait_for(dialect, &mut out, first, &own, &columns, *i, guessed);
                } else {
                    if whole {
                        guess(&mut out, first, *i);
                    } else {
                        for &shape in &complete {
                            guess(&mut out, shape, *i);
                        }
                    }
                }
            }
        }
    }
    if scratch {
        scratch_rules(cs, declared, dialect, guessed, &mut out);
    }
    let infer_modules = !out.constraints().is_empty();
    // A function moved past a held table reader must not pass a module it
    // calls. Keep the same lexical and declared prerequisites as the differ.
    for (provider, consumer, declared) in module_dependencies(&modules, deps, dialect, scratch) {
        if declared {
            out.edge(
                provider,
                consumer,
                Source::Annotation,
                OrderReason::Structural,
            );
        } else if infer_modules {
            guess(&mut out, provider, consumer);
        }
    }
    out.constraints().iter().cloned().collect()
}

/// The indexes a change creates and keeps nameable before any module, by
/// name: an index that holds no expression, a unique constraint and a primary
/// key are each a relation of that name where indexes share the namespace of
/// tables, and a new table brings the ones it keeps inline.
#[allow(clippy::wildcard_enum_match_arm)]
fn early_index_names(change: &Change) -> Vec<&str> {
    match change {
        Change::AddIndex { name, index, .. } if !index.holds_expression() => vec![name.raw()],
        Change::AddUnique { name, .. } => vec![name.raw()],
        Change::SetPrimaryKey { to: Some(key), .. } => key.name.iter().map(|n| n.raw()).collect(),
        Change::CreateTable { table, .. } => table
            .primary_key
            .iter()
            .flat_map(|key| key.name.iter())
            .map(|n| n.raw())
            .chain(table.unique.keys().map(|n| n.raw()))
            .chain(
                table
                    .indexes
                    .iter()
                    .filter(|(_, index)| !index.holds_expression())
                    .map(|(name, _)| name.raw()),
            )
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether the resolver judges what this change binds against the names made
/// resolvable after it: a module, and a generated expression, wherever the
/// engine installs it.
#[allow(clippy::wildcard_enum_match_arm)]
fn is_judged(change: &Change) -> bool {
    match change {
        Change::CreateModule { .. }
        | Change::AlterModule { .. }
        | Change::AlterColumnExpression { .. } => true,
        Change::AddColumn { column, .. } => {
            matches!(column.value_source, ValueSource::Generated(_))
        }
        Change::CreateTable { table, .. } => table
            .columns
            .values()
            .any(|c| matches!(c.value_source, ValueSource::Generated(_))),
        _ => false,
    }
}

/// What a domain's definition names: its default and checks, and the base type
/// as `types` says.
fn domain_texts(definition: &pbps_model::ManagedType, types: Types) -> Vec<&str> {
    match definition {
        pbps_model::ManagedType::Domain {
            base,
            default,
            checks,
            ..
        } => spelling(base, types)
            .into_iter()
            .chain(default.as_deref())
            .chain(checks.values().map(String::as_str))
            .collect(),
        pbps_model::ManagedType::Enum { .. } => Vec::new(),
    }
}

/// What the resolver's scratch reconstruction orders beyond the lexical and
/// structural constraints above, each measured on PostgreSQL 16 and 18
/// (DEC-2389.1), in an engine whose indexes share the namespace of tables:
///
/// - an index is a relation that a reader may name by an OID-alias literal
///   (`'app.ix'::regclass`), and the engine refuses the reader while the
///   index does not exist (SQLSTATE 42P01): an index that holds no
///   expression precedes every change whose text names it;
/// - an index that holds an expression or a predicate follows every module
///   and generated expression, and the modules it may call are what the
///   engine needs before it (SQLSTATE 42883 without them): what is judged
///   then counts the index among the names made resolvable after it;
/// - a partition's own default follows the default its parent sets on the
///   same column, because the parent's `SET DEFAULT` reaches every partition
///   and replaces the partition's own;
/// - a declared enum or domain precedes every change whose text names it,
///   a domain over it included (DEC-2407.1): the engine refuses a column,
///   routine or domain over a type that does not exist, and a type is
///   needed by nothing a bootstrap makes.
#[allow(clippy::wildcard_enum_match_arm)]
fn scratch_rules(
    cs: &ChangeSet,
    declared: &Schema,
    dialect: &(impl Dialect + ?Sized),
    guessed: bool,
    out: &mut Schedule,
) {
    let edge = |out: &mut Schedule, a, b| {
        if a != b {
            out.edge(a, b, Source::Lexical, OrderReason::Structural);
        }
    };
    // The edges of the rules that match a name in a text (DEC-2389.4).
    let guess = |out: &mut Schedule, a, b| {
        if guessed {
            if a != b {
                out.prefer(a, b, OrderReason::Structural);
            }
        } else {
            edge(out, a, b);
        }
    };
    // What each change reads, with the type spellings `types` says: a name in
    // a type position names a type, so the type rule reads them and the rule
    // for indexes, which are no type, does not (DEC-2389.4).
    let readers = |types: Types| -> Vec<(usize, Vec<std::borrow::Cow<'_, str>>)> {
        cs.changes
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let module = match &p.change {
                    Change::CreateModule { module, .. } | Change::AlterModule { module, .. } => {
                        Some(dialect.module_creation_text(module))
                    }
                    _ => None,
                };
                let domain = match &p.change {
                    Change::CreateType { definition, .. } => domain_texts(definition, types),
                    _ => Vec::new(),
                };
                let all = gather(&p.change, types)
                    .into_iter()
                    .chain(domain)
                    .map(std::borrow::Cow::Borrowed)
                    .chain(module)
                    .collect();
                (i, all)
            })
            .collect()
    };
    let type_readers = readers(reading_types(dialect, true));
    for (at, p) in cs.changes.iter().enumerate() {
        let Change::CreateType { name, .. } = &p.change else {
            continue;
        };
        for (reader, texts) in &type_readers {
            if texts
                .iter()
                .any(|t| dialect.generation_may_call(t, name.name()))
            {
                guess(out, at, *reader);
            }
        }
    }
    if dialect.indexes_share_namespace_with_tables() {
        let name_readers = readers(if guessed { Types::Omit } else { Types::All });
        for (at, p) in cs.changes.iter().enumerate() {
            for name in early_index_names(&p.change) {
                for (reader, texts) in &name_readers {
                    if texts.iter().any(|t| dialect.generation_may_call(t, name)) {
                        guess(out, at, *reader);
                    }
                }
            }
            if matches!(&p.change, Change::AddIndex { index, .. } if index.holds_expression()) {
                for (judged, q) in cs.changes.iter().enumerate() {
                    if is_judged(&q.change) {
                        guess(out, judged, at);
                    }
                }
            }
        }
    }
    for (at, p) in cs.changes.iter().enumerate() {
        let Change::AlterColumnDefault { column, .. } = &p.change else {
            continue;
        };
        let mut table = &column.table;
        let mut seen = BTreeSet::new();
        while let Some(of) = declared
            .tables
            .get(table)
            .and_then(|t| t.partition_of.as_ref())
            && seen.insert(table)
        {
            for (parent, q) in cs.changes.iter().enumerate() {
                if matches!(&q.change, Change::AlterColumnDefault { column: c, .. }
                    if c.table == of.parent && c.name == column.name)
                {
                    edge(out, parent, at);
                }
            }
            table = &of.parent;
        }
    }
}

/// The generated schedule consumes and returns only an owned intermediate.
pub fn settle(
    plan: &mut super::PlanningPlan,
    declared: &Schema,
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
) -> Result<(), String> {
    let mut edges = plan_constraints(plan, declared, deps, dialect, false);
    edges.extend(plan.annotation_edges()?);
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
