//! Operations: the steps a plan runs as, each tied to the logical change it
//! came from (#1776 slice S1a, DEC-1961.1).
//!
//! # Why a layer between a `Change` and a `Statement`
//!
//! A logical change is not always one step. The resolver splits a
//! `CreateTable` into the bare table and the defaults, checks and indexes it
//! sets afterwards, appends a teardown and a restoration for every surface its
//! evidence rebinds, and splits a rebuilt module into its drop and its create;
//! the connected PostgreSQL plan weaves the same teardowns and restorations
//! around a dropped module. Whatever reads what a plan *does* — its effects
//! (#1954) and who owns a name at each step (#1871) — has to read it per step,
//! from that step's own payload. Read from the desired model instead, the bare
//! table would claim the indexes a later step adds.
//!
//! An [`Operation`] is that step: its payload, the logical change it came
//! from ([`Source`]), and its role in it ([`Role`]). It is an in-memory
//! planning layer and nothing else. No type here derives `Serialize`: a saved
//! plan is still the list of changes it was, so its bytes and its checksum
//! are unchanged, and an operation id can never reach one.
//!
//! # Keys are identities, not names
//!
//! A name is an address, not an identity (DEC-1851.1). An appended teardown
//! is spelled by its table's base name and its restoration by the final one
//! (DEC-1498.1), so a key built from the spelled name would give one check two
//! keys, and a dropped table's check and the check of the table renamed into
//! its name one. [`ObjectKey`] therefore holds the uid each object, or its
//! table, was recorded under. Which name a step spells by — the name in force
//! at its position, the base name or the final one — follows from its role
//! ([`Spelling`]), and the models answer it through [`Owners`]: they supply
//! ownership and nothing about what the step does.
//!
//! An object the model gives no uid — a check, an index, a module, a reference
//! row, a permission — is keyed by what identifies it: its table's uid and its
//! name, its [`ModuleId`], its table's uid and row key, its grantee and target.
//! [`Recorded`] has no public constructor, so no caller can make a uid up for
//! one of them (DEC-1961.1).
//!
//! # Incarnations
//!
//! Within one plan an object is at most two instances: the one the baseline
//! holds and the one the plan leaves. A teardown, a drop, ends the first
//! ([`Incarnation::Base`]); a restoration, a create or a re-add produces the
//! second ([`Incarnation::Final`]). An alteration in place rewrites whichever
//! instance stands and changes no incarnation: two rewrites of one attribute
//! are ordered by the rule for two writes, not told apart by a number
//! (DEC-1961.2). [`Incarnations::replay`] refuses a plan in which a key would
//! need a third instance.

use std::collections::BTreeMap;
use std::fmt;

use crate::plan_names::{At, Holder, PlanNames};
use crate::resolver::Surface;
use crate::{
    Change, ColumnRef, GrantTarget, IdsFile, IndexPart, ModuleId, PlannedChange, RowKey, TableName,
    Uid,
};

/// Which operation of a plan this is, deterministic for a given plan: the
/// logical change it came from, and its place among that change's
/// operations. Never serialized, so never part of a saved plan.
///
/// A saved plan's steps serialize:
///
/// ```
/// fn saved<T: serde::Serialize>() {}
/// saved::<pbps_model::PlannedChange>();
/// ```
///
/// and neither an operation nor its id can, under the same bound:
///
/// ```compile_fail
/// fn saved<T: serde::Serialize>() {}
/// saved::<pbps_model::operation::OperationId>();
/// ```
///
/// ```compile_fail
/// fn saved<T: serde::Serialize>() {}
/// saved::<pbps_model::operation::Operation>();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId {
    source: u32,
    part: u32,
}

impl OperationId {
    /// The number of the logical change: its position in the logical plan,
    /// or, for a rebinding, a number after every change of it.
    pub fn source(self) -> u32 {
        self.source
    }

    /// The operation's place among those of its logical change, from 0.
    pub fn part(self) -> u32 {
        self.part
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.source, self.part)
    }
}

