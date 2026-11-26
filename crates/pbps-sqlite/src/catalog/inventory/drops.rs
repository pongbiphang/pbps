//! Drop-blocker evaluation over SQLite's [`Inventory`] (#2950).
//!
//! Shared by `PlanSession::check_final_order` and `ApplySession::drop_blockers`.
//! Reuses [`super::require_stored_sql`] and [`super::sql_may_read`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;

use pbps_db::DbError;
use pbps_db::impact::DropReport;
use pbps_model::escape::Escaped;
use pbps_model::physical_sources::{PhysicalSources, Source};
use pbps_model::plan_names::At;
use pbps_model::{
    Change, ChangeSet, ColumnChange as CC, ColumnRef, ConstraintChange as KC, IndexChange,
    ModuleChange as MC, ModuleKind, ObjectLocalName, TableChange, TableName,
};

use super::{
    ColumnEntry, EntryKind, ForeignKeyEntry, IndexEntry, Inventory, SchemaEntry, TableColumns,
    require_stored_sql, sql_may_read,
};
use crate::catalog::text;
use crate::{ident, lexis};

enum DropOp<'a> {
    Table(&'a TableName),
    Column(&'a ColumnRef),
    Index(&'a TableName, &'a ObjectLocalName),
    Refuse(String),
}

fn no_rebuild(what: impl Display) -> DropOp<'static> {
    DropOp::Refuse(format!(
        "SQLite does not support {what} yet (needs table rebuild)"
    ))
}

fn drop_op(change: &Change) -> Option<DropOp<'_>> {
    if let Change::Table(TableChange::DropTable {
        name, detach_from, ..
    }) = change
    {
        return Some(if detach_from.is_some() {
            DropOp::Refuse("SQLite does not support partitions".into())
        } else {
            DropOp::Table(name)
        });
    }
    if let Change::Column(CC::DropColumn { column, .. }) = change {
        return Some(DropOp::Column(column));
    }
    if let Change::Column(CC::AlterColumnType { column, .. }) = change {
        return Some(no_rebuild(format!(
            "changing the type of `{}`",
            column.shown()
        )));
    }
    if let Change::Column(CC::AlterColumnNumbering { column, .. }) = change {
        return Some(DropOp::Refuse(format!(
            "SQLite does not support identity or sequence numbering on `{}`",
            column.shown()
        )));
    }
    if let Change::Constraint(KC::SetPrimaryKey {
        table,
        from: Some(_),
        ..
    }) = change
    {
        return Some(no_rebuild(format!(
            "dropping or replacing the primary key on `{}`",
            table.shown()
        )));
    }
    if let Change::Constraint(KC::DropUnique { table, name }) = change {
        return Some(no_rebuild(format!(
            "dropping unique constraint `{}` on `{}`",
            name.shown(),
            table.shown()
        )));
    }
    if let Change::Index(IndexChange::DropIndex { table, name }) = change {
        return Some(DropOp::Index(table, name));
    }
    None
}

/// Whether `changes` contains any drop, key removal or retype that [`drop_blockers`] handles.
#[must_use]
pub fn has_drop_blocker_target(changes: &ChangeSet) -> bool {
    changes.changes.iter().any(|p| drop_op(&p.change).is_some())
}

/// Stored catalog table names whose columns and indexes [`super::read_for_drops`] reads.
pub(super) fn inspected_tables(sources: &PhysicalSources<'_>) -> Vec<String> {
    let mut tables: Vec<String> = sources
        .names()
        .changes()
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match drop_op(&p.change) {
            Some(DropOp::Column(c)) => match sources.source(c, At::Before(i)) {
                Source::Stored(s) => Some(s.table),
                Source::Created { .. } | Source::Retired | Source::Unavailable => None,
            },
            Some(DropOp::Index(t, _)) => match sources.table_source(t, At::Before(i)) {
                Source::Stored(s) => Some(s),
                Source::Created { .. } | Source::Retired | Source::Unavailable => None,
            },
            Some(DropOp::Table(_) | DropOp::Refuse(_)) | None => None,
        })
        .filter(|t| ident::same(&t.schema, crate::validate::SCHEMA))
        .map(|t| t.name)
        .collect();
    tables.sort();
    tables.dedup();
    tables
}

fn stored<A>(source: Source<A>, kind: &str, shown: impl Display) -> Result<Option<A>, DbError> {
    match source {
        Source::Stored(val) => Ok(Some(val)),
        Source::Created { .. } => Ok(None),
        Source::Retired | Source::Unavailable => Err(DbError::Refused(format!(
            "SQLite drop_blockers: physical source for {kind} `{shown}` is unavailable"
        ))),
    }
}

