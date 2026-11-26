//! Raw `main.sqlite_schema` inventory, `PRAGMA main.table_xinfo` column reads,
//! ASCII case-folding [`CatalogFacts`] and slot verification (#2950).
//!
//! Reads `main` in the caller's transaction without loading any virtual table's
//! module (DEC-2950.14). Any failed query is an error, never an empty inventory.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use pbps_db::DbError;
use pbps_db::sqlite::Connection;
use pbps_dialect::Dialect as _;
use pbps_dialect::slots::{CatalogFacts, Namespace, Occupant, SlotKey, TableSpace};
use pbps_diff::order::catalog::{Dependent, Holds};
use pbps_diff::order::slots::SlotViolation;
use pbps_model::escape::Escaped;
use pbps_model::operation::{
    ModuleEdits, ObjectKey, Operations, PlanOwners, Snapshot, base_table_named,
};
use pbps_model::physical_sources::PhysicalSources;
use pbps_model::plan_names::PlanNames;
use pbps_model::{Change, ChangeSet, ColumnChange, ModuleId, ModuleKind, TableChange, TableName};

use crate::catalog::facts;
use crate::catalog::text::{self, Kind as TextKind};
use crate::dialect::Sqlite;
use crate::pragma::{self, table_list, table_xinfo};
use crate::{ident, lexis, names};

mod drops;
mod renames;
pub use drops::{drop_blockers, has_drop_blocker_target};
pub use renames::rename_report;

/// The kind of a row in `main.sqlite_schema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKind {
    Table,
    View,
    Index,
    Trigger,
}

impl EntryKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::Index => "index",
            Self::Trigger => "trigger",
        }
    }

    fn parse(raw: &str) -> Result<Self, DbError> {
        match raw {
            "table" => Ok(Self::Table),
            "view" => Ok(Self::View),
            "index" => Ok(Self::Index),
            "trigger" => Ok(Self::Trigger),
            other => Err(DbError::BadRow(format!(
                "`main.sqlite_schema` reported unknown object type `{}`",
                Escaped(other)
            ))),
        }
    }
}

/// One row of `main.sqlite_schema` (`type`, `name`, `tbl_name`, `sql`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEntry {
    pub kind: EntryKind,
    pub name: String,
    pub table: String,
    pub sql: Option<String>,
}

impl SchemaEntry {
    fn label(&self) -> &'static str {
        match self.kind {
            EntryKind::Table if names::is_ledger(&self.name) => "ledger table",
            EntryKind::Table if ident::has_reserved_prefix(&self.name) => "SQLite internal table",
            EntryKind::Table if names::is_intermediate(&self.name) => "intermediate rename table",
            EntryKind::Table => "table",
            EntryKind::View => "view",
            EntryKind::Index => "index",
            EntryKind::Trigger => "trigger",
        }
    }
}

/// One column reported by `PRAGMA main.table_xinfo` (`hidden`: 0 ordinary, 1 hidden, 2 virtual, 3 stored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnEntry {
    pub name: String,
    /// 1-based position in the table's primary key, or 0 if not part of it.
    pub pk: i64,
    pub hidden: i64,
}

impl ColumnEntry {
    #[must_use]
    pub const fn kind_label(&self) -> &'static str {
        match self.hidden {
            1 => "hidden column",
            2 => "virtual generated column",
            3 => "stored generated column",
            _ => "column",
        }
    }
}

/// One index reported by `PRAGMA main.index_list` and `index_xinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    pub unique: bool,
    pub origin: String,
    pub partial: bool,
    pub keys: Vec<(Option<String>, String)>,
}

/// One foreign key reported by `PRAGMA main.foreign_key_list` (grouped by `id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyEntry {
    pub name: Option<String>,
    pub parent: String,
    pub from: Vec<String>,
    pub to: Vec<Option<String>>,
}

/// What `PRAGMA main.table_list` / `table_xinfo` answered for a requested name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableColumns {
    Absent,
    Columns(Vec<ColumnEntry>),
    /// Virtual table: `table_xinfo` is never called so its module is not loaded.
    Virtual,
    /// View: `table_xinfo` is never called on views.
    View,
}

