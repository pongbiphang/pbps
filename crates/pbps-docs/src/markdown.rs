//! Markdown rendering.
//!
//! The four things a live database cannot tell you (SPEC §9.4) are what this
//! output exists for: `description` fields, deprecation reasons, the ids file's
//! tombstones, and the foreign-key edges of the ERD. Everything else here — the
//! column types, the keys — is available from introspection too, and is included
//! only so the descriptions have something to hang on.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pbps_model::{ComputedColumn, IdsFile, Module, ModuleId, Role, Schema, Table, TableName};

use crate::erd;

/// Renders the whole project as one Markdown document.
pub fn render(schema: &Schema, ids: &IdsFile, title: &str) -> String {
    // A new schema family must be considered by every prose renderer.
    let Schema {
        tables,
        modules,
        roles,
    } = schema;
    let mut s = String::new();
    let _ = writeln!(s, "# {title}\n");
    let _ = writeln!(
        s,
        "{} table(s), {} column(s), {} module(s).\n",
        tables.len(),
        tables
            .values()
            .map(|t| t.columns.len() + t.computed.len())
            .sum::<usize>(),
        modules.len()
    );

    s.push_str("## Diagram\n\n```mermaid\n");
    s.push_str(&erd::render(schema));
    s.push_str("```\n\n");

    s.push_str("## Tables\n");
    for (name, table) in tables {
        one_table(&mut s, name, table);
    }

    modules_section(&mut s, modules);
    roles_section(&mut s, roles);
    deprecated_section(&mut s, schema);
    graveyard(&mut s, ids);
    s
}

/// Views, procedures, functions and triggers, with their definitions.
///
/// The definition is included in full rather than summarized: it *is* the
/// object (ADR-0002), and a reader asking "what does this view do" is asking to
/// read it.
fn modules_section(s: &mut String, modules: &BTreeMap<ModuleId, Module>) {
    if modules.is_empty() {
        return;
    }
    s.push_str("\n## Views, procedures, functions and triggers\n");
    for (name, m) in modules {
        let _ = writeln!(s, "\n### `{name}`\n", name = name.raw());
        let on = name
            .attached_to()
            .map(|t| format!(" on `{t}`", t = t.raw()))
            .unwrap_or_default();
        let _ = writeln!(s, "*{}{on}*\n", m.kind);
        if let Some(d) = &m.description {
            let _ = writeln!(s, "{}\n", d.raw());
        }
        let _ = writeln!(s, "```sql\n{}\n```", m.definition.trim_end());
    }
}

/// Database roles and what each is granted (ADR-0005). Membership is not
/// here: it is each environment's own, and pbps never reads it.
fn roles_section(s: &mut String, roles: &BTreeMap<pbps_model::RoleName, Role>) {
    if roles.is_empty() {
        return;
    }
    s.push_str("\n## Roles\n");
    for (name, role) in roles {
        let _ = writeln!(s, "\n### `{name}`\n", name = name.raw());
        if let Some(d) = &role.description {
            let _ = writeln!(s, "{}\n", d.raw());
        }
        let grants: Vec<_> = role.declarable_grants().collect();
        if grants.is_empty() {
            s.push_str("No grants.\n");
            continue;
        }
        s.push_str("| Granted on | Permissions |\n|---|---|\n");
        for (target, permissions) in grants {
            let list: Vec<&str> = permissions.iter().map(|p| p.as_str()).collect();
            let _ = writeln!(
                s,
                "| `{target}` | {} |",
                list.join(", "),
                target = target.raw()
            );
        }
    }
}

