//! Name slots: the namespace that holds a name, and whether two spellings are
//! one name (#1776 slice S1b-ii, DEC-1964.1).
//!
//! `pbps_model::operation::names` lists the names each step claims and
//! releases, as addresses: a kind, a schema, a table, a name. Which addresses
//! compete is the engine's to say. PostgreSQL keeps tables, views, sequences
//! and indexes in one relation namespace per schema; SQL Server keeps every
//! object and constraint in one per schema, and an index name per table.
//! [`slot`] maps an address to its slot from the dialect's own answers, so no
//! namespace is hard-coded twice.
//!
//! Two spellings in one namespace are one name when the engine compares them
//! equal. PostgreSQL compares identifiers exactly. SQL Server compares them
//! under the database's collation, which a plan computed offline does not
//! know (SPEC 7.3): there the answer is a catalog fact, and without it the
//! check is skipped and reported, never read as "different names".

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::operation::{
    Address, NameKind, Named, Names, ObjectKey, Operation, OwnerError, Owners, Snapshot,
    base_table_named,
};
use pbps_model::{Change, Module, ModuleId, ModuleKind, TableName};

use crate::Dialect;

/// The namespace a name is unique in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Namespace {
    /// A schema's objects that share one namespace with its tables.
    Relations(String),
    /// A schema's routines, where they do not share the tables' namespace.
    Routines(String),
    /// One table's names of one kind, by the table's identity: a table keeps
    /// its columns, its indexes, its constraints and its triggers apart.
    Table(ObjectKey, TableSpace),
    /// Roles, which belong to no schema.
    Roles,
}

/// Which of a table's own namespaces a name is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TableSpace {
    Columns,
    Indexes,
    Constraints,
    Triggers,
    Policies,
}

/// One name in one namespace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotKey {
    pub namespace: Namespace,
    pub name: String,
}

impl std::fmt::Display for SlotKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.namespace {
            Namespace::Relations(schema) => write!(f, "{schema}.{}", self.name),
            Namespace::Routines(schema) => write!(f, "{schema}.{} (routine)", self.name),
            Namespace::Table(table, space) => write!(f, "{} ({space:?}) on {table:?}", self.name),
            Namespace::Roles => write!(f, "role {}", self.name),
        }
    }
}

/// Whether two slots are one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Same {
    Yes,
    No,
    /// The engine compares the two spellings under a rule this plan has no
    /// fact for.
    Unknown,
}

/// What a connected plan's catalog reads say about names: who holds a name
/// the declarations do not account for, and which spellings it compared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogFacts {
    pub occupants: Vec<Occupant>,
    /// Exact catalog releases by a partition's detach, keyed by its recorded
    /// identity rather than a position a later planning pass may change.
    pub detach_releases: BTreeMap<SlotKey, pbps_model::Uid>,
    /// Every slot the catalog was asked to compare.
    pub compared: BTreeSet<SlotKey>,
    /// The pairs among them it reports alike.
    pub alike: BTreeSet<(SlotKey, SlotKey)>,
}

/// A name the catalog holds for an object the declarations do not name: a
/// carried sequence or index, an adopted or fallback default constraint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Occupant {
    pub slot: SlotKey,
    /// The catalog's word for it, for the report.
    pub kind: String,
    /// The table it belongs to, by the baseline's name: it goes where that
    /// table goes and leaves when that table is dropped.
    pub owner: Option<pbps_model::TableName>,
    /// The owner's column it belongs to, by the baseline's name: an owned
    /// sequence, or a column's default constraint.
    pub column: Option<pbps_model::ColumnName>,
}

/// Names whose existence the dialect decides, beside the portable names.
/// A constraint trigger's companion and clones belong to the trigger, not
/// to independently declared constraints (DEC-2100.1).
fn constraint_names(
    dialect: &(impl Dialect + ?Sized),
    id: &ModuleId,
    module: &Module,
    before: Snapshot<'_>,
    owners: &dyn Owners,
    scope: Option<ObjectKey>,
    introduced: &[Operation],
) -> Vec<Named> {
    let ModuleId::Trigger { on, name } = id else {
        return Vec::new();
    };
    if !dialect.declares_a_constraint(module) {
        return Vec::new();
    }
    let Some(ObjectKey::Table(uid)) = scope else {
        return Vec::new();
    };
    let address = |owner, schema: String| Named {
        address: Address {
            kind: NameKind::Constraint,
            schema,
            table: Some(owner),
            name: name.clone(),
            signature: None,
        },
        holder: ObjectKey::Module(id.clone()),
    };
    let mut out = vec![address(ObjectKey::Table(uid.clone()), on.schema.clone())];
    if dialect.clones_to_partitions(module) {
        let ids = before.ids.index();
        for (table, shape) in &before.schema.tables {
            if let Some(of) = &shape.partition_of
                && ids.table_uid(&of.parent) == Some(uid.uid())
                && let Some(partition) = base_table_named(owners, before, table)
            {
                out.push(address(partition.table_key(), table.schema.clone()));
            }
        }
        // A release names prospective clones too: creating or attaching a
        // partition before the release would create them. Their slots let
        // the existing settle delay that arrival (DEC-2100.2).
        for op in introduced {
            if let Some((table, parent)) = partition(op)
                && owners.table(parent, op.origin().spelling()).ok() == Some(uid.uid().clone())
                && let Ok(key) = op.key(owners)
            {
                let named = address(key, table.schema.clone());
                if !out.contains(&named) {
                    out.push(named);
                }
            }
        }
    }
    out
}

