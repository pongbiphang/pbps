//! What each operation creates, removes, rewrites and requires (#1776 slice
//! S1b-i, DEC-1954.1).
//!
//! `docs/ORDERING.md` states, per kind of change, what has to hold before it
//! runs and what it takes away. [`effects`] is that table in code: an
//! exhaustive match over [`Change`] with no wildcard arm, so a kind that
//! arrives without its effects does not compile, which is the point of the
//! slice. The match reads the operation's own payload. The two snapshots
//! supply only the other end of a change: the baseline says what a drop takes
//! with it and what stands over a column, the desired state says what a
//! created table must be able to rely on. Neither is asked what the step
//! does (DEC-1961.1).
//!
//! # Reading the vocabulary
//!
//! A [`Resource`] is an instance of an object ([`Resource::Object`]) or the
//! values a column or table holds ([`Resource::Column`]). Every kind of
//! change lists what it makes, ends or rewrites, and what it needs, as
//! [`Requirement`]s:
//!
//! - [`Requirement::Exists`]: some instance stands. Where the plan both ends
//!   and recreates the object, this is ambiguous and the edge builder says so
//!   rather than guessing a side.
//! - [`Requirement::Is`]: one named instance stands: the baseline's (a step
//!   that removes what is there) or the plan's (a step that builds on what the
//!   plan leaves).
//! - [`Requirement::Absent`]: nothing of the object stands, because it would
//!   block the step. A dependent in the way, such as a foreign key into a table
//!   about to be dropped.
//! - [`Requirement::Reads`]: the values under a key, after every write of
//!   them. A check or a key validates the rows, and a tightened NOT NULL reads
//!   the NULLs the writes before it leave.
//! - [`Requirement::Bound`]: an input the resolver's evidence binds the object
//!   to. `effects` never builds one: evidence is not in a step's payload.
//!
//! # What is deliberately not here
//!
//! - What an expression or a body names is a lexical question, answered by the
//!   dialect's lexer. It is the engine's, so `Dialect::engine_effects` adds it
//!   (DEC-1954.3), and `Lexical` edges come from S6a.
//! - Name slots, the claims a step makes on an address: S1b-ii (#1964). A step
//!   that reads a table by name is ordered against the renames of the table by
//!   the name it spells (DEC-1954.4), which is the evidence and not a
//!   position.

use std::collections::BTreeSet;

use super::{
    ColumnKey, Definition, Incarnation, ObjectKey, Operation, OwnerError, Owners, PartKey,
    Recorded, Spelling, Transition,
};
use crate::{
    Change, ColumnRef, GrantTarget, IdsFile, IndexKey, ReplicaIdentity, RowKey, Schema, Table,
    TableName, Value, ValueSource,
};

/// A thing a step makes, ends or rewrites.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Resource {
    /// One instance of an object: the baseline's or the plan's.
    Object(ObjectKey, Incarnation),
    /// The values under a key: a column's values, or, under a table's key,
    /// the rows it holds.
    Column(ObjectKey),
}

impl Resource {
    pub fn key(&self) -> &ObjectKey {
        match self {
            Resource::Object(key, _) | Resource::Column(key) => key,
        }
    }
}

/// An input an object is bound to by the resolver's evidence, and the
/// instance of it the binding names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BoundInput {
    pub input: ObjectKey,
    pub incarnation: Incarnation,
}

/// What has to hold before a step runs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Requirement {
    Exists(ObjectKey),
    Is(ObjectKey, Incarnation),
    Bound(ObjectKey, BoundInput),
    Absent(ObjectKey),
    /// The values under the key, after every write of them. Not in the
    /// issue's list of four: `Exists` on a table cannot tell a reader of its
    /// rows from a user of its definition (DEC-1954.2).
    Reads(ObjectKey),
}

impl Requirement {
    /// The key a requirement is about.
    pub fn key(&self) -> &ObjectKey {
        match self {
            Requirement::Exists(key)
            | Requirement::Is(key, _)
            | Requirement::Bound(key, _)
            | Requirement::Absent(key)
            | Requirement::Reads(key) => key,
        }
    }
}

/// One step's effects, from its payload and the other end of its change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effects {
    pub creates: Vec<Resource>,
    pub removes: Vec<Resource>,
    pub rewrites: Vec<Resource>,
    pub requires: Vec<Requirement>,
}

impl Effects {
    pub fn is_empty(&self) -> bool {
        self.creates.is_empty()
            && self.removes.is_empty()
            && self.rewrites.is_empty()
            && self.requires.is_empty()
    }

    /// What `other` adds, merged: the engine's effects beside the portable
    /// ones.
    pub fn merged(mut self, other: Effects) -> Effects {
        for (mine, theirs) in [
            (&mut self.creates, other.creates),
            (&mut self.removes, other.removes),
            (&mut self.rewrites, other.rewrites),
        ] {
            for r in theirs {
                if !mine.contains(&r) {
                    mine.push(r);
                }
            }
        }
        for r in other.requires {
            if !self.requires.contains(&r) {
                self.requires.push(r);
            }
        }
        self
    }
}

/// One side of a change: a state and its identity mapping.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot<'a> {
    pub schema: &'a Schema,
    pub ids: &'a IdsFile,
}

struct Cx<'a> {
    op: &'a Operation,
    owners: &'a dyn Owners,
    before: Snapshot<'a>,
    after: Snapshot<'a>,
    spelling: Spelling,
    fx: Effects,
}

impl<'a> Cx<'a> {
    fn table(&self, name: &TableName) -> Result<Recorded, OwnerError> {
        self.owners.table(name, self.spelling).map(Recorded)
    }

    fn column(&self, column: &ColumnRef) -> Result<ColumnKey, OwnerError> {
        self.owners
            .column(column, self.spelling)
            .map(|uid| ColumnKey::Recorded(Recorded(uid)))
    }

    fn role(&self, name: &str) -> Result<Recorded, OwnerError> {
        self.owners.role(name, self.spelling).map(Recorded)
    }

    /// A name as the baseline spells it. A name the identity file holds no
    /// uid for is skipped by the scans that use it: an unmanaged object is
    /// nobody's key.
    fn base_table(&self, name: &TableName) -> Option<Recorded> {
        self.owners.table(name, Spelling::Base).ok().map(Recorded)
    }

    fn final_table(&self, name: &TableName) -> Option<Recorded> {
        self.owners.table(name, Spelling::Final).ok().map(Recorded)
    }

    fn base_column(&self, column: &ColumnRef) -> Option<ColumnKey> {
        self.owners
            .column(column, Spelling::Base)
            .ok()
            .map(|uid| ColumnKey::Recorded(Recorded(uid)))
    }

    fn final_column(&self, column: &ColumnRef) -> Option<ColumnKey> {
        self.owners
            .column(column, Spelling::Final)
            .ok()
            .map(|uid| ColumnKey::Recorded(Recorded(uid)))
    }