/// The logical change an operation came from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// The change at this position of the logical plan: the differ's, before
    /// any expansion split it or added to it.
    Change(usize),
    /// A surface the resolver's evidence says binds differently (DEC-1498.1).
    /// No change of the logical plan asks for it, so it is a logical change
    /// of its own, whose steps are a teardown and a restoration.
    Rebinding(Surface),
}

/// What an operation is to its logical change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The logical change, unexpanded.
    Whole,
    /// One of the steps the change was split into, in order.
    SplitPart,
    /// The removal of an object an obligation takes down, before what needs
    /// it gone. It ends the [`Incarnation::Base`].
    Teardown,
    /// The object put back after that. It produces the
    /// [`Incarnation::Final`].
    Restoration,
}

/// Where an expansion says a step came from.
///
/// An enum over the pairs that exist rather than a source beside a role: a
/// rebinding has no step of its own to be whole or split, so a whole
/// rebinding cannot be written down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Whole(usize),
    SplitPart(usize),
    Teardown(Source),
    Restoration(Source),
}

impl Origin {
    pub fn source(&self) -> Source {
        match self {
            Origin::Whole(i) | Origin::SplitPart(i) => Source::Change(*i),
            Origin::Teardown(source) | Origin::Restoration(source) => source.clone(),
        }
    }

    pub fn role(&self) -> Role {
        match self {
            Origin::Whole(_) => Role::Whole,
            Origin::SplitPart(_) => Role::SplitPart,
            Origin::Teardown(_) => Role::Teardown,
            Origin::Restoration(_) => Role::Restoration,
        }
    }

    /// Which names the step spells its objects by.
    ///
    /// A whole change names its table as the logical plan has it at its own
    /// position, and a part as it is once its change has run: the bare
    /// table's parts follow the `CREATE`. A teardown is spelled by the base
    /// name and a restoration by the final one, wherever they sit
    /// (DEC-1498.1), which is what makes ownership readable before anything
    /// is ordered (#1871).
    pub fn spelling(&self) -> Spelling {
        match self {
            Origin::Whole(i) => Spelling::At(At::Before(*i)),
            Origin::SplitPart(i) => Spelling::At(At::After(*i)),
            Origin::Teardown(_) => Spelling::Base,
            Origin::Restoration(_) => Spelling::Final,
        }
    }
}

/// The names a step spells by: those in force at a boundary of the logical
/// plan, or those of one side of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spelling {
    At(At),
    Base,
    Final,
}

impl Spelling {
    /// The spelling of what the step itself brings into being: after its
    /// own change, where a whole step is spelled before it.
    fn produced(self) -> Self {
        match self {
            Spelling::At(At::Before(i)) => Spelling::At(At::After(i)),
            Spelling::At(At::Start | At::End | At::After(_)) | Spelling::Base | Spelling::Final => {
                self
            }
        }
    }
}

/// How a module edit reaches the engine, which the dialect decides
/// (`Dialect::rebuilds_modules`): replaced in place, keeping the object, or
/// dropped and created again as a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleEdits {
    InPlace,
    Rebuilt,
}

/// A uid the plan's own changes or identity files recorded.
///
/// No public constructor: a key holds one only where a change carries it, or
/// where [`Owners`] found the table that owns a part. A check, an index or a
/// row has no uid of its own, and there is no way to give it one here
/// (DEC-1961.1).
///
/// A key's uid can be read:
///
/// ```
/// fn owner(key: &pbps_model::operation::Recorded) -> &pbps_model::Uid {
///     key.uid()
/// }
/// let _: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
/// ```
///
/// and not made up, under the same names:
///
/// ```compile_fail
/// let fabricated: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
/// let _ = pbps_model::operation::Recorded(fabricated);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Recorded(Uid);

impl Recorded {
    pub fn uid(&self) -> &Uid {
        &self.0
    }
}