/// Raw catalog inventory read inside the caller's transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub entries: Vec<SchemaEntry>,
    pub columns: BTreeMap<String, TableColumns>,
    pub indexes: BTreeMap<String, Vec<IndexEntry>>,
    pub foreign_keys: Option<BTreeMap<String, Vec<ForeignKeyEntry>>>,
}

/// Reads `main.sqlite_schema` (`type`, `name`, `tbl_name`, `sql`) and, for each
/// table in `column_tables`, its columns via `PRAGMA main.table_xinfo`, without
/// loading any virtual table's module.
pub fn read(c: &Connection, column_tables: &[&str]) -> Result<Inventory, DbError> {
    let entries = c
        .query(
            "SELECT type, name, tbl_name, sql FROM main.sqlite_schema ORDER BY type, name",
            &[],
        )?
        .into_iter()
        .map(|row| {
            Ok(SchemaEntry {
                kind: EntryKind::parse(row.text(0)?)?,
                name: row.text(1)?.to_owned(),
                table: row.text(2)?.to_owned(),
                sql: row.optional_text(3)?.map(str::to_owned),
            })
        })
        .collect::<Result<Vec<_>, DbError>>()?;

    let mut columns = BTreeMap::new();
    if !column_tables.is_empty() {
        let version = c.version()?;
        if version < crate::catalog::NEEDED {
            return Err(DbError::Refused(format!(
                "reading SQLite table metadata needs {} or later (`PRAGMA table_list`); this library is {version}",
                crate::catalog::NEEDED
            )));
        }
        let mut unique: Vec<&str> = column_tables.to_vec();
        unique.sort_unstable();
        unique.dedup();
        for &requested in &unique {
            columns.insert(requested.to_owned(), read_table_columns(c, requested)?);
        }
    }

    Ok(Inventory {
        entries,
        columns,
        indexes: BTreeMap::new(),
        foreign_keys: None,
    })
}

/// Reads everything [`drop_blockers`] needs inside the caller's transaction
/// without loading any virtual table's module (DEC-2950.14).
pub fn read_for_drops(c: &Connection, sources: &PhysicalSources<'_>) -> Result<Inventory, DbError> {
    let tables = drops::inspected_tables(sources);
    let refs: Vec<&str> = tables.iter().map(String::as_str).collect();
    enrich_relations(c, read(c, &refs)?)
}

/// Reads everything [`rename_report`] needs for `table` without loading any
/// virtual table's module (DEC-2950.14).
pub fn read_for_rename(c: &Connection, table: &str) -> Result<Inventory, DbError> {
    enrich_relations(c, read(c, &[table])?)
}

fn enrich_relations(c: &Connection, mut inv: Inventory) -> Result<Inventory, DbError> {
    for (req, cols) in &inv.columns {
        if matches!(cols, TableColumns::Columns(_)) {
            let actual = inv
                .entries
                .iter()
                .find(|e| e.kind == EntryKind::Table && ident::same(&e.name, req))
                .map_or(req.as_str(), |e| e.name.as_str());
            inv.indexes.insert(req.clone(), read_indexes(c, actual)?);
        }
    }
    inv.foreign_keys = Some(read_all_foreign_keys(c, &inv.entries)?);
    Ok(inv)
}

