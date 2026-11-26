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

use pbps_model::ModuleChange;
use pbps_model::PartitionChange;
use pbps_model::TableChange;
use pbps_model::operation::{
    Address, NameKind, Named, Names, ObjectKey, Operation, OwnerError, Owners, Snapshot, Spelling,
    base_table_named, table_reference, trigger_table,
};
use pbps_model::{Change, Module, ModuleId, ModuleKind, TableName};

use crate::Dialect;

/// The namespace a name is unique in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Namespace {
    /// A schema's objects that share one namespace with its tables.
    Relations(String),
    /// Declared types and relations with row types; indexes and sequences do not occupy it.
    Types(String),
    /// A schema's routines, where they do not share the tables' namespace.
    Routines(String),
    /// One table's names of one kind, by the table's identity: a table keeps
    /// its columns, its indexes, its constraints and its triggers apart.
    Table(ObjectKey, TableSpace),
    /// Roles, which belong to no schema.
    Roles,
    /// Extensions, which are named once per database and belong to no schema.
    Extensions,
    /// A schema's text search configurations, or its dictionaries: two namespaces, both apart
    /// from the types and the relations (DEC-2728.2).
    Search(pbps_model::SearchKind, String),
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
            Namespace::Types(schema) => write!(f, "{schema}.{} (type)", self.name),
            Namespace::Routines(schema) => write!(f, "{schema}.{} (routine)", self.name),
            Namespace::Table(table, space) => write!(f, "{} ({space:?}) on {table:?}", self.name),
            Namespace::Roles => write!(f, "role {}", self.name),
            Namespace::Extensions => write!(f, "extension {}", self.name),
            Namespace::Search(kind, schema) => {
                write!(f, "{schema}.{} ({})", self.name, kind.engine_word())
            }
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
    /// Uniquely matched clones whose detach keeps their catalog name. These
    /// belong to that batch but release no name to another operation.
    pub detach_retained: BTreeMap<SlotKey, pbps_model::Uid>,
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
) -> Result<Vec<Named>, OwnerError> {
    let ModuleId::Trigger { on, name } = id else {
        return Ok(Vec::new());
    };
    if !dialect.declares_a_constraint(module) {
        return Ok(Vec::new());
    }
    let Some(ObjectKey::Table(uid)) = scope else {
        return Ok(Vec::new());
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
                && let Some(partition) = base_table_named(owners, before, table)?
            {
                out.push(address(partition.table_key(), table.schema.clone()));
            }
        }
        // A release names prospective clones too: creating or attaching a
        // partition before the release would create them. Their slots let
        // the existing settle delay that arrival (DEC-2100.2).
        for op in introduced {
            if let Some((table, parent)) = partition(op)
                && table_reference(&op.input_owners(owners), parent, op.origin().spelling())?
                    .as_ref()
                    == Some(uid.uid())
            {
                let named = address(op.key(owners)?, table.schema.clone());
                if !out.contains(&named) {
                    out.push(named);
                }
            }
        }
    }
    Ok(out)
}