/// The column a default or a NOT NULL belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ColumnKey {
    /// A table's own column, by its uid.
    Recorded(Recorded),
    /// A partition's column, which it inherits and holds no uid for: its
    /// table's uid and the column's name (DEC-1581.1).
    Partition { table: Recorded, column: String },
}

/// A named part of a table. The kind travels with the name because indexes
/// and constraints are separate namespaces (DECISIONS 168).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PartKey {
    /// One per table, and a declaration need not name it.
    PrimaryKey,
    Unique(String),
    ForeignKey(String),
    Check(String),
    Index(String),
    /// A SQL Server computed column, added and dropped whole by name.
    Computed(String),
}

impl From<&IndexPart> for PartKey {
    fn from(part: &IndexPart) -> Self {
        match part {
            IndexPart::PrimaryKey => PartKey::PrimaryKey,
            IndexPart::Unique(name) => PartKey::Unique(name.clone()),
            IndexPart::Index(name) => PartKey::Index(name.clone()),
        }
    }
}

/// Who a permission is held by. `PUBLIC` is not a role this model has
/// (ADR-0010 §5), so it is a grantee of its own rather than a role's name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Grantee {
    Role(String),
    Public,
}

/// The identity of what an operation acts on, over every identity a
/// [`Change`] carries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectKey {
    Table(Recorded),
    Column(ColumnKey),
    Role(Recorded),
    /// A column's default or generation expression: the expression object
    /// the engine keeps beside the column, which a plan can take off and put
    /// back while the column stands (DEC-1168.1).
    Default(ColumnKey),
    Module(ModuleId),
    Part {
        table: Recorded,
        part: PartKey,
    },
    Row {
        table: Recorded,
        key: RowKey,
    },
    /// A grantee's permissions on one target, by the names the change
    /// carries: roles and targets of grants are spelled where the grant runs.
    Grant {
        grantee: Grantee,
        target: GrantTarget,
    },
}

/// Which instance of an object: the baseline's or the plan's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Incarnation {
    Base,
    Final,
}

/// What an operation's own payload does to one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The step removes the object: the [`Incarnation::Base`] ends.
    Ends,
    /// The step brings the object into being: the [`Incarnation::Final`]
    /// starts.
    Produces,
}

impl Transition {
    pub fn incarnation(self) -> Incarnation {
        match self {
            Transition::Ends => Incarnation::Base,
            Transition::Produces => Incarnation::Final,
        }
    }
}

/// One instance an operation ends or produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub key: ObjectKey,
    pub transition: Transition,
}

/// Why the models could not say who owns what a step names.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum OwnerError {
    /// Neither the plan nor the identity files record anything under the
    /// name: not evidence that nothing is there.
    #[error("no managed object is recorded under `{name}` ({spelling:?})")]
    Unmanaged { name: String, spelling: Spelling },
    /// A managed name nothing holds at that boundary.
    #[error("nothing holds `{name}` at {spelling:?}")]
    Vacant { name: String, spelling: Spelling },
    /// The step's position is outside the logical plan the owners index.
    #[error("{spelling:?} is outside the indexed plan")]
    Outside { spelling: Spelling },
}

/// Who owns a name a step spells: ownership, which is all the models supply
/// to a step (#1776's step-local rule).
pub trait Owners {
    /// The uid of the table a step spells as `name`.
    fn table(&self, name: &TableName, spelling: Spelling) -> Result<Uid, OwnerError>;
    /// The uid of the column a step spells as `column`.
    fn column(&self, column: &ColumnRef, spelling: Spelling) -> Result<Uid, OwnerError>;
}

/// Ownership from the logical plan's own identity history and the two
/// identity files.
///
/// `names` indexes the logical plan, the one [`Origin`]'s positions count:
/// an executable order, which the differ's is, so a name in force at a
/// position is a fact of it (DEC-1851.1). The base and final names come from
/// the identity files and nothing else.
pub struct PlanOwners<'p> {
    names: PlanNames<'p>,
    base: &'p IdsFile,
    desired: &'p IdsFile,
}