fn one_table(s: &mut String, name: &TableName, table: &Table) {
    let _ = writeln!(s, "\n### `{name}`\n", name = name.raw());
    if let Some(d) = &table.description {
        let _ = writeln!(s, "{}\n", d.raw());
    }

    s.push_str("| Column | Type | Null | Default | Description |\n");
    s.push_str("|---|---|---|---|---|\n");
    for (col_name, c) in &table.columns {
        let mut label = code_cell(col_name.raw());
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
            match &c.value_source {
                pbps_model::ValueSource::Default(text) => code_cell(text),
                pbps_model::ValueSource::None
                | pbps_model::ValueSource::Identity(_)
                | pbps_model::ValueSource::Sequence(_)
                | pbps_model::ValueSource::Generated(_) => "—".into(),
            },
            cell(c.description.as_ref().map(pbps_model::Text::raw))
        );
    }

    computed_table(s, table);

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
            fk.references_table.raw(),
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
                    format!("`{}` desc", c.key.text())
                } else {
                    format!("`{}`", c.key.text())
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

/// SQL Server computed columns (#1174), which the model holds beside the
/// columns rather than among them. They get their own table because they have
/// no type to show: the engine infers it and the model holds none
/// (DEC-1174.1), and the expression is not a default.
fn computed_table(s: &mut String, table: &Table) {
    if table.computed.is_empty() {
        return;
    }
    s.push_str("\n| Computed column | Expression | Stored |\n|---|---|---|\n");
    for (col_name, c) in &table.computed {
        let mut label = code_cell(col_name.raw());
        if table
            .primary_key
            .as_ref()
            .is_some_and(|pk| pk.columns.iter().any(|p| p == col_name))
        {
            label.push_str(" **PK**");
        }
        let stored = computed_flags(c).unwrap_or_else(|| "—".into());
        let _ = writeln!(s, "| {label} | {} | {stored} |", code_cell(&c.expression));
    }
}

/// Everything marked deprecated, in one place — the section a developer checks
/// before using a column they have not seen before.
fn deprecated_section(s: &mut String, schema: &Schema) {
    let mut rows = Vec::new();
    for (name, table) in &schema.tables {
        for (col_name, c) in &table.columns {
            if let Some(reason) = &c.deprecated {
                rows.push((
                    format!(
                        "{name}.{col_name}",
                        name = name.raw(),
                        col_name = col_name.raw(),
                    ),
                    reason.clone(),
                ));
            }
        }
    }
    if rows.is_empty() {
        return;
    }
    s.push_str("\n## Do not use\n\nThese columns still exist but are on their way out.\n\n");
    s.push_str("| Column | Reason |\n|---|---|\n");
    for (name, reason) in rows {
        let _ = writeln!(s, "| `{name}` | {} |", reason.raw());
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

/// `persisted` and `not null` where declared, or `None` for a column
/// computed on read with an inferred nullability.
pub(crate) fn computed_flags(c: &ComputedColumn) -> Option<String> {
    let flags: Vec<&str> = [(c.persisted, "persisted"), (c.not_null, "not null")]
        .into_iter()
        .filter_map(|(set, flag)| set.then_some(flag))
        .collect();
    (!flags.is_empty()).then(|| flags.join(", "))
}

fn columns(cols: &[pbps_model::ColumnName]) -> String {
    cols.iter()
        .map(|c| format!("`{c}`", c = c.raw()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `text` as a code span that stays inside one table cell. A line ending
/// would end the row, so it becomes the space a code span renders it as
/// anyway; a backtick run in the text is outfenced by a longer one; and a
/// `|`, which ends a cell even inside a code span, is escaped.
fn code_cell(text: &str) -> String {
    let text = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
    let run = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(run + 1);
    // A span that starts or ends with a backtick needs a space between it
    // and the fence, which the renderer strips again.
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{}{pad}{fence}", text.replace('|', "\\|"))
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
    use pbps_model::{Column, ColumnType, Index, IndexColumn, PrimaryKey, Tombstone, Uid};

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
            storage_parameters: Default::default(),
        });
        let mut s = Schema::default();
        s.tables.insert(TableName::new("dbo", "customer"), t);
        s
    }

    #[test]
    fn a_default_stays_inside_its_cell() {
        // A `|` ends a cell even inside a code span, a line break ends the
        // row, and a backtick ends a one-backtick span (#1873).
        let mut s = documented();
        let t = s
            .tables
            .get_mut(&TableName::new("dbo", "customer"))
            .unwrap();
        for (name, default) in [
            ("a_pipe", "'a|b'"),
            ("a_break", "CASE WHEN 1 = 1\nTHEN 'x'\r\nEND"),
            ("a_tick", "'x`y'"),
            ("z_after", "0"),
        ] {
            let mut c = Column::new(ty("nvarchar(10)"));
            c.value_source = pbps_model::ValueSource::Default(default.into());
            t.columns.insert(name.into(), c);
        }
        let out = render(&s, &IdsFile::default(), "Schema");
        for row in [
            "| `a_pipe` | `nvarchar(10)` | yes | `'a\\|b'` | — |\n",
            "| `a_break` | `nvarchar(10)` | yes | `CASE WHEN 1 = 1 THEN 'x' END` | — |\n",
            "| `a_tick` | `nvarchar(10)` | yes | ``'x`y'`` | — |\n",
            "| `z_after` | `nvarchar(10)` | yes | `0` | — |\n",
        ] {
            assert!(out.contains(row), "{row}{out}");
        }
        // Each row keeps its five cells: no unescaped `|` beyond its six.
        for line in out
            .lines()
            .filter(|l| l.starts_with("| `a_") || l.starts_with("| `z_"))
        {
            assert_eq!(line.replace("\\|", "").matches('|').count(), 6, "{line}");
        }
    }

    #[test]
    fn a_column_name_stays_inside_its_cell() {
        // A `|` in a name split the cell and a backtick ended the span
        // (#1874): columns and computed columns alike.
        let mut s = documented();
        let t = s
            .tables
            .get_mut(&TableName::new("dbo", "customer"))
            .unwrap();
        t.columns
            .insert("a|b".into(), Column::new(ty("int")).not_null());
        t.columns
            .insert("x`y".into(), Column::new(ty("int")).not_null());
        t.computed.insert(
            "c|d".into(),
            ComputedColumn {
                expression: "1".into(),
                persisted: false,
                not_null: false,
            },
        );
        let out = render(&s, &IdsFile::default(), "Schema");
        for row in [
            "| `a\\|b` | `int` | no | — | — |\n",
            "| ``x`y`` | `int` | no | — | — |\n",
            "| `c\\|d` | `1` | — |\n",
        ] {
            assert!(out.contains(row), "{row}{out}");
        }
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

    /// `documented()` plus two SQL Server computed columns (#1174), one
    /// persisted and one computed on read, and an index over the persisted
    /// one.
    fn with_computed() -> Schema {
        let mut s = documented();
        let t = s
            .tables
            .get_mut(&TableName::new("dbo", "customer"))
            .unwrap();
        t.computed.insert(
            "total".into(),
            ComputedColumn {
                expression: "[qty] * [price]".into(),
                persisted: true,
                not_null: true,
            },
        );
        t.computed.insert(
            "flags".into(),
            ComputedColumn {
                expression: "[a] | 4".into(),
                persisted: false,
                not_null: false,
            },
        );
        t.indexes.insert(
            "ix_total".into(),
            Index {
                columns: vec![IndexColumn::column("total")],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        s
    }

    /// The model holds computed columns beside the columns (#1174); a
    /// renderer reading only `columns` drops them while the index over one
    /// still names it.
    #[test]
    fn computed_columns_are_rendered_beside_the_columns() {
        let out = render(&with_computed(), &IdsFile::default(), "Schema");
        assert!(
            out.contains("| `total` | `[qty] * [price]` | persisted, not null |"),
            "{out}"
        );
        // Computed on read: no flags, and the `|` cannot end the cell.
        assert!(out.contains(r"| `flags` | `[a] \| 4` | — |"), "{out}");
        assert!(out.contains("- **Index** `ix_total`: `total`"), "{out}");
        assert!(out.contains("4 column(s)"), "{out}");
    }

    /// A multiline expression (a YAML block keeps its line breaks) would end
    /// the table row, and a backtick in a string literal would end a
    /// one-backtick span.
    #[test]
    fn a_computed_expression_stays_in_its_cell() {
        let mut s = documented();
        s.tables
            .get_mut(&TableName::new("dbo", "customer"))
            .unwrap()
            .computed
            .insert(
                "kind".into(),
                ComputedColumn {
                    expression: "CASE WHEN [a] = 'x`y'\nTHEN 1\r\nEND".into(),
                    persisted: true,
                    not_null: false,
                },
            );
        let out = render(&s, &IdsFile::default(), "Schema");
        assert!(
            out.contains("| `kind` | ``CASE WHEN [a] = 'x`y' THEN 1 END`` | persisted |\n"),
            "{out}"
        );
        assert_eq!(code_cell("`a`"), "`` `a` ``");
        assert_eq!(code_cell("a ``b"), "```a ``b```");
        assert_eq!(code_cell("a"), "`a`");
    }

    #[test]
    fn a_table_without_computed_columns_has_no_computed_table() {
        let out = render(&documented(), &IdsFile::default(), "Schema");
        assert!(!out.contains("Computed column"), "{out}");
        assert!(out.contains("2 column(s)"), "{out}");
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
