//! The names each operation takes and gives up (#1776 slice S1b-ii,
//! DEC-1964.1).
//!
//! A name is an address, not an identity: two objects can hold one name in
//! turn, and a step that takes a name another object still holds fails in the
//! engine whatever its own effects say. [`names`] lists, per operation, the
//! names it claims and the names it releases, each with the object that holds
//! it. Like [`super::effects`], it is an exhaustive match over [`Change`] with
//! no wildcard arm, so a kind that arrives without its names does not compile.
//!
//! The model says only what kind of name a step takes and what scopes it. It
//! never says which namespace holds it: that is the engine's, and the dialect
//! answers it (`Dialect::slot`). Whether two spellings are one name is a
//! catalog fact, also the dialect's.
//!
//! Only declared names are here. A name the engine generates, an unnamed
//! primary key's or a SQL Server default constraint's, is one alternative of
//! several (review of #1346), and a PostgreSQL array type is not a slot at all
//! (#1503). They reach the order only through the catalog, as occupants.

use super::effects::door;
use super::{ColumnKey, ObjectKey, Operation, OwnerError, Owners, PartKey, Recorded, Spelling};
use crate::ColumnChange;
use crate::ConstraintChange;
use crate::ExtensionChange;
use crate::IndexChange;
use crate::ModuleChange;
use crate::PartitionChange;
use crate::PolicyChange;
use crate::RoleChange;
use crate::SearchChange;
use crate::TableChange;
use crate::TypeChange;
use crate::TypeValueChange;
use crate::change::DetachedKind;
use crate::{Change, ModuleId, ModuleKind, Table, TableName};

/// The kind of a name. The dialect maps each to the namespace that holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NameKind {
    Type,
    Table,
    Module(ModuleKind),
    Index,
    /// A primary key or a unique constraint: a constraint backed by an index,
    /// so it is in whichever namespace holds either.
    Key,
    /// A check or a foreign key.
    Constraint,
    Policy,
    Column,
    Role,
    /// A package name: database-global, in its own namespace (DEC-2447.6).
    /// Only the package's own name is claimed; the objects it brings are read
    /// back and checked after the rehearsal, not ordered (DEC-2447.7).
    Extension,
    /// A text search configuration or dictionary: each kind its own namespace in a schema
    /// (DEC-2728.2).
    Search(crate::SearchKind),
}

/// A name as a step spells it, with what scopes it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address {
    pub kind: NameKind,
    /// The schema the name is in, as the step spells it. Empty for a role,
    /// which belongs to no schema.
    pub schema: String,
    /// The table a part or a column belongs to, by identity: a name that is
    /// unique per table stays with its table through a rename.
    pub table: Option<ObjectKey>,
    pub name: String,
    /// A routine's argument types, which tell it apart from another of one
    /// name only where the engine overloads (`Dialect::overloads`).
    pub signature: Option<String>,
}

/// A name and the object that holds it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Named {
    pub address: Address,
    pub holder: ObjectKey,
}

/// The names one operation claims and releases.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    /// Inputs must find this exact holder at this address before the step.
    pub reads: Vec<Named>,
    pub claims: Vec<Named>,
    pub releases: Vec<Named>,
}

impl Names {
    pub fn is_empty(&self) -> bool {
        self.reads.is_empty() && self.claims.is_empty() && self.releases.is_empty()
    }
}

struct Nx<'a> {
    op: &'a Operation,
    owners: &'a dyn Owners,
    names: Names,
}

impl Nx<'_> {
    fn table(&self, name: &TableName) -> Result<Recorded, OwnerError> {
        self.owners
            .table(name, self.op.origin().spelling())
            .map(Recorded)
    }

    /// A table a name only points at (DEC-2993.2): one the project does not
    /// declare is nobody's scope, and any other failure is the refusal.
    fn table_reference(&self, name: &TableName) -> Result<Option<Recorded>, OwnerError> {
        door(
            self.owners
                .table(name, self.op.origin().spelling())
                .map(Recorded),
        )?
        .reference()
    }

    fn claim(&mut self, address: Address, holder: ObjectKey) {
        self.names.claims.push(Named { address, holder });
    }

    fn release(&mut self, address: Address, holder: ObjectKey) {
        self.names.releases.push(Named { address, holder });
    }

    fn part(
        &mut self,
        claim: bool,
        table: &TableName,
        kind: NameKind,
        key: PartKey,
        name: &str,
    ) -> Result<(), OwnerError> {
        let owner = self.table(table)?;
        let address = Address {
            kind,
            schema: table.schema.clone(),
            table: Some(ObjectKey::Table(owner.clone())),
            name: name.to_owned(),
            signature: None,
        };
        let holder = ObjectKey::Part {
            table: owner,
            part: key,
        };
        if claim {
            self.claim(address, holder);
        } else {
            self.release(address, holder);
        }
        Ok(())
    }

    fn column(
        &mut self,
        claim: bool,
        table: &TableName,
        uid: &crate::Uid,
        name: &str,
    ) -> Result<(), OwnerError> {
        let owner = self.table(table)?;
        let address = Address {
            kind: NameKind::Column,
            schema: table.schema.clone(),
            table: Some(ObjectKey::Table(owner)),
            name: name.to_owned(),
            signature: None,
        };
        let holder = ObjectKey::Column(ColumnKey::Recorded(Recorded(uid.clone())));
        if claim {
            self.claim(address, holder);
        } else {
            self.release(address, holder);
        }
        Ok(())
    }
}

