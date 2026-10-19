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

use super::{ColumnKey, ObjectKey, Operation, OwnerError, Owners, PartKey, Recorded, Spelling};
use crate::change::DetachedKind;
use crate::{Change, ModuleId, ModuleKind, Table, TableName};

/// The kind of a name. The dialect maps each to the namespace that holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NameKind {
    Table,
    Module(ModuleKind),
    Index,
    /// A primary key or a unique constraint: a constraint backed by an index,
    /// so it is in whichever namespace holds either.
    Key,
    /// A check or a foreign key.
    Constraint,
    Column,
    Role,
}

/// A name as a step spells it, with what scopes it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Named {
    pub address: Address,
    pub holder: ObjectKey,
}

/// The names one operation claims and releases.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    pub claims: Vec<Named>,
    pub releases: Vec<Named>,
}

impl Names {
    pub fn is_empty(&self) -> bool {
        self.claims.is_empty() && self.releases.is_empty()
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
fn whole_table(name: &TableName, owner: &Recorded, table: &Table) -> Vec<Named> {
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
    if let Some(pk_name) = table.primary_key.as_ref().and_then(|pk| pk.name.as_deref()) {
        out.push(part(NameKind::Key, PartKey::PrimaryKey, pk_name));
    }
    for n in table.unique.keys() {
        out.push(part(NameKind::Key, PartKey::Unique(n.clone()), n));
    }
    for n in table.indexes.keys() {
        out.push(part(NameKind::Index, PartKey::Index(n.clone()), n));
    }
    for n in table.checks.keys() {
        out.push(part(NameKind::Constraint, PartKey::Check(n.clone()), n));
    }
    for n in table.foreign_keys.keys() {
        out.push(part(
            NameKind::Constraint,
            PartKey::ForeignKey(n.clone()),
            n,
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
    table: impl Fn(&TableName) -> Option<Recorded>,
    id: &ModuleId,
    kind: ModuleKind,
) -> Option<Address> {
    Some(match id {
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
            table: Some(ObjectKey::Table(table(on)?)),
            name: name.clone(),
            signature: None,
        },
    })
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
    let mut nx = Nx {
        op,
        owners,
        names: Names::default(),
    };
    match op.change() {
        Change::CreateTable {
            uid, name, table, ..
        } => {
            let owner = Recorded(uid.clone());
            for named in whole_table(name, &owner, table) {
                nx.names.claims.push(named);
            }
            for (column, _) in &table.columns {
                if let Ok(column_uid) = owners.column(&name.column(column), Spelling::Final) {
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
        Change::DropTable { uid, name, .. } => {
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
                    for named in whole_table(name, &owner, table) {
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
        Change::DetachPartition { table, names, .. } => {
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
                };
                nx.part(true, table, kind, key, name)?;
            }
        }
        Change::RenameTable { uid, from, to, .. } => {
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
                for named in whole_table(from, &owner, base.table) {
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
        Change::AddColumn {
            uid, table, name, ..
        } => nx.column(true, table, uid, name.raw())?,
        Change::DropColumn { uid, column } => {
            nx.column(false, &column.table, uid, column.name.raw())?
        }
        Change::RenameColumn {
            uid,
            table,
            from,
            to,
            ..
        } => {
            nx.column(false, table, uid, from.raw())?;
            nx.column(true, table, uid, to.raw())?;
        }
        Change::AddComputedColumn { table, name, .. } => {
            nx.part(
                true,
                table,
                NameKind::Column,
                PartKey::Computed(name.clone()),
                name.raw(),
            )?;
        }
        Change::DropComputedColumn { table, name, .. } => {
            nx.part(
                false,
                table,
                NameKind::Column,
                PartKey::Computed(name.clone()),
                name.raw(),
            )?;
        }
        Change::SetPrimaryKey {
            table, from, to, ..
        } => {
            if let Some(name) = from.as_ref().and_then(|pk| pk.name.as_deref()) {
                nx.part(false, table, NameKind::Key, PartKey::PrimaryKey, name)?;
            }
            if let Some(name) = to.as_ref().and_then(|pk| pk.name.as_deref()) {
                nx.part(true, table, NameKind::Key, PartKey::PrimaryKey, name)?;
            }
        }
        Change::AddUnique { table, name, .. } => {
            nx.part(
                true,
                table,
                NameKind::Key,
                PartKey::Unique(name.clone()),
                name,
            )?;
        }
        Change::DropUnique { table, name } => {
            nx.part(
                false,
                table,
                NameKind::Key,
                PartKey::Unique(name.clone()),
                name,
            )?;
        }
        Change::AddForeignKey { table, name, .. } => {
            nx.part(
                true,
                table,
                NameKind::Constraint,
                PartKey::ForeignKey(name.clone()),
                name,
            )?;
        }
        Change::DropForeignKey { table, name } => {
            nx.part(
                false,
                table,
                NameKind::Constraint,
                PartKey::ForeignKey(name.clone()),
                name,
            )?;
        }
        Change::AddCheck { table, name, .. } => {
            nx.part(
                true,
                table,
                NameKind::Constraint,
                PartKey::Check(name.clone()),
                name,
            )?;
        }
        Change::DropCheck { table, name } => {
            nx.part(
                false,
                table,
                NameKind::Constraint,
                PartKey::Check(name.clone()),
                name,
            )?;
        }
        Change::AddIndex { table, name, .. } => {
            nx.part(
                true,
                table,
                NameKind::Index,
                PartKey::Index(name.clone()),
                name,
            )?;
        }
        Change::DropIndex { table, name } => {
            nx.part(
                false,
                table,
                NameKind::Index,
                PartKey::Index(name.clone()),
                name,
            )?;
        }
        Change::CreateModule { id, module } => {
            if let Some(address) = module_address(|t| nx.table(t).ok(), id, module.kind) {
                nx.claim(address, ObjectKey::Module(id.clone()));
            }
        }
        Change::DropModule { id, kind } => {
            if let Some(address) = module_address(|t| nx.table(t).ok(), id, *kind) {
                nx.release(address, ObjectKey::Module(id.clone()));
            }
        }
        Change::CreateRole { uid, name } => {
            nx.claim(role_address(name), ObjectKey::Role(Recorded(uid.clone())));
        }
        Change::DropRole { uid, name, .. } => {
            nx.release(role_address(name), ObjectKey::Role(Recorded(uid.clone())));
        }
        Change::RenameRole { uid, from, to } => {
            let holder = ObjectKey::Role(Recorded(uid.clone()));
            nx.release(role_address(from), holder.clone());
            nx.claim(role_address(to), holder);
        }
        // A module rebuilt in place keeps its name; the rest take none.
        Change::AlterModule { .. }
        | Change::AttachPartition { .. }
        | Change::AlterColumnType { .. }
        | Change::AlterColumnNullability { .. }
        | Change::AlterColumnDefault { .. }
        | Change::AlterColumnExpression { .. }
        | Change::AlterColumnNumbering { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetReplicaIdentity { .. }
        | Change::SetStorageParameters { .. }
        | Change::SetTablePersistence { .. }
        | Change::SetPartitionDefault { .. }
        | Change::SetPartitionNotNull { .. }
        | Change::SetIndexStorageParameters { .. }
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::SetEnsureKeys { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. }
        | Change::PublicExecution { .. } => {}
    }
    Ok(nx.names)
}

fn role_address(name: &str) -> Address {
    Address {
        kind: NameKind::Role,
        schema: String::new(),
        table: None,
        name: name.to_owned(),
        signature: None,
    }
}

/// Every name the baseline holds, with its holder: what stands before the
/// first step. A name the identity file records no owner for is nobody's.
pub fn baseline_names(owners: &dyn Owners, before: super::Snapshot<'_>) -> Vec<Named> {
    let table = |name: &TableName| owners.table(name, Spelling::Base).ok().map(Recorded);
    let mut out = Vec::new();
    for (name, t) in &before.schema.tables {
        let Some(owner) = table(name) else {
            continue;
        };
        out.extend(whole_table(name, &owner, t));
        for column in t.columns.keys() {
            if let Ok(uid) = owners.column(&name.column(column), Spelling::Base) {
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
        if let Some(address) = module_address(table, id, module.kind) {
            out.push(Named {
                address,
                holder: ObjectKey::Module(id.clone()),
            });
        }
    }
    for role in before.schema.roles.keys() {
        if let Ok(uid) = owners.role(role, Spelling::Base) {
            out.push(Named {
                address: role_address(role),
                holder: ObjectKey::Role(Recorded(uid)),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_routine_claim_and_release_use_the_same_raw_signature() {
        let part = "x\n\\\u{200b}";
        let id: ModuleId = format!("app.{part}(s.\"{part}\")").parse().unwrap();
        let claim = module_address(|_| None, &id, ModuleKind::Function).unwrap();
        // A catalog/serialized identity is the other side of the slot lookup.
        let parsed: ModuleId = id.raw().to_string().parse().unwrap();
        let release = module_address(|_| None, &parsed, ModuleKind::Function).unwrap();
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
            module_address(|_| None, &sibling, ModuleKind::Function).unwrap()
        );
    }
}