    /// The baseline's table under this uid, with the name it holds.
    fn base_of(&self, owner: &Recorded) -> Option<(&'a TableName, &'a Table)> {
        let name = self.before.ids.tables.get(owner.uid())?;
        Some((name, self.before.schema.tables.get(name)?))
    }

    /// The desired state's table under this uid.
    fn final_of(&self, owner: &Recorded) -> Option<(&'a TableName, &'a Table)> {
        let name = self.after.ids.tables.get(owner.uid())?;
        Some((name, self.after.schema.tables.get(name)?))
    }

    fn requires(&mut self, requirement: Requirement) {
        if !self.fx.requires.contains(&requirement) {
            self.fx.requires.push(requirement);
        }
    }

    fn exists(&mut self, key: ObjectKey) {
        self.requires(Requirement::Exists(key));
    }

    fn rewrites(&mut self, resource: Resource) {
        if !self.fx.rewrites.contains(&resource) {
            self.fx.rewrites.push(resource);
        }
    }

    fn creates(&mut self, resource: Resource) {
        if !self.fx.creates.contains(&resource) {
            self.fx.creates.push(resource);
        }
    }

    fn removes(&mut self, resource: Resource) {
        if !self.fx.removes.contains(&resource) {
            self.fx.removes.push(resource);
        }
    }

    /// The values of a column are written. A reader of the whole table's
    /// rows is ordered after this through the table the step requires, not
    /// by a second write: two retypes of different columns of one table do
    /// not write one thing (DEC-1954.2).
    fn writes_values(&mut self, column: ObjectKey, _owner: &Recorded) {
        self.rewrites(Resource::Column(column));
    }

    fn part(owner: &Recorded, part: PartKey) -> ObjectKey {
        ObjectKey::Part {
            table: owner.clone(),
            part,
        }
    }

    fn not_null(column: ColumnKey) -> ObjectKey {
        ObjectKey::NotNull(column)
    }
}

/// What `op` creates, removes, rewrites and requires.
///
/// `before` and `after` are the baseline and the desired state. A drop reads
/// what stood from the first; a creation reads what its table must rely on
/// from the second. The engine-specific part, what only the dialect's lexer
/// or catalog knows, is `Dialect::engine_effects`, merged by the caller.
pub fn effects(
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
) -> Result<Effects, OwnerError> {
    let mut cx = Cx {
        op,
        owners,
        before,
        after,
        spelling: op.origin().spelling(),
        fx: Effects::default(),
    };
    let defined = op.defines(owners)?;
    for Definition { key, transition } in defined {
        match transition {
            Transition::Ends => cx.removes(Resource::Object(key, Incarnation::Base)),
            Transition::Produces => cx.creates(Resource::Object(key, Incarnation::Final)),
        }
    }
    cx.by_kind()?;
    Ok(cx.fx)
}