fn read_table_columns(c: &Connection, requested: &str) -> Result<TableColumns, DbError> {
    // Check `table_list` first so `table_xinfo` never runs on a virtual table
    // (which would load its module) or on a view.
    let listed = pragma::table(c, requested)?;
    let Some(row) = listed.first() else {
        return Ok(TableColumns::Absent);
    };
    let actual = row.text(table_list::NAME)?;
    match row.text(table_list::KIND)? {
        "virtual" => Ok(TableColumns::Virtual),
        "view" => Ok(TableColumns::View),
        "table" | "shadow" => {
            let raw = pragma::columns(c, actual)?;
            if raw.is_empty() {
                return Err(DbError::BadRow(format!(
                    "table `{}` is listed by the catalog and has no columns to read",
                    Escaped(actual)
                )));
            }
            let cols = pragma::sorted(raw, |r| r.integer(table_xinfo::CID))?
                .into_iter()
                .map(|col| {
                    Ok(ColumnEntry {
                        name: col.text(table_xinfo::NAME)?.to_owned(),
                        pk: col.integer(table_xinfo::PK)?,
                        hidden: col.integer(table_xinfo::HIDDEN)?,
                    })
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            Ok(TableColumns::Columns(cols))
        }
        other => Err(DbError::BadRow(format!(
            "`{}` is listed with unknown kind `{}`",
            Escaped(actual),
            Escaped(other)
        ))),
    }
}

fn read_indexes(c: &Connection, table: &str) -> Result<Vec<IndexEntry>, DbError> {
    Ok(facts::indexes(c, table)?
        .into_iter()
        .map(|ix| IndexEntry {
            name: ix.name,
            unique: ix.unique,
            origin: ix.origin,
            partial: ix.partial,
            keys: ix.keys.into_iter().map(|(n, _, coll)| (n, coll)).collect(),
        })
        .collect())
}

fn read_all_foreign_keys(
    c: &Connection,
    entries: &[SchemaEntry],
) -> Result<BTreeMap<String, Vec<ForeignKeyEntry>>, DbError> {
    let mut out = BTreeMap::new();
    for entry in entries.iter().filter(|e| e.kind == EntryKind::Table) {
        let mut grouped: Vec<(i64, ForeignKeyEntry)> = Vec::new();
        for r in facts::foreign(c, &entry.name)? {
            if let Some((prev, fk)) = grouped.last_mut()
                && *prev == r.id
            {
                fk.from.push(r.from);
                fk.to.push(r.to);
            } else {
                grouped.push((
                    r.id,
                    ForeignKeyEntry {
                        name: None,
                        parent: r.table,
                        from: vec![r.from],
                        to: vec![r.to],
                    },
                ));
            }
        }
        if grouped.is_empty() {
            continue;
        }
        // A name comes from the table's text, clause by clause in the order
        // the pragma lists the keys. It is taken only when every clause is the
        // key it stands beside, so a name is never bound to another key: an
        // unnamed key is one no `DropForeignKey` can clear.
        let sql = require_stored_sql(entry)?;
        if let Ok(t) = text::table(&entry.name, sql) {
            let clauses: Vec<_> = t
                .constraints
                .into_iter()
                .filter_map(|c| match c.kind {
                    TextKind::ForeignKey {
                        columns, parent, ..
                    } => Some((c.name, columns, parent)),
                    TextKind::PrimaryKey { .. }
                    | TextKind::Unique { .. }
                    | TextKind::Check { .. }
                    | TextKind::NotNull
                    | TextKind::Null
                    | TextKind::Default { .. } => None,
                })
                .collect();
            let same_key = |fk: &ForeignKeyEntry, columns: &[String], parent: &str| {
                ident::same(&fk.parent, parent)
                    && fk.from.len() == columns.len()
                    && fk.from.iter().zip(columns).all(|(a, b)| ident::same(a, b))
            };
            if clauses.len() == grouped.len()
                && grouped
                    .iter()
                    .zip(&clauses)
                    .all(|((_, fk), (_, columns, parent))| same_key(fk, columns, parent))
            {
                for ((_, fk), (name, _, _)) in grouped.iter_mut().zip(clauses) {
                    fk.name = name;
                }
            }
        }
        out.insert(
            entry.name.clone(),
            grouped.into_iter().map(|(_, fk)| fk).collect(),
        );
    }
    Ok(out)
}

/// Pre-rename catalog names of existing tables whose columns `changes` edits.
// The complement is every change that neither adds, renames nor drops a column.
#[allow(clippy::wildcard_enum_match_arm)]
#[must_use]
pub fn column_tables(
    changes: &ChangeSet,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
) -> Vec<String> {
    let (mut created, mut renamed) = (BTreeSet::new(), Vec::new());
    for p in &changes.changes {
        if let Change::Table(TableChange::CreateTable { name, .. }) = &p.change {
            created.insert(ident::fold(&name.name).into_owned());
        } else if let Change::Table(TableChange::RenameTable { from, to, .. }) = &p.change {
            renamed.push((from, to));
        }
    }
    let resolve = |t: &TableName| {
        if let Some((from, _)) = renamed
            .iter()
            .find(|(_, to)| ident::same(&to.name, &t.name))
        {
            return Some(from.name.clone());
        }
        if let Some(prev) = desired
            .ids
            .table_uid(t)
            .and_then(|u| base.ids.tables.get(u))
        {
            return Some(prev.name.clone());
        }
        (!created.contains(ident::fold(&t.name).as_ref())).then(|| t.name.clone())
    };
    let mut tables: Vec<String> = changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::Column(
                ColumnChange::AddColumn { table, .. } | ColumnChange::RenameColumn { table, .. },
            ) => resolve(table),
            Change::Column(ColumnChange::DropColumn { column, .. }) => resolve(&column.table),
            _ => None,
        })
        .collect();
    tables.sort();
    tables.dedup();
    tables
}

