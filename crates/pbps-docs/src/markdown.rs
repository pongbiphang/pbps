//! Markdown rendering.
//!
//! The four things a live database cannot tell you (SPEC §9.4) are what this
//! output exists for: `description` fields, deprecation reasons, the ids file's
//! tombstones, and the foreign-key edges of the ERD. Everything else here — the
//! column types, the keys — is available from introspection too, and is included
//! only so the descriptions have something to hang on.

use std::fmt::Write as _;

use pbps_model::{IdsFile, Schema, Table, TableName};

use crate::erd;

/// Renders the whole project as one Markdown document.
pub fn render(schema: &Schema, ids: &IdsFile, title: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# {title}\n");
    let _ = writeln!(
        s,
        "{} table(s), {} column(s), {} module(s).\n",
        schema.tables.len(),
        schema
            .tables
            .values()
            .map(|t| t.columns.len())
            .sum::<usize>(),
        schema.modules.len()
    );

    s.push_str("## Diagram\n\n```mermaid\n");
    s.push_str(&erd::render(schema));
    s.push_str("```\n\n");

    s.push_str("## Tables\n");
    for (name, table) in &schema.tables {
        one_table(&mut s, name, table);
    }

    modules_section(&mut s, schema);
    deprecated_section(&mut s, schema);
    graveyard(&mut s, ids);
    s
}

/// Views, procedures, functions and triggers, with their definitions.
///
/// The definition is included in full rather than summarized: it *is* the
/// object (ADR-0002), and a reader asking "what does this view do" is asking to
/// read it.
fn modules_section(s: &mut String, schema: &Schema) {
    if schema.modules.is_empty() {
        return;
    }
    s.push_str("\n## Views, procedures, functions and triggers\n");
    for (name, m) in &schema.modules {
        let _ = writeln!(s, "\n### `{name}`\n");
        let on =
            m.on.as_ref()
                .map(|t| format!(" on `{t}`"))
                .unwrap_or_default();
        let _ = writeln!(s, "*{}{on}*\n", m.kind);
        if let Some(d) = &m.description {
            let _ = writeln!(s, "{d}\n");
        }
        let _ = writeln!(s, "```sql\n{}\n```", m.definition.trim_end());
    }
}

fn one_table(s: &mut String, name: &TableName, table: &Table) {
    let _ = writeln!(s, "\n### `{name}`\n");
    if let Some(d) = &table.description {
        let _ = writeln!(s, "{d}\n");
    }

    s.push_str("| Column | Type | Null | Default | Description |\n");
    s.push_str("|---|---|---|---|---|\n");
    for (col_name, c) in &table.columns {
        let mut label = format!("`{col_name}`");
        if table
            .primary_key
            .as_ref()
            .is_some_and(|pk| pk.columns.iter().any(|p| p == col_name))
        {
            label.push_str(" **PK**");
        }
        if c.is_deprecated() {
            label.push_str(" ~~deprecated~~");
        }
        let _ = writeln!(
            s,
            "| {label} | `{}` | {} | {} | {} |",
            c.ty,
            if c.nullable { "yes" } else { "no" },
            c.default
                .as_deref()
                .map(|d| format!("`{d}`"))
                .unwrap_or_else(|| "—".into()),
            cell(c.description.as_deref())
        );
    }

    if let Some(pk) = &table.primary_key {
        let _ = writeln!(
            s,
            "\n- **Primary key**{}: {}",
            pk.name
                .as_deref()
                .map(|n| format!(" `{n}`"))
                .unwrap_or_default(),
            columns(&pk.columns)
        );
    }
    for (n, u) in &table.unique {
        let _ = writeln!(s, "- **Unique** `{n}`: {}", columns(&u.columns));
    }
    for (n, fk) in &table.foreign_keys {
        let _ = writeln!(
            s,
            "- **Foreign key** `{n}`: {} → `{}` ({})",
            columns(&fk.columns),
            fk.references_table,
            columns(&fk.references_columns)
        );
    }
    for (n, c) in &table.checks {
        let _ = writeln!(s, "- **Check** `{n}`: `{}`", c.expression);
    }
    for (n, ix) in &table.indexes {
        let cols: Vec<String> = ix
            .columns
            .iter()
            .map(|c| {
                if c.descending {
                    format!("`{}` desc", c.name)
                } else {
                    format!("`{}`", c.name)
                }
            })
            .collect();
        let _ = write!(
            s,
            "- **Index** `{n}`{}: {}",
            if ix.unique { " (unique)" } else { "" },
            cols.join(", ")
        );
        if !ix.include.is_empty() {
            let _ = write!(s, " including {}", columns(&ix.include));
        }
        if let Some(f) = &ix.filter {
            let _ = write!(s, " where `{f}`");
        }
        s.push('\n');
    }
}

/// Everything marked deprecated, in one place — the section a developer checks
/// before using a column they have not seen before.
fn deprecated_section(s: &mut String, schema: &Schema) {
    let mut rows = Vec::new();
    for (name, table) in &schema.tables {
        for (col_name, c) in &table.columns {
            if let Some(reason) = &c.deprecated {
                rows.push((format!("{name}.{col_name}"), reason.clone()));
            }
        }
    }
    if rows.is_empty() {
        return;
    }
    s.push_str("\n## Do not use\n\nThese columns still exist but are on their way out.\n\n");
    s.push_str("| Column | Reason |\n|---|---|\n");
    for (name, reason) in rows {
        let _ = writeln!(s, "| `{name}` | {reason} |");
    }
}