impl Cx<'_> {
    #[allow(clippy::too_many_lines)]
    fn by_kind(&mut self) -> Result<(), OwnerError> {
        let op = self.op;
        match op.change() {
            Change::CreateTable {
                uid,
                name,
                table,
                beside_default,
                ..
            } => {
                let owner = Recorded(uid.clone());
                if let Some(of) = &table.partition_of {
                    let parent = self.table(&of.parent)?;
                    self.exists(ObjectKey::Table(parent));
                    self.relies_on_parent_columns(&of.parent)?;
                }
                for fk in table.foreign_keys.values() {
                    if fk.references_table != *name {
                        let referenced = self.table(&fk.references_table)?;
                        self.exists(ObjectKey::Table(referenced));
                    }
                }
                if let Some(default) = beside_default {
                    // The DEFAULT partition's rows of the new range move
                    // into the new table.
                    let default = self.table(default)?;
                    self.exists(ObjectKey::Table(default.clone()));
                    self.rewrites(Resource::Column(ObjectKey::Table(default)));
                    self.creates(Resource::Column(ObjectKey::Table(owner)));
                }
            }
            Change::DropTable {
                uid, detach_from, ..
            } => {
                let owner = Recorded(uid.clone());
                self.removes(Resource::Column(ObjectKey::Table(owner.clone())));
                if let Some(parent) = detach_from {
                    let parent = self.table(parent)?;
                    self.exists(ObjectKey::Table(parent));
                }
                self.takes_its_contents(&owner);
                self.requires_inbound_keys_gone(&owner);
            }
            Change::DetachPartition {
                uid, parent, shape, ..
            }
            | Change::AttachPartition {
                uid, parent, shape, ..
            } => {
                let owner = Recorded(uid.clone());
                self.exists(ObjectKey::Table(owner.clone()));
                let parent = self.table(parent)?;
                self.exists(ObjectKey::Table(parent));
                // The partition's own entries on its parent's columns are the
                // parent's once attached: the writes an attach makes there
                // are what its own alterations are ordered against
                // (DEC-1545.1, DEC-1954.5).
                if let Change::AttachPartition { .. } = op.change() {
                    self.attach_rewrites(&owner, shape)?;
                } else {
                    // Detached, the table holds columns of its own, which
                    // carry the identities the declaration records for them.
                    self.holds_its_own_columns(&owner, shape);
                    self.leaves_parents_columns(&owner);
                }
            }
            Change::RenameTable { uid, .. } => {
                let owner = Recorded(uid.clone());
                self.exists(ObjectKey::Table(owner.clone()));
                self.rewrites(Resource::Object(
                    ObjectKey::Table(owner),
                    Incarnation::Final,
                ));
            }
            Change::AddColumn {
                uid, table, name, ..
            } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner));
                let column = ColumnKey::Recorded(Recorded(uid.clone()));
                self.creates(Resource::Column(ObjectKey::Column(column.clone())));
                let _ = name;
            }
            Change::DropColumn { uid, column } => {
                let owner = self.table(&column.table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                let key = ColumnKey::Recorded(Recorded(uid.clone()));
                self.removes(Resource::Column(ObjectKey::Column(key)));
                self.requires_dependents_of_column_gone(&owner, &column.name);
            }
            Change::RenameColumn { uid, table, .. } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner));
                let key = ObjectKey::Column(ColumnKey::Recorded(Recorded(uid.clone())));
                self.exists(key.clone());
                self.rewrites(Resource::Object(key, Incarnation::Final));
            }
            Change::AlterColumnType {
                uid,
                column,
                from_nullable,
                to_nullable,
                ..
            } => {
                let owner = self.table(&column.table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                let key = ColumnKey::Recorded(Recorded(uid.clone()));
                self.rewrites(Resource::Object(
                    ObjectKey::Column(key.clone()),
                    Incarnation::Final,
                ));
                self.writes_values(ObjectKey::Column(key.clone()), &owner);
                self.nullability_moves(&key, *from_nullable, *to_nullable);
                // A default written for the old type blocks the retype, and
                // one written for the new type follows it (DEC-1168.1).
                if self.default_goes(&owner, &key) {
                    self.requires(Requirement::Absent(ObjectKey::Default(key)));
                }
            }
            Change::AlterColumnNullability {
                uid,
                column,
                to_nullable,
                ..
            } => {
                let owner = self.table(&column.table)?;
                self.exists(ObjectKey::Table(owner));
                let key = ColumnKey::Recorded(Recorded(uid.clone()));
                self.exists(ObjectKey::Column(key.clone()));
                self.nullability_moves(&key, !*to_nullable, *to_nullable);
            }
            Change::AlterColumnDefault {
                uid, column, to, ..
            } => {
                let owner = self.table(&column.table)?;
                self.exists(ObjectKey::Table(owner));
                let column = ObjectKey::Column(ColumnKey::Recorded(Recorded(uid.clone())));
                if to.is_some() {
                    // A new default is written for the type the column ends
                    // with.
                    self.exists(column);
                } else {
                    // Taking one off reads the column as it stands: before
                    // the retype it would block, and before it leaves.
                    self.requires(Requirement::Is(column, Incarnation::Base));
                }
            }
            Change::AlterColumnExpression { uid, column, .. } => {
                let owner = self.table(&column.table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                let key = ColumnKey::Recorded(Recorded(uid.clone()));
                self.exists(ObjectKey::Column(key.clone()));
                self.writes_values(ObjectKey::Column(key.clone()), &owner);
                // A recomputed value may be NULL where the column stood NOT
                // NULL: a relaxation goes first, a tightening after
                // (DEC-1168.1). `Absent` of the NOT NULL says both: it
                // follows the step that ends it and precedes the one that
                // produces it.
                if self.relaxes(&owner, &key) || self.tightens(&owner, &key) {
                    self.requires(Requirement::Absent(Self::not_null(key)));
                }
            }
            Change::AddComputedColumn { table, .. } | Change::DropComputedColumn { table, .. } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                if let Change::DropComputedColumn { name, .. } = op.change() {
                    self.requires_computed_dependents_gone(&owner, name);
                }
            }
            Change::SetColumnDeprecated { uid, column, .. } => {
                let owner = self.table(&column.table)?;
                self.exists(ObjectKey::Table(owner));
                let key = ObjectKey::Column(ColumnKey::Recorded(Recorded(uid.clone())));
                // Metadata only: nothing reads a column's deprecation, so it
                // is not a rewrite of the column, which would order the
                // column's other alterations against it.
                self.exists(key);
            }
            Change::SetReplicaIdentity { uid, to, .. } => {
                let owner = Recorded(uid.clone());
                self.exists(ObjectKey::Table(owner.clone()));
                // The identity binds an index, and an index the plan rebuilds
                // is not the identity until it is set again (DEC-1444.1).
                let target = match to {
                    Some(ReplicaIdentity::PrimaryKey) => Some(PartKey::PrimaryKey),
                    Some(ReplicaIdentity::Unique(n)) => Some(PartKey::Unique(n.clone())),
                    Some(ReplicaIdentity::Index(n)) => Some(PartKey::Index(n.clone())),
                    Some(ReplicaIdentity::Full | ReplicaIdentity::Nothing) | None => None,
                };
                if let Some(part) = &target {
                    self.requires(Requirement::Is(
                        Self::part(&owner, part.clone()),
                        Incarnation::Final,
                    ));
                }
                // And the old identity's index stays until the identity
                // moves: the drop reads it as it is now.
                let old = self
                    .base_of(&owner)
                    .and_then(|(_, t)| match &t.replica_identity {
                        Some(ReplicaIdentity::PrimaryKey) => Some(PartKey::PrimaryKey),
                        Some(ReplicaIdentity::Unique(n)) => Some(PartKey::Unique(n.clone())),
                        Some(ReplicaIdentity::Index(n)) => Some(PartKey::Index(n.clone())),
                        Some(ReplicaIdentity::Full | ReplicaIdentity::Nothing) | None => None,
                    });
                // Read, not rewritten: the old index's own alterations are
                // not ordered by where the identity moves. It is read only
                // when the target already stands, so the switch goes first
                // and the old index can then be dropped (ORDERING.md, at
                // (0, 1)); a target the plan adds is switched to after the
                // additions, the old index long since dropped. When the
                // identity stays on the one index the plan rebuilds, the
                // switch is the later `Is(Final)` above, and the old
                // incarnation is not one to read.
                // "Stands" is the differ's own test: the target is usable in
                // the baseline as it is (a key over nullable columns is not,
                // and the identity then goes through FULL and is set last),
                // and the plan leaves it as it is.
                let stands = match (self.base_of(&owner), self.final_of(&owner), &target) {
                    (Some((_, was)), Some((_, will)), Some(part)) => {
                        let mut probe = was.clone();
                        probe.replica_identity.clone_from(to);
                        let same = match part {
                            PartKey::PrimaryKey => was.primary_key == will.primary_key,
                            PartKey::Unique(n) => was.unique.get(n) == will.unique.get(n),
                            PartKey::Index(n) => was.indexes.get(n) == will.indexes.get(n),
                            PartKey::ForeignKey(_) | PartKey::Check(_) | PartKey::Computed(_) => {
                                false
                            }
                        };
                        same && probe.replica_identity_problems().is_empty()
                    }
                    _ => false,
                };
                if let Some(part) = old
                    && stands
                    && target.as_ref() != Some(&part)
                {
                    self.requires(Requirement::Is(Self::part(&owner, part), Incarnation::Base));
                }
            }
            Change::SetStorageParameters { uid, .. } | Change::SetTablePersistence { uid, .. } => {
                self.exists(ObjectKey::Table(Recorded(uid.clone())));
            }
            Change::SetPartitionDefault {
                uid,
                parent,
                column,
                to,
                fallback,
                ..
            } => {
                self.exists(ObjectKey::Table(Recorded(uid.clone())));
                self.exists(ObjectKey::Column(ColumnKey::Partition {
                    table: Recorded(uid.clone()),
                    column: column.clone(),
                }));
                // The parent's `SET DEFAULT` and `DROP DEFAULT` reach every
                // partition and overwrite its own, so what sets the
                // partition's default follows whatever the plan does to the
                // parent's (DEC-1687.1, DEC-1954.5). Its release has nothing
                // to keep.
                if to.is_some() || fallback.is_some() {
                    self.after_parent(parent, column, ParentAttribute::Default);
                }
            }
            Change::SetPartitionNotNull {
                uid,
                column,
                not_null,
                ..
            } => {
                let owner = Recorded(uid.clone());
                self.exists(ObjectKey::Table(owner.clone()));
                let key = ColumnKey::Partition {
                    table: owner.clone(),
                    column: column.clone(),
                };
                self.exists(ObjectKey::Column(key.clone()));
                if let Some((_, own)) = self.final_of(&owner)
                    && let Some(of) = &own.partition_of
                {
                    let parent = of.parent.clone();
                    self.after_parent(&parent, column, ParentAttribute::NotNull);
                }
                if *not_null {
                    // The partition's own rows, with whatever a create beside
                    // a DEFAULT moved into it, must hold no NULL.
                    self.requires(Requirement::Reads(ObjectKey::Table(owner)));
                }
            }
            Change::SetIndexStorageParameters { table, target, .. } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                self.exists(Self::part(&owner, PartKey::from(target)));
            }
            Change::SetPrimaryKey {
                table, from, to, ..
            } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                if from.is_some()
                    && let Some(old) = from
                {
                    self.requires_key_unreferenced(&owner, &old.columns);
                }
                if let Some(new) = to {
                    self.requires_valid_data(&owner);
                    for c in &new.columns {
                        self.requires_column(table, c, true);
                    }
                }
            }
            Change::AddUnique {
                table, constraint, ..
            } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                self.requires_valid_data(&owner);
                for c in &constraint.columns {
                    self.requires_column(table, c, false);
                }
            }
            Change::DropUnique { table, .. } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner));
            }
            Change::AddForeignKey {
                table, constraint, ..
            } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                self.requires_valid_data(&owner);
                for c in &constraint.columns {
                    self.requires_column(table, c, false);
                }
                let referenced = self.table(&constraint.references_table)?;
                self.exists(ObjectKey::Table(referenced.clone()));
                for c in &constraint.references_columns {
                    self.requires_column(&constraint.references_table, c, false);
                }
                // The key it references, as the plan leaves it.
                if let Some((_, target)) = self.final_of(&referenced)
                    && let Some(part) = key_of(target, &constraint.references_columns)
                {
                    self.requires(Requirement::Is(
                        Self::part(&referenced, part),
                        Incarnation::Final,
                    ));
                }
            }
            Change::DropForeignKey { table, name } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                // The key it references stands until the foreign key is
                // gone.
                if let Some((_, own)) = self.base_of(&owner)
                    && let Some(fk) = own.foreign_keys.get(name)
                    && let Some(referenced) = self.base_table(&fk.references_table)
                    && let Some((_, target)) = self.base_of(&referenced)
                    && let Some(part) = key_of(target, &fk.references_columns)
                {
                    self.requires(Requirement::Is(
                        Self::part(&referenced, part),
                        Incarnation::Base,
                    ));
                }
            }
            Change::AddCheck { table, .. } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                self.requires_valid_data(&owner);
            }
            Change::DropCheck { table, .. } | Change::DropIndex { table, .. } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner));
            }
            Change::AddIndex { table, index, .. } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                if index.unique {
                    self.requires_valid_data(&owner);
                }
                for key in index.columns.iter().map(|c| &c.key) {
                    if let IndexKey::Column(c) = key {
                        self.requires_column(table, c, false);
                    }
                }
                for c in &index.include {
                    self.requires_column(table, c, false);
                }
            }
            Change::InsertRow {
                table,
                key_column,
                key,
                row,
                defaults,
                ..
            } => {
                let owner = self.table(table)?;
                self.row_basics(&owner, table, key);
                let mut written = vec![key_column.as_str()];
                written.extend(row.columns().map(|(c, _)| c.as_str()));
                written.extend(defaults.keys().map(String::as_str));
                self.row_writes(table, &owner, &written);
                for (c, expression) in defaults {
                    let _ = expression;
                    if let Ok(column) = self.column(&table.column(c.as_str())) {
                        self.requires(Requirement::Is(
                            ObjectKey::Default(column),
                            Incarnation::Final,
                        ));
                    }
                }
                self.row_meets_constraints(&owner, table, &written);
                let cells: Vec<(&str, &Value)> =
                    row.columns().map(|(c, v)| (c.as_str(), v)).collect();
                self.row_references(&owner, &cells, true);
                for (c, v) in &cells {
                    if v.is_null() {
                        self.row_needs_nullable(&owner, table, c);
                    }
                }
                self.creates(Resource::Object(
                    ObjectKey::Row {
                        table: owner,
                        key: key.clone(),
                    },
                    Incarnation::Final,
                ));
            }
            Change::UpdateRow {
                table,
                key,
                columns,
                ..
            } => {
                let owner = self.table(table)?;
                self.row_basics(&owner, table, key);
                let written: Vec<&str> = columns.keys().map(String::as_str).collect();
                self.row_writes(table, &owner, &written);
                self.row_meets_constraints(&owner, table, &written);
                let mut cells: Vec<(&str, &Value)> = Vec::new();
                for (c, (from, to)) in columns {
                    if let crate::Cell::Value(v) = to {
                        cells.push((c.as_str(), v));
                        if v.is_null() {
                            self.row_needs_nullable(&owner, table, c);
                        }
                    }
                    let _ = from;
                }
                self.row_references(&owner, &cells, true);
                let old: Vec<(&str, &Value)> = columns
                    .iter()
                    .filter_map(|(c, (from, _))| match from {
                        crate::Cell::Value(v) => Some((c.as_str(), v)),
                        crate::Cell::Default(_) => None,
                    })
                    .collect();
                self.row_references(&owner, &old, false);
                self.rewrites(Resource::Object(
                    ObjectKey::Row {
                        table: owner,
                        key: key.clone(),
                    },
                    Incarnation::Final,
                ));
            }
            Change::DeleteRow {
                table, key, row, ..
            } => {
                let owner = self.table(table)?;
                self.row_basics(&owner, table, key);
                let written: Vec<&str> = row.keys().map(String::as_str).collect();
                self.row_writes(table, &owner, &written);
                let mut cells: Vec<(&str, &Value)> = Vec::new();
                for (c, cell) in row {
                    if let crate::Cell::Value(v) = cell {
                        cells.push((c.as_str(), v));
                    }
                }
                self.row_references(&owner, &cells, false);
                self.row_blockers_gone(&owner);
            }
            Change::SetDataMode { table, .. } | Change::SetEnsureKeys { table, .. } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                self.requires(Requirement::Reads(ObjectKey::Table(owner)));
            }
            Change::CreateModule { .. } | Change::DropModule { .. } => {}
            Change::AlterModule { id, .. } => {
                if matches!(op.modules, super::ModuleEdits::InPlace) {
                    let key = ObjectKey::Module(id.clone());
                    self.exists(key.clone());
                    self.rewrites(Resource::Object(key, Incarnation::Final));
                }
            }
            Change::CreateRole { .. } | Change::DropRole { .. } => {}
            Change::RenameRole { uid, .. } => {
                let key = ObjectKey::Role(Recorded(uid.clone()));
                self.exists(key.clone());
                self.rewrites(Resource::Object(key, Incarnation::Final));
            }
            // A grant gives a rebuilt object's permissions back, so it follows
            // the rebuild; a revoke leaves while the object stands.
            Change::Grant { role, target, .. } => self.permission(role, target, true)?,
            Change::Revoke { role, target, .. } => self.permission(role, target, false)?,
            Change::PublicExecution { routine, .. } => {
                self.requires(Requirement::Is(
                    ObjectKey::Module(crate::ModuleId::Routine(routine.clone())),
                    Incarnation::Final,
                ));
                let key = op.key(self.owners)?;
                self.rewrites(Resource::Object(key, Incarnation::Final));
            }
        }
        Ok(())
    }

    fn permission(
        &mut self,
        role: &str,
        target: &GrantTarget,
        gives: bool,
    ) -> Result<(), OwnerError> {
        // A principal no identity file records is the cluster's: no
        // requirement the plan could satisfy.
        if let Ok(role) = self.role(role) {
            self.exists(ObjectKey::Role(role));
        }
        if let Some(target) = self.target(target) {
            if gives {
                self.requires(Requirement::Is(target, Incarnation::Final));
            } else if matches!(&target, ObjectKey::Module(id) if self.before.schema.modules.contains_key(id))
            {
                // A revoke reads the module that stands: it is not ordered
                // after an in-place alteration of its definition, which an
                // `Exists` would do, and it leaves before a rebuild ends it.
                self.requires(Requirement::Is(target, Incarnation::Base));
            } else {
                self.exists(target);
            }
        }
        let key = self.op.key(self.owners)?;
        self.rewrites(Resource::Object(key, Incarnation::Final));
        Ok(())
    }

    /// The object a grant is on, when the plan's names say what it is.
    fn target(&self, target: &GrantTarget) -> Option<ObjectKey> {
        match target {
            GrantTarget::Object(name) => {
                if self.after.schema.tables.contains_key(name)
                    || self.before.schema.tables.contains_key(name)
                {
                    self.table(name).ok().map(ObjectKey::Table)
                } else {
                    Some(ObjectKey::Module(crate::ModuleId::Named(name.clone())))
                }
            }
            GrantTarget::Routine(routine) => {
                Some(ObjectKey::Module(crate::ModuleId::Routine(routine.clone())))
            }
            GrantTarget::Schema(_) => None,
        }
    }

    /// A column the step needs, in the final type if it reads values.
    fn requires_column(&mut self, table: &TableName, column: &str, _strict: bool) {
        if let Ok(key) = self.column(&table.column(column)) {
            self.exists(ObjectKey::Column(key));
        }
    }

    /// A constraint over the rows validates them as the plan leaves them.
    fn requires_valid_data(&mut self, owner: &Recorded) {
        self.requires(Requirement::Reads(ObjectKey::Table(owner.clone())));
    }

    /// A partition is created with its own entries on its parent's columns as
    /// they end (DEC-1687.1).
    fn relies_on_parent_columns(&mut self, parent: &TableName) -> Result<(), OwnerError> {
        let Some(parent_table) = self.after.schema.tables.get(parent) else {
            // The parent as the desired state names it, or as the plan names
            // it at this position: either way a missing table has no
            // columns to rely on.
            let by_uid = self.table(parent).ok().and_then(|o| self.final_of(&o));
            return match by_uid {
                Some((name, t)) => self.relies_on(name, t),
                None => Ok(()),
            };
        };
        self.relies_on(parent, parent_table)
    }

    fn relies_on(&mut self, parent: &TableName, table: &Table) -> Result<(), OwnerError> {
        for (name, column) in &table.columns {
            let Some(key) = self.final_column(&parent.column(name.as_str())) else {
                continue;
            };
            self.exists(ObjectKey::Column(key.clone()));
            if has_expression(&column.value_source) {
                self.exists(ObjectKey::Default(key));
            }
        }
        Ok(())
    }

    fn attach_rewrites(&mut self, owner: &Recorded, shape: &Table) -> Result<(), OwnerError> {
        for name in partition_columns(shape) {
            let key = ColumnKey::Partition {
                table: owner.clone(),
                column: name.clone(),
            };
            self.rewrites(Resource::Object(
                ObjectKey::Default(key.clone()),
                Incarnation::Final,
            ));
            self.rewrites(Resource::Object(ObjectKey::Column(key), Incarnation::Final));
        }
        for part in table_parts(shape) {
            self.rewrites(Resource::Object(
                Self::part(owner, part),
                Incarnation::Final,
            ));
        }
        // The table's own columns become its parent's: they end, after the
        // steps that still address them, and a default of their own is
        // released first.
        if let Some((name, table)) = self.base_of(owner) {
            let name = name.clone();
            for c in table.columns.keys() {
                let Some(key) = self.base_column(&name.column(c.as_str())) else {
                    continue;
                };
                self.exists(ObjectKey::Column(key.clone()));
                self.removes(Resource::Object(ObjectKey::Column(key), Incarnation::Base));
            }
        }
        for c in partition_columns(shape) {
            self.creates(Resource::Object(
                ObjectKey::Column(ColumnKey::Partition {
                    table: owner.clone(),
                    column: c.clone(),
                }),
                Incarnation::Final,
            ));
        }
        Ok(())
    }

    /// A partition's own entry on a parent's column follows what the plan
    /// does to the parent's: its default or NOT NULL as the plan leaves it,
    /// or gone.
    fn after_parent(&mut self, parent: &TableName, column: &str, attribute: ParentAttribute) {
        let Ok(key) = self.column(&parent.column(column)) else {
            return;
        };
        let (Ok(owner), true) = (self.table(parent), true) else {
            return;
        };
        let (Some((_, was)), Some((_, will))) = (self.base_of(&owner), self.final_of(&owner))
        else {
            return;
        };
        let (Some(was), Some(will)) = (was.columns.get(column), will.columns.get(column)) else {
            return;
        };
        match attribute {
            ParentAttribute::Default => {
                let object = ObjectKey::Default(key);
                if has_default(&will.value_source) {
                    self.requires(Requirement::Is(object, Incarnation::Final));
                } else if has_default(&was.value_source) {
                    self.requires(Requirement::Absent(object));
                }
            }
            ParentAttribute::NotNull => {
                let object = Self::not_null(key);
                if !will.nullable {
                    self.requires(Requirement::Is(object, Incarnation::Final));
                } else if !was.nullable {
                    self.requires(Requirement::Absent(object));
                }
            }
        }
    }

    /// A partition's entries on its parent's columns end when it leaves, so
    /// what releases one of its own (a default, a NOT NULL) runs before.
    fn leaves_parents_columns(&mut self, owner: &Recorded) {
        let Some((_, table)) = self.base_of(owner) else {
            return;
        };
        let mut names: BTreeSet<&String> = table.columns.keys().collect();
        if let Some(of) = &table.partition_of {
            names.extend(of.columns.keys());
        }
        for c in names {
            self.removes(Resource::Object(
                ObjectKey::Column(ColumnKey::Partition {
                    table: owner.clone(),
                    column: c.clone(),
                }),
                Incarnation::Base,
            ));
        }
    }

    fn holds_its_own_columns(&mut self, owner: &Recorded, shape: &Table) {
        let Some((name, _)) = self.final_of(owner) else {
            return;
        };
        let name = name.clone();
        for (c, column) in &shape.columns {
            let Some(key) = self.final_column(&name.column(c.as_str())) else {
                continue;
            };
            self.creates(Resource::Object(
                ObjectKey::Column(key.clone()),
                Incarnation::Final,
            ));
            if has_expression(&column.value_source) {
                self.creates(Resource::Object(
                    ObjectKey::Default(key),
                    Incarnation::Final,
                ));
            }
        }
    }

    /// A drop takes with it whatever the baseline holds under the table.
    fn takes_its_contents(&mut self, owner: &Recorded) {
        let Some((name, table)) = self.base_of(owner) else {
            return;
        };
        for (column_name, column) in &table.columns {
            let Some(key) = self.base_column(&name.column(column_name.as_str())) else {
                continue;
            };
            self.removes(Resource::Object(
                ObjectKey::Column(key.clone()),
                Incarnation::Base,
            ));
            self.removes(Resource::Column(ObjectKey::Column(key.clone())));
            if has_expression(&column.value_source) {
                self.removes(Resource::Object(ObjectKey::Default(key), Incarnation::Base));
            }
            if !column.nullable
                && let Some(key) = self.base_column(&name.column(column_name.as_str()))
            {
                self.removes(Resource::Object(Self::not_null(key), Incarnation::Base));
            }
        }
        for part in table_parts(table) {
            self.removes(Resource::Object(Self::part(owner, part), Incarnation::Base));
        }
    }

    /// Foreign keys of other tables that reference this one must be gone.
    fn requires_inbound_keys_gone(&mut self, owner: &Recorded) {
        let Some((name, _)) = self.base_of(owner) else {
            return;
        };
        let name = name.clone();
        let mut gone = Vec::new();
        for (other_name, other) in &self.before.schema.tables {
            if *other_name == name {
                continue;
            }
            let Some(other_owner) = self.base_table(other_name) else {
                continue;
            };
            for (fk_name, fk) in &other.foreign_keys {
                if fk.references_table == name {
                    gone.push(Self::part(
                        &other_owner,
                        PartKey::ForeignKey(fk_name.clone()),
                    ));
                }
            }
        }
        for key in gone {
            self.requires(Requirement::Absent(key));
        }
    }

    /// The indexes, keys and foreign keys that name a column stand in the way
    /// of dropping it.
    fn requires_dependents_of_column_gone(&mut self, owner: &Recorded, column: &str) {
        let Some((name, table)) = self.base_of(owner) else {
            return;
        };
        let name = name.clone();
        let mut gone = Vec::new();
        if let Some(pk) = &table.primary_key
            && pk.columns.iter().any(|c| c == column)
        {
            gone.push(Self::part(owner, PartKey::PrimaryKey));
        }
        for (n, u) in &table.unique {
            if u.columns.iter().any(|c| c == column) {
                gone.push(Self::part(owner, PartKey::Unique(n.clone())));
            }
        }
        for (n, fk) in &table.foreign_keys {
            if fk.columns.iter().any(|c| c == column) {
                gone.push(Self::part(owner, PartKey::ForeignKey(n.clone())));
            }
        }
        for (n, index) in &table.indexes {
            let keyed = index
                .columns
                .iter()
                .any(|c| matches!(&c.key, IndexKey::Column(k) if k == column));
            if keyed || index.include.iter().any(|c| c == column) {
                gone.push(Self::part(owner, PartKey::Index(n.clone())));
            }
        }
        for (other_name, other) in &self.before.schema.tables {
            let Some(other_owner) = self.base_table(other_name) else {
                continue;
            };
            for (fk_name, fk) in &other.foreign_keys {
                if fk.references_table == name && fk.references_columns.iter().any(|c| c == column)
                {
                    gone.push(Self::part(
                        &other_owner,
                        PartKey::ForeignKey(fk_name.clone()),
                    ));
                }
            }
        }
        for key in gone {
            self.requires(Requirement::Absent(key));
        }
    }

    /// A foreign key into the old key stands in the way of dropping it.
    fn requires_key_unreferenced(&mut self, owner: &Recorded, columns: &[String]) {
        let Some((name, _)) = self.base_of(owner) else {
            return;
        };
        let name = name.clone();
        let mut gone = Vec::new();
        for (other_name, other) in &self.before.schema.tables {
            let Some(other_owner) = self.base_table(other_name) else {
                continue;
            };
            for (fk_name, fk) in &other.foreign_keys {
                if fk.references_table == name && fk.references_columns == columns {
                    gone.push(Self::part(
                        &other_owner,
                        PartKey::ForeignKey(fk_name.clone()),
                    ));
                }
            }
        }
        for key in gone {
            self.requires(Requirement::Absent(key));
        }
    }

    /// A computed column's own dependents (SQL Server): the indexes, uniques
    /// and checks over it.
    fn requires_computed_dependents_gone(&mut self, owner: &Recorded, column: &str) {
        let Some((_, table)) = self.base_of(owner) else {
            return;
        };
        let mut gone = Vec::new();
        for (n, u) in &table.unique {
            if u.columns.iter().any(|c| c == column) {
                gone.push(Self::part(owner, PartKey::Unique(n.clone())));
            }
        }
        for (n, index) in &table.indexes {
            let keyed = index
                .columns
                .iter()
                .any(|c| matches!(&c.key, IndexKey::Column(k) if k == column));
            if keyed || index.include.iter().any(|c| c == column) {
                gone.push(Self::part(owner, PartKey::Index(n.clone())));
            }
        }
        for key in gone {
            self.requires(Requirement::Absent(key));
        }
    }

    /// NOT NULL moves with a change of nullability: a relaxation ends the
    /// constraint, a tightening produces it. A tightening reads no values: a
    /// write of a non-NULL value is valid on either side of it, and one of
    /// NULL is refused after it whatever the order, so a read would order
    /// every later write before it and refuse the differ's own plan.
    fn nullability_moves(&mut self, column: &ColumnKey, from_nullable: bool, to_nullable: bool) {
        let key = Self::not_null(column.clone());
        if from_nullable == to_nullable {
            return;
        }
        if to_nullable {
            self.removes(Resource::Object(key, Incarnation::Base));
        } else {
            self.creates(Resource::Object(key, Incarnation::Final));
        }
    }

    /// A column's names on the two sides: the plan may rename it, and a
    /// comparison by one name would read a rename as a drop.
    fn names_of(&self, column: &ColumnKey) -> Option<(String, String)> {
        let ColumnKey::Recorded(Recorded(uid)) = column else {
            return None;
        };
        let was = self.before.ids.columns.get(uid)?;
        let will = self.after.ids.columns.get(uid)?;
        Some((was.name.clone(), will.name.clone()))
    }

    /// Whether the plan takes the column's default away: the baseline has one
    /// and the desired state has a different one or none.
    fn default_goes(&self, owner: &Recorded, column: &ColumnKey) -> bool {
        let (Some((_, was)), Some((_, will))) = (self.base_of(owner), self.final_of(owner)) else {
            return false;
        };
        let Some((was_name, will_name)) = self.names_of(column) else {
            return false;
        };
        let default = |t: &Table, name: &str| match t.columns.get(name).map(|c| &c.value_source) {
            Some(ValueSource::Default(d)) => Some(d.clone()),
            _ => None,
        };
        default(was, &was_name).is_some() && default(was, &was_name) != default(will, &will_name)
    }

    /// Whether the plan tightens the column to NOT NULL.
    fn tightens(&self, owner: &Recorded, column: &ColumnKey) -> bool {
        let (Some((_, was)), Some((_, will))) = (self.base_of(owner), self.final_of(owner)) else {
            return false;
        };
        let Some((was_name, will_name)) = self.names_of(column) else {
            return false;
        };
        match (was.columns.get(&was_name), will.columns.get(&will_name)) {
            (Some(a), Some(b)) => a.nullable && !b.nullable,
            _ => false,
        }
    }

    /// Whether the plan relaxes the column from NOT NULL.
    fn relaxes(&self, owner: &Recorded, column: &ColumnKey) -> bool {
        let (Some((_, was)), Some((_, will))) = (self.base_of(owner), self.final_of(owner)) else {
            return false;
        };
        let Some((was_name, will_name)) = self.names_of(column) else {
            return false;
        };
        match (was.columns.get(&was_name), will.columns.get(&will_name)) {
            (Some(a), Some(b)) => !a.nullable && b.nullable,
            _ => false,
        }
    }

    // Rows.

    fn row_basics(&mut self, owner: &Recorded, table: &TableName, _key: &RowKey) {
        let _ = table;
        self.exists(ObjectKey::Table(owner.clone()));
    }

    fn row_writes(&mut self, table: &TableName, owner: &Recorded, written: &[&str]) {
        self.rewrites(Resource::Column(ObjectKey::Table(owner.clone())));
        for c in written {
            if let Ok(key) = self.column(&table.column(*c)) {
                self.exists(ObjectKey::Column(key.clone()));
                self.rewrites(Resource::Column(ObjectKey::Column(key)));
            }
        }
    }

    /// A row meets the constraints the plan leaves it under, so those it takes
    /// away go first.
    fn row_meets_constraints(&mut self, owner: &Recorded, _table: &TableName, written: &[&str]) {
        let (Some((_, was)), Some((_, will))) = (self.base_of(owner), self.final_of(owner)) else {
            return;
        };
        let mut gone = Vec::new();
        for n in was.checks.keys().filter(|n| !will.checks.contains_key(*n)) {
            gone.push(Self::part(owner, PartKey::Check(n.clone())));
        }
        for n in was.unique.keys().filter(|n| !will.unique.contains_key(*n)) {
            gone.push(Self::part(owner, PartKey::Unique(n.clone())));
        }
        for n in was
            .foreign_keys
            .keys()
            .filter(|n| !will.foreign_keys.contains_key(*n))
        {
            gone.push(Self::part(owner, PartKey::ForeignKey(n.clone())));
        }
        for (n, index) in &was.indexes {
            if (index.unique || index.filter.is_some()) && !will.indexes.contains_key(n) {
                gone.push(Self::part(owner, PartKey::Index(n.clone())));
            }
        }
        // A default the row would take, dropped by the plan: the row is
        // written after it goes, so it does not take it.
        for (name, column) in &was.columns {
            if written.contains(&name.as_str()) {
                continue;
            }
            if matches!(column.value_source, ValueSource::Default(_))
                && let Some((tname, _)) = self.base_of(owner)
                && let Some(key) = self.base_column(&tname.column(name.as_str()))
            {
                // By the column's name on the desired side: a rename keeps
                // its default, and only another default or none takes it away.
                let kept = self.names_of(&key).is_some_and(|(_, now)| {
                    matches!(
                        will.columns.get(&now).map(|c| &c.value_source),
                        Some(ValueSource::Default(_))
                    )
                });
                if !kept {
                    gone.push(ObjectKey::Default(key));
                }
            }
        }
        for key in gone {
            self.requires(Requirement::Absent(key));
        }
    }

    /// A row that holds NULL needs the column relaxed first, when the plan
    /// relaxes it.
    fn row_needs_nullable(&mut self, owner: &Recorded, _table: &TableName, column: &str) {
        if let Some((name, _)) = self.final_of(owner)
            && let Some(key) = self.final_column(&name.column(column))
            && self.relaxes(owner, &key)
        {
            self.requires(Requirement::Absent(Self::not_null(key)));
        }
    }

    /// A row that references a parent row needs it to stand; a row that stops
    /// referencing one leaves before it goes.
    fn row_references(&mut self, owner: &Recorded, cells: &[(&str, &Value)], _adds: bool) {
        // Only a key that stands throughout constrains the row: one the plan
        // adds is installed after the rows it checks, one it drops is gone
        // before them.
        let (Some((_, was)), Some((_, will))) = (self.base_of(owner), self.final_of(owner)) else {
            return;
        };
        let mut wanted = Vec::new();
        for (name, fk) in &will.foreign_keys {
            if was.foreign_keys.get(name) != Some(fk) {
                continue;
            }
            let ([column], [referenced]) =
                (fk.columns.as_slice(), fk.references_columns.as_slice())
            else {
                continue;
            };
            let Some((_, value)) = cells.iter().find(|(c, _)| c == column) else {
                continue;
            };
            let key = match value {
                Value::Int(i) => i.to_string(),
                Value::Text(t) => t.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Null => continue,
            };
            let Some(parent) = self
                .final_table(&fk.references_table)
                .or_else(|| self.base_table(&fk.references_table))
            else {
                continue;
            };
            // A row is keyed by its primary key: a value of any other
            // referenced column does not name a row this plan writes.
            let keyed = self
                .final_of(&parent)
                .or_else(|| self.base_of(&parent))
                .is_some_and(|(_, t)| {
                    t.primary_key
                        .as_ref()
                        .is_some_and(|pk| pk.columns.as_slice() == std::slice::from_ref(referenced))
                });
            if !keyed {
                continue;
            }
            wanted.push(ObjectKey::Row {
                table: parent,
                key: RowKey(key),
            });
        }
        for key in wanted {
            self.exists(key);
        }
    }

    /// The rows the baseline's constraints held: nothing here. Deleting a
    /// row is blocked by a child row only; that is `row_references`' edge.
    fn row_blockers_gone(&mut self, _owner: &Recorded) {}
}

