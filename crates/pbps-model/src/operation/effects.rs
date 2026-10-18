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
//! - Name slots, the claims a step makes on an address: [`super::names`]
//!   (DEC-1964.1), read beside these by `pbps_diff::order::slots`. A step
//!   that reads a table by name is ordered against the renames of the table by
//!   the name it spells (DEC-1954.4), which is the evidence and not a
//!   position.

use std::collections::BTreeSet;

use super::{
    ColumnKey, Definition, Incarnation, ObjectKey, Operation, OwnerError, Owners, PartKey,
    Recorded, RowSide, Spelling, Transition,
};
use crate::{
    Change, ColumnRef, GrantTarget, IdsFile, IndexKey, ReplicaIdentity, RowKey, Schema, Table,
    TableName, ValueSource,
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
                    // The parent's columns and defaults are not required: a
                    // partition created first is reached by the parent's own
                    // `ADD COLUMN` and `SET DEFAULT`, which recurse, and one
                    // created after copies them; both orders are valid
                    // (DEC-1954.9).
                    self.exists(ObjectKey::Table(parent));
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
                self.exists(ObjectKey::Table(owner.clone()));
                let column = ColumnKey::Recorded(Recorded(uid.clone()));
                self.creates(Resource::Column(ObjectKey::Column(column.clone())));
                self.appends_a_column(owner);
                let _ = name;
            }
            Change::DropColumn { uid, column } => {
                let owner = self.table(&column.table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                let key = ColumnKey::Recorded(Recorded(uid.clone()));
                self.removes(Resource::Column(ObjectKey::Column(key.clone())));
                // What the column owns goes with it: its default, and its
                // NOT NULL.
                if let Some((_, was)) = self.base_of(&owner)
                    && let Some(base) = was.columns.get(&column.name)
                {
                    if has_expression(&base.value_source) {
                        self.removes(Resource::Object(
                            ObjectKey::Default(key.clone()),
                            Incarnation::Base,
                        ));
                    }
                    if !base.nullable {
                        self.removes(Resource::Object(Self::not_null(key), Incarnation::Base));
                    }
                }
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
                // A foreign key the plan drops, over the column or into it,
                // goes first: the engine refuses a retype that leaves the
                // key's two sides incompatible. One that stands throughout is
                // no order (it is rebuilt or kept by the engine).
                self.requires_foreign_keys_over_column_gone(&owner, uid);
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
            Change::AlterColumnNumbering {
                uid,
                column,
                from,
                to,
                ..
            } => {
                let owner = self.table(&column.table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                let key = ColumnKey::Recorded(Recorded(uid.clone()));
                // A source put on, or kept through a retype, follows the
                // column's last rewrite: a sequence is created `AS` the
                // final type. One given up, or an identity turned into a
                // sequence, goes before the retype (DEC-1889.1, rank -3
                // before -1), so it does not wait for the column's rewrites.
                let early = matches!(
                    (from, to),
                    (
                        ValueSource::Identity(_) | ValueSource::Sequence(_),
                        ValueSource::None | ValueSource::Default(_)
                    ) | (ValueSource::Identity(_), ValueSource::Sequence(_))
                        // A kept identity retuned: whether it follows a retype
                        // of its column or goes before it depends on whether
                        // the retype narrows, which is the engine's to say
                        // (DEC-1921.1); `Dialect::engine_effects` adds the
                        // dependency where the retype does not.
                        | (ValueSource::Identity(_), ValueSource::Identity(_))
                );
                if !early {
                    self.exists(ObjectKey::Column(key.clone()));
                }
                // The engine refuses an identity on a nullable column, so a
                // tightening goes first; and it refuses to relax an identity
                // column, so a numbering given up goes before the relaxation
                // (DEC-1889.1). The retype and the rows are not modelled: a
                // read of the column's values would order every row write
                // before the numbering.
                if matches!(to, ValueSource::Identity(_)) && self.tightens(&owner, &key) {
                    self.exists(Self::not_null(key.clone()));
                }
                if matches!(from, ValueSource::Identity(_)) && self.relaxes(&owner, &key) {
                    self.exists(Self::not_null(key.clone()));
                }
                // And a plain default is dropped before a source goes on:
                // measured on 16 and 18, an identity on a column with a
                // default is refused.
                // The differ removes that default in a change of its own and
                // gives this one `from: None`, so the baseline says whether
                // there was one (#1998).
                let had_default = matches!(from, ValueSource::Default(_))
                    || self.base_default(&owner, &key).is_some();
                if had_default && !matches!(to, ValueSource::Default(_)) {
                    self.requires(Requirement::Absent(ObjectKey::Default(key)));
                }
            }
            Change::AddComputedColumn { table, .. } | Change::DropComputedColumn { table, .. } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                if let Change::DropComputedColumn { name, .. } = op.change() {
                    self.requires_computed_dependents_gone(&owner, name);
                } else {
                    // A computed column takes its place among the columns as
                    // an ordinary one does.
                    self.appends_a_column(owner);
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
                    // An index is usable as the identity only over NOT NULL
                    // columns, so a tightening that makes it so goes first
                    // (#1998).
                    self.identity_columns_not_null(&owner, part);
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
                //
                // Only an own default (`to`) can be overwritten into a
                // different value. A step with `to: None` writes only the
                // `fallback`, the parent's final text, so the parent's
                // recursion writes the same value in either order
                // (DEC-1954.9).
                if to.is_some() {
                    self.after_parent(parent, column, ParentAttribute::Default);
                }
                // A partition declares no column: its entry is spelled by its
                // parent's column, so the parent's rename moves the name this
                // step uses. A release (nothing own, nothing to fall back
                // to) takes the old default off under the name it was set
                // under, and runs before the rename (DEC-1699.1); anything
                // that sets one is spelled by the name the column ends with.
                // The release reads the base incarnation, not `Exists`, so
                // that the parent's retype does not order it (DEC-1168.1).
                if let Ok(uid) = self.owners.column(&parent.column(column), self.spelling) {
                    let key = ObjectKey::Column(ColumnKey::Recorded(Recorded(uid)));
                    if to.is_none() && fallback.is_none() {
                        self.requires(Requirement::Is(key, Incarnation::Base));
                    } else {
                        self.exists(key);
                    }
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
                    self.releases_constraint(&owner, PartKey::PrimaryKey);
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
            Change::DropUnique { table, name } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                self.releases_constraint(&owner, PartKey::Unique(name.clone()));
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
                // It validates the child's values against the referenced
                // rows, so it follows every write of them too (#1998).
                if referenced != owner {
                    self.requires_valid_data(&referenced);
                }
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
                self.releases_constraint(&owner, PartKey::ForeignKey(name.clone()));
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
            Change::DropCheck { table, name } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                self.releases_constraint(&owner, PartKey::Check(name.clone()));
            }
            Change::DropIndex { table, name } => {
                let owner = self.table(table)?;
                self.exists(ObjectKey::Table(owner.clone()));
                self.releases_constraint(&owner, PartKey::Index(name.clone()));
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
                // A partitioned parent's index, built after a partition's
                // own of its shape, would take that one as its clone: the
                // parent's goes first (DEC-1688.1). Which of the partition's
                // indexes it could take is a question of shape, so every
                // index the parent ends with is waited for; one that stands
                // throughout, or a parent that is not rebuilt, draws no edge.
                if let Some((_, own)) = self.final_of(&owner)
                    && let Some(of) = &own.partition_of
                    && let Ok(parent) = self.table(&of.parent)
                    && let Some((_, parent_table)) = self.final_of(&parent)
                {
                    for name in parent_table.indexes.keys() {
                        self.requires(Requirement::Is(
                            Self::part(&parent, PartKey::Index(name.clone())),
                            Incarnation::Final,
                        ));
                    }
                }
            }
            Change::InsertRow {
                table,
                key_column,
                key,
                row,
                defaults,
                types,
                ..
            } => {
                let owner = self.table(table)?;
                self.row_basics(&owner, table, key);
                let mut written = vec![key_column.as_str()];
                written.extend(row.columns().map(|(c, _)| c.as_str()));
                written.extend(defaults.keys().map(String::as_str));
                self.row_writes(table, &owner, &written);
                // The statement's postcondition holds every column in
                // `types` to what the insert left in it, NULL for a column
                // the row and the defaults leave out, so it names those
                // columns too: one the plan adds goes first (#2079).
                for c in types.keys().map(String::as_str) {
                    if !written.contains(&c)
                        && let Ok(column) = self.column(&table.column(c))
                    {
                        self.exists(ObjectKey::Column(column));
                    }
                }
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
                self.row_leaves_no_required_column(&owner, table, &written);
                self.row_parents(&owner, true, false);
                self.row_cycle(&owner);
                for (c, v) in row.columns() {
                    if v.is_null() {
                        self.row_needs_nullable(&owner, table, c);
                    }
                }
                self.creates(Resource::Object(
                    ObjectKey::Rows {
                        table: owner.clone(),
                        side: RowSide::Written,
                    },
                    Incarnation::Final,
                ));
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
                key_column,
                key,
                columns,
                unchanged,
                ..
            } => {
                let owner = self.table(table)?;
                self.row_basics(&owner, table, key);
                let written: Vec<&str> = columns.keys().map(String::as_str).collect();
                self.row_writes(table, &owner, &written);
                // The statement holds the row to the cells it leaves alone
                // and finds it by its key, before and after it runs, so it
                // names those columns too: a rename of one goes first
                // (DECISIONS 136).
                for c in unchanged
                    .keys()
                    .map(String::as_str)
                    .chain([key_column.as_str()])
                {
                    if !written.contains(&c)
                        && let Ok(column) = self.column(&table.column(c))
                    {
                        self.exists(ObjectKey::Column(column));
                    }
                }
                self.row_meets_constraints(&owner, table, &written);
                for (c, (_, to)) in columns {
                    match to {
                        crate::Cell::Value(v) => {
                            if v.is_null() {
                                self.row_needs_nullable(&owner, table, c);
                            }
                        }
                        // `SET c = DEFAULT` writes the default the plan
                        // leaves, as an insert that omits the column does
                        // (#1998).
                        crate::Cell::Default(_) => {
                            if let Ok(column) = self.column(&table.column(c.as_str())) {
                                self.requires(Requirement::Is(
                                    ObjectKey::Default(column),
                                    Incarnation::Final,
                                ));
                            }
                        }
                    }
                }
                self.row_parents(&owner, true, true);
                self.row_cycle(&owner);
                self.creates(Resource::Object(
                    ObjectKey::Rows {
                        table: owner.clone(),
                        side: RowSide::Written,
                    },
                    Incarnation::Final,
                ));
                // The row must stand; its values change, its identity does
                // not, so a lookup of it by another row waits for nothing
                // here.
                self.exists(ObjectKey::Row {
                    table: owner,
                    key: key.clone(),
                });
            }
            Change::DeleteRow {
                table,
                key_column,
                key,
                row,
                ..
            } => {
                let owner = self.table(table)?;
                self.row_basics(&owner, table, key);
                let written: Vec<&str> = row.keys().map(String::as_str).collect();
                self.row_writes(table, &owner, &written);
                // The statement finds the row by its key, which a payload
                // leaves out, so a rename of that column goes first.
                if !written.contains(&key_column.as_str())
                    && let Ok(column) = self.column(&table.column(key_column))
                {
                    self.exists(ObjectKey::Column(column));
                }
                self.row_parents(&owner, false, true);
                self.row_cycle(&owner);
                self.row_inbound_keys_gone(&owner);
                self.removes(Resource::Object(
                    ObjectKey::Rows {
                        table: owner,
                        side: RowSide::Held,
                    },
                    Incarnation::Base,
                ));
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

    /// A column the step needs. A `strict` one must also be NOT NULL where it
    /// ends (a primary key's), so the tightening that makes it so goes first;
    /// a column the plan creates has no producer of its NOT NULL here and
    /// adds nothing.
    fn requires_column(&mut self, table: &TableName, column: &str, strict: bool) {
        let Ok(key) = self.column(&table.column(column)) else {
            // A SQL Server computed column has no column uid, only its part
            // of the table: an index or key over one needs that part (#1998).
            if let Ok(owner) = self.table(table)
                && [self.final_of(&owner), self.base_of(&owner)]
                    .into_iter()
                    .flatten()
                    .any(|(_, t)| t.computed.contains_key(column))
            {
                self.exists(Self::part(&owner, PartKey::Computed(column.to_owned())));
            }
            return;
        };
        self.exists(ObjectKey::Column(key.clone()));
        if strict
            && let Ok(owner) = self.table(table)
            && let Some((_, will)) = self.final_of(&owner)
            && let Some((_, now)) = self.names_of(&key)
            && will.columns.get(&now).is_some_and(|c| !c.nullable)
        {
            self.exists(Self::not_null(key));
        }
    }

    /// The NOT NULL of each column of the identity's target, where the plan
    /// leaves the column NOT NULL: produced by a tightening when the plan
    /// makes it, held by the baseline otherwise.
    fn identity_columns_not_null(&mut self, owner: &Recorded, part: &PartKey) {
        let Some((name, will)) = self.final_of(owner) else {
            return;
        };
        let columns: Vec<String> = match part {
            PartKey::PrimaryKey => will
                .primary_key
                .as_ref()
                .map(|pk| pk.columns.clone())
                .unwrap_or_default(),
            PartKey::Unique(n) => will
                .unique
                .get(n)
                .map(|u| u.columns.clone())
                .unwrap_or_default(),
            PartKey::Index(n) => will
                .indexes
                .get(n)
                .map(|i| {
                    i.columns
                        .iter()
                        .filter_map(|c| c.key.column().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            PartKey::ForeignKey(_) | PartKey::Check(_) | PartKey::Computed(_) => Vec::new(),
        };
        // A column the plan creates has no producer of its NOT NULL, as in
        // `requires_column`: it is created NOT NULL, before any switch.
        for c in columns {
            if will.columns.get(&c).is_some_and(|c| !c.nullable)
                && let Some(key) = self.final_column(&name.column(c.as_str()))
                && self.names_of(&key).is_some()
            {
                self.exists(Self::not_null(key));
            }
        }
    }

    /// A constraint over the rows validates them as the plan leaves them.
    fn requires_valid_data(&mut self, owner: &Recorded) {
        self.requires(Requirement::Reads(ObjectKey::Table(owner.clone())));
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
        // The column under each side's own name: the parent may rename it, and
        // the step spells the name it ends with.
        let (was_name, will_name) = self
            .names_of(&key)
            .unwrap_or_else(|| (column.to_owned(), column.to_owned()));
        let (Some(was), Some(will)) = (was.columns.get(&was_name), will.columns.get(&will_name))
        else {
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

    /// The foreign keys over a column, and those into it, must be gone. The
    /// keys are the baseline's, so the column is named as the baseline names
    /// it: through its uid, since the step spells the name it ends with.
    fn requires_foreign_keys_over_column_gone(&mut self, owner: &Recorded, column: &crate::Uid) {
        let Some((name, table)) = self.base_of(owner) else {
            return;
        };
        let Some(column) = self.before.ids.columns.get(column).map(|c| c.name.clone()) else {
            return;
        };
        let name = name.clone();
        let mut gone = Vec::new();
        for (n, fk) in &table.foreign_keys {
            if fk.columns.contains(&column) {
                gone.push(Self::part(owner, PartKey::ForeignKey(n.clone())));
            }
        }
        for (other_name, other) in &self.before.schema.tables {
            let Some(other_owner) = self.base_table(other_name) else {
                continue;
            };
            for (fk_name, fk) in &other.foreign_keys {
                if fk.references_table == name && fk.references_columns.contains(&column) {
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

    /// The plan's renames that bring the baseline's `table` forward, as the
    /// differ brings it before comparing its constraints
    /// (`Renames::apply`): of its own columns, of the columns its foreign keys
    /// reference, of a partition's parent's columns, and of those tables. Read
    /// from the two identity files, by uid.
    fn renames_of(&self, name: &TableName, table: &Table) -> crate::rename::Renames {
        let mut renames = crate::rename::Renames::default();
        let mut tables: Vec<&TableName> = vec![name];
        tables.extend(table.foreign_keys.values().map(|fk| &fk.references_table));
        if let Some(of) = &table.partition_of {
            tables.push(&of.parent);
        }
        for t in tables {
            let Some(uid) = self.owners.table(t, Spelling::Base).ok() else {
                continue;
            };
            if let Some(now) = self.after.ids.tables.get(&uid)
                && now != t
            {
                renames.rename_table(t.clone(), now.clone());
            }
            let Some(base) = self.before.schema.tables.get(t) else {
                continue;
            };
            for c in base.columns.keys() {
                if let Some(key) = self.base_column(&t.column(c.as_str()))
                    && let Some((was, now)) = self.names_of(&key)
                    && was != now
                {
                    renames.rename_column(t.column(was.as_str()), now);
                }
            }
        }
        renames
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

    /// The plain default the baseline gives the column, if any.
    fn base_default(&self, owner: &Recorded, column: &ColumnKey) -> Option<&str> {
        let (_, was) = self.base_of(owner)?;
        let (was_name, _) = self.names_of(column)?;
        match &was.columns.get(&was_name)?.value_source {
            ValueSource::Default(d) => Some(d.as_str()),
            ValueSource::None
            | ValueSource::Identity(_)
            | ValueSource::Sequence(_)
            | ValueSource::Generated(_) => None,
        }
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

    /// A constraint the plan takes away is released: a row written after it
    /// meets none of it (`row_meets_constraints`).
    fn releases_constraint(&mut self, owner: &Recorded, part: PartKey) {
        self.creates(Resource::Object(
            ObjectKey::Rows {
                table: owner.clone(),
                side: RowSide::Released(part),
            },
            Incarnation::Final,
        ));
    }

    fn row_basics(&mut self, owner: &Recorded, table: &TableName, _key: &RowKey) {
        let _ = table;
        self.exists(ObjectKey::Table(owner.clone()));
    }

    /// A column added to `owner` goes after the ones before it in the plan:
    /// the engine appends it, so their order is the table's (#2080).
    fn appends_a_column(&mut self, owner: Recorded) {
        self.rewrites(Resource::Object(
            ObjectKey::ColumnOrder(owner),
            Incarnation::Final,
        ));
    }

    /// An inserted row writes no value into a baseline column the row and
    /// its defaults leave out, so such a column that is `NOT NULL` with
    /// nothing to fill it, no default, identity, sequence or generation,
    /// refuses the row while its NOT NULL stands: the insert waits for that
    /// NOT NULL to go, with the column's drop or its relaxation (#2086). Only
    /// the baseline's columns, as `row_meets_constraints` reads them: a NOT
    /// NULL column the plan adds with nothing to fill it is one the row has
    /// to write in any order. Nor one the plan gives something to fill it:
    /// the engine fills it once that change runs.
    fn row_leaves_no_required_column(
        &mut self,
        owner: &Recorded,
        table: &TableName,
        written: &[&str],
    ) {
        let Some((base_name, was)) = self.base_of(owner) else {
            return;
        };
        // A trigger on the table may fill the column before the row is
        // checked, and whether it does is the engine's to say: the release
        // pass lets a drop pass the insert there (`Dependence::base_required`,
        // #1397), so no requirement is stated (#2175 review). Only a trigger
        // that stands at the insert: one declared under the same id at both
        // ends, on a table the plan neither renames nor replaces. The differ
        // changes such a trigger's text in one step, so it stands throughout,
        // and what its new text fills is the engine's to say too. A trigger
        // under a new id, a renamed table's among them, is dropped before
        // the rows and created after them. Only a declared trigger is seen
        // here; an undeclared one is the catalog's, which the effects do not
        // read.
        let stands = |id: &crate::ModuleId| {
            matches!(id, crate::ModuleId::Trigger { on, .. }
                if on.schema == base_name.schema && on.name == base_name.name)
        };
        if self
            .final_of(owner)
            .is_some_and(|(final_name, _)| final_name == base_name)
            && self.before.schema.modules.iter().any(|(id, was)| {
                stands(id)
                    && self
                        .after
                        .schema
                        .modules
                        .get(id)
                        .is_some_and(|will| will.kind == was.kind)
            })
        {
            return;
        }
        let written: Vec<ColumnKey> = written
            .iter()
            .filter_map(|c| self.column(&table.column(*c)).ok())
            .collect();
        // A column the plan gives a numbering source, a default or a
        // generation is filled by the engine once that change has run,
        // which the insert follows already (#2175 review): only a column
        // left with nothing to fill it needs its NOT NULL gone.
        let filled_at_the_end: Vec<ColumnKey> = self
            .final_of(owner)
            .map(|(final_name, will)| {
                will.columns
                    .iter()
                    .filter(|(_, c)| !matches!(c.value_source, crate::ValueSource::None))
                    .filter_map(|(n, _)| self.final_column(&final_name.column(n.as_str())))
                    .collect()
            })
            .unwrap_or_default();
        for (name, column) in &was.columns {
            if column.nullable || !matches!(column.value_source, crate::ValueSource::None) {
                continue;
            }
            if let Some(key) = self.base_column(&base_name.column(name.as_str()))
                && !written.contains(&key)
                && !filled_at_the_end.contains(&key)
            {
                self.requires(Requirement::Absent(Self::not_null(key)));
            }
        }
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
    /// away go first: dropped, or replaced under the same name, which the old
    /// one's drop ends as surely (#1998). A constraint the plan keeps is
    /// compared as the differ compares it, through the plan's renames and
    /// without its storage parameters, which change in place (DEC-1442.1): one
    /// the differ does not rebuild must not be required gone, or the plan that
    /// keeps it is refused (DEC-1998.1).
    fn row_meets_constraints(&mut self, owner: &Recorded, _table: &TableName, written: &[&str]) {
        let (Some((base_name, was)), Some((_, will))) = (self.base_of(owner), self.final_of(owner))
        else {
            return;
        };
        // Whatever the plan takes away goes before a row is written, by its
        // name and whatever its text: a check the resolver rebinds is torn
        // down and put back around the rows (DEC-1968.2).
        let mut parts_held: Vec<PartKey> = Vec::new();
        if was.primary_key.is_some() {
            parts_held.push(PartKey::PrimaryKey);
        }
        parts_held.extend(was.checks.keys().cloned().map(PartKey::Check));
        parts_held.extend(was.unique.keys().cloned().map(PartKey::Unique));
        parts_held.extend(was.foreign_keys.keys().cloned().map(PartKey::ForeignKey));
        parts_held.extend(
            was.indexes
                .iter()
                .filter(|(_, i)| i.unique || i.filter.is_some())
                .map(|(n, _)| PartKey::Index(n.clone())),
        );
        for part in parts_held {
            self.requires(Requirement::Is(
                ObjectKey::Rows {
                    table: owner.clone(),
                    side: RowSide::Released(part),
                },
                Incarnation::Final,
            ));
        }
        // Only the parts compared below, and what `apply` looks a partition's
        // columns up by: the whole table would bring its reference rows into
        // every row step's copy.
        let parts = Table {
            unique: was.unique.clone(),
            foreign_keys: was.foreign_keys.clone(),
            indexes: was.indexes.clone(),
            partition_of: was.partition_of.clone(),
            ..Table::default()
        };
        let forward = self
            .renames_of(base_name, was)
            .apply(&parts, base_name)
            .into_owned();
        let mut gone = Vec::new();
        for (n, check) in &was.checks {
            if will.checks.get(n) != Some(check) {
                gone.push(Self::part(owner, PartKey::Check(n.clone())));
            }
        }
        let unique_bare = |u: &crate::UniqueConstraint| {
            let mut u = u.clone();
            u.storage_parameters.clear();
            u
        };
        for (n, unique) in &forward.unique {
            if will.unique.get(n).map(unique_bare) != Some(unique_bare(unique)) {
                gone.push(Self::part(owner, PartKey::Unique(n.clone())));
            }
        }
        for (n, fk) in &forward.foreign_keys {
            if will.foreign_keys.get(n) != Some(fk) {
                gone.push(Self::part(owner, PartKey::ForeignKey(n.clone())));
            }
        }
        let index_bare = |i: &crate::Index| {
            let mut i = i.clone();
            i.storage_parameters.clear();
            i
        };
        for (n, index) in &forward.indexes {
            if (index.unique || index.filter.is_some())
                && will.indexes.get(n).map(index_bare) != Some(index_bare(index))
            {
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

    /// A row waits for the rows of the tables its foreign keys reference, and
    /// a row that goes leaves before they do (DEC-1968.2). Table against
    /// table, never row against row: a value does not name the row it needs
    /// (a default, a unique key, a tuple, a rename), and the differ already
    /// orders the rows of two tables by the foreign key between them. A key
    /// counts only if it stands throughout, compared through the plan's
    /// renames as the differ compares it: one the plan adds is installed
    /// after the rows it checks, one it drops is gone before them. Two tables
    /// in one cycle of foreign keys, a table and itself included, are left in
    /// the order given, as the differ leaves them (`data::supply_order`).
    fn row_parents(&mut self, owner: &Recorded, written: bool, held: bool) {
        let (Some((base_name, was)), Some((own_name, will))) =
            (self.base_of(owner), self.final_of(owner))
        else {
            return;
        };
        let forward = self
            .renames_of(base_name, was)
            .apply(
                &Table {
                    foreign_keys: was.foreign_keys.clone(),
                    partition_of: was.partition_of.clone(),
                    ..Table::default()
                },
                base_name,
            )
            .into_owned();
        let mut parents = BTreeSet::new();
        for (name, fk) in &will.foreign_keys {
            if forward.foreign_keys.get(name) != Some(fk) {
                continue;
            }
            if self.references_back(&fk.references_table, own_name) {
                continue;
            }
            if let Some(parent) = self
                .final_table(&fk.references_table)
                .or_else(|| self.base_table(&fk.references_table))
            {
                parents.insert(parent);
            }
        }
        for parent in parents {
            if written {
                self.requires(Requirement::Is(
                    ObjectKey::Rows {
                        table: parent.clone(),
                        side: RowSide::Written,
                    },
                    Incarnation::Final,
                ));
            }
            if held {
                self.requires(Requirement::Is(
                    ObjectKey::Rows {
                        table: parent,
                        side: RowSide::Held,
                    },
                    Incarnation::Base,
                ));
            }
        }
    }

    /// The rows of the tables in one cycle of foreign keys with this one keep
    /// the order given: a write of one resource that all of them share, which
    /// the effect graph orders by the operations' ids (DEC-1954.6). The
    /// differ leaves such tables in name order and draws no edge between them
    /// from the keys (`data::supply_order`), so neither do the effects.
    fn row_cycle(&mut self, owner: &Recorded) {
        let Some((own_name, _)) = self.final_of(owner) else {
            return;
        };
        if !self.references_back_from_own(own_name) {
            return;
        }
        let mut members = BTreeSet::new();
        members.insert(owner.clone());
        let mut seen: BTreeSet<&TableName> = BTreeSet::new();
        let mut queue = vec![own_name];
        while let Some(current) = queue.pop() {
            if !seen.insert(current) {
                continue;
            }
            let Some(table) = self.after.schema.tables.get(current) else {
                continue;
            };
            for fk in table.foreign_keys.values() {
                queue.push(&fk.references_table);
            }
        }
        for name in seen {
            if name != own_name
                && self.references_back(name, own_name)
                && let Some(member) = self.final_table(name)
            {
                members.insert(member);
            }
        }
        if let Some(representative) = members.into_iter().next() {
            self.rewrites(Resource::Column(ObjectKey::Rows {
                table: representative,
                side: RowSide::Written,
            }));
        }
    }

    /// Whether the table reaches itself through the desired state's foreign
    /// keys.
    fn references_back_from_own(&self, own: &TableName) -> bool {
        self.after.schema.tables.get(own).is_some_and(|table| {
            table
                .foreign_keys
                .values()
                .any(|fk| self.references_back(&fk.references_table, own))
        })
    }

    /// A foreign key into this table that the plan drops, a rebuild's too, is
    /// gone before the table's rows go: a delete leaves the rows that
    /// reference them standing otherwise. By the drop's own key, as a row
    /// waits for a check's (`row_meets_constraints`).
    fn row_inbound_keys_gone(&mut self, owner: &Recorded) {
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
                if fk.references_table == name {
                    gone.push(ObjectKey::Rows {
                        table: other_owner.clone(),
                        side: RowSide::Released(PartKey::ForeignKey(fk_name.clone())),
                    });
                }
            }
        }
        for key in gone {
            self.requires(Requirement::Is(key, Incarnation::Final));
        }
    }

    /// Whether `from` reaches `to` through the desired state's foreign keys,
    /// or is it: they are then in one cycle with the foreign key that was
    /// followed.
    fn references_back(&self, from: &TableName, to: &TableName) -> bool {
        let mut seen: BTreeSet<&TableName> = BTreeSet::new();
        let mut queue = vec![from];
        while let Some(current) = queue.pop() {
            if current == to {
                return true;
            }
            if !seen.insert(current) {
                continue;
            }
            if let Some(table) = self.after.schema.tables.get(current) {
                queue.extend(table.foreign_keys.values().map(|fk| &fk.references_table));
            }
        }
        false
    }
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
        ObjectKey::Row { .. } | ObjectKey::Rows { .. } | ObjectKey::Grant { .. } => true,
        // Never required, only written: nothing asks the baseline about it.
        ObjectKey::ColumnOrder(_) => true,
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

/// The table the plan leaves where the baseline's `base` was, by identity:
/// what a dependent of the baseline's table becomes, or does not.
pub fn final_table<'a>(after: Snapshot<'a>, base: &BaseTable<'_>) -> Option<&'a Table> {
    let name = after.ids.tables.get(&base.owner.0)?;
    after.schema.tables.get(name)
}

/// The key of the column a step names by uid, so a dialect can add a
/// requirement on it without making up a key.
pub fn column_key(uid: &crate::Uid) -> ObjectKey {
    ObjectKey::Column(ColumnKey::Recorded(Recorded(uid.clone())))
}

/// A column's type in the baseline and in the desired state, found by
/// identity: its name may differ between the two.
pub fn column_types<'a>(
    uid: &crate::Uid,
    before: Snapshot<'a>,
    after: Snapshot<'a>,
) -> Option<(&'a crate::ColumnType, &'a crate::ColumnType)> {
    let of = |side: Snapshot<'a>| {
        let at = side.ids.columns.get(uid)?;
        Some(&side.schema.tables.get(&at.table)?.columns.get(&at.name)?.ty)
    };
    Some((of(before)?, of(after)?))
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