// Other changes introduce no partition into a parent.
#[allow(clippy::wildcard_enum_match_arm)]
fn partition(op: &Operation) -> Option<(&TableName, &TableName)> {
    match op.change() {
        Change::Table(TableChange::CreateTable { name, table, .. }) => {
            table.partition_of.as_ref().map(|of| (name, &of.parent))
        }
        Change::Partition(PartitionChange::AttachPartition { table, parent, .. }) => {
            Some((table, parent))
        }
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
    let parent = op
        .input_owners(owners)
        .table(parent, op.origin().spelling())?;
    let scope = op.key(owners)?;
    let mut triggers = BTreeMap::new();
    for (id, module) in &before.schema.modules {
        if let ModuleId::Trigger { on, name } = id
            && let Some(owner) = table_reference(owners, on, Spelling::Base)?
        {
            triggers.insert((owner, name.clone()), (id, module));
        }
    }
    let previous = preceding(operations, op);
    for earlier in previous {
        if let Some(id @ ModuleId::Trigger { on, name }) = earlier.change().module_id() {
            let owner = earlier
                .input_owners(owners)
                .table(on, earlier.origin().spelling())?;
            let key = (owner, name.clone());
            if let Change::Module(ModuleChange::CreateModule { module, .. })
            | Change::Module(ModuleChange::AlterModule { module, .. }) = earlier.change()
            {
                triggers.insert(key, (id, module));
            } else if matches!(
                earlier.change(),
                Change::Module(ModuleChange::DropModule { .. })
            ) {
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
    if !clones.is_empty()
        && matches!(
            op.change(),
            Change::Partition(PartitionChange::AttachPartition { .. })
        )
    {
        // An attach keeps the table's own constraints. Restate only the
        // ones a clone would collide with, as they stand at this operation;
        // using the final shape would also claim checks installed later.
        let mut held = pbps_model::operation::baseline_names(owners, before)?;
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
    let named_scope = out
        .claims
        .iter()
        .chain(&out.releases)
        .find(|n| n.address.kind == NameKind::Module(ModuleKind::Trigger))
        .and_then(|n| n.address.table.clone());
    let dropping_or_altering = matches!(
        op.change(),
        Change::Module(ModuleChange::DropModule { .. } | ModuleChange::AlterModule { .. })
    );
    let base = if (named_scope.is_none() || dropping_or_altering)
        && let Some(ModuleId::Trigger { on, .. }) = op.change().module_id()
    {
        trigger_table(&op.input_owners(owners), on, op.origin().spelling())?
            .and_then(|uid| before.ids.tables.get(&uid))
    } else {
        None
    };
    let scope = match (named_scope, base) {
        (None, Some(name)) => base_table_named(owners, before, name)?.map(|t| t.table_key()),
        (scope, _) => scope,
    };
    if let Change::Module(ModuleChange::DropModule { id, .. })
    | Change::Module(ModuleChange::AlterModule { id, .. }) = op.change()
    {
        let old_id = match id {
            ModuleId::Trigger { name, .. } => base.map(|on| ModuleId::Trigger {
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
            )?);
        }
    }
    if let Change::Module(ModuleChange::CreateModule { id, module })
    | Change::Module(ModuleChange::AlterModule { id, module }) = op.change()
    {
        out.claims.extend(constraint_names(
            dialect,
            id,
            module,
            before,
            owners,
            scope,
            preceding(operations, op),
        )?);
    }
    Ok(out)
}

/// The same companion names seed the replay and identify managed catalog
/// occupants, so a kept constraint trigger cannot be mistaken for absence.
pub fn baseline_names(
    dialect: &(impl Dialect + ?Sized),
    owners: &dyn Owners,
    before: Snapshot<'_>,
) -> Result<Vec<Named>, OwnerError> {
    let mut out = pbps_model::operation::baseline_names(owners, before)?;
    for (id, module) in &before.schema.modules {
        let scope = match id {
            ModuleId::Trigger { on, .. } => {
                base_table_named(owners, before, on)?.map(|t| t.table_key())
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
        )?);
    }
    Ok(out)
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
        NameKind::Type => vec![Namespace::Types(address.schema.clone())],
        NameKind::Table if dialect.relations_have_row_types() => {
            vec![schema(), Namespace::Types(address.schema.clone())]
        }
        NameKind::Module(ModuleKind::View) if dialect.relations_have_row_types() => {
            vec![schema(), Namespace::Types(address.schema.clone())]
        }
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
        NameKind::Key if dialect.keys_claim_index_name() => vec![index(), constraint()],
        NameKind::Key => vec![constraint()],
        NameKind::Constraint => vec![constraint()],
        NameKind::Policy => vec![table(TableSpace::Policies)],
        NameKind::Column => vec![table(TableSpace::Columns)],
        NameKind::Role => vec![Namespace::Roles],
        NameKind::Extension => vec![Namespace::Extensions],
        NameKind::Search(kind) => vec![Namespace::Search(kind, address.schema.clone())],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DialectError, Lexicon, MinimalDialect, TransactionFraming, TypeChangeRisk};
    use pbps_model::operation::{ModuleEdits, Operations, Spelling};
    use pbps_model::{
        ColumnRef, ColumnType, IdsFile, PartitionBound, PartitionOf, PlannedChange, RoleName,
        Schema, SessionZone, Strategy, Table, Uid, UidKind,
    };

    struct ConstraintTriggers;

    impl Dialect for ConstraintTriggers {
        fn name(&self) -> &'static str {
            "constraint triggers"
        }
        fn lexicon(&self) -> Lexicon {
            MinimalDialect.lexicon()
        }
        fn transaction_framing(&self, zone: &SessionZone) -> TransactionFraming {
            MinimalDialect.transaction_framing(zone)
        }
        fn probe_framing(&self) -> Option<TransactionFraming> {
            None
        }
        fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
            MinimalDialect.normalize_type(ty)
        }
        fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(ident)
        }
        fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
            MinimalDialect.quote_ident(ident)
        }
        fn validate_table(&self, name: &TableName, table: &Table) -> Vec<DialectError> {
            MinimalDialect.validate_table(name, table)
        }
        fn intermediate_tables(
            &self,
            change: &Change,
            strategy: Strategy,
        ) -> Result<Vec<TableName>, DialectError> {
            MinimalDialect.intermediate_tables(change, strategy)
        }
        fn declares_a_constraint(&self, module: &Module) -> bool {
            module.kind == ModuleKind::Trigger
        }
        fn clones_to_partitions(&self, module: &Module) -> bool {
            module.kind == ModuleKind::Trigger
        }
    }

    struct Parent {
        name: TableName,
        uid: Uid,
        at: Spelling,
        answer: Result<Uid, OwnerError>,
        reads: std::cell::Cell<usize>,
    }

    impl Owners for Parent {
        fn table(&self, name: &TableName, spelling: Spelling) -> Result<Uid, OwnerError> {
            assert_eq!(name, &self.name);
            if spelling == self.at {
                self.reads.set(self.reads.get() + 1);
                self.answer.clone()
            } else {
                Ok(self.uid.clone())
            }
        }

        fn column(&self, _: &ColumnRef, _: Spelling) -> Result<Uid, OwnerError> {
            unreachable!("the trigger clone has no column lookup")
        }

        fn role(&self, _: &RoleName, _: Spelling) -> Result<Uid, OwnerError> {
            unreachable!("the trigger clone has no role lookup")
        }
    }

    struct Arrival {
        parent: TableName,
        uid: Uid,
        id: ModuleId,
        module: Module,
        before: Schema,
        ids: IdsFile,
        ops: Operations,
    }
    impl Arrival {
        fn new() -> Self {
            let parent = TableName::new("app", "parent");
            let child = TableName::new("app", "child");
            let uid = Uid::derived(UidKind::Table, "parent", 0);
            let id = ModuleId::Trigger {
                on: parent.clone(),
                name: "audit".into(),
            };
            let module = Module {
                kind: ModuleKind::Trigger,
                definition: "CONSTRAINT AFTER INSERT FOR EACH ROW EXECUTE FUNCTION app.f()".into(),
                options: Default::default(),
                description: None,
            };
            let mut before = Schema::default();
            before.tables.insert(parent.clone(), Table::default());
            before.modules.insert(id.clone(), module.clone());
            let mut ids = IdsFile::default();
            ids.tables.insert(uid.clone(), parent.clone());
            let ops = Operations::of_plan(
                &[PlannedChange::new(Change::Table(
                    TableChange::CreateTable {
                        uid: Uid::derived(UidKind::Table, "child", 0),
                        name: child.clone(),
                        table: Box::new(Table {
                            partition_of: Some(PartitionOf {
                                parent: parent.clone(),
                                bound: PartitionBound::Default,
                                columns: Default::default(),
                            }),
                            ..Default::default()
                        }),
                        beside_default: None,
                        moves_at_most: None,
                    },
                ))],
                ModuleEdits::InPlace,
            );
            Self {
                parent,
                uid,
                id,
                module,
                before,
                ids,
                ops,
            }
        }

        fn snapshot(&self) -> Snapshot<'_> {
            Snapshot {
                schema: &self.before,
                ids: &self.ids,
            }
        }

        fn references(
            &self,
            spelling: Spelling,
        ) -> impl Iterator<Item = (Parent, Result<bool, OwnerError>)> {
            let name = self.parent.shown().to_string();
            let errors = [
                OwnerError::Unmanaged {
                    name: name.clone(),
                    spelling,
                },
                OwnerError::Vacant {
                    name: name.clone(),
                    spelling,
                },
                OwnerError::Names {
                    name: name.clone(),
                    spelling,
                    why: "two holders".into(),
                },
                OwnerError::Outside { spelling },
                OwnerError::Rebuild {
                    name,
                    why: "unreadable rebuild",
                },
            ];
            let parent = move |answer| Parent {
                name: self.parent.clone(),
                uid: self.uid.clone(),
                at: spelling,
                answer,
                reads: std::cell::Cell::new(0),
            };
            std::iter::once((parent(Ok(self.uid.clone())), Ok(true))).chain(errors.into_iter().map(
                move |error| {
                    let expected = if matches!(error, OwnerError::Unmanaged { .. }) {
                        Ok(false)
                    } else {
                        Err(error.clone())
                    };
                    (parent(Err(error)), expected)
                },
            ))
        }
    }

    #[test]
    fn prospective_constraint_clones_require_a_readable_parent_reference() {
        let scene = Arrival::new();
        let before = scene.snapshot();
        let op = &scene.ops.as_slice()[0];
        for (owners, clone) in scene.references(op.origin().spelling()) {
            let scope = base_table_named(&owners, before, &scene.parent)
                .unwrap()
                .map(|table| table.table_key());
            let result = constraint_names(
                &ConstraintTriggers,
                &scene.id,
                &scene.module,
                before,
                &owners,
                scope,
                scene.ops.as_slice(),
            );
            assert_eq!(
                result.map(|named| named.len()),
                clone.map(|clone| 1 + usize::from(clone))
            );
        }
    }

    #[test]
    fn arriving_partitions_require_readable_baseline_trigger_references() {
        let scene = Arrival::new();
        let op = &scene.ops.as_slice()[0];
        for (owners, clone) in scene.references(Spelling::Base) {
            let result = arriving_clones(
                &ConstraintTriggers,
                op,
                &owners,
                scene.snapshot(),
                scene.ops.as_slice(),
            );
            assert_eq!(result.map(|named| named.len()), clone.map(usize::from));
        }
    }

    #[test]
    fn trigger_companions_share_one_table_read_and_allow_a_view_at_a_vacant_table_name() {
        let scene = Arrival::new();
        let ops = Operations::of_plan(
            &[PlannedChange::new(Change::Module(
                ModuleChange::AlterModule {
                    id: scene.id.clone(),
                    module: Box::new(scene.module.clone()),
                },
            ))],
            ModuleEdits::InPlace,
        );
        let op = &ops.as_slice()[0];
        for (owners, expected) in scene.references(op.origin().spelling()) {
            let expected = match expected {
                Err(OwnerError::Vacant { .. }) => Ok(false),
                other => other,
            };
            let names = names(
                &ConstraintTriggers,
                op,
                &owners,
                scene.snapshot(),
                ops.as_slice(),
            );
            assert_eq!(
                names.map(|names| (names.claims.len(), names.releases.len())),
                expected.map(|held| (usize::from(held), usize::from(held)))
            );
            assert_eq!(owners.reads.get(), 1);
        }
    }

    #[test]
    fn named_trigger_claims_skip_the_companion_lookup_and_named_releases_keep_it() {
        let scene = Arrival::new();
        for (change, reads) in [
            (
                ModuleChange::CreateModule {
                    id: scene.id.clone(),
                    module: Box::new(scene.module.clone()),
                },
                1,
            ),
            (
                ModuleChange::DropModule {
                    id: scene.id.clone(),
                    kind: ModuleKind::Trigger,
                },
                2,
            ),
        ] {
            let ops = Operations::of_plan(
                &[PlannedChange::new(Change::Module(change))],
                ModuleEdits::InPlace,
            );
            let op = &ops.as_slice()[0];
            let (owners, _) = scene.references(op.origin().spelling()).next().unwrap();
            names(
                &ConstraintTriggers,
                op,
                &owners,
                scene.snapshot(),
                ops.as_slice(),
            )
            .unwrap();
            assert_eq!(owners.reads.get(), reads);
        }
    }
}