impl<'p> PlanOwners<'p> {
    pub fn new(names: PlanNames<'p>, base: &'p IdsFile, desired: &'p IdsFile) -> Self {
        Self {
            names,
            base,
            desired,
        }
    }

    fn inside(&self, spelling: Spelling) -> Result<At, OwnerError> {
        let Spelling::At(at) = spelling else {
            unreachable!("only a boundary is looked up in the plan's history")
        };
        let n = self.names.changes().len();
        match at {
            At::Before(i) | At::After(i) if i >= n => Err(OwnerError::Outside { spelling }),
            At::Start | At::End | At::Before(_) | At::After(_) => Ok(at),
        }
    }
}

fn held(holder: Holder<'_>, name: String, spelling: Spelling) -> Result<Uid, OwnerError> {
    match holder {
        Holder::Tracked(uid) => Ok(uid.clone()),
        Holder::Unmanaged => Err(OwnerError::Unmanaged { name, spelling }),
        Holder::Vacant => Err(OwnerError::Vacant { name, spelling }),
    }
}

impl Owners for PlanOwners<'_> {
    fn table(&self, name: &TableName, spelling: Spelling) -> Result<Uid, OwnerError> {
        let recorded = |ids: &IdsFile| {
            ids.table_uid(name)
                .cloned()
                .ok_or_else(|| OwnerError::Unmanaged {
                    name: name.to_string(),
                    spelling,
                })
        };
        match spelling {
            Spelling::Base => recorded(self.base),
            Spelling::Final => recorded(self.desired),
            Spelling::At(_) => {
                let at = self.inside(spelling)?;
                held(self.names.holder(name, at), name.to_string(), spelling)
            }
        }
    }

    fn column(&self, column: &ColumnRef, spelling: Spelling) -> Result<Uid, OwnerError> {
        let recorded = |ids: &IdsFile| {
            ids.column_uid(column)
                .cloned()
                .ok_or_else(|| OwnerError::Unmanaged {
                    name: column.to_string(),
                    spelling,
                })
        };
        match spelling {
            Spelling::Base => recorded(self.base),
            Spelling::Final => recorded(self.desired),
            Spelling::At(_) => {
                let at = self.inside(spelling)?;
                held(
                    self.names.column_holder(column, at),
                    column.to_string(),
                    spelling,
                )
            }
        }
    }
}

/// One step of a plan, with the change it performs as its whole payload.
///
/// No `Serialize`, deliberately: see the module documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    id: OperationId,
    origin: Origin,
    step: PlannedChange,
    modules: ModuleEdits,
}