fn describe_fk(child: &str, fk: &ForeignKeyEntry) -> String {
    let c = Escaped(child);
    match &fk.name {
        Some(n) => format!("foreign key `{}` on table `main.{c}`", Escaped(n)),
        None => {
            let cols: Vec<_> = fk.from.iter().map(|col| Escaped(col).to_string()).collect();
            format!("foreign key on table `main.{c}` ({})", cols.join(", "))
        }
    }
}

fn plain_unique_key(ix: &IndexEntry) -> Option<BTreeMap<String, String>> {
    if !ix.unique || ix.partial || ix.keys.is_empty() {
        return None;
    }
    let mut map = BTreeMap::new();
    for (col, coll) in &ix.keys {
        if map
            // SQLite compares a collation's name without regard to ASCII case.
            .insert(
                ident::fold(col.as_deref()?).into_owned(),
                coll.to_ascii_lowercase(),
            )
            .is_some()
        {
            return None;
        }
    }
    Some(map)
}

struct State<'a> {
    inv: &'a Inventory,
    sources: &'a PhysicalSources<'a>,
    fks: BTreeMap<String, Vec<ForeignKeyEntry>>,
    views: Vec<(String, Option<String>)>,
    triggers: Vec<(&'a SchemaEntry, &'a str)>,
    dropped_indexes: BTreeSet<(String, String)>,
}

/// Evaluates catalog drop blockers for every table, column and index drop in
/// `sources` against `inventory`, clearing blockers removed earlier in the
/// plan, refusing rebuild-requiring/unsupported changes, and treating absent
/// targets and unreadable/missing metadata as errors.
pub fn drop_blockers(
    inventory: &Inventory,
    sources: &PhysicalSources<'_>,
) -> Result<Vec<DropReport>, DbError> {
    let changes = sources.names().changes();
    let mut has_inspect = false;
    for p in changes {
        match drop_op(&p.change) {
            Some(DropOp::Refuse(why)) => return Err(DbError::Refused(why)),
            Some(DropOp::Table(_) | DropOp::Column(_) | DropOp::Index(_, _)) => has_inspect = true,
            None => {}
        }
    }
    if !has_inspect {
        return Ok(Vec::new());
    }
    let Some(all_fks) = &inventory.foreign_keys else {
        return Err(DbError::BadRow(
            "SQLite drop_blockers requires foreign-key metadata to be read from the catalog".into(),
        ));
    };
    let (mut views, mut triggers) = (Vec::new(), Vec::new());
    for entry in &inventory.entries {
        match entry.kind {
            EntryKind::View => {
                let sql = require_stored_sql(entry)?;
                let body = text::view(&entry.name, sql)
                    .map(|v| v.body)
                    .unwrap_or_else(|_| sql.to_owned());
                views.push((entry.name.clone(), Some(body)));
            }
            EntryKind::Trigger => triggers.push((entry, require_stored_sql(entry)?)),
            EntryKind::Table | EntryKind::Index => {}
        }
    }
    let mut state = State {
        inv: inventory,
        sources,
        fks: all_fks.clone(),
        views,
        triggers,
        dropped_indexes: BTreeSet::new(),
    };
    let mut reports = Vec::new();
    for (index, p) in changes.iter().enumerate() {
        if let Some(op) = drop_op(&p.change) {
            let (target, blocking) = state.check(index, op)?;
            reports.push(DropReport {
                change_index: index,
                target,
                blocking: blocking.into_iter().collect(),
            });
        }
        state.apply_step(index, &p.change);
    }
    Ok(reports)
}