/// Every named part a table holds, as keys' parts.
fn table_parts(table: &Table) -> Vec<PartKey> {
    let mut out = Vec::new();
    if table.primary_key.is_some() {
        out.push(PartKey::PrimaryKey);
    }
    out.extend(table.unique.keys().cloned().map(PartKey::Unique));
    out.extend(table.foreign_keys.keys().cloned().map(PartKey::ForeignKey));
    out.extend(table.checks.keys().cloned().map(PartKey::Check));
    out.extend(table.indexes.keys().cloned().map(PartKey::Index));
    out.extend(table.computed.keys().cloned().map(PartKey::Computed));
    out
}

/// The key of `table` a foreign key over these columns references.
fn key_of(table: &Table, columns: &[String]) -> Option<PartKey> {
    if table
        .primary_key
        .as_ref()
        .is_some_and(|pk| pk.columns == columns)
    {
        return Some(PartKey::PrimaryKey);
    }
    if let Some((name, _)) = table.unique.iter().find(|(_, u)| u.columns == columns) {
        return Some(PartKey::Unique(name.clone()));
    }
    table
        .indexes
        .iter()
        .find(|(_, i)| {
            i.unique
                && i.columns.len() == columns.len()
                && i.columns
                    .iter()
                    .zip(columns)
                    .all(|(c, n)| matches!(&c.key, IndexKey::Column(k) if k == n))
        })
        .map(|(name, _)| PartKey::Index(name.clone()))
}

