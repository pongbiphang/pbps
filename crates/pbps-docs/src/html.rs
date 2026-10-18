//! A single self-contained HTML file.
//!
//! **No CDN, no external anything.** The air-gap rule that decided the driver
//! (SPEC §11.3) applies to artifacts too: a documentation page that silently
//! renders unstyled on a machine without internet is worse than a plain one that
//! always looks the same. The CSS is inlined and there is no JavaScript.
//!
//! That rules Mermaid's renderer out, so the ERD travels as its source in a
//! labelled block, ready to paste where it does render (a GitLab or GitHub
//! Markdown view, SPEC §9.4). Pretending to draw it by shipping a script tag
//! that fails offline would be the worse trade.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pbps_model::{IdsFile, Module, ModuleId, Role, Schema, Table, TableName};

use crate::erd;

const STYLE: &str = "\
:root { color-scheme: light dark; }
body { font: 16px/1.6 system-ui, -apple-system, Segoe UI, sans-serif;
       max-width: 60rem; margin: 2rem auto; padding: 0 1rem; }
h1, h2 { border-bottom: 1px solid #8884; padding-bottom: .2em; }
h3 { margin-top: 2em; }
table { border-collapse: collapse; width: 100%; margin: 1em 0; }
th, td { border: 1px solid #8884; padding: .35em .6em; text-align: left;
         vertical-align: top; }
th { background: #8881; }
code, pre { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
code { background: #8882; padding: .1em .3em; border-radius: 3px; }
pre { background: #8881; padding: 1em; overflow-x: auto; border-radius: 4px; }
.dep { text-decoration: line-through; opacity: .7; }
.muted { opacity: .65; }";

/// Exact stylesheet text, including whitespace, for the viewer's CSP hash.
pub fn style_contents() -> String {
    // CSP hashes the text between the tags, including both newlines. Keep the
    // same bytes available to the CLI without giving pbps-ui a renderer edge.
    format!("\n{STYLE}\n")
}

/// Escapes the five characters that can change the structure of the document.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn code(s: &str) -> String {
    format!("<code>{}</code>", esc(s))
}

pub fn render(schema: &Schema, ids: &IdsFile, title: &str) -> String {
    // A new schema family must be considered by every prose renderer.
    let Schema {
        tables,
        modules,
        roles,
    } = schema;
    let mut s = String::new();
    let _ = write!(
        s,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<style>\n{STYLE}\n</style>\n</head>\n<body>\n<h1>{}</h1>\n",
        esc(title),
        esc(title)
    );
    let _ = writeln!(
        s,
        "<p class=\"muted\">{} table(s), {} column(s), {} module(s).</p>",
        tables.len(),
        tables
            .values()
            .map(|t| t.columns.len() + t.computed.len())
            .sum::<usize>(),
        modules.len()
    );

    s.push_str("<h2>Diagram</h2>\n<p class=\"muted\">Mermaid source — paste it into a Markdown view that renders Mermaid (GitLab, GitHub). This file carries no scripts, so it renders identically offline.</p>\n<pre>");
    s.push_str(&esc(&erd::render(schema)));
    s.push_str("</pre>\n");

    s.push_str("<h2>Tables</h2>\n");
    for (name, table) in tables {
        one_table(&mut s, name, table);
    }

    modules_section(&mut s, modules);
    roles_section(&mut s, roles);
    deprecated_section(&mut s, schema);
    graveyard(&mut s, ids);

    s.push_str("</body>\n</html>\n");
    s
}

/// Views, procedures, functions and triggers, with their definitions — which
/// for this family of objects *is* the object (ADR-0002).
fn modules_section(s: &mut String, modules: &BTreeMap<ModuleId, Module>) {
    if modules.is_empty() {
        return;
    }
    s.push_str("<h2>Views, procedures, functions and triggers</h2>\n");
    for (name, m) in modules {
        let _ = writeln!(
            s,
            "<h3 id=\"{}\">{}</h3>",
            esc(&name.raw().to_string()),
            esc(&name.raw().to_string())
        );
        let on = name
            .attached_to()
            .map(|t| format!(" on {}", esc(&t.raw().to_string())))
            .unwrap_or_default();
        let _ = writeln!(s, "<p class=\"muted\">{}{on}</p>", m.kind);
        if let Some(d) = &m.description {
            let _ = writeln!(s, "<p>{}</p>", esc(d.raw()));
        }
        let _ = writeln!(s, "<pre>{}</pre>", esc(m.definition.trim_end()));
    }
}

/// Roles share a name namespace with neither tables nor modules, so their
/// headings carry no document id that could collide with those objects.
fn roles_section(s: &mut String, roles: &BTreeMap<String, Role>) {
    if roles.is_empty() {
        return;
    }
    s.push_str("<h2>Roles</h2>\n");
    for (name, role) in roles {
        let _ = writeln!(s, "<h3>{}</h3>", code(name));
        if let Some(description) = &role.description {
            let _ = writeln!(s, "<p>{}</p>", esc(description.raw()));
        }
        let grants: Vec<_> = role.declarable_grants().collect();
        if grants.is_empty() {
            s.push_str("<p class=\"muted\">No grants.</p>\n");
            continue;
        }
        s.push_str("<table>\n<tr><th>Granted on</th><th>Permissions</th></tr>\n");
        for (target, permissions) in grants {
            let list: Vec<_> = permissions.iter().map(|p| p.as_str()).collect();
            let _ = writeln!(
                s,
                "<tr><td>{}</td><td>{}</td></tr>",
                code(&target.to_string()),
                esc(&list.join(", "))
            );
        }
        s.push_str("</table>\n");
    }
}

fn one_table(s: &mut String, name: &TableName, table: &Table) {
    let _ = writeln!(
        s,
        "<h3 id=\"{}\">{}</h3>",
        esc(&name.raw().to_string()),
        code(&name.raw().to_string())
    );
    if let Some(d) = &table.description {
        let _ = writeln!(s, "<p>{}</p>", esc(d.raw()));
    }
    s.push_str("<table>\n<tr><th>Column</th><th>Type</th><th>Null</th><th>Default</th><th>Description</th></tr>\n");
    for (col_name, c) in &table.columns {
        let mut label = code(col_name);
        if c.is_deprecated() {
            label = format!("<span class=\"dep\">{label}</span>");
        }
        if table
            .primary_key
            .as_ref()
            .is_some_and(|pk| pk.columns.iter().any(|p| p == col_name))
        {
            label.push_str(" <strong>PK</strong>");
        }
        let _ = writeln!(
            s,
            "<tr><td>{label}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            code(&c.ty.to_string()),
            if c.nullable { "yes" } else { "no" },
            match &c.value_source {
                pbps_model::ValueSource::Default(text) => code(text),
                pbps_model::ValueSource::None
                | pbps_model::ValueSource::Identity(_)
                | pbps_model::ValueSource::Sequence(_)
                | pbps_model::ValueSource::Generated(_) => dash(),
            },
            c.description
                .as_ref()
                .map(pbps_model::Text::raw)
                .map(esc)
                .unwrap_or_else(dash)
        );
    }
    s.push_str("</table>\n");
    computed_table(s, table);

    let mut facts: Vec<String> = Vec::new();
    if let Some(pk) = &table.primary_key {
        facts.push(format!(
            "<strong>Primary key</strong>{}: {}",
            pk.name
                .as_deref()
                .map(|n| format!(" {}", code(n)))
                .unwrap_or_default(),
            columns(&pk.columns)
        ));
    }
    for (n, u) in &table.unique {
        facts.push(format!(
            "<strong>Unique</strong> {}: {}",
            code(n),
            columns(&u.columns)
        ));
    }
    for (n, fk) in &table.foreign_keys {
        facts.push(format!(
            "<strong>Foreign key</strong> {}: {} &rarr; {} ({})",
            code(n),
            columns(&fk.columns),
            code(&fk.references_table.raw().to_string()),
            columns(&fk.references_columns)
        ));
    }
    for (n, c) in &table.checks {
        facts.push(format!(
            "<strong>Check</strong> {}: {}",
            code(n),
            code(&c.expression)
        ));
    }
    for (n, ix) in &table.indexes {
        let cols: Vec<String> = ix
            .columns
            .iter()
            .map(|c| {
                if c.descending {
                    format!("{} desc", code(c.key.text()))
                } else {
                    code(c.key.text())
                }
            })
            .collect();
        facts.push(format!(
            "<strong>Index</strong> {}{}: {}",
            code(n),
            if ix.unique { " (unique)" } else { "" },
            cols.join(", ")
        ));
    }
    if !facts.is_empty() {
        s.push_str("<ul>\n");
        for f in facts {
            let _ = writeln!(s, "<li>{f}</li>");
        }
        s.push_str("</ul>\n");
    }
}

fn deprecated_section(s: &mut String, schema: &Schema) {
    let mut rows = Vec::new();
    for (name, table) in &schema.tables {
        for (col_name, c) in &table.columns {
            if let Some(reason) = &c.deprecated {
                rows.push((
                    format!("{name}.{col_name}", name = name.raw()),
                    reason.clone(),
                ));
            }
        }
    }
    if rows.is_empty() {
        return;
    }
    s.push_str("<h2>Do not use</h2>\n<p>These columns still exist but are on their way out.</p>\n<table>\n<tr><th>Column</th><th>Reason</th></tr>\n");
    for (name, reason) in rows {
        let _ = writeln!(
            s,
            "<tr><td>{}</td><td>{}</td></tr>",
            code(&name),
            esc(reason.raw())
        );
    }
    s.push_str("</table>\n");
}

fn graveyard(s: &mut String, ids: &IdsFile) {
    if ids.tombstones.is_empty() {
        return;
    }
    s.push_str("<h2>Graveyard</h2>\n<p>Dropped objects, kept so an audit can be answered.</p>\n<table>\n<tr><th>Was</th><th>Dropped</th><th>By</th><th>Reason</th></tr>\n");
    let mut stones: Vec<_> = ids.tombstones.values().collect();
    stones.sort_by(|a, b| b.dropped_at.cmp(&a.dropped_at).then(a.was.cmp(&b.was)));
    for t in stones {
        let _ = writeln!(
            s,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            code(&t.was),
            esc(&t.dropped_at),
            esc(&t.operator),
            esc(&t.reason)
        );
    }
    s.push_str("</table>\n");
}

/// SQL Server computed columns (#1174), held beside the columns. Their own
/// table, since they have no type to show (DEC-1174.1) and the expression is
/// not a default.
fn computed_table(s: &mut String, table: &Table) {
    if table.computed.is_empty() {
        return;
    }
    s.push_str("<table>\n<tr><th>Computed column</th><th>Expression</th><th>Stored</th></tr>\n");
    for (col_name, c) in &table.computed {
        let mut label = code(col_name);
        if table
            .primary_key
            .as_ref()
            .is_some_and(|pk| pk.columns.iter().any(|p| p == col_name))
        {
            label.push_str(" <strong>PK</strong>");
        }
        let _ = writeln!(
            s,
            "<tr><td>{label}</td><td>{}</td><td>{}</td></tr>",
            code(&c.expression),
            crate::markdown::computed_flags(c)
                .map(|f| esc(&f))
                .unwrap_or_else(dash)
        );
    }
    s.push_str("</table>\n");
}

fn columns(cols: &[String]) -> String {
    cols.iter().map(|c| code(c)).collect::<Vec<_>>().join(", ")
}

fn dash() -> String {
    "&mdash;".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, ColumnType, ComputedColumn, Index, IndexColumn};

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    #[test]
    fn roles_keep_every_grant_kind_and_empty_roles_without_duplicate_ids() {
        use pbps_model::Permission;
        let mut schema = schema_with_description("Customers");
        schema.roles.insert(
            "dbo.t".into(),
            Role {
                description: Some("Read-only reporting".into()),
                grants: [
                    (
                        "dbo.t".parse().unwrap(),
                        [Permission::Select, Permission::References].into(),
                    ),
                    (
                        "schema::public".parse().unwrap(),
                        [Permission::Usage].into(),
                    ),
                    (
                        "public.report(integer)".parse().unwrap(),
                        [Permission::Execute].into(),
                    ),
                ]
                .into(),
            },
        );
        schema
            .roles
            .insert("pending_access".into(), Role::default());
        let out = render(&schema, &IdsFile::default(), "Schema");
        for expected in [
            "<h2>Roles</h2>",
            "<h3><code>dbo.t</code></h3>",
            "<p>Read-only reporting</p>",
            "<tr><td><code>dbo.t</code></td><td>select, references</td></tr>",
            "<tr><td><code>schema::public</code></td><td>usage</td></tr>",
            "<tr><td><code>public.report(integer)</code></td><td>execute</td></tr>",
            "<h3><code>pending_access</code></h3>\n<p class=\"muted\">No grants.</p>",
        ] {
            assert!(out.contains(expected), "missing {expected}: {out}");
        }
        assert_eq!(out.matches("id=\"dbo.t\"").count(), 1);
        assert_eq!(out, render(&schema, &IdsFile::default(), "Schema"));
    }

    #[test]
    fn role_text_is_escaped_and_the_page_remains_self_contained() {
        use pbps_model::{GrantTarget, Permission};
        let name = "報表<&>\"'";
        let mut schema = Schema::default();
        schema.roles.insert(
            name.into(),
            Role {
                description: Some("<script>\"R&D's\"</script>".into()),
                grants: [(GrantTarget::Schema(name.into()), [Permission::Usage].into())].into(),
            },
        );
        let out = render(&schema, &IdsFile::default(), "Schema");
        assert!(
            out.contains("<h3><code>報表&lt;&amp;&gt;&quot;&#39;</code></h3>"),
            "{out}"
        );
        assert!(
            out.contains("<p>&lt;script&gt;&quot;R&amp;D&#39;s&quot;&lt;/script&gt;</p>"),
            "{out}"
        );
        assert!(
            out.contains("<code>schema::報表&lt;&amp;&gt;&quot;&#39;</code>"),
            "{out}"
        );
        for forbidden in ["http://", "https://", "<script", "<link", "src="] {
            assert!(!out.contains(forbidden), "found {forbidden}: {out}");
        }
        assert!(out.contains(&format!("<style>{}</style>", style_contents())));
    }

    #[test]
    fn no_roles_means_no_roles_heading_or_grants_table() {
        let out = render(&Schema::default(), &IdsFile::default(), "Schema");
        for absent in ["<h2>Roles</h2>", "Granted on", "No grants."] {
            assert!(!out.contains(absent), "{out}");
        }
    }

    fn schema_with_description(d: &str) -> Schema {
        let mut t = Table {
            description: Some(d.into()),
            ..Default::default()
        };
        t.columns.insert("id".into(), Column::new(ty("int")));
        let mut s = Schema::default();
        s.tables.insert(TableName::new("dbo", "t"), t);
        s
    }

    /// The air-gap rule (SPEC §11.3) applies to artifacts: a page that needs the
    /// network to look right is not self-contained.
    #[test]
    fn the_page_references_nothing_external() {
        let out = render(&schema_with_description("x"), &IdsFile::default(), "Schema");
        for forbidden in ["http://", "https://", "<script", "<link", "src="] {
            assert!(
                !out.contains(forbidden),
                "found `{forbidden}` in the output"
            );
        }
        assert!(out.contains("<style>"), "the CSS must be inlined");
    }

    /// A description is user text; it must not be able to inject markup.
    #[test]
    fn user_text_cannot_inject_markup() {
        let out = render(
            &schema_with_description("<script>alert(1)</script>"),
            &IdsFile::default(),
            "Schema",
        );
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("&lt;script&gt;"), "{out}");
    }

    #[test]
    fn the_erd_travels_as_escaped_source() {
        let out = render(&schema_with_description("x"), &IdsFile::default(), "Schema");
        assert!(out.contains("erDiagram"), "{out}");
        assert!(out.contains("<pre>"), "{out}");
    }

    /// The model holds SQL Server computed columns beside the columns
    /// (#1174); the page must show them, since an index can name one.
    #[test]
    fn computed_columns_are_rendered_beside_the_columns() {
        let mut s = schema_with_description("x");
        let t = s.tables.get_mut(&TableName::new("dbo", "t")).unwrap();
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
                expression: "[a] & 4".into(),
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
        let out = render(&s, &IdsFile::default(), "Schema");
        assert!(
            out.contains(
                "<tr><td><code>total</code></td><td><code>[qty] * [price]</code></td>\
                 <td>persisted, not null</td></tr>"
            ),
            "{out}"
        );
        // Computed on read: no flags, and the expression is escaped.
        assert!(
            out.contains(
                "<tr><td><code>flags</code></td><td><code>[a] &amp; 4</code></td>\
                 <td>&mdash;</td></tr>"
            ),
            "{out}"
        );
        assert!(
            out.contains("<strong>Index</strong> <code>ix_total</code>: <code>total</code>"),
            "{out}"
        );
        assert!(out.contains("3 column(s)"), "{out}");
    }

    #[test]
    fn a_table_without_computed_columns_has_no_computed_table() {
        let out = render(&schema_with_description("x"), &IdsFile::default(), "Schema");
        assert!(!out.contains("Computed column"), "{out}");
        assert!(out.contains("1 column(s)"), "{out}");
    }

    #[test]
    fn rendering_is_deterministic() {
        let (s, ids) = (schema_with_description("x"), IdsFile::default());
        let first = render(&s, &ids, "Schema");
        for _ in 0..10 {
            assert_eq!(render(&s, &ids, "Schema"), first);
        }
    }
}