impl<'a> State<'a> {
    fn find_table(&self, table: &TableName) -> Result<&'a str, DbError> {
        let shown = table.shown();
        if !ident::same(&table.schema, crate::validate::SCHEMA) {
            return Err(DbError::Refused(format!(
                "SQLite manages only schema `main`, not `{shown}`"
            )));
        }
        self.inv
            .entries
            .iter()
            .find(|e| e.kind == EntryKind::Table && ident::same(&e.name, &table.name))
            .map(|e| e.name.as_str())
            .ok_or_else(|| DbError::Refused(format!("table `{shown}` is not in SQLite's catalog")))
    }

    fn find_columns(&self, table: &str) -> Result<&'a [ColumnEntry], DbError> {
        let t = Escaped(table);
        match self
            .inv
            .columns
            .iter()
            .find(|(k, _)| ident::same(k, table))
            .map(|(_, v)| v)
        {
            Some(TableColumns::Columns(cols)) => Ok(cols.as_slice()),
            Some(TableColumns::Absent) => Err(DbError::Refused(format!(
                "table `main.{t}` is not in SQLite's catalog"
            ))),
            Some(TableColumns::Virtual) => Err(DbError::Refused(format!(
                "`main.{t}` is a virtual table in SQLite"
            ))),
            Some(TableColumns::View) => Err(DbError::Refused(format!(
                "`main.{t}` is a view in SQLite, not an ordinary table"
            ))),
            None => Err(DbError::BadRow(format!(
                "SQLite drop_blockers: column metadata for table `main.{t}` was not read"
            ))),
        }
    }

    fn check(&self, index: usize, op: DropOp<'_>) -> Result<(String, BTreeSet<String>), DbError> {
        match op {
            DropOp::Refuse(why) => Err(DbError::Refused(why)),
            DropOp::Table(name) => {
                let label = format!("table {}", name.shown());
                let blocking = match stored(
                    self.sources.table_source(name, At::Before(index)),
                    "table",
                    name.shown(),
                )? {
                    Some(st) => self.check_table(self.find_table(&st)?),
                    None => BTreeSet::new(),
                };
                Ok((label, blocking))
            }
            DropOp::Index(table, idx) => {
                let label = format!("index `{}` on {}", idx.shown(), table.shown());
                let blocking = match stored(
                    self.sources.table_source(table, At::Before(index)),
                    "table",
                    table.shown(),
                )? {
                    Some(st) => self.check_index(&st, self.find_table(&st)?, idx)?,
                    None => BTreeSet::new(),
                };
                Ok((label, blocking))
            }
            DropOp::Column(col_ref) => {
                let label = format!("column {}", col_ref.shown());
                let Some(st) = stored(
                    self.sources.source(col_ref, At::Before(index)),
                    "column",
                    col_ref.shown(),
                )?
                else {
                    return Ok((label, BTreeSet::new()));
                };
                let tname = self.find_table(&st.table)?;
                let col = self
                    .find_columns(&st.table.name)?
                    .iter()
                    .find(|c| ident::same(&c.name, st.name.raw()))
                    .ok_or_else(|| {
                        DbError::Refused(format!(
                            "column `{}` is not in SQLite's catalog",
                            st.shown()
                        ))
                    })?;
                Ok((label, self.check_column(tname, col)))
            }
        }
    }

    fn apply_step(&mut self, index: usize, change: &Change) {
        let stored_main = |t: &TableName| match self.sources.table_source(t, At::Before(index)) {
            Source::Stored(s) if ident::same(&s.schema, crate::validate::SCHEMA) => Some(s.name),
            Source::Stored(_) | Source::Created { .. } | Source::Retired | Source::Unavailable => {
                None
            }
        };
        if let Change::Table(TableChange::DropTable { name, .. }) = change
            && let Some(t) = stored_main(name)
        {
            self.fks.retain(|child, _| !ident::same(child, &t));
        } else if let Change::Constraint(KC::DropForeignKey { table, name }) = change
            && let Some(t) = stored_main(table)
            && let Some((_, list)) = self.fks.iter_mut().find(|(c, _)| ident::same(c, &t))
            && let Some(pos) = list.iter().position(|fk| {
                fk.name
                    .as_deref()
                    .is_some_and(|n| ident::same(n, name.raw()))
            })
        {
            list.remove(pos);
        } else if let Change::Index(IndexChange::DropIndex { table, name }) = change
            && let Some(t) = stored_main(table)
        {
            self.dropped_indexes.insert((
                ident::fold(&t).into_owned(),
                ident::fold(name.raw()).into_owned(),
            ));
        } else if let Change::Module(m) = change {
            let update = if let MC::DropModule {
                id,
                kind: ModuleKind::View,
            } = m
            {
                Some((id.object_name(), None))
            } else if let MC::CreateModule { id, module } | MC::AlterModule { id, module } = m
                && module.kind == ModuleKind::View
            {
                Some((id.object_name(), Some(module.definition.clone())))
            } else {
                None
            };
            if let Some((o, body)) = update
                && ident::same(&o.schema, crate::validate::SCHEMA)
            {
                if let Some((_, slot)) =
                    self.views.iter_mut().find(|(v, _)| ident::same(v, &o.name))
                {
                    *slot = body;
                } else if body.is_some() {
                    self.views.push((o.name, body));
                }
            }
        }
    }

    fn push_readers(&self, tname: &str, col: Option<&str>, blocking: &mut BTreeSet<String>) {
        let t = Escaped(tname);
        let what = match col {
            None => format!("table `main.{t}`"),
            Some(c) => format!("column `main.{t}.{}`", Escaped(c)),
        };
        let reads = |sql: &str| col.is_none_or(|c| sql_may_read(sql, c) || lexis::has_star(sql));
        for (v_name, body) in &self.views {
            if let Some(b) = body.as_deref()
                && sql_may_read(b, tname)
                && reads(b)
            {
                blocking.insert(format!("view `main.{}` may read {what}", Escaped(v_name)));
            }
        }
        for &(tr, sql) in &self.triggers {
            let on_t = ident::same(&tr.table, tname);
            let (tn, tt) = (Escaped(&tr.name), Escaped(&tr.table));
            if col.is_none() && on_t {
                blocking.insert(format!(
                    "trigger `main.{tn}` on table `main.{tt}` would be dropped with it by SQLite"
                ));
            } else if (on_t || sql_may_read(sql, tname)) && reads(sql) {
                blocking.insert(format!(
                    "trigger `main.{tn}` on table `main.{tt}` may read {what}"
                ));
            }
        }
    }

    fn check_table(&self, tname: &str) -> BTreeSet<String> {
        let t = Escaped(tname);
        let mut blocking = BTreeSet::new();
        for (child, list) in &self.fks {
            if ident::same(child, tname) {
                continue; // Measured: a self-referencing FK does not block DROP TABLE.
            }
            for fk in list.iter().filter(|fk| ident::same(&fk.parent, tname)) {
                blocking.insert(format!(
                    "{} references table `main.{t}`",
                    describe_fk(child, fk)
                ));
            }
        }
        self.push_readers(tname, None, &mut blocking);
        blocking
    }

    fn check_index(
        &self,
        st: &TableName,
        tname: &str,
        idx_name: &ObjectLocalName,
    ) -> Result<BTreeSet<String>, DbError> {
        let cols = self.find_columns(&st.name)?;
        let st_esc = Escaped(&st.name);
        let indexes = self
            .inv
            .indexes
            .iter()
            .find(|(k, _)| ident::same(k, &st.name))
            .map(|(_, v)| v.as_slice())
            .ok_or_else(|| {
                DbError::BadRow(format!(
                    "SQLite drop_blockers: index metadata for table `main.{st_esc}` was not read"
                ))
            })?;
        let ix = indexes
            .iter()
            .find(|ix| ident::same(&ix.name, idx_name.raw()))
            .ok_or_else(|| {
                DbError::Refused(format!(
                    "index `main.{}` on table `{}` is not in SQLite's catalog",
                    idx_name.shown(),
                    st.shown()
                ))
            })?;
        let (ixn, t) = (Escaped(&ix.name), Escaped(tname));
        let mut blocking = BTreeSet::new();
        if ix.origin != "c" {
            blocking.insert(format!("index `main.{ixn}` backs a PRIMARY KEY or UNIQUE constraint on `main.{t}` and cannot be dropped with DROP INDEX"));
            return Ok(blocking);
        }
        let Some(idx_key) = plain_unique_key(ix) else {
            return Ok(blocking);
        };
        let idx_cols: BTreeSet<String> = idx_key.keys().cloned().collect();
        let pk_cols: BTreeSet<String> = cols
            .iter()
            .filter(|c| c.pk > 0)
            .map(|c| ident::fold(&c.name).into_owned())
            .collect();
        let tf = ident::fold(tname);
        let covered = pk_cols == idx_cols
            || indexes.iter().any(|o| {
                !ident::same(&o.name, &ix.name)
                    && !self
                        .dropped_indexes
                        .contains(&(tf.as_ref().to_owned(), ident::fold(&o.name).into_owned()))
                    && plain_unique_key(o).is_some_and(|k| k == idx_key)
            });
        if covered {
            return Ok(blocking);
        }
        for (child, list) in &self.fks {
            for fk in list.iter().filter(|fk| ident::same(&fk.parent, tname)) {
                let fk_cols: Option<BTreeSet<String>> = fk
                    .to
                    .iter()
                    .map(|c| c.as_deref().map(|n| ident::fold(n).into_owned()))
                    .collect();
                if fk_cols.is_some_and(|c| c.len() == fk.to.len() && c == idx_cols) {
                    let d = describe_fk(child, fk);
                    blocking.insert(format!("{d} relies on unique index `main.{ixn}` on `main.{t}` (dropping it would leave a foreign key mismatch)"));
                }
            }
        }
        Ok(blocking)
    }

    fn check_column(&self, tname: &str, col: &ColumnEntry) -> BTreeSet<String> {
        let (cname, t, c) = (col.name.as_str(), Escaped(tname), Escaped(&col.name));
        let mut blocking = BTreeSet::new();
        if col.pk > 0 {
            blocking.insert(format!(
                "primary key on table `main.{t}` includes column `{c}`"
            ));
        }
        for (child, list) in &self.fks {
            for fk in list {
                let d = describe_fk(child, fk);
                if ident::same(child, tname) && fk.from.iter().any(|col| ident::same(col, cname)) {
                    blocking.insert(format!("{d} references column `{c}`"));
                }
                if ident::same(&fk.parent, tname)
                    && (fk
                        .to
                        .iter()
                        .any(|to| to.as_deref().is_some_and(|n| ident::same(n, cname)))
                        || (col.pk > 0 && fk.to.iter().all(Option::is_none)))
                {
                    blocking.insert(format!("{d} references column `main.{t}.{c}`"));
                }
            }
        }
        self.push_readers(tname, Some(cname), &mut blocking);
        blocking
    }
}