/// Every declared name `table` holds under `name`, owned by `owner`: what its
/// creation claims and its drop releases.
fn whole_table(name: &TableName, owner: &Recorded, table: &Table, policies: bool) -> Vec<Named> {
    let scope = Some(ObjectKey::Table(owner.clone()));
    let part = |kind: NameKind, key: PartKey, part_name: &str| Named {
        address: Address {
            kind,
            schema: name.schema.clone(),
            table: scope.clone(),
            name: part_name.to_owned(),
            signature: None,
        },
        holder: ObjectKey::Part {
            table: owner.clone(),
            part: key,
        },
    };
    let mut out = vec![Named {
        address: Address {
            kind: NameKind::Table,
            schema: name.schema.clone(),
            table: None,
            name: name.name.clone(),
            signature: None,
        },
        holder: ObjectKey::Table(owner.clone()),
    }];
    if let Some(pk_name) = table
        .primary_key
        .as_ref()
        .and_then(|pk| pk.name.as_ref().map(crate::ObjectLocalName::raw))
    {
        out.push(part(NameKind::Key, PartKey::PrimaryKey, pk_name));
    }
    for n in table.unique.keys() {
        out.push(part(NameKind::Key, PartKey::Unique(n.clone()), n.raw()));
    }
    for n in table.exclude.keys() {
        out.push(part(NameKind::Key, PartKey::Exclusion(n.clone()), n.raw()));
    }
    for n in table.indexes.keys() {
        out.push(part(NameKind::Index, PartKey::Index(n.clone()), n.raw()));
    }
    if policies {
        for n in table.row_policies.keys() {
            out.push(part(NameKind::Policy, PartKey::Policy(n.clone()), n));
        }
    }
    for n in table.checks.keys() {
        out.push(part(
            NameKind::Constraint,
            PartKey::Check(n.clone()),
            n.raw(),
        ));
    }
    for n in table.foreign_keys.keys() {
        out.push(part(
            NameKind::Constraint,
            PartKey::ForeignKey(n.clone()),
            n.raw(),
        ));
    }
    for n in table.computed.keys() {
        out.push(part(
            NameKind::Column,
            PartKey::Computed(n.clone()),
            n.raw(),
        ));
    }
    out
}

/// The address a module's id takes: a view or a routine by its name in its
/// schema, a trigger by its name on its table. `None` for a trigger on a table
/// the identity file does not record, which is nobody's scope.
fn module_address(
    table: impl Fn(&TableName) -> Result<Option<Recorded>, OwnerError>,
    id: &ModuleId,
    kind: ModuleKind,
) -> Result<Option<Address>, OwnerError> {
    Ok(Some(match id {
        ModuleId::Named(name) => Address {
            kind: NameKind::Module(kind),
            schema: name.schema.clone(),
            table: None,
            name: name.name.clone(),
            signature: None,
        },
        ModuleId::Routine(routine) => Address {
            kind: NameKind::Module(kind),
            schema: routine.name.schema.clone(),
            table: None,
            name: routine.name.name.clone(),
            signature: Some(routine.raw().to_string()),
        },
        ModuleId::Trigger { on, name } => Address {
            kind: NameKind::Module(kind),
            schema: on.schema.clone(),
            table: Some(ObjectKey::Table(match table(on)? {
                Some(owner) => owner,
                None => return Ok(None),
            })),
            name: name.clone(),
            signature: None,
        },
    }))
}