#[derive(Clone, Copy)]
enum ParentAttribute {
    Default,
    NotNull,
}

/// The columns a partition has entries on: those its shape names, and those
/// it overrides.
fn partition_columns(shape: &Table) -> BTreeSet<&String> {
    let mut names: BTreeSet<&String> = shape.columns.keys().collect();
    if let Some(of) = &shape.partition_of {
        names.extend(of.columns.keys());
    }
    names
}

fn has_default(source: &ValueSource) -> bool {
    matches!(source, ValueSource::Default(_))
}

fn has_expression(source: &ValueSource) -> bool {
    match source {
        ValueSource::Default(_) | ValueSource::Generated(_) => true,
        ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => false,
    }
}

/// Every object the baseline holds that a requirement can name, by key.
///
/// Rows and permissions are not enumerated: the baseline model does not hold
/// the rows a database has, nor its grants, so [`holds_unknown`] says which
/// keys have no answer rather than letting their absence read as "nothing
/// there" (absent, empty and unreadable are three different things).
pub fn baseline_keys(before: Snapshot<'_>, owners: &dyn Owners) -> BTreeSet<ObjectKey> {
    let mut held = BTreeSet::new();
    let table = |name: &TableName| owners.table(name, Spelling::Base).ok().map(Recorded);
    for (name, t) in &before.schema.tables {
        let Some(owner) = table(name) else { continue };
        held.insert(ObjectKey::Table(owner.clone()));
        for (c, column) in &t.columns {
            let Ok(uid) = owners.column(&name.column(c.as_str()), Spelling::Base) else {
                continue;
            };
            let key = ColumnKey::Recorded(Recorded(uid));
            held.insert(ObjectKey::Column(key.clone()));
            if has_expression(&column.value_source) {
                held.insert(ObjectKey::Default(key.clone()));
            }
            if !column.nullable {
                held.insert(ObjectKey::NotNull(key));
            }
        }
        if let Some(of) = &t.partition_of {
            for (c, own) in &of.columns {
                let key = ColumnKey::Partition {
                    table: owner.clone(),
                    column: c.clone(),
                };
                if own.default.is_some() {
                    held.insert(ObjectKey::Default(key.clone()));
                }
            }
        }
        for part in table_parts(t) {
            held.insert(ObjectKey::Part {
                table: owner.clone(),
                part,
            });
        }
    }
    held.extend(before.schema.modules.keys().cloned().map(ObjectKey::Module));
    held.extend(
        before
            .ids
            .roles
            .keys()
            .map(|uid| ObjectKey::Role(Recorded(uid.clone()))),
    );
    held
}