// Other changes introduce no partition into a parent.
#[allow(clippy::wildcard_enum_match_arm)]
fn partition(op: &Operation) -> Option<(&TableName, &TableName)> {
    match op.change() {
        Change::CreateTable { name, table, .. } => {
            table.partition_of.as_ref().map(|of| (name, &of.parent))
        }
        Change::AttachPartition { table, parent, .. } => Some((table, parent)),
        _ => None,
    }
}

fn preceding<'a>(operations: &'a [Operation], op: &Operation) -> &'a [Operation] {
    let at = operations
        .iter()
        .position(|p| p.id() == op.id())
        .expect("names are read in the operation list their step belongs to");
    &operations[..at]
}

/// Creating or attaching a partition clones the parent's trigger as it
/// stands then, not as it stood at baseline or will stand in the declaration.
fn arriving_clones(
    dialect: &(impl Dialect + ?Sized),
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    operations: &[Operation],
) -> Result<Vec<Named>, OwnerError> {
    let Some((table, parent)) = partition(op) else {
        return Ok(Vec::new());
    };
    let parent = owners.table(parent, op.origin().spelling())?;
    let scope = op.key(owners)?;
    let mut triggers = BTreeMap::new();
    for (id, module) in &before.schema.modules {
        if let ModuleId::Trigger { on, name } = id
            && let Ok(owner) = owners.table(on, pbps_model::operation::Spelling::Base)
        {
            triggers.insert((owner, name.clone()), (id, module));
        }
    }
    let previous = preceding(operations, op);
    for earlier in previous {
        if let Some(id @ ModuleId::Trigger { on, name }) = earlier.change().module_id() {
            let owner = owners.table(on, earlier.origin().spelling())?;
            let key = (owner, name.clone());
            if let Change::CreateModule { module, .. } | Change::AlterModule { module, .. } =
                earlier.change()
            {
                triggers.insert(key, (id, module));
            } else if matches!(earlier.change(), Change::DropModule { .. }) {
                triggers.remove(&key);
            }
        }
    }
    let mut clones = Vec::new();
    for ((owner, name), (id, module)) in triggers {
        if owner == parent
            && dialect.declares_a_constraint(module)
            && dialect.clones_to_partitions(module)
        {
            clones.push(Named {
                address: Address {
                    kind: NameKind::Constraint,
                    schema: table.schema.clone(),
                    table: Some(scope.clone()),
                    name,
                    signature: None,
                },
                holder: ObjectKey::Module(id.clone()),
            });
        }
    }
    if !clones.is_empty() && matches!(op.change(), Change::AttachPartition { .. }) {
        // An attach keeps the table's own constraints. Restate only the
        // ones a clone would collide with, as they stand at this operation;
        // using the final shape would also claim checks installed later.
        let mut held = pbps_model::operation::baseline_names(owners, before);
        for earlier in previous {
            let names = pbps_model::operation::names(earlier, owners, before)?;
            held.retain(|n| !names.releases.contains(n));
            held.extend(names.claims);
        }
        let clone_slots: BTreeSet<_> = clones
            .iter()
            .flat_map(|n| slots(dialect, &n.address))
            .collect();
        let kept = held.into_iter().filter(|n| {
            matches!(n.address.kind, NameKind::Constraint | NameKind::Key)
                && slots(dialect, &n.address)
                    .iter()
                    .any(|s| clone_slots.contains(s))
        });
        clones.extend(kept);
    }
    Ok(clones)
}