/// The graveyard: what was dropped, when, by whom and why.
///
/// This is the section no introspection can produce — the database has no
/// memory of what is gone, and the tombstones in the ids file are the only
/// record.
fn graveyard(s: &mut String, ids: &IdsFile) {
    if ids.tombstones.is_empty() {
        return;
    }
    s.push_str("\n## Graveyard\n\nDropped objects, kept so an audit can be answered.\n\n");
    s.push_str("| Was | Dropped | By | Reason |\n|---|---|---|---|\n");
    // BTreeMap over uid gives a stable order, but the useful order for a reader
    // is by date, newest first.
    let mut stones: Vec<_> = ids.tombstones.values().collect();
    stones.sort_by(|a, b| b.dropped_at.cmp(&a.dropped_at).then(a.was.cmp(&b.was)));
    for t in stones {
        let _ = writeln!(
            s,
            "| `{}` | {} | {} | {} |",
            t.was, t.dropped_at, t.operator, t.reason
        );
    }
}

fn columns(cols: &[String]) -> String {
    cols.iter()
        .map(|c| format!("`{c}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn cell(v: Option<&str>) -> String {
    match v {
        // A pipe inside a cell would end it and shift every later column.
        Some(d) => d.replace('|', "\\|"),
        None => "—".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, ColumnType, PrimaryKey, Tombstone, Uid};

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    fn documented() -> Schema {
        let mut t = Table {
            description: Some("One row per customer.".into()),
            ..Default::default()
        };
        let mut id = Column::new(ty("bigint")).not_null();
        id.description = Some("Surrogate key.".into());
        t.columns.insert("id".into(), id);
        let mut old = Column::new(ty("nvarchar(50)"));
        old.deprecated = Some("replaced by contact_email".into());
        t.columns.insert("legacy_email".into(), old);
        t.primary_key = Some(PrimaryKey {
            name: Some("pk_customer".into()),
            columns: vec!["id".into()],
        });
        let mut s = Schema::default();
        s.tables.insert(TableName::new("dbo", "customer"), t);
        s
    }

    #[test]
    fn descriptions_reach_the_output() {
        let out = render(&documented(), &IdsFile::default(), "Schema");
        assert!(out.contains("One row per customer."), "{out}");
        assert!(out.contains("Surrogate key."), "{out}");
        assert!(out.contains("`id` **PK**"), "{out}");
        assert!(out.contains("**Primary key** `pk_customer`"), "{out}");
    }

    #[test]
    fn deprecated_columns_get_their_own_section() {
        let out = render(&documented(), &IdsFile::default(), "Schema");
        assert!(out.contains("## Do not use"), "{out}");
        assert!(out.contains("replaced by contact_email"), "{out}");
    }

    /// A schema with nothing deprecated must not grow an empty section.
    #[test]
    fn sections_with_nothing_to_say_are_omitted() {
        let mut s = documented();
        s.tables
            .get_mut(&TableName::new("dbo", "customer"))
            .unwrap()
            .columns
            .shift_remove("legacy_email");
        let out = render(&s, &IdsFile::default(), "Schema");
        assert!(!out.contains("## Do not use"), "{out}");
        assert!(!out.contains("## Graveyard"), "{out}");
    }

    /// The one section no live database can produce.
    #[test]
    fn tombstones_become_the_graveyard_newest_first() {
        let mut ids = IdsFile::default();
        for (uid, was, at) in [
            ("c_k7x2mq", "dbo.customer.national_id", "2026-01-05"),
            ("c_p3n8vd", "dbo.customer.fax", "2026-07-20"),
        ] {
            ids.tombstones.insert(
                uid.parse::<Uid>().unwrap(),
                Tombstone {
                    was: was.into(),
                    dropped_at: at.into(),
                    reason: "no longer collected".into(),
                    operator: "leon".into(),
                },
            );
        }
        let out = render(&documented(), &ids, "Schema");
        assert!(out.contains("## Graveyard"), "{out}");
        let fax = out.find("dbo.customer.fax").expect("fax listed");
        let nid = out
            .find("dbo.customer.national_id")
            .expect("national_id listed");
        assert!(fax < nid, "the newest drop must come first");
    }

    /// A pipe in a description would end the table cell and shift every column
    /// after it.
    #[test]
    fn a_pipe_in_a_description_cannot_break_the_table() {
        let mut s = documented();
        s.tables
            .get_mut(&TableName::new("dbo", "customer"))
            .unwrap()
            .columns["id"]
            .description = Some("a | b".into());
        assert!(render(&s, &IdsFile::default(), "Schema").contains(r"a \| b"));
    }

    #[test]
    fn rendering_is_deterministic() {
        let (s, ids) = (documented(), IdsFile::default());
        let first = render(&s, &ids, "Schema");
        for _ in 0..10 {
            assert_eq!(render(&s, &ids, "Schema"), first);
        }
    }
}