type Analysed<'a> = (
    Operations,
    PlanOwners<'a>,
    Snapshot<'a>,
    pbps_diff::order::slots::Slots,
);

fn analyse_plan<'a>(
    changes: &'a ChangeSet,
    base: pbps_diff::Side<'a>,
    desired: pbps_diff::Side<'a>,
    dialect: &Sqlite,
) -> Result<Analysed<'a>, String> {
    let names = PlanNames::of(&changes.changes, desired.ids).map_err(|e| e.to_string())?;
    let owners = PlanOwners::new(names, base.ids, desired.ids);
    let modes = if dialect.rebuilds_modules() {
        ModuleEdits::Rebuilt
    } else {
        ModuleEdits::InPlace
    };
    let ops = Operations::of_plan(&changes.changes, modes);
    let before = Snapshot {
        schema: base.schema,
        ids: base.ids,
    };
    let slots = pbps_diff::order::slots::analyse(ops.as_slice(), &owners, before, dialect);
    Ok((ops, owners, before, slots))
}

/// Builds SQLite's [`CatalogFacts`] in memory from `inventory` and `ident::same`
/// with no database round trip, covering every slot `observe_scheduled` compares.
pub fn catalog_facts(
    inventory: &Inventory,
    changes: &ChangeSet,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    dialect: &Sqlite,
) -> Result<CatalogFacts, String> {
    let (_, owners, before, analysed) = analyse_plan(changes, base, desired, dialect)?;
    let baseline =
        pbps_dialect::slots::baseline_names(dialect, &owners, before).map_err(|e| e.to_string())?;
    let base_table = |n: &str| base.schema.tables.keys().find(|t| ident::same(&t.name, n));
    let is_managed = |e: &SchemaEntry| match e.kind {
        EntryKind::Table => base_table(&e.name).is_some(),
        EntryKind::View => base.schema.modules.iter().any(|(id, m)| {
            m.kind == ModuleKind::View && ident::same(&id.object_name().name, &e.name)
        }),
        EntryKind::Index => base_table(&e.table).is_some_and(|t| {
            base.schema.tables[t]
                .indexes
                .keys()
                .any(|ix| ident::same(ix.raw(), &e.name))
        }),
        EntryKind::Trigger => false,
    };
    let mut occupants: Vec<Occupant> = inventory
        .entries
        .iter()
        .filter(|e| !matches!(e.kind, EntryKind::Trigger) && !is_managed(e))
        .map(|e| Occupant {
            slot: SlotKey {
                namespace: Namespace::Relations(crate::validate::SCHEMA.to_owned()),
                name: e.name.clone(),
            },
            kind: e.kind.as_str().to_owned(),
            owner: matches!(e.kind, EntryKind::Index)
                .then(|| base_table(&e.table).cloned())
                .flatten(),
            column: None,
        })
        .collect();
    for (table_name, cols) in &inventory.columns {
        let TableColumns::Columns(cols) = cols else {
            continue;
        };
        let lookup = base_table(table_name)
            .cloned()
            .unwrap_or_else(|| TableName::new(crate::validate::SCHEMA, table_name));
        let Some(found) = base_table_named(&owners, before, &lookup).map_err(|e| e.to_string())?
        else {
            continue;
        };
        let (key, managed) = (
            found.table_key(),
            base.schema.tables.get(&lookup).map(|t| &t.columns),
        );
        let owner = base.schema.tables.contains_key(&lookup).then_some(lookup);
        for col in cols.iter().filter(|c| {
            c.hidden != 0
                || !managed.is_some_and(|m| m.keys().any(|k| ident::same(k.raw(), &c.name)))
        }) {
            occupants.push(Occupant {
                slot: SlotKey {
                    namespace: Namespace::Table(key.clone(), TableSpace::Columns),
                    name: col.name.clone(),
                },
                kind: col.kind_label().to_owned(),
                owner: owner.clone(),
                column: None,
            });
        }
    }
    let mut compared: BTreeSet<SlotKey> = occupants.iter().map(|o| o.slot.clone()).collect();
    for fx in &analysed.effects {
        compared.extend(
            fx.reads
                .iter()
                .chain(&fx.claims)
                .chain(&fx.releases)
                .map(|(s, _)| s.clone()),
        );
        compared.extend(fx.intermediate.iter().cloned());
    }
    for named in &baseline {
        compared.extend(pbps_dialect::slots::slots(dialect, &named.address));
    }
    let mut groups: BTreeMap<(Namespace, String), Vec<SlotKey>> = BTreeMap::new();
    for slot in &compared {
        let ns = if let Namespace::Relations(s) = &slot.namespace {
            Namespace::Relations(ident::fold(s).into_owned())
        } else {
            slot.namespace.clone()
        };
        groups
            .entry((ns, ident::fold(&slot.name).into_owned()))
            .or_default()
            .push(slot.clone());
    }
    let mut alike = BTreeSet::new();
    for g in groups.values() {
        for i in 0..g.len() {
            for j in (i + 1)..g.len() {
                alike.insert((g[i].clone(), g[j].clone()));
            }
        }
    }
    Ok(CatalogFacts {
        occupants,
        compared,
        alike,
        ..CatalogFacts::default()
    })
}

