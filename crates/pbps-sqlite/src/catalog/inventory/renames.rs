//! Rename impact analysis over SQLite's [`Inventory`] (#2950).
//!
//! With `PRAGMA legacy_alter_table = OFF` (enforced by preflight), SQLite
//! rewrites direct references in `main.sqlite_schema` during `ALTER TABLE ...
//! RENAME TO` and `RENAME COLUMN` (views, plain/partial/expression indexes,
//! CHECK constraints, outbound and inbound foreign keys, triggers including
//! `WHEN` and `UPDATE OF`, and generated columns), so those are `carried`.
//! Objects whose own name embeds the old name, plus downstream views and
//! `INSTEAD OF` triggers over a direct view on a column rename (SQLite rewrites
//! views without keeping the old output column name as an alias), are `advisory`.

use std::collections::BTreeSet;

use pbps_db::DbError;
use pbps_db::impact::{ImpactError, ImpactReport, Referrer, RenameTarget};
use pbps_db::sqlite::Connection;
use pbps_dialect::DialectError;
use pbps_model::{ModuleId, ModuleKind, ObjectName};

use super::drops::describe_fk;
use super::{
    ColumnEntry, EntryKind, IndexEntry, TableColumns, read_for_rename, require_stored_sql,
    sql_may_read,
};
use crate::catalog::text::{self, Kind as TextKind};
use crate::{ident, lexis};

const SCHEMA: &str = crate::validate::SCHEMA;

fn invalid(message: String) -> ImpactError {
    ImpactError::Name(DialectError::Invalid {
        dialect: crate::types::DIALECT,
        message,
    })
}

fn referrer(kind: &str, name: String, detail: Option<String>) -> Referrer {
    Referrer {
        kind: kind.to_owned(),
        name,
        detail,
        removed_with: None,
    }
}