/// An operation's portable names and its dialect's companion names. The
/// old trigger kind is read from the baseline, never from its replacement.
/// `operations` is the order containing `op`, so arrivals read the kind in
/// force at their position rather than guessing it from the final schema.
pub fn names(
    dialect: &(impl Dialect + ?Sized),
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    operations: &[Operation],
) -> Result<Names, OwnerError> {
    let mut out = pbps_model::operation::names(op, owners, before)?;
    out.claims
        .extend(arriving_clones(dialect, op, owners, before, operations)?);
    let scope = out
        .claims
        .iter()
        .chain(&out.releases)
        .find(|n| n.address.kind == NameKind::Module(ModuleKind::Trigger))
        .and_then(|n| n.address.table.clone())
        .or_else(|| match op.change().module_id()? {
            ModuleId::Trigger { on, .. } => owners
                .table(on, op.origin().spelling())
                .ok()
                .and_then(|uid| before.ids.tables.get(&uid))
                .and_then(|name| base_table_named(owners, before, name))
                .map(|table| table.table_key()),
            ModuleId::Named(_) | ModuleId::Routine(_) => None,
        });
    if let Change::DropModule { id, .. } | Change::AlterModule { id, .. } = op.change() {
        let old_id = match id {
            ModuleId::Trigger { on, name } => owners
                .table(on, op.origin().spelling())
                .ok()
                .and_then(|uid| before.ids.tables.get(&uid))
                .map(|on| ModuleId::Trigger {
                    on: on.clone(),
                    name: name.clone(),
                }),
            ModuleId::Named(_) | ModuleId::Routine(_) => None,
        };
        if let Some(module) = old_id.as_ref().and_then(|id| before.schema.modules.get(id)) {
            out.releases.extend(constraint_names(
                dialect,
                id,
                module,
                before,
                owners,
                scope.clone(),
                operations,
            ));
        }
    }
    if let Change::CreateModule { id, module } | Change::AlterModule { id, module } = op.change() {
        out.claims.extend(constraint_names(
            dialect,
            id,
            module,
            before,
            owners,
            scope,
            preceding(operations, op),
        ));
    }
    Ok(out)
}

/// The same companion names seed the replay and identify managed catalog
/// occupants, so a kept constraint trigger cannot be mistaken for absence.
pub fn baseline_names(
    dialect: &(impl Dialect + ?Sized),
    owners: &dyn Owners,
    before: Snapshot<'_>,
) -> Vec<Named> {
    let mut out = pbps_model::operation::baseline_names(owners, before);
    for (id, module) in &before.schema.modules {
        let scope = match id {
            ModuleId::Trigger { on, .. } => {
                base_table_named(owners, before, on).map(|t| t.table_key())
            }
            ModuleId::Named(_) | ModuleId::Routine(_) => None,
        };
        out.extend(constraint_names(
            dialect,
            id,
            module,
            before,
            owners,
            scope,
            &[],
        ));
    }
    out
}

/// The slots `address` takes, by the dialect's namespace answers: one, or
/// two for a named key, which is a constraint backed by an index and holds its
/// name in both namespaces.
pub fn slots(dialect: &(impl Dialect + ?Sized), address: &Address) -> Vec<SlotKey> {
    let schema = || Namespace::Relations(address.schema.clone());
    // A per-table name whose table no identity records has no table to be
    // scoped by; its schema is the narrowest scope there is.
    let table = |space: TableSpace| {
        address
            .table
            .clone()
            .map_or_else(schema, |t| Namespace::Table(t, space))
    };
    let index = || {
        if dialect.indexes_share_namespace_with_tables() {
            schema()
        } else {
            table(TableSpace::Indexes)
        }
    };
    let constraint = || {
        if dialect.constraints_share_namespace_with_tables() {
            schema()
        } else {
            table(TableSpace::Constraints)
        }
    };
    let mut name = address.name.clone();
    let mut namespaces = match address.kind {
        NameKind::Table => vec![schema()],
        NameKind::Module(kind) if dialect.shares_namespace_with_tables(kind) => vec![schema()],
        NameKind::Module(ModuleKind::Trigger) => vec![table(TableSpace::Triggers)],
        NameKind::Module(kind) => {
            if dialect.overloads(kind)
                && let Some(signature) = &address.signature
            {
                name.clone_from(signature);
            }
            vec![Namespace::Routines(address.schema.clone())]
        }
        NameKind::Index => vec![index()],
        NameKind::Key => vec![index(), constraint()],
        NameKind::Constraint => vec![constraint()],
        NameKind::Policy => vec![table(TableSpace::Policies)],
        NameKind::Column => vec![table(TableSpace::Columns)],
        NameKind::Role => vec![Namespace::Roles],
    };
    namespaces.dedup();
    namespaces
        .into_iter()
        .map(|namespace| SlotKey {
            namespace,
            name: name.clone(),
        })
        .collect()
}

/// Whether `a` and `b` are one name: equal slots are; otherwise the engine's
/// rule, or the catalog's answer where the rule is a collation.
pub fn same(
    dialect: &(impl Dialect + ?Sized),
    facts: Option<&CatalogFacts>,
    a: &SlotKey,
    b: &SlotKey,
) -> Same {
    if a == b {
        return Same::Yes;
    }
    // Two tables' own namespaces are two namespaces, whatever their names.
    if matches!((&a.namespace, &b.namespace), (Namespace::Table(x, p), Namespace::Table(y, q)) if (x, p) != (y, q))
        || std::mem::discriminant(&a.namespace) != std::mem::discriminant(&b.namespace)
    {
        return Same::No;
    }
    if dialect.compares_names_exactly() {
        return Same::No;
    }
    match facts {
        Some(facts) if facts.compared.contains(a) && facts.compared.contains(b) => {
            if facts.alike.contains(&(a.clone(), b.clone()))
                || facts.alike.contains(&(b.clone(), a.clone()))
            {
                Same::Yes
            } else {
                Same::No
            }
        }
        _ => Same::Unknown,
    }
}