/// Verifies `changes` in its current order against `inventory` and `facts`
/// through [`pbps_diff::order::slots::verify`], after refusing a column change
/// on anything that is not an ordinary table.
pub fn verify_names(
    inventory: &Inventory,
    changes: &ChangeSet,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    dialect: &Sqlite,
    facts: &CatalogFacts,
    label: &str,
) -> Result<(), String> {
    // The inventory holds exactly the tables whose columns this plan adds,
    // renames or drops ([`column_tables`]). `ALTER TABLE` takes none of those
    // on anything but an ordinary table, and the facts hold no column of one,
    // so each is refused here and not left for SQLite to refuse mid-apply.
    let mut problems: Vec<String> = inventory
        .columns
        .iter()
        .filter_map(|(table, columns)| {
            let found = match columns {
                TableColumns::Columns(_) => return None,
                TableColumns::Virtual => "is a virtual table",
                TableColumns::View => "is a view",
                TableColumns::Absent => "does not exist",
            };
            Some(format!(
                "`main.{}` {found} in SQLite, so this plan cannot add, rename or drop its columns",
                Escaped(table)
            ))
        })
        .collect();
    let (ops, owners, before, analysed) = analyse_plan(changes, base, desired, dialect)?;
    let (violations, _) = pbps_diff::order::slots::verify(
        ops.as_slice(),
        &analysed,
        dialect,
        Some(facts),
        &owners,
        before,
    )
    .map_err(|e| e.to_string())?;
    let existing = |slot: &SlotKey, fallback: &str| match &slot.namespace {
        Namespace::Relations(_) => inventory
            .entries
            .iter()
            .find(|e| !matches!(e.kind, EntryKind::Trigger) && ident::same(&e.name, &slot.name))
            .map_or_else(
                || fallback.to_owned(),
                |e| format!("{} `main.{}`", e.label(), Escaped(&e.name)),
            ),
        Namespace::Table(ObjectKey::Table(rec), TableSpace::Columns) => base
            .ids
            .tables
            .get(rec.uid())
            .and_then(|tn| {
                inventory.columns.iter().find_map(|(k, v)| match v {
                    TableColumns::Columns(cols) if ident::same(k, &tn.name) => {
                        cols.iter().find(|c| ident::same(&c.name, &slot.name))
                    }
                    TableColumns::Absent
                    | TableColumns::Columns(_)
                    | TableColumns::Virtual
                    | TableColumns::View => None,
                })
            })
            .map_or_else(
                || fallback.to_owned(),
                |c| format!("{} `{}`", c.kind_label(), Escaped(&c.name)),
            ),
        Namespace::Types(_)
        | Namespace::Routines(_)
        | Namespace::Table(..)
        | Namespace::Roles
        | Namespace::Extensions
        | Namespace::Search(..) => fallback.to_owned(),
    };
    for v in &violations {
        problems.push(match v {
            SlotViolation::Contested { slot, holder, .. }
            | SlotViolation::Held { slot, holder, .. } => {
                format!(
                    "`{}`: SQLite already has {}",
                    Escaped(&slot.name),
                    existing(slot, holder)
                )
            }
            SlotViolation::IntermediateHeld { slot, holder, .. } => format!(
                "intermediate rename name `{}` is already held in SQLite by {}",
                Escaped(&slot.name),
                existing(slot, holder)
            ),
            SlotViolation::Edge { slot, .. } => format!(
                "name handover for `{}` runs out of order on SQLite",
                Escaped(&slot.name)
            ),
            SlotViolation::MissingInput { slot, .. } => format!(
                "input name `{}` is not held on SQLite when this change runs",
                Escaped(&slot.name)
            ),
        });
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "SQLite target `{}` already holds {} name(s) this plan claims (tables, views and \
         indexes share one ASCII case-insensitive namespace, and columns are unique per table):\n  \
         {}\nRename the declaration, drop or rename the existing object in the database, or \
         adopt a readable object with `pbps baseline --reason ...`.",
        Escaped(label),
        problems.len(),
        problems.join("\n  "),
    ))
}