/// Queries SQLite's catalog inventory for everything affected by renaming
/// `target` (a table or column in `main`), classifying each referrer as
/// `carried` or `advisory`.
///
/// `ImpactReport::blocking` is always empty on SQLite, as it is on PostgreSQL.
/// If the file already holds a broken view or trigger, SQLite refuses the
/// rename itself with an error naming that object and the single-transaction
/// apply changes nothing. We do not probe for pre-existing damage here because
/// `EXPLAIN` on a view over a virtual table would load that table's module
/// (DEC-2950.14) and probing triggers would require maintaining a trigger-header
/// parser.
pub fn rename_report(c: &Connection, target: &RenameTarget) -> Result<ImpactReport, ImpactError> {
    if let RenameTarget::Module(module) = target {
        return Err(invalid(format!(
            "module {} is not a rename target on SQLite: dependents of a dropped view are checked by `view_dependents`",
            module.shown()
        )));
    }
    if !ident::same(&target.table().schema, SCHEMA) {
        return Err(invalid(format!(
            "SQLite manages only schema `{SCHEMA}`, not `{}`",
            target.table().shown()
        )));
    }
    ident::quote(&target.table().name)?;
    if let Some(col) = target.column() {
        ident::quote(col)?;
    }

    let inv = read_for_rename(c, &target.table().name)?;
    let listed = inv
        .columns
        .iter()
        .find(|(k, _)| ident::same(k, &target.table().name))
        .map(|(_, v)| v);
    let why = match listed {
        Some(TableColumns::Columns(_)) => None,
        Some(TableColumns::Virtual) => Some("is a virtual table in SQLite"),
        Some(TableColumns::View) => Some("is a view in SQLite, not an ordinary table"),
        Some(TableColumns::Absent) | None => Some("is not a table this database has"),
    };
    let (Some(TableColumns::Columns(cols)), None) = (listed, why) else {
        return Err(invalid(format!(
            "{} {}, so what a rename of {target} would affect cannot be read",
            target.table().shown(),
            why.unwrap_or("could not be read")
        )));
    };
    let cols = cols.as_slice();
    let table_entry = inv
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::Table && ident::same(&e.name, &target.table().name))
        .ok_or_else(|| {
            ImpactError::Query(DbError::BadRow(format!(
                "table `{}` is listed by `PRAGMA table_list` and missing from `main.sqlite_schema`",
                target.table().shown()
            )))
        })?;
    let (tname, table_sql) = (table_entry.name.as_str(), require_stored_sql(table_entry)?);
    let target_col: Option<&ColumnEntry> = match target.column() {
        None => None,
        Some(col_name) => Some(cols.iter().find(|c| ident::same(&c.name, col_name)).ok_or_else(|| {
            invalid(format!("{target} is not a column this database has, so what a rename of it would affect cannot be read"))
        })?),
    };

    let (mut views, mut triggers) = (Vec::new(), Vec::new());
    for entry in &inv.entries {
        match entry.kind {
            EntryKind::View => {
                let sql = require_stored_sql(entry)?;
                let body = text::view(&entry.name, sql)
                    .map(|v| v.body)
                    .unwrap_or_else(|_| sql.to_owned());
                views.push((entry, body));
            }
            EntryKind::Trigger => triggers.push((entry, require_stored_sql(entry)?)),
            EntryKind::Table | EntryKind::Index => {}
        }
    }

    let mut report = ImpactReport {
        target: target.to_string(),
        ..Default::default()
    };
    let folded_needle = ident::fold(target_col.map_or(tname, |c| c.name.as_str()));
    let name_detail = if target_col.is_some() {
        "the name embeds the old column name"
    } else {
        "the name embeds the old table name"
    };
    let carry = |carried: &mut Vec<Referrer>, kind: &str, name: String| {
        carried.push(referrer(kind, name, None));
    };
    let note_name = |advisory: &mut Vec<Referrer>, kind: &str, name: String, check: &str| {
        if !folded_needle.is_empty() && ident::fold(check).contains(folded_needle.as_ref()) {
            advisory.push(referrer(kind, name, Some(name_detail.to_owned())));
        }
    };

    // Views (carrying `removed_with` so `excuse_dropped_referrers` works) and downstream advisories.
    let mut carried_views: Vec<&str> = Vec::new();
    for (entry, body) in &views {
        if !sql_may_read(body, tname)
            || !target_col.is_none_or(|c| sql_may_read(body, &c.name) || lexis::has_star(body))
        {
            continue;
        }
        carried_views.push(&entry.name);
        report.carried.push(Referrer {
            kind: "view".to_owned(),
            name: format!("view {SCHEMA}.{}", entry.name),
            detail: None,
            removed_with: Some((
                ModuleKind::View,
                ModuleId::Named(ObjectName::new(SCHEMA, &entry.name)),
            )),
        });
        note_name(
            &mut report.advisory,
            "view",
            format!("{SCHEMA}.{}", entry.name),
            &entry.name,
        );
    }
    if let Some(col) = target_col
        && !carried_views.is_empty()
    {
        let direct: BTreeSet<String> = carried_views
            .iter()
            .map(|n| ident::fold(n).into_owned())
            .collect();
        for (entry, body) in &views {
            if !direct.contains(ident::fold(&entry.name).as_ref())
                && let Some(&via) = carried_views.iter().find(|&&v| sql_may_read(body, v))
                && (sql_may_read(body, &col.name) || lexis::has_star(body))
            {
                let d = format!(
                    "reads view {SCHEMA}.{via}, whose output column name changes on column rename unless aliased"
                );
                report.advisory.push(referrer(
                    "view",
                    format!("{SCHEMA}.{}", entry.name),
                    Some(d),
                ));
            }
        }
        for &(entry, sql) in &triggers {
            if let Some(&via) = carried_views
                .iter()
                .find(|&&v| ident::same(&entry.table, v))
                && (sql_may_read(sql, &col.name) || lexis::has_star(sql))
            {
                let d = format!(
                    "attached to view {SCHEMA}.{via}, whose output column name changes on column rename unless aliased"
                );
                report.advisory.push(referrer(
                    "trigger",
                    format!("{SCHEMA}.{}", entry.name),
                    Some(d),
                ));
            }
        }
    }

    // Explicit indexes on `t` (plain, partial and expression).
    let indexes: &[IndexEntry] = inv
        .indexes
        .iter()
        .find(|(k, _)| ident::same(k, &target.table().name))
        .map_or(&[], |(_, v)| v.as_slice());
    for ix in indexes.iter().filter(|ix| ix.origin == "c") {
        let ix_sql = inv
            .entries
            .iter()
            .find(|e| e.kind == EntryKind::Index && ident::same(&e.name, &ix.name))
            .map(require_stored_sql)
            .transpose()?;
        let affected = target_col.is_none_or(|col| {
            ix.keys
                .iter()
                .any(|(k, _)| k.as_deref().is_some_and(|n| ident::same(n, &col.name)))
                || ((ix.partial || ix.keys.iter().any(|(k, _)| k.is_none()))
                    && ix_sql.is_some_and(|s| sql_may_read(s, &col.name)))
        });
        if affected {
            carry(
                &mut report.carried,
                "index",
                format!("index {SCHEMA}.{} on table {SCHEMA}.{tname}", ix.name),
            );
        }
        note_name(&mut report.advisory, "index", ix.name.clone(), &ix.name);
    }

    // CHECK constraints, generated columns and named constraints on `t`.
    if let Ok(parsed) = text::table(tname, table_sql) {
        if let Some(col) = target_col {
            for cons in &parsed.constraints {
                if let TextKind::Check { expression } = &cons.kind
                    && sql_may_read(expression, &col.name)
                {
                    let desc = match &cons.name {
                        Some(n) => format!("check constraint {n} on table {SCHEMA}.{tname}"),
                        None => {
                            format!("check constraint ({expression}) on table {SCHEMA}.{tname}")
                        }
                    };
                    carry(&mut report.carried, "check constraint", desc);
                }
            }
            for (at, expr) in &parsed.generated_exprs {
                if let Some(gname) = parsed.columns.get(*at)
                    && !ident::same(gname, &col.name)
                    && sql_may_read(expr, &col.name)
                {
                    carry(
                        &mut report.carried,
                        "generated column",
                        format!("generated column {SCHEMA}.{tname}.{gname}"),
                    );
                }
            }
        }
        for cons in &parsed.constraints {
            let Some(cname) = &cons.name else { continue };
            let kind = match &cons.kind {
                TextKind::PrimaryKey { .. } => "primary key",
                TextKind::Unique { .. } => "unique constraint",
                TextKind::Check { .. } => "check constraint",
                TextKind::ForeignKey { .. } => "foreign key",
                TextKind::NotNull | TextKind::Null | TextKind::Default { .. } => continue,
            };
            note_name(&mut report.advisory, kind, cname.clone(), cname);
        }
    } else if let Some(col) = target_col {
        for gcol in cols
            .iter()
            .filter(|c| (c.hidden == 2 || c.hidden == 3) && !ident::same(&c.name, &col.name))
        {
            carry(
                &mut report.carried,
                "generated column",
                format!("generated column {SCHEMA}.{tname}.{}", gcol.name),
            );
        }
    }

    // Foreign keys (outbound on `t`, self-FKs and inbound from other tables).
    let Some(all_fks) = &inv.foreign_keys else {
        return Err(ImpactError::Query(DbError::BadRow(
            "SQLite rename_report requires foreign-key metadata to be read from the catalog".into(),
        )));
    };
    for (child, list) in all_fks {
        let on_target = ident::same(child, tname);
        for fk in list {
            let points_to = ident::same(&fk.parent, tname);
            let affected =
                match target_col {
                    None => on_target || points_to,
                    Some(col) => {
                        (on_target && fk.from.iter().any(|c| ident::same(c, &col.name)))
                            || (points_to
                                && (fk.to.iter().any(|t| {
                                    t.as_deref().is_some_and(|n| ident::same(n, &col.name))
                                }) || (col.pk > 0 && fk.to.iter().all(Option::is_none))))
                    }
                };
            if !affected {
                continue;
            }
            carry(&mut report.carried, "foreign key", describe_fk(child, fk));
            if !on_target && let Some(fk_name) = &fk.name {
                note_name(
                    &mut report.advisory,
                    "foreign key",
                    format!("{fk_name} on table {SCHEMA}.{child}"),
                    fk_name,
                );
            }
        }
    }

    // Triggers attached to `t` or reading `t` from another table (whole statement scanned,
    // matching `sql_may_read`'s documented conservative superset).
    for &(entry, sql) in &triggers {
        let on_t = ident::same(&entry.table, tname);
        let reads = match target_col {
            None => on_t || sql_may_read(sql, tname),
            Some(col) => {
                (on_t || sql_may_read(sql, tname))
                    && (sql_may_read(sql, &col.name) || lexis::has_star(sql))
            }
        };
        if reads {
            carry(
                &mut report.carried,
                "trigger",
                format!(
                    "trigger {SCHEMA}.{} on table {SCHEMA}.{}",
                    entry.name, entry.table
                ),
            );
            note_name(
                &mut report.advisory,
                "trigger",
                format!("{SCHEMA}.{}", entry.name),
                &entry.name,
            );
        }
    }

    Ok(report)
}