/// Whether the baseline model can say if it holds `key` at all.
pub fn holds_unknown(key: &ObjectKey) -> bool {
    match key {
        ObjectKey::Row { .. } | ObjectKey::Grant { .. } => true,
        ObjectKey::Column(ColumnKey::Partition { .. })
        | ObjectKey::NotNull(ColumnKey::Partition { .. }) => true,
        ObjectKey::Table(_)
        | ObjectKey::Column(ColumnKey::Recorded(_))
        | ObjectKey::Role(_)
        | ObjectKey::Default(_)
        | ObjectKey::Module(_)
        | ObjectKey::NotNull(ColumnKey::Recorded(_))
        | ObjectKey::Part { .. } => false,
    }
}

/// A table of the baseline, found by the name a step spells it with, and the
/// keys of what it holds. The way a dialect builds a key without making up a
/// uid: it asks the owners (DEC-1961.1).
#[derive(Debug, Clone)]
pub struct BaseTable<'a> {
    pub name: &'a TableName,
    pub table: &'a Table,
    owner: Recorded,
}

impl BaseTable<'_> {
    pub fn part(&self, part: PartKey) -> ObjectKey {
        ObjectKey::Part {
            table: self.owner.clone(),
            part,
        }
    }

    pub fn table_key(&self) -> ObjectKey {
        ObjectKey::Table(self.owner.clone())
    }

    /// The key of this table's column, when the baseline's identity file
    /// records one.
    pub fn column(&self, owners: &dyn Owners, column: &str) -> Option<ObjectKey> {
        owners
            .column(&self.name.column(column), Spelling::Base)
            .ok()
            .map(|uid| ObjectKey::Column(ColumnKey::Recorded(Recorded(uid))))
    }
}

/// The baseline's table that `op` means by `name`, if it has one.
pub fn base_table<'a>(
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'a>,
    name: &TableName,
) -> Result<Option<BaseTable<'a>>, OwnerError> {
    let uid = owners.table(name, op.origin().spelling())?;
    let Some(base_name) = before.ids.tables.get(&uid) else {
        return Ok(None);
    };
    let Some(table) = before.schema.tables.get(base_name) else {
        return Ok(None);
    };
    Ok(Some(BaseTable {
        name: base_name,
        table,
        owner: Recorded(uid),
    }))
}

/// A key for a module, so a dialect need not name `ObjectKey` variants.
pub fn module_key(id: &crate::ModuleId) -> ObjectKey {
    ObjectKey::Module(id.clone())
}

/// The baseline's table under a name the baseline itself gives it.
pub fn base_table_named<'a>(
    owners: &dyn Owners,
    before: Snapshot<'a>,
    name: &'a TableName,
) -> Option<BaseTable<'a>> {
    let table = before.schema.tables.get(name)?;
    let uid = owners.table(name, Spelling::Base).ok()?;
    Some(BaseTable {
        name,
        table,
        owner: Recorded(uid),
    })
}