/// Returns, for each view in `views`, every view and trigger in `inventory`
/// that may read it (directly or through other views) or is attached to it,
/// ordered deepest-first so each dependent is dropped before what it reads.
///
/// Absent, empty and unreadable stay distinct: a view in `views` missing from
/// `main.sqlite_schema`, or any view or trigger row with NULL or empty `sql`,
/// returns an error rather than "no readers".
pub fn view_dependents(
    inventory: &Inventory,
    views: &[ModuleId],
    known_modules: &BTreeSet<ModuleId>,
) -> Result<BTreeMap<ModuleId, Vec<Dependent>>, DbError> {
    if views.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut catalog_views: Vec<(&SchemaEntry, Cow<'_, str>)> = Vec::new();
    let mut catalog_triggers: Vec<(&SchemaEntry, &str)> = Vec::new();
    for entry in &inventory.entries {
        match entry.kind {
            EntryKind::View => {
                let sql = require_stored_sql(entry)?;
                let body = crate::catalog::text::view(&entry.name, sql)
                    .map(|v| Cow::Owned(v.body))
                    .unwrap_or(Cow::Borrowed(sql));
                catalog_views.push((entry, body));
            }
            EntryKind::Trigger => {
                let sql = require_stored_sql(entry)?;
                catalog_triggers.push((entry, sql));
            }
            EntryKind::Table | EntryKind::Index => {}
        }
    }

    let resolve_view = |name: &str| -> ModuleId {
        known_modules
            .iter()
            .find(|id| {
                matches!(
                    id,
                    ModuleId::Named(n)
                        if ident::same(&n.schema, crate::validate::SCHEMA)
                            && ident::same(&n.name, name)
                )
            })
            .cloned()
            .unwrap_or_else(|| ModuleId::Named(TableName::new(crate::validate::SCHEMA, name)))
    };

    let mut out = BTreeMap::new();
    for root in views {
        let ModuleId::Named(root_name) = root else {
            return Err(DbError::Refused(format!(
                "SQLite only manages views, not `{}`",
                root.shown()
            )));
        };
        if !ident::same(&root_name.schema, crate::validate::SCHEMA) {
            return Err(DbError::Refused(format!(
                "SQLite manages only schema `main`, not `{}`",
                root.shown()
            )));
        }
        let Some((root_entry, _)) = catalog_views
            .iter()
            .find(|(e, _)| ident::same(&e.name, &root_name.name))
        else {
            return Err(DbError::Refused(format!(
                "view `{}` is not in SQLite's catalog",
                root.shown()
            )));
        };

        let root_folded = ident::fold(&root_entry.name).into_owned();
        let mut found: BTreeMap<String, Dependent> = BTreeMap::new();
        let mut depends_on_me: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut visited_views: BTreeSet<String> = [root_folded.clone()].into_iter().collect();
        let mut frontier: Vec<(String, Option<String>)> = vec![(root_entry.name.clone(), None)];

        while !frontier.is_empty() {
            let mut next = Vec::new();
            for (target, parent_key) in std::mem::take(&mut frontier) {
                for (view, body) in &catalog_views {
                    if ident::same(&view.name, &target) || !sql_may_read(body, &target) {
                        continue;
                    }
                    let folded = ident::fold(&view.name).into_owned();
                    if folded == root_folded {
                        continue;
                    }
                    let key = format!("view:{folded}");
                    if let Some(parent) = &parent_key {
                        depends_on_me
                            .entry(parent.clone())
                            .or_default()
                            .insert(key.clone());
                    }
                    if !found.contains_key(&key) {
                        found.insert(
                            key.clone(),
                            Dependent {
                                // Shared dependent formatting escapes `described` once.
                                described: format!("view main.{}", view.name),
                                holds: Holds::Module(resolve_view(&view.name)),
                            },
                        );
                    }
                    if visited_views.insert(folded) {
                        next.push((view.name.clone(), Some(key)));
                    }
                }
                for (trigger, body) in &catalog_triggers {
                    let attached = ident::same(&trigger.table, &target);
                    if !attached && !sql_may_read(body, &target) {
                        continue;
                    }
                    let key = format!("trigger:{}", ident::fold(&trigger.name));
                    if let Some(parent) = &parent_key {
                        depends_on_me
                            .entry(parent.clone())
                            .or_default()
                            .insert(key.clone());
                    }
                    found.entry(key).or_insert_with(|| Dependent {
                        described: format!(
                            "trigger main.{} on main.{}",
                            trigger.name, trigger.table
                        ),
                        holds: Holds::Unrepresentable(if attached {
                            "an INSTEAD OF trigger on this view would be dropped with it by \
                             SQLite, and triggers are not managed on SQLite"
                                .to_owned()
                        } else {
                            "triggers are not managed on SQLite, so this plan cannot remove or \
                             restore it"
                                .to_owned()
                        }),
                    });
                }
            }
            frontier = next;
        }
        out.insert(root.clone(), drop_order(found, &depends_on_me));
    }
    Ok(out)
}

fn require_stored_sql(entry: &SchemaEntry) -> Result<&str, DbError> {
    match entry.sql.as_deref() {
        Some(sql) if lexis::has_token(sql) => Ok(sql),
        Some(_) | None => Err(DbError::BadRow(format!(
            "{} `main.{}` is listed by SQLite's catalog and has no stored SQL to read",
            entry.kind.as_str(),
            Escaped(&entry.name)
        ))),
    }
}

/// Conservative lexical match against stored SQLite DDL (`lexis::names_or_quotes`
/// with `ident::same`, as `crate::ordering` uses). Because this scans all
/// identifier and quoted tokens without parsing `FROM`/`JOIN` clauses, a view
/// or trigger that merely names a column, alias or string literal like the
/// target view is reported as a reader. A trigger's statement is scanned
/// whole, so a trigger that is itself named like the view is reported too.
/// Reading the `FROM` and `JOIN` targets is the way to narrow this.
fn sql_may_read(sql: &str, view_name: &str) -> bool {
    lexis::names_or_quotes(sql, view_name, ident::same)
}

fn drop_order(
    mut found: BTreeMap<String, Dependent>,
    depends_on_me: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<Dependent> {
    let mut remaining: BTreeSet<String> = found.keys().cloned().collect();
    let mut out = Vec::new();
    while !remaining.is_empty() {
        let ready: Vec<String> = remaining
            .iter()
            .filter(|node| {
                depends_on_me
                    .get(*node)
                    .is_none_or(|theirs| theirs.iter().all(|d| !remaining.contains(d)))
            })
            .cloned()
            .collect();
        if ready.is_empty() {
            // Two views mutually naming each other (e.g. as column aliases):
            // SQLite accepts dropping views in any order, so emit deterministically.
            for node in std::mem::take(&mut remaining) {
                if let Some(dependent) = found.remove(&node) {
                    out.push(dependent);
                }
            }
            break;
        }
        for node in ready {
            remaining.remove(&node);
            if let Some(dependent) = found.remove(&node) {
                out.push(dependent);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