impl Operation {
    pub fn id(&self) -> OperationId {
        self.id
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    pub fn source(&self) -> Source {
        self.origin.source()
    }

    pub fn role(&self) -> Role {
        self.origin.role()
    }

    /// The change this step performs: its own payload, and the only thing
    /// what it does is read from.
    pub fn change(&self) -> &Change {
        &self.step.change
    }

    pub fn planned(&self) -> &PlannedChange {
        &self.step
    }

    /// The object this step acts on.
    pub fn key(&self, owners: &dyn Owners) -> Result<ObjectKey, OwnerError> {
        let spelling = self.origin.spelling();
        let table = |name: &TableName| owners.table(name, spelling).map(Recorded);
        let part = |name: &TableName, part: PartKey| {
            Ok(ObjectKey::Part {
                table: table(name)?,
                part,
            })
        };
        let recorded = |uid: &Uid| Recorded(uid.clone());
        let column = |uid: &Uid| ObjectKey::Column(ColumnKey::Recorded(recorded(uid)));
        Ok(match &self.step.change {
            Change::CreateTable { uid, .. }
            | Change::DropTable { uid, .. }
            | Change::DetachPartition { uid, .. }
            | Change::AttachPartition { uid, .. }
            | Change::RenameTable { uid, .. }
            | Change::SetReplicaIdentity { uid, .. }
            | Change::SetStorageParameters { uid, .. }
            | Change::SetTablePersistence { uid, .. } => ObjectKey::Table(recorded(uid)),
            // The declaration's own metadata about its rows: the table's.
            Change::SetDataMode { table: name, .. } | Change::SetEnsureKeys { table: name, .. } => {
                ObjectKey::Table(table(name)?)
            }
            Change::AddColumn { uid, .. }
            | Change::DropColumn { uid, .. }
            | Change::RenameColumn { uid, .. }
            | Change::AlterColumnType { uid, .. }
            | Change::AlterColumnNullability { uid, .. }
            | Change::SetColumnDeprecated { uid, .. } => column(uid),
            Change::AlterColumnDefault { uid, .. } | Change::AlterColumnExpression { uid, .. } => {
                ObjectKey::Default(ColumnKey::Recorded(recorded(uid)))
            }
            Change::SetPartitionDefault { uid, column, .. } => {
                ObjectKey::Default(ColumnKey::Partition {
                    table: recorded(uid),
                    column: column.clone(),
                })
            }
            Change::SetPartitionNotNull { uid, column, .. } => {
                ObjectKey::Column(ColumnKey::Partition {
                    table: recorded(uid),
                    column: column.clone(),
                })
            }
            Change::AddComputedColumn { table, name, .. }
            | Change::DropComputedColumn { table, name, .. } => {
                part(table, PartKey::Computed(name.clone()))?
            }
            Change::SetIndexStorageParameters { table, target, .. } => {
                part(table, PartKey::from(target))?
            }
            Change::SetPrimaryKey { table, .. } => part(table, PartKey::PrimaryKey)?,
            Change::AddUnique { table, name, .. } | Change::DropUnique { table, name } => {
                part(table, PartKey::Unique(name.clone()))?
            }
            Change::AddForeignKey { table, name, .. } | Change::DropForeignKey { table, name } => {
                part(table, PartKey::ForeignKey(name.clone()))?
            }
            Change::AddCheck { table, name, .. } | Change::DropCheck { table, name } => {
                part(table, PartKey::Check(name.clone()))?
            }
            Change::AddIndex { table, name, .. } | Change::DropIndex { table, name } => {
                part(table, PartKey::Index(name.clone()))?
            }
            Change::InsertRow {
                table: name, key, ..
            }
            | Change::UpdateRow {
                table: name, key, ..
            }
            | Change::DeleteRow {
                table: name, key, ..
            } => ObjectKey::Row {
                table: table(name)?,
                key: key.clone(),
            },
            Change::CreateModule { id, .. }
            | Change::AlterModule { id, .. }
            | Change::DropModule { id, .. } => ObjectKey::Module(id.clone()),
            Change::CreateRole { uid, .. }
            | Change::DropRole { uid, .. }
            | Change::RenameRole { uid, .. } => ObjectKey::Role(recorded(uid)),
            Change::Grant { role, target, .. } | Change::Revoke { role, target, .. } => {
                ObjectKey::Grant {
                    grantee: Grantee::Role(role.clone()),
                    target: target.clone(),
                }
            }
            Change::PublicExecution { routine, .. } => ObjectKey::Grant {
                grantee: Grantee::Public,
                target: GrantTarget::Routine(routine.clone()),
            },
        })
    }

    /// The instances this step ends or produces, read from its own payload
    /// alone, in the order it does them.
    ///
    /// The models are not asked what the step does, only who owns what it
    /// names. So a `DropTable` ends the table and says nothing of its
    /// columns and parts, which go with it: what a drop releases is the other
    /// end of the change, the baseline's to say (#1954). And the bare table
    /// the resolver splits off a `CREATE TABLE` produces no check, index or
    /// default, because its payload holds none (#1961).
    pub fn defines(&self, owners: &dyn Owners) -> Result<Vec<Definition>, OwnerError> {
        use Transition::{Ends, Produces};
        let spelling = self.origin.spelling();
        let key = || self.key(owners);
        let ends = |key| Definition {
            key,
            transition: Ends,
        };
        let produces = |key| Definition {
            key,
            transition: Produces,
        };
        let exists = |before: bool, after: bool| -> Result<Vec<Definition>, OwnerError> {
            let mut out = Vec::new();
            if before {
                out.push(ends(key()?));
            }
            if after {
                out.push(produces(key()?));
            }
            Ok(out)
        };
        Ok(match &self.step.change {
            Change::CreateTable {
                uid, name, table, ..
            } => {
                let owner = Recorded(uid.clone());
                // Its columns are named as they are once it stands.
                let at = spelling.produced();
                let column = |c: &str| -> Result<ColumnKey, OwnerError> {
                    owners
                        .column(&name.column(c), at)
                        .map(|uid| ColumnKey::Recorded(Recorded(uid)))
                };
                let part = |part| {
                    produces(ObjectKey::Part {
                        table: owner.clone(),
                        part,
                    })
                };
                let mut out = vec![produces(ObjectKey::Table(owner.clone()))];
                for c in table.columns.keys() {
                    out.push(produces(ObjectKey::Column(column(c)?)));
                }
                for c in table.expression_columns() {
                    let key = if table.columns.contains_key(c) {
                        column(c)?
                    } else {
                        // A partition's own default: it declares no column.
                        ColumnKey::Partition {
                            table: owner.clone(),
                            column: c.to_owned(),
                        }
                    };
                    out.push(produces(ObjectKey::Default(key)));
                }
                if table.primary_key.is_some() {
                    out.push(part(PartKey::PrimaryKey));
                }
                out.extend(table.unique.keys().cloned().map(PartKey::Unique).map(part));
                out.extend(
                    table
                        .foreign_keys
                        .keys()
                        .cloned()
                        .map(PartKey::ForeignKey)
                        .map(part),
                );
                out.extend(table.checks.keys().cloned().map(PartKey::Check).map(part));
                out.extend(table.indexes.keys().cloned().map(PartKey::Index).map(part));
                out.extend(
                    table
                        .computed
                        .keys()
                        .cloned()
                        .map(PartKey::Computed)
                        .map(part),
                );
                out
            }
            Change::AddColumn { uid, column, .. } => {
                let mut out = vec![produces(ObjectKey::Column(ColumnKey::Recorded(Recorded(
                    uid.clone(),
                ))))];
                if column_has_expression(column) {
                    out.push(produces(ObjectKey::Default(ColumnKey::Recorded(Recorded(
                        uid.clone(),
                    )))));
                }
                out
            }
            Change::DropTable { .. }
            | Change::DropColumn { .. }
            | Change::DropComputedColumn { .. }
            | Change::DropUnique { .. }
            | Change::DropForeignKey { .. }
            | Change::DropCheck { .. }
            | Change::DropIndex { .. }
            | Change::DeleteRow { .. }
            | Change::DropModule { .. }
            | Change::DropRole { .. } => exists(true, false)?,
            Change::AddComputedColumn { .. }
            | Change::AddUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::AddIndex { .. }
            | Change::InsertRow { .. }
            | Change::CreateModule { .. }
            | Change::CreateRole { .. } => exists(false, true)?,
            Change::AlterColumnDefault { from, to, .. } => exists(from.is_some(), to.is_some())?,
            // `SET EXPRESSION` replaces the expression the engine keeps for
            // the column, as a changed default does (DEC-1168.1).
            Change::AlterColumnExpression { .. } => exists(true, true)?,
            // Payload alone: a partition with no own default before may
            // still hold its parent's copy, which is the baseline's to say.
            Change::SetPartitionDefault {
                from, to, fallback, ..
            } => exists(from.is_some(), to.is_some() || fallback.is_some())?,
            Change::SetPrimaryKey { from, to, .. } => exists(from.is_some(), to.is_some())?,
            Change::AlterModule { .. } => match self.modules {
                ModuleEdits::Rebuilt => exists(true, true)?,
                ModuleEdits::InPlace => Vec::new(),
            },
            // Renames move an address, and the rest rewrite the instance that
            // stands: neither starts or ends one.
            Change::DetachPartition { .. }
            | Change::AttachPartition { .. }
            | Change::RenameTable { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetReplicaIdentity { .. }
            | Change::SetStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::UpdateRow { .. }
            | Change::SetDataMode { .. }
            | Change::SetEnsureKeys { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => Vec::new(),
        })
    }
}

/// Whether an added column brings an expression object with it: a default or
/// a generation expression. An identity's or a `serial`'s is the engine's,
/// not one the declaration holds (#1631).
fn column_has_expression(column: &crate::Column) -> bool {
    match column.value_source {
        crate::ValueSource::Default(_) | crate::ValueSource::Generated(_) => true,
        crate::ValueSource::None
        | crate::ValueSource::Identity(_)
        | crate::ValueSource::Sequence(_) => false,
    }
}

/// A plan's operations, in the order given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Operations {
    operations: Vec<Operation>,
}

impl Operations {
    /// A plan nothing expanded: one whole operation per change.
    pub fn of_plan(changes: &[PlannedChange], modules: ModuleEdits) -> Self {
        Self::expanded(
            changes.len(),
            changes
                .iter()
                .enumerate()
                .map(|(i, p)| (Origin::Whole(i), p.clone())),
            modules,
        )
    }

    /// What an expansion produced from a logical plan of `logical` changes:
    /// each step with where it came from.
    ///
    /// Ids are assigned here, from the origins alone, so equal input gives
    /// equal ids. A change's number is its position; a rebinding's is
    /// counted after every change, in the order of its surface. A part is
    /// counted in the order the steps are given.
    pub fn expanded(
        logical: usize,
        steps: impl IntoIterator<Item = (Origin, PlannedChange)>,
        modules: ModuleEdits,
    ) -> Self {
        let steps: Vec<(Origin, PlannedChange)> = steps.into_iter().collect();
        let rebindings: BTreeMap<&Surface, u32> = {
            let mut surfaces: Vec<&Surface> = steps
                .iter()
                .filter_map(|(origin, _)| match origin {
                    Origin::Teardown(Source::Rebinding(s))
                    | Origin::Restoration(Source::Rebinding(s)) => Some(s),
                    Origin::Whole(_)
                    | Origin::SplitPart(_)
                    | Origin::Teardown(Source::Change(_))
                    | Origin::Restoration(Source::Change(_)) => None,
                })
                .collect();
            surfaces.sort();
            surfaces.dedup();
            surfaces
                .into_iter()
                .zip(0u32..)
                .map(|(s, n)| (s, number(logical) + n))
                .collect()
        };
        let mut parts: BTreeMap<u32, u32> = BTreeMap::new();
        let operations = steps
            .iter()
            .map(|(origin, step)| {
                let source = match origin {
                    Origin::Whole(i)
                    | Origin::SplitPart(i)
                    | Origin::Teardown(Source::Change(i))
                    | Origin::Restoration(Source::Change(i)) => number(*i),
                    Origin::Teardown(Source::Rebinding(s))
                    | Origin::Restoration(Source::Rebinding(s)) => rebindings[s],
                };
                let part = parts.entry(source).or_insert(0);
                let id = OperationId {
                    source,
                    part: *part,
                };
                *part += 1;
                Operation {
                    id,
                    origin: origin.clone(),
                    step: step.clone(),
                    modules,
                }
            })
            .collect();
        Self { operations }
    }

    /// The same operations, keeping their ids, in the order `sequence`
    /// gives as positions into this one: how a scheduler hands back an
    /// order without renumbering what it ordered.
    ///
    /// # Panics
    ///
    /// When `sequence` is not a permutation of the positions.
    pub fn reordered(&self, sequence: &[usize]) -> Self {
        let mut seen = vec![false; self.operations.len()];
        assert_eq!(sequence.len(), seen.len(), "not a permutation");
        let operations = sequence
            .iter()
            .map(|&i| {
                assert!(!std::mem::replace(&mut seen[i], true), "not a permutation");
                self.operations[i].clone()
            })
            .collect();
        Self { operations }
    }

    pub fn as_slice(&self) -> &[Operation] {
        &self.operations
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Operation> {
        self.operations.iter()
    }

    pub fn len(&self) -> usize {
        self.operations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }
}

impl<'a> IntoIterator for &'a Operations {
    type Item = &'a Operation;
    type IntoIter = std::slice::Iter<'a, Operation>;
    fn into_iter(self) -> Self::IntoIter {
        self.operations.iter()
    }
}

fn number(i: usize) -> u32 {
    u32::try_from(i).expect("a plan has fewer than 2^32 changes")
}

/// Which instance of a key stands at a boundary of an ordered plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InForce {
    /// No operation of the plan starts or ends an instance of the key, so
    /// whether one stands is the baseline's to say, not this replay's.
    Untouched,
    Base,
    /// The baseline's instance has ended and the plan's has not started, or
    /// the plan's has not started on a key the baseline did not hold.
    Absent,
    Final,
}

/// Why a replay refused a plan.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ReplayError {
    #[error("operation {operation}: {error}")]
    Owner {
        operation: OperationId,
        error: OwnerError,
    },
    /// A key whose instances would be more than the baseline's and the
    /// plan's: a second removal, a second creation, or a removal of what the
    /// plan created. Each reads one instance as another.
    #[error("{key:?} would have a third instance: operation {at} after {earlier:?}")]
    Third {
        // Boxed: the key is most of the error, and every replay returns one.
        key: Box<ObjectKey>,
        earlier: Vec<OperationId>,
        at: OperationId,
    },
}

#[derive(Debug, Clone, Default)]
struct Lifetime {
    ends: Option<(usize, OperationId)>,
    produces: Option<(usize, OperationId)>,
}

/// The instances of every key an ordered plan starts or ends, replayed from
/// each operation's own payload.
#[derive(Debug, Clone, Default)]
pub struct Incarnations {
    keys: BTreeMap<ObjectKey, Lifetime>,
}

impl Incarnations {
    /// Replays `operations` in the order given, which has to be one the plan
    /// runs in: a position here is a boundary of execution.
    pub fn replay(operations: &[Operation], owners: &dyn Owners) -> Result<Self, ReplayError> {
        let mut keys: BTreeMap<ObjectKey, Lifetime> = BTreeMap::new();
        for (position, operation) in operations.iter().enumerate() {
            let defined = operation
                .defines(owners)
                .map_err(|error| ReplayError::Owner {
                    operation: operation.id,
                    error,
                })?;
            for Definition { key, transition } in defined {
                let life = keys.entry(key.clone()).or_default();
                let earlier: Vec<OperationId> = life
                    .ends
                    .iter()
                    .chain(&life.produces)
                    .map(|(_, id)| *id)
                    .collect();
                let slot = match transition {
                    // Ending after the plan's own instance started would end
                    // that one, not the baseline's.
                    Transition::Ends if life.ends.is_none() && life.produces.is_none() => {
                        &mut life.ends
                    }
                    Transition::Produces if life.produces.is_none() => &mut life.produces,
                    Transition::Ends | Transition::Produces => {
                        return Err(ReplayError::Third {
                            key: Box::new(key),
                            earlier,
                            at: operation.id,
                        });
                    }
                };
                *slot = Some((position, operation.id));
            }
        }
        Ok(Self { keys })
    }

    /// Which instance of `key` stands just before the operation at
    /// `position` runs; `position` equal to the plan's length is its end.
    pub fn in_force(&self, key: &ObjectKey, position: usize) -> InForce {
        let Some(life) = self.keys.get(key) else {
            return InForce::Untouched;
        };
        match (life.ends, life.produces) {
            (_, Some((p, _))) if position > p => InForce::Final,
            (Some((e, _)), _) if position <= e => InForce::Base,
            (Some(_), _) | (None, Some(_)) => InForce::Absent,
            (None, None) => InForce::Untouched,
        }
    }
}

#[cfg(test)]
mod tests;