/// What `op` claims and releases.
///
/// `before` is the baseline: a dropped table's names are read from it. The
/// names a creation claims are in its own payload.
pub fn names(
    op: &Operation,
    owners: &dyn Owners,
    before: super::Snapshot<'_>,
) -> Result<Names, OwnerError> {
    let input_owners = op.input_owners(owners);
    let owners: &dyn Owners = &input_owners;
    let mut nx = Nx {
        op,
        owners,
        names: Names {
            reads: read_names(op, owners)?,
            ..Names::default()
        },
    };
    match op.change() {
        Change::Index(IndexChange::BuildPartitionedIndex(r)) => {
            r.validate().map_err(|why| OwnerError::Rebuild {
                name: r.parent.name.shown().to_string(),
                why,
            })?;
            let holder = op.key(owners)?;
            let kind = match r.definition {
                crate::PartitionIndexDefinition::Index(_) => NameKind::Index,
                crate::PartitionIndexDefinition::PrimaryKey(_)
                | crate::PartitionIndexDefinition::Unique(_)
                | crate::PartitionIndexDefinition::Exclusion(_) => NameKind::Key,
            };
            for (relation, name) in std::iter::once((&r.parent, &r.name))
                .chain(r.clones.iter().map(|c| (&c.leaf, &c.name)))
            {
                let table = super::checked_relation(owners, relation, Spelling::Final)?;
                nx.claim(
                    Address {
                        kind,
                        schema: relation.name.schema.clone(),
                        table: Some(ObjectKey::Table(table)),
                        name: name.raw().to_owned(),
                        signature: None,
                    },
                    holder.clone(),
                );
            }
        }
        Change::Partition(PartitionChange::RebuildPartitionedTable(_))
        | Change::Partition(PartitionChange::CopyTableRows(_))
        | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => {}
        Change::Type(TypeChange::CreateType { uid, name, .. }) => {
            nx.claim(type_address(name), ObjectKey::Type(Recorded(uid.clone())))
        }
        Change::Type(TypeChange::DropType { uid, name, .. }) => {
            nx.release(type_address(name), ObjectKey::Type(Recorded(uid.clone())))
        }
        Change::Search(SearchChange::CreateSearch { uid, name, .. }) => nx.claim(
            search_address(name),
            ObjectKey::Search(Recorded(uid.clone())),
        ),
        Change::Search(SearchChange::DropSearch { uid, name, .. }) => nx.release(
            search_address(name),
            ObjectKey::Search(Recorded(uid.clone())),
        ),
        // A rename hands the address over: `from` is free and `to` taken once it has run,
        // in the search kind's own namespace (DEC-2728.5).
        Change::Search(SearchChange::RenameSearch { uid, from, to, .. }) => {
            let holder = ObjectKey::Search(Recorded(uid.clone()));
            nx.release(search_address(from), holder.clone());
            nx.claim(search_address(to), holder);
        }
        // The replacement holds its temporary name from its creation to the
        // swap, which hands the type back its own (DEC-1635.7).
        Change::TypeValue(TypeValueChange::EnumRebuild {
            uid,
            tmp,
            step: crate::EnumRebuildStep::Create { .. },
            ..
        }) => nx.claim(type_address(tmp), ObjectKey::Type(Recorded(uid.clone()))),
        Change::TypeValue(TypeValueChange::EnumRebuild {
            uid,
            tmp,
            step: crate::EnumRebuildStep::Swap { .. },
            ..
        }) => nx.release(type_address(tmp), ObjectKey::Type(Recorded(uid.clone()))),
        Change::Type(TypeChange::RenameType { uid, from, to, .. }) => {
            let holder = ObjectKey::Type(Recorded(uid.clone()));
            nx.release(type_address(from), holder.clone());
            nx.claim(type_address(to), holder);
        }

        Change::Table(TableChange::CreateTable {
            uid, name, table, ..
        }) => {
            let owner = Recorded(uid.clone());
            for named in whole_table(name, &owner, table, false) {
                nx.names.claims.push(named);
            }
            for (column, _) in &table.columns {
                if let Some(column_uid) =
                    door(owners.column(&name.column(column), Spelling::Final))?.reference()?
                {
                    nx.claim(
                        Address {
                            kind: NameKind::Column,
                            schema: name.schema.clone(),
                            table: Some(ObjectKey::Table(owner.clone())),
                            name: column.raw().to_owned(),
                            signature: None,
                        },
                        ObjectKey::Column(ColumnKey::Recorded(Recorded(column_uid))),
                    );
                }
            }
        }
        Change::Table(TableChange::DropTable { uid, name, .. }) => {
            let owner = Recorded(uid.clone());
            // The baseline's parts, under the name the drop spells: a table
            // the plan drops is the baseline's, at wherever the plan has put
            // it by then.
            let base = before
                .ids
                .tables
                .get(uid)
                .and_then(|base| before.schema.tables.get(base));
            match base {
                Some(table) => {
                    for named in whole_table(name, &owner, table, true) {
                        nx.names.releases.push(named);
                    }
                }
                None => nx.release(
                    Address {
                        kind: NameKind::Table,
                        schema: name.schema.clone(),
                        table: None,
                        name: name.name.clone(),
                        signature: None,
                    },
                    ObjectKey::Table(owner),
                ),
            }
        }
        Change::Partition(PartitionChange::DetachPartition { table, names, .. }) => {
            // The detached table's clones take the names its declaration
            // gives them. The names they had were the engine's.
            for detached in names {
                let Some(name) = &detached.name else {
                    continue;
                };
                let (kind, key) = match detached.kind {
                    DetachedKind::PrimaryKey => (NameKind::Key, PartKey::PrimaryKey),
                    DetachedKind::Unique => (NameKind::Key, PartKey::Unique(name.clone())),
                    DetachedKind::ForeignKey => {
                        (NameKind::Constraint, PartKey::ForeignKey(name.clone()))
                    }
                    DetachedKind::Check => (NameKind::Constraint, PartKey::Check(name.clone())),
                    DetachedKind::Index => (NameKind::Index, PartKey::Index(name.clone())),
                    // An index-backed constraint, named as a unique constraint is.
                    DetachedKind::Exclusion => (NameKind::Key, PartKey::Exclusion(name.clone())),
                };
                nx.part(true, table, kind, key, name.raw())?;
            }
        }
        Change::Table(TableChange::RenameTable { uid, from, to, .. }) => {
            let owner = Recorded(uid.clone());
            let holder = ObjectKey::Table(owner.clone());
            let address = |name: &TableName| Address {
                kind: NameKind::Table,
                schema: name.schema.clone(),
                table: None,
                name: name.name.clone(),
                signature: None,
            };
            nx.release(address(from), holder.clone());
            nx.claim(address(to), holder);
            // A move to another schema carries the table's parts with it.
            // Each is released where it was and claimed where it lands; the
            // dialect puts a per-table name in one namespace on both sides,
            // and the pair cancels there.
            if from.schema != to.schema
                && let Some(base) = super::base_table(op, owners, before, from)?
            {
                for named in whole_table(from, &owner, base.table, true) {
                    if named.address.kind == NameKind::Table {
                        continue;
                    }
                    let mut landed = named.clone();
                    landed.address.schema = to.schema.clone();
                    nx.names.releases.push(named);
                    nx.names.claims.push(landed);
                }
            }
        }
        Change::Column(ColumnChange::AddColumn {
            uid, table, name, ..
        }) => nx.column(true, table, uid, name.raw())?,
        Change::Column(ColumnChange::DropColumn { uid, column }) => {
            nx.column(false, &column.table, uid, column.name.raw())?
        }
        Change::Column(ColumnChange::RenameColumn {
            uid,
            table,
            from,
            to,
            ..
        }) => {
            nx.column(false, table, uid, from.raw())?;
            nx.column(true, table, uid, to.raw())?;
        }
        Change::Column(ColumnChange::AddComputedColumn { table, name, .. }) => {
            nx.part(
                true,
                table,
                NameKind::Column,
                PartKey::Computed(name.clone()),
                name.raw(),
            )?;
        }
        Change::Column(ColumnChange::DropComputedColumn { table, name, .. }) => {
            nx.part(
                false,
                table,
                NameKind::Column,
                PartKey::Computed(name.clone()),
                name.raw(),
            )?;
        }
        Change::Constraint(ConstraintChange::SetPrimaryKey {
            table, from, to, ..
        }) => {
            if let Some(name) = from
                .as_ref()
                .and_then(|pk| pk.name.as_ref().map(crate::ObjectLocalName::raw))
            {
                nx.part(false, table, NameKind::Key, PartKey::PrimaryKey, name)?;
            }
            if let Some(name) = to
                .as_ref()
                .and_then(|pk| pk.name.as_ref().map(crate::ObjectLocalName::raw))
            {
                nx.part(true, table, NameKind::Key, PartKey::PrimaryKey, name)?;
            }
        }
        Change::Constraint(ConstraintChange::AddUnique { table, name, .. }) => {
            nx.part(
                true,
                table,
                NameKind::Key,
                PartKey::Unique(name.clone()),
                name.raw(),
            )?;
        }
        Change::Constraint(ConstraintChange::DropUnique { table, name }) => {
            nx.part(
                false,
                table,
                NameKind::Key,
                PartKey::Unique(name.clone()),
                name.raw(),
            )?;
        }
        Change::Constraint(ConstraintChange::SetExclusion {
            table,
            name,
            from,
            to,
        }) => {
            if from.is_some() {
                nx.part(
                    false,
                    table,
                    NameKind::Key,
                    PartKey::Exclusion(name.clone()),
                    name.raw(),
                )?;
            }
            if to.is_some() {
                nx.part(
                    true,
                    table,
                    NameKind::Key,
                    PartKey::Exclusion(name.clone()),
                    name.raw(),
                )?;
            }
        }
        Change::Constraint(ConstraintChange::AddForeignKey { table, name, .. }) => {
            nx.part(
                true,
                table,
                NameKind::Constraint,
                PartKey::ForeignKey(name.clone()),
                name.raw(),
            )?;
        }
        Change::Constraint(ConstraintChange::DropForeignKey { table, name }) => {
            nx.part(
                false,
                table,
                NameKind::Constraint,
                PartKey::ForeignKey(name.clone()),
                name.raw(),
            )?;
        }
        Change::Policy(PolicyChange::CreatePolicy { table, name, .. })
        | Change::Policy(PolicyChange::DropPolicy { table, name, .. }) => {
            nx.part(
                matches!(
                    op.change(),
                    Change::Policy(PolicyChange::CreatePolicy { .. })
                ),
                table,
                NameKind::Policy,
                PartKey::Policy(name.clone()),
                name,
            )?;
        }
        Change::Constraint(ConstraintChange::AddCheck { table, name, .. }) => {
            nx.part(
                true,
                table,
                NameKind::Constraint,
                PartKey::Check(name.clone()),
                name.raw(),
            )?;
        }
        Change::Constraint(ConstraintChange::DropCheck { table, name }) => {
            nx.part(
                false,
                table,
                NameKind::Constraint,
                PartKey::Check(name.clone()),
                name.raw(),
            )?;
        }
        Change::Index(IndexChange::AddIndex { table, name, .. }) => {
            nx.part(
                true,
                table,
                NameKind::Index,
                PartKey::Index(name.clone()),
                name.raw(),
            )?;
        }
        Change::Index(IndexChange::DropIndex { table, name }) => {
            nx.part(
                false,
                table,
                NameKind::Index,
                PartKey::Index(name.clone()),
                name.raw(),
            )?;
        }
        Change::Module(ModuleChange::CreateModule { id, module }) => {
            if let Some(address) = module_address(|t| nx.table_reference(t), id, module.kind)? {
                nx.claim(address, ObjectKey::Module(id.clone()));
            }
        }
        Change::Module(ModuleChange::DropModule { id, kind }) => {
            if let Some(address) = module_address(|t| nx.table_reference(t), id, *kind)? {
                nx.release(address, ObjectKey::Module(id.clone()));
            }
        }
        Change::Role(RoleChange::CreateRole { uid, name }) => {
            nx.claim(role_address(name), ObjectKey::Role(Recorded(uid.clone())));
        }
        Change::Role(RoleChange::DropRole { uid, name, .. }) => {
            nx.release(role_address(name), ObjectKey::Role(Recorded(uid.clone())));
        }
        Change::Role(RoleChange::RenameRole { uid, from, to }) => {
            let holder = ObjectKey::Role(Recorded(uid.clone()));
            nx.release(role_address(from), holder.clone());
            nx.claim(role_address(to), holder);
        }
        Change::Extension(ExtensionChange::DropExtension { uid, name, .. }) => {
            nx.release(
                extension_address(name),
                ObjectKey::Extension(Recorded(uid.clone())),
            );
        }
        Change::Extension(ExtensionChange::CreateExtension { uid, name, .. }) => {
            nx.claim(
                extension_address(name),
                ObjectKey::Extension(Recorded(uid.clone())),
            );
        }
        // A module rebuilt in place keeps its name; the rest take none.
        Change::TypeValue(_)
        | Change::Module(ModuleChange::AlterModule { .. })
        | Change::Search(SearchChange::AlterSearchMapping { .. })
        | Change::Search(SearchChange::AlterSearchDictionary { .. })
        | Change::Extension(ExtensionChange::UpdateExtension { .. })
        | Change::Extension(ExtensionChange::SetExtensionSchema { .. })
        | Change::Partition(PartitionChange::AttachPartition { .. })
        | Change::Column(ColumnChange::AlterColumnType { .. })
        | Change::Column(ColumnChange::AlterColumnNullability { .. })
        | Change::Column(ColumnChange::AlterColumnDefault { .. })
        | Change::Column(ColumnChange::AlterColumnExpression { .. })
        | Change::Column(ColumnChange::AlterColumnNumbering { .. })
        | Change::Column(ColumnChange::SetColumnDeprecated { .. })
        | Change::Policy(PolicyChange::SetRowSecurity { .. })
        | Change::TableProperty(_)
        | Change::Partition(PartitionChange::SetPartitionDefault { .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
        | Change::Index(IndexChange::SetIndexStorageParameters { .. })
        | Change::Row(_)
        | Change::Permission(_) => {}
    }
    Ok(nx.names)
}

fn type_address(name: &crate::TypeRef) -> Address {
    Address {
        kind: NameKind::Type,
        schema: name.schema().to_owned(),
        table: None,
        name: name.name().to_owned(),
        signature: None,
    }
}

fn search_address(name: &crate::SearchRef) -> Address {
    Address {
        kind: NameKind::Search(name.kind),
        schema: name.name.schema().to_owned(),
        table: None,
        name: name.name.name().to_owned(),
        signature: None,
    }
}

fn role_address(name: &crate::RoleName) -> Address {
    Address {
        kind: NameKind::Role,
        schema: String::new(),
        table: None,
        name: name.raw().to_owned(),
        signature: None,
    }
}

fn extension_address(name: &crate::ObjectLocalName) -> Address {
    Address {
        kind: NameKind::Extension,
        schema: String::new(),
        table: None,
        name: name.raw().to_owned(),
        signature: None,
    }
}

/// Every name the baseline holds, with its holder: what stands before the
/// first step. A name the project does not declare is nobody's; one the models
/// cannot read is the original refusal, not a name nobody holds (DEC-2993.2).
pub fn baseline_names(
    owners: &dyn Owners,
    before: super::Snapshot<'_>,
) -> Result<Vec<Named>, OwnerError> {
    let table =
        |name: &TableName| door(owners.table(name, Spelling::Base).map(Recorded))?.reference();
    let mut out = Vec::new();
    for name in before.schema.types.keys() {
        if let Some(uid) = door(owners.managed_type(name, Spelling::Base))?.reference()? {
            out.push(Named {
                address: type_address(name),
                holder: ObjectKey::Type(Recorded(uid)),
            });
        }
    }
    for name in before.schema.search.keys() {
        if let Some(uid) = before.ids.search_uid(name) {
            out.push(Named {
                address: search_address(name),
                holder: ObjectKey::Search(Recorded(uid.clone())),
            });
        }
    }
    for (name, t) in &before.schema.tables {
        let Some(owner) = table(name)? else {
            continue;
        };
        out.extend(whole_table(name, &owner, t, true));
        for column in t.columns.keys() {
            if let Some(uid) =
                door(owners.column(&name.column(column), Spelling::Base))?.reference()?
            {
                out.push(Named {
                    address: Address {
                        kind: NameKind::Column,
                        schema: name.schema.clone(),
                        table: Some(ObjectKey::Table(owner.clone())),
                        name: column.raw().to_owned(),
                        signature: None,
                    },
                    holder: ObjectKey::Column(ColumnKey::Recorded(Recorded(uid))),
                });
            }
        }
    }
    for (id, module) in &before.schema.modules {
        if let Some(address) = module_address(table, id, module.kind)? {
            out.push(Named {
                address,
                holder: ObjectKey::Module(id.clone()),
            });
        }
    }
    for role in before.schema.roles.keys() {
        if let Some(uid) = door(owners.role(role, Spelling::Base))?.reference()? {
            out.push(Named {
                address: role_address(role),
                holder: ObjectKey::Role(Recorded(uid)),
            });
        }
    }
    for name in before.schema.extensions.keys() {
        if let Some(uid) = before.ids.extension_uid(name) {
            out.push(Named {
                address: extension_address(name),
                holder: ObjectKey::Extension(Recorded(uid.clone())),
            });
        }
    }
    Ok(out)
}

/// Shared input addresses for explicit intermediate operations. Ordinary plans
/// keep their origin-based fallback; no origin splitting is needed (DEC-2298.9).
pub(super) fn read_names(op: &Operation, owners: &dyn Owners) -> Result<Vec<Named>, OwnerError> {
    let input_owners = op.input_owners(owners);
    let owners: &dyn Owners = &input_owners;
    let change = op.change();
    if !owners.address_qualified()
        && !matches!(
            change,
            Change::Partition(PartitionChange::CopyTableRows(_))
                | Change::Partition(PartitionChange::AnalyzeTableTree(_))
                | Change::Index(IndexChange::BuildPartitionedIndex(_))
        )
    {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut table = |name: &TableName, uid: crate::Uid| {
        let named = Named {
            holder: ObjectKey::Table(Recorded(uid)),
            address: Address {
                kind: NameKind::Table,
                schema: name.schema.clone(),
                table: None,
                name: name.name.clone(),
                signature: None,
            },
        };
        if !out.contains(&named) {
            out.push(named);
        }
    };
    if let Change::Partition(PartitionChange::CopyTableRows(c)) = change {
        for (relations, spelling) in [
            (
                std::iter::once(&c.source)
                    .chain(&c.source_partitions)
                    .collect::<Vec<_>>(),
                Spelling::Base,
            ),
            (
                std::iter::once(&c.target)
                    .chain(&c.target_partitions)
                    .collect(),
                Spelling::Final,
            ),
        ] {
            for relation in relations {
                table(
                    &relation.name,
                    super::checked_relation(owners, relation, spelling)?
                        .uid()
                        .clone(),
                );
            }
        }
    } else if let Change::Partition(PartitionChange::AnalyzeTableTree(a)) = change {
        for relation in std::iter::once(&a.parent).chain(&a.partitions) {
            table(
                &relation.name,
                super::checked_relation(owners, relation, Spelling::Final)?
                    .uid()
                    .clone(),
            );
        }
    } else if let Change::Index(IndexChange::BuildPartitionedIndex(i)) = change {
        for relation in std::iter::once(&i.parent).chain(i.clones.iter().map(|c| &c.leaf)) {
            table(
                &relation.name,
                super::checked_relation(owners, relation, Spelling::Final)?
                    .uid()
                    .clone(),
            );
        }
    } else if let Change::Table(TableChange::DropTable { uid, name, .. })
    | Change::Table(TableChange::RenameTable {
        uid, from: name, ..
    }) = change
    {
        table(name, uid.clone());
    } else if !matches!(
        change,
        Change::Table(TableChange::CreateTable { .. })
            | Change::Partition(PartitionChange::RebuildPartitionedTable(_))
    ) && let Some(name) = change.table()
    {
        table(name, owners.table(name, op.origin().spelling())?);
    }
    let mut input = |name: &TableName| -> Result<(), OwnerError> {
        table(name, owners.table(name, op.origin().spelling())?);
        Ok(())
    };
    if let Change::Table(TableChange::CreateTable { table, name, .. }) = change {
        if let Some(of) = &table.partition_of {
            input(&of.parent)?;
        }
        // A self-reference reads the table this creation supplies, not an
        // earlier holder, as effects.rs's CreateTable arm also treats it.
        for fk in table.foreign_keys.values() {
            if fk.references_table != *name {
                input(&fk.references_table)?;
            }
        }
    }
    if let Change::Table(TableChange::DropTable {
        detach_from: Some(parent),
        ..
    })
    | Change::Partition(PartitionChange::AttachPartition { parent, .. })
    | Change::Partition(PartitionChange::DetachPartition { parent, .. }) = change
    {
        input(parent)?;
    }
    if let Change::Constraint(ConstraintChange::AddForeignKey { constraint, .. }) = change {
        input(&constraint.references_table)?;
    }
    if let Some(crate::ModuleId::Trigger { on, .. }) = change.module_id() {
        input(on)?;
    }
    if let Change::Partition(PartitionChange::CopyTableRows(c)) = change {
        for projection in &c.columns {
            for (column, spelling) in projection
                .source
                .iter()
                .map(|s| (&s.column, Spelling::Base))
                .chain(std::iter::once((&projection.target, Spelling::Final)))
            {
                out.push(Named {
                    holder: ObjectKey::Column(ColumnKey::Recorded(Recorded(
                        owners.column(column, spelling)?,
                    ))),
                    address: Address {
                        kind: NameKind::Column,
                        schema: column.table.schema.clone(),
                        table: Some(ObjectKey::Table(Recorded(
                            owners.table(&column.table, spelling)?,
                        ))),
                        name: column.name.raw().to_owned(),
                        signature: None,
                    },
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {

    #[test]
    fn role_slots_keep_raw_names_distinct_from_their_shown_lookalikes() {
        let name = crate::RoleName::from("role\nline\\\u{202e}名前");
        let lookalike = crate::RoleName::from(name.shown().to_string());
        let raw = super::role_address(&name);
        assert_eq!(raw.name, name.raw());
        assert_eq!(raw.kind, super::NameKind::Role);
        assert_ne!(raw, super::role_address(&lookalike));
        assert_eq!(
            raw,
            super::role_address(
                &serde_json::from_str::<crate::RoleName>(&serde_json::to_string(&name).unwrap())
                    .unwrap()
            )
        );
    }

    use super::*;
    use crate::TableChange;

    #[test]
    fn a_created_table_reads_the_tables_it_references_but_not_itself() {
        struct Qualified;
        impl Owners for Qualified {
            fn address_qualified(&self) -> bool {
                true
            }
            fn table(&self, name: &TableName, _: Spelling) -> Result<crate::Uid, OwnerError> {
                Ok(crate::Uid::derived(crate::UidKind::Table, &name.name, 0))
            }
            fn column(&self, _: &crate::ColumnRef, _: Spelling) -> Result<crate::Uid, OwnerError> {
                unreachable!("a creation reads no column")
            }
            fn role(&self, _: &crate::RoleName, _: Spelling) -> Result<crate::Uid, OwnerError> {
                unreachable!("a creation reads no role")
            }
        }
        let fk = |to: &str| crate::ForeignKey {
            columns: vec!["p".into()],
            references_table: TableName::new("app", to),
            references_columns: vec!["id".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        };
        let mut table = crate::Table::default();
        table.foreign_keys.insert("self_fk".into(), fk("t"));
        table.foreign_keys.insert("other_fk".into(), fk("other"));
        let create = crate::PlannedChange::new(Change::Table(TableChange::CreateTable {
            uid: crate::Uid::derived(crate::UidKind::Table, "t", 0),
            name: TableName::new("app", "t"),
            table: Box::new(table),
            beside_default: None,
            moves_at_most: None,
        }));
        let ops = super::super::Operations::of_plan(&[create], super::super::ModuleEdits::InPlace);
        let read: Vec<_> = read_names(&ops.as_slice()[0], &Qualified)
            .unwrap()
            .into_iter()
            .map(|named| named.address.name)
            .collect();
        // The creation supplies `t`, so no earlier holder may be required.
        assert_eq!(read, ["other"]);
    }

    /// A search rename frees the old name and takes the new one, each in the namespace of
    /// its own kind, and moves nothing else (DEC-2728.5). Negatives: an in-place alteration
    /// takes no name, and the same spelling of the other kind is a different address.
    #[test]
    fn a_search_rename_releases_its_old_name_and_claims_its_new_one() {
        struct Unused;
        impl Owners for Unused {
            fn address_qualified(&self) -> bool {
                true
            }
            fn table(&self, _: &TableName, _: Spelling) -> Result<crate::Uid, OwnerError> {
                unreachable!("a search rename reads no table")
            }
            fn column(&self, _: &crate::ColumnRef, _: Spelling) -> Result<crate::Uid, OwnerError> {
                unreachable!("a search rename reads no column")
            }
            fn role(&self, _: &crate::RoleName, _: Spelling) -> Result<crate::Uid, OwnerError> {
                unreachable!("a search rename reads no role")
            }
        }
        let uid = crate::Uid::derived(crate::UidKind::Search, "words", 0);
        let from: crate::SearchRef = "dictionary app.words".parse().unwrap();
        let to: crate::SearchRef = "dictionary app.terms".parse().unwrap();
        let definition = Box::new(crate::SearchObject::Dictionary {
            template: "pg_catalog.simple".parse().unwrap(),
            options: Default::default(),
        });
        let held = |change: Change| {
            let ops = super::super::Operations::of_plan(
                &[crate::PlannedChange::new(change)],
                super::super::ModuleEdits::InPlace,
            );
            let (schema, ids) = (crate::Schema::default(), crate::IdsFile::default());
            names(
                &ops.as_slice()[0],
                &Unused,
                super::super::Snapshot {
                    schema: &schema,
                    ids: &ids,
                },
            )
            .unwrap()
        };
        let renamed = held(Change::Search(SearchChange::RenameSearch {
            uid: uid.clone(),
            from: from.clone(),
            to: to.clone(),
            definition: definition.clone(),
        }));
        let holder = ObjectKey::Search(Recorded(uid.clone()));
        assert_eq!(
            renamed.releases,
            [Named {
                address: search_address(&from),
                holder: holder.clone()
            }]
        );
        assert_eq!(
            renamed.claims,
            [Named {
                address: search_address(&to),
                holder
            }]
        );
        let other_kind: crate::SearchRef = "config app.words".parse().unwrap();
        assert_ne!(search_address(&from), search_address(&other_kind));
        let altered = held(Change::Search(SearchChange::AlterSearchDictionary {
            uid,
            name: from,
            before: definition.clone(),
            after: definition,
            opaque: Vec::new(),
        }));
        assert!(altered.claims.is_empty() && altered.releases.is_empty());
    }

    #[test]
    fn a_routine_claim_and_release_use_the_same_raw_signature() {
        let part = "x\n\\\u{200b}";
        let id: ModuleId = format!("app.{part}(s.\"{part}\")").parse().unwrap();
        let claim = module_address(|_| Ok(None), &id, ModuleKind::Function)
            .unwrap()
            .unwrap();
        // A catalog/serialized identity is the other side of the slot lookup.
        let parsed: ModuleId = id.raw().to_string().parse().unwrap();
        let release = module_address(|_| Ok(None), &parsed, ModuleKind::Function)
            .unwrap()
            .unwrap();
        assert_eq!(claim, release);
        assert_eq!(
            claim.signature.as_deref(),
            Some(id.raw().to_string().as_str())
        );
        assert_ne!(
            claim.signature.as_deref(),
            Some(id.shown().to_string().as_str())
        );
        let sibling: ModuleId = format!("app.{part}(text)").parse().unwrap();
        assert_ne!(
            claim,
            module_address(|_| Ok(None), &sibling, ModuleKind::Function)
                .unwrap()
                .unwrap()
        );
    }
}
