//! Identity-preserving address translation through one ordered plan (DEC-1851.1).
//!
//! A name is an address, not an identity. Histories are shared by table,
//! column and role namespaces; only the exhaustive extraction of typed changes
//! knows which kind of identity a change carries. This is not a catalog name
//! allocator: even a managed address vacated by the plan can have an unmanaged
//! catalog occupant that this input does not describe.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use crate::{Change, ColumnRef, IdsFile, PlannedChange, TableName, Uid, UidKind};

/// A boundary in the borrowed plan. Before/After require an existing change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    Start,
    Before(usize),
    After(usize),
    End,
}

/// Whether this plan knows the managed holder of an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder<'a> {
    Tracked(&'a Uid),
    /// Neither the typed changes nor the final identities describe this name.
    Unmanaged,
    /// A known managed address has no holder at this boundary. This says
    /// nothing about engine namespace availability.
    Vacant,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanNamesError {
    #[error("invalid final identities: {0}")]
    Ids(#[from] crate::ids::IdsError),
    #[error("{uid} has the wrong identity kind for a {namespace} change")]
    Kind { namespace: &'static str, uid: Uid },
    #[error("{namespace} identity {uid} does not hold {expected} before change {step}")]
    Source {
        namespace: &'static str,
        uid: Uid,
        expected: String,
        step: usize,
    },
    #[error("{namespace} address {address} is held by both {a} and {b} at boundary {boundary}")]
    Collision {
        namespace: &'static str,
        address: String,
        a: Uid,
        b: Uid,
        boundary: usize,
    },
    #[error("{namespace} histories disagree with the final identity mapping")]
    End { namespace: &'static str },
    #[error("column {column} has no managed owning table before change {step}")]
    Owner { column: ColumnRef, step: usize },
    #[error("column identity {uid} changes owning table")]
    ColumnOwner { uid: Uid },
}

#[derive(Debug, Clone)]
struct Transition<A> {
    step: usize,
    uid: Uid,
    before: Option<A>,
    after: Option<A>,
}

impl<A> Transition<A> {
    fn new(step: usize, uid: &Uid, before: Option<A>, after: Option<A>) -> Self {
        Self {
            step,
            uid: uid.clone(),
            before,
            after,
        }
    }
}

type Timeline<A> = Vec<(usize, Option<A>)>;

#[derive(Debug)]
struct History<A> {
    names: BTreeMap<Uid, Timeline<A>>,
    holders: BTreeMap<A, Timeline<Uid>>,
}

fn at<A>(history: &[(usize, Option<A>)], boundary: usize) -> Option<&A> {
    let n = history.partition_point(|(step, _)| *step <= boundary);
    n.checked_sub(1).and_then(|i| history[i].1.as_ref())
}

impl<A: Clone + Ord + Debug> History<A> {
    fn of(
        events: &[Transition<A>],
        after: &BTreeMap<Uid, A>,
        namespace: &'static str,
        kind: UidKind,
    ) -> Result<Self, PlanNamesError> {
        let mut initial: BTreeMap<Uid, Option<A>> = BTreeMap::new();
        for event in events {
            if event.uid.kind() != kind {
                return Err(PlanNamesError::Kind {
                    namespace,
                    uid: event.uid.clone(),
                });
            }
            initial
                .entry(event.uid.clone())
                .or_insert_with(|| event.before.clone());
        }
        for (uid, address) in after {
            initial
                .entry(uid.clone())
                .or_insert_with(|| Some(address.clone()));
        }
        Self::from_initial(events, after, initial, namespace)
    }

    fn from_initial(
        events: &[Transition<A>],
        after: &BTreeMap<Uid, A>,
        initial: BTreeMap<Uid, Option<A>>,
        namespace: &'static str,
    ) -> Result<Self, PlanNamesError> {
        let mut this = Self {
            names: BTreeMap::new(),
            holders: BTreeMap::new(),
        };
        // Reserve all known addresses, so a not-yet-created or vacated name
        // is distinct from a name for which the plan supplied no evidence.
        for event in events {
            for address in event.before.iter().chain(&event.after) {
                this.holders
                    .entry(address.clone())
                    .or_insert_with(|| vec![(0, None)]);
            }
        }
        for (uid, address) in initial {
            this.names.insert(uid.clone(), vec![(0, None)]);
            this.set(&uid, address, 0, namespace)?;
        }
        for event in events {
            let held = this.name(&event.uid, event.step);
            if held != event.before.as_ref() {
                return Err(PlanNamesError::Source {
                    namespace,
                    uid: event.uid.clone(),
                    expected: format!("{:?}", event.before),
                    step: event.step,
                });
            }
            if event.before != event.after {
                this.set(&event.uid, event.after.clone(), event.step + 1, namespace)?;
            }
        }
        let end: BTreeMap<_, _> = this
            .names
            .iter()
            .filter_map(|(uid, history)| {
                at(history, usize::MAX).map(|address| (uid.clone(), address.clone()))
            })
            .collect();
        if &end != after {
            return Err(PlanNamesError::End { namespace });
        }
        Ok(this)
    }

    fn name(&self, uid: &Uid, boundary: usize) -> Option<&A> {
        self.names.get(uid).and_then(|h| at(h, boundary))
    }

    fn holder(&self, address: &A, boundary: usize) -> Holder<'_> {
        match self.holders.get(address) {
            None => Holder::Unmanaged,
            Some(history) => match at(history, boundary) {
                Some(uid) => Holder::Tracked(uid),
                None => Holder::Vacant,
            },
        }
    }

    fn set(
        &mut self,
        uid: &Uid,
        address: Option<A>,
        boundary: usize,
        namespace: &'static str,
    ) -> Result<(), PlanNamesError> {
        if let Some(address) = &address
            && let Holder::Tracked(other) = self.holder(address, boundary)
            && other != uid
        {
            return Err(PlanNamesError::Collision {
                namespace,
                address: format!("{address:?}"),
                a: other.clone(),
                b: uid.clone(),
                boundary,
            });
        }
        if let Some(old) = self.name(uid, boundary).cloned() {
            self.holders.entry(old).or_default().push((boundary, None));
        }
        if let Some(address) = &address {
            self.holders
                .entry(address.clone())
                .or_default()
                .push((boundary, Some(uid.clone())));
        }
        self.names
            .entry(uid.clone())
            .or_default()
            .push((boundary, address));
        Ok(())
    }
}

#[derive(Debug)]
enum IdentityChange {
    Table(Transition<TableName>, ColumnLife),
    Column(Transition<ColumnRef>),
    Role(Transition<String>),
    Other,
}

/// A partition inherits names but owns no column UIDs. Attach/Detach therefore
/// change the scope of known column identities without creating/dropping a table.
#[derive(Debug)]
enum ColumnLife {
    Keep,
    Begin(BTreeSet<String>),
    End,
}

/// This match is the one place a future identity-changing variant must answer.
fn identity_change(step: usize, change: &Change) -> IdentityChange {
    use IdentityChange::{Column, Other, Role, Table};
    let kept = |column: &ColumnRef| Some(column.clone());
    match change {
        Change::CreateTable {
            uid, name, table, ..
        } => Table(
            Transition::new(step, uid, None, Some(name.clone())),
            ColumnLife::Begin(table.columns.keys().cloned().collect()),
        ),
        Change::DropTable { uid, name, .. } => Table(
            Transition::new(step, uid, Some(name.clone()), None),
            ColumnLife::End,
        ),
        Change::RenameTable { uid, from, to, .. } => Table(
            Transition::new(step, uid, Some(from.clone()), Some(to.clone())),
            ColumnLife::Keep,
        ),
        Change::DetachPartition {
            uid, table, shape, ..
        } => Table(
            Transition::new(step, uid, Some(table.clone()), Some(table.clone())),
            ColumnLife::Begin(shape.columns.keys().cloned().collect()),
        ),
        Change::AttachPartition { uid, table, .. } => Table(
            Transition::new(step, uid, Some(table.clone()), Some(table.clone())),
            ColumnLife::End,
        ),
        Change::SetTablePersistence { uid, table, .. }
        | Change::SetStorageParameters { uid, table, .. }
        | Change::SetReplicaIdentity { uid, table, .. }
        | Change::SetPartitionDefault { uid, table, .. }
        | Change::SetPartitionNotNull { uid, table, .. } => Table(
            Transition::new(step, uid, Some(table.clone()), Some(table.clone())),
            ColumnLife::Keep,
        ),
        Change::AddColumn {
            uid, table, name, ..
        } => Column(Transition::new(step, uid, None, Some(table.column(name)))),
        Change::DropColumn { uid, column } => {
            Column(Transition::new(step, uid, kept(column), None))
        }
        Change::RenameColumn {
            uid,
            table,
            from,
            to,
            ..
        } => Column(Transition::new(
            step,
            uid,
            Some(table.column(from)),
            Some(table.column(to)),
        )),
        Change::AlterColumnType { uid, column, .. }
        | Change::AlterColumnNullability { uid, column, .. }
        | Change::AlterColumnDefault { uid, column, .. }
        | Change::AlterColumnExpression { uid, column, .. }
        | Change::SetColumnDeprecated { uid, column, .. } => {
            Column(Transition::new(step, uid, kept(column), kept(column)))
        }
        Change::CreateRole { uid, name } => {
            Role(Transition::new(step, uid, None, Some(name.clone())))
        }
        Change::DropRole { uid, name, .. } => {
            Role(Transition::new(step, uid, Some(name.clone()), None))
        }
        Change::RenameRole { uid, from, to } => Role(Transition::new(
            step,
            uid,
            Some(from.clone()),
            Some(to.clone()),
        )),
        Change::AddComputedColumn { .. }
        | Change::DropComputedColumn { .. }
        | Change::SetPrimaryKey { .. }
        | Change::SetIndexStorageParameters { .. }
        | Change::AddUnique { .. }
        | Change::DropUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::DropForeignKey { .. }
        | Change::AddCheck { .. }
        | Change::DropCheck { .. }
        | Change::AddIndex { .. }
        | Change::DropIndex { .. }
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::CreateModule { .. }
        | Change::AlterModule { .. }
        | Change::DropModule { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. }
        | Change::PublicExecution { .. } => Other,
    }
}

/// A derived index tied to the order it describes. Reordering the borrowed
/// changes requires dropping this index and constructing another one.
///
/// ```compile_fail,E0502
/// use pbps_model::{IdsFile, PlannedChange, plan_names::PlanNames};
/// let mut changes: Vec<PlannedChange> = Vec::new();
/// let ids = IdsFile::default();
/// let names = PlanNames::of(&changes, &ids).unwrap();
/// changes.reverse();
/// assert!(names.changes().is_empty());
/// ```
#[derive(Debug)]
pub struct PlanNames<'p> {
    changes: &'p [PlannedChange],
    tables: History<TableName>,
    columns: History<(Uid, String)>,
    roles: History<String>,
}

impl<'p> PlanNames<'p> {
    pub fn of(changes: &'p [PlannedChange], after: &'p IdsFile) -> Result<Self, PlanNamesError> {
        after.validate()?;
        let mut table_events = Vec::new();
        let mut owner_events: BTreeMap<Uid, Vec<(usize, ColumnLife)>> = BTreeMap::new();
        let mut column_events = Vec::new();
        let mut role_events = Vec::new();
        for (i, p) in changes.iter().enumerate() {
            match identity_change(i, &p.change) {
                IdentityChange::Table(e, life) => {
                    match life {
                        ColumnLife::Keep => {}
                        ColumnLife::Begin(_) | ColumnLife::End => {
                            owner_events
                                .entry(e.uid.clone())
                                .or_default()
                                .push((i, life));
                        }
                    }
                    table_events.push(e);
                }
                IdentityChange::Column(e) => column_events.push(e),
                IdentityChange::Role(e) => role_events.push(e),
                IdentityChange::Other => {}
            }
        }
        let tables = History::of(&table_events, &after.tables, "table", UidKind::Table)?;
        let roles = History::of(&role_events, &after.roles, "role", UidKind::Role)?;
        let owned =
            |column: &ColumnRef, boundary: usize| match tables.holder(&column.table, boundary) {
                Holder::Tracked(uid) => Ok((uid.clone(), column.name.clone())),
                Holder::Unmanaged | Holder::Vacant => Err(PlanNamesError::Owner {
                    column: column.clone(),
                    step: boundary,
                }),
            };
        let mut columns_after = BTreeMap::new();
        for (uid, column) in &after.columns {
            columns_after.insert(uid.clone(), owned(column, changes.len())?);
        }
        let mut events = Vec::new();
        for e in column_events {
            events.push(Transition {
                step: e.step,
                uid: e.uid,
                before: e.before.as_ref().map(|c| owned(c, e.step)).transpose()?,
                after: e.after.as_ref().map(|c| owned(c, e.step)).transpose()?,
            });
        }
        // A table's lifetime bounds its columns even when CREATE/DROP carries
        // no individual column changes. Only known UIDs are represented: a
        // tombstone spelling is not permission to manufacture an identity.
        let mut seeds: BTreeMap<Uid, Option<(Uid, String)>> = BTreeMap::new();
        let mut owners = BTreeMap::new();
        for e in &events {
            seeds
                .entry(e.uid.clone())
                .or_insert_with(|| e.before.clone());
            for (owner, _) in e.before.iter().chain(&e.after) {
                if let Some(old) = owners.insert(e.uid.clone(), owner.clone())
                    && old != *owner
                {
                    return Err(PlanNamesError::ColumnOwner { uid: e.uid.clone() });
                }
            }
        }
        for (uid, address) in &columns_after {
            seeds
                .entry(uid.clone())
                .or_insert_with(|| Some(address.clone()));
            if let Some(old) = owners.insert(uid.clone(), address.0.clone())
                && old != address.0
            {
                return Err(PlanNamesError::ColumnOwner { uid: uid.clone() });
            }
        }
        let mut authored: BTreeMap<Uid, Vec<Transition<(Uid, String)>>> = BTreeMap::new();
        for e in events {
            if e.uid.kind() != UidKind::Column {
                return Err(PlanNamesError::Kind {
                    namespace: "column",
                    uid: e.uid,
                });
            }
            authored.entry(e.uid.clone()).or_default().push(e);
        }
        let mut events = Vec::new();
        let mut initial = BTreeMap::new();
        for (uid, seed) in seeds {
            let owner = &owners[&uid];
            let lifetime = owner_events.get(owner).map_or(&[][..], Vec::as_slice);
            let explicit = authored.remove(&uid).unwrap_or_default();
            let first_evidence = explicit.first().map_or(changes.len(), |event| event.step);
            let starts_later = lifetime
                .iter()
                .any(|(step, life)| *step < first_evidence && matches!(life, ColumnLife::Begin(_)));
            let start = seed.filter(|_| !starts_later && tables.name(owner, 0).is_some());
            initial.insert(uid.clone(), start.clone());
            let mut held = start;
            let mut next = 0;
            for (scope, (step, life)) in lifetime.iter().enumerate() {
                while next < explicit.len() && explicit[next].step < *step {
                    held = explicit[next].after.clone();
                    events.push(explicit[next].clone());
                    next += 1;
                }
                match life {
                    ColumnLife::Begin(columns) => {
                        // The shape proves an inline column exists; its next
                        // typed observation or final mapping supplies the UID.
                        // Evidence from a later ownership scope cannot give
                        // this scope that later UID (attach retires columns).
                        let until = lifetime
                            .get(scope + 1)
                            .map_or(changes.len(), |(step, _)| *step);
                        let address = match explicit.get(next) {
                            Some(event) if event.step < until => event.before.clone(),
                            None if until == changes.len() => columns_after.get(&uid).cloned(),
                            _ => None,
                        };
                        if let Some(address) = address
                            && columns.contains(&address.1)
                        {
                            events.push(Transition::new(*step, &uid, None, Some(address.clone())));
                            held = Some(address);
                        }
                    }
                    ColumnLife::End => {
                        if let Some(address) = held.take() {
                            events.push(Transition::new(*step, &uid, Some(address), None));
                        }
                    }
                    ColumnLife::Keep => {}
                }
            }
            events.extend(explicit.into_iter().skip(next));
        }
        events.sort_by_key(|e| e.step);
        let columns = History::from_initial(&events, &columns_after, initial, "column")?;
        Ok(Self {
            changes,
            tables,
            columns,
            roles,
        })
    }

    /// The exact ordered slice whose positions this index accepts.
    pub fn changes(&self) -> &'p [PlannedChange] {
        self.changes
    }

    fn boundary(&self, at: At) -> usize {
        match at {
            At::Start => 0,
            At::End => self.changes.len(),
            At::Before(i) => {
                assert!(
                    i < self.changes.len(),
                    "change position outside indexed plan"
                );
                i
            }
            At::After(i) => {
                assert!(
                    i < self.changes.len(),
                    "change position outside indexed plan"
                );
                i + 1
            }
        }
    }

    pub fn table_at(&self, table: &Uid, at: At) -> Option<&TableName> {
        self.tables.name(table, self.boundary(at))
    }

    pub fn holder(&self, name: &TableName, at: At) -> Holder<'_> {
        self.tables.holder(name, self.boundary(at))
    }

    pub fn column_at(&self, column: &Uid, at: At) -> Option<(&Uid, &str)> {
        let boundary = self.boundary(at);
        let (owner, name) = self.columns.name(column, boundary)?;
        self.tables.name(owner, boundary)?;
        Some((owner, name))
    }

    /// First managed address of this UID, even when its scope starts after
    /// `Start`. This is a spelling history, not evidence of a stored catalog
    /// column: scope changes and physical creation have different lifetimes.
    pub fn column_first(&self, column: &Uid) -> Option<(&Uid, &str)> {
        let (owner, name) = self
            .columns
            .names
            .get(column)?
            .iter()
            .find_map(|(_, address)| address.as_ref())?;
        Some((owner, name))
    }

    pub fn column_holder(&self, column: &ColumnRef, at: At) -> Holder<'_> {
        match self.holder(&column.table, at) {
            Holder::Tracked(owner) => self
                .columns
                .holder(&(owner.clone(), column.name.clone()), self.boundary(at)),
            Holder::Unmanaged => Holder::Unmanaged,
            Holder::Vacant => Holder::Vacant,
        }
    }

    /// Surviving columns whose own spelling changed, under their final table.
    /// Table renames alone do not change a column's own name.
    pub fn renamed_columns(&self, table: &TableName) -> impl Iterator<Item = (&Uid, &str, &str)> {
        let owner = match self.holder(table, At::End) {
            Holder::Tracked(uid) => Some(uid),
            Holder::Unmanaged | Holder::Vacant => None,
        };
        self.columns.names.keys().filter_map(move |uid| {
            let (_, from) = self.column_at(uid, At::Start)?;
            let (held, to) = self.column_at(uid, At::End)?;
            (Some(held) == owner && from != to).then_some((uid, from, to))
        })
    }

    pub fn role_at(&self, role: &Uid, at: At) -> Option<&str> {
        self.roles.name(role, self.boundary(at)).map(String::as_str)
    }

    pub fn role_holder(&self, name: &str, at: At) -> Holder<'_> {
        self.roles.holder(&name.to_owned(), self.boundary(at))
    }

    /// The current holder's starting address, never an earlier occupant of
    /// the same name. A created holder has no stored address.
    pub fn stored(&self, address: &TableName, at: At) -> Option<TableName> {
        match self.holder(address, at) {
            Holder::Tracked(uid) => self.table_at(uid, At::Start).cloned(),
            Holder::Unmanaged | Holder::Vacant => None,
        }
    }

    pub fn stored_column(&self, address: &ColumnRef, at: At) -> Option<ColumnRef> {
        let Holder::Tracked(uid) = self.column_holder(address, at) else {
            return None;
        };
        let (owner, name) = self.column_at(uid, At::Start)?;
        Some(self.table_at(owner, At::Start)?.column(name))
    }
}

#[cfg(test)]
mod tests;
