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

use std::fmt::Write as _;

use pbps_model::{IdsFile, Schema, Table, TableName};

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
        schema.tables.len(),
        schema
            .tables
            .values()
            .map(|t| t.columns.len())
            .sum::<usize>(),
        schema.modules.len()
    );

    s.push_str("<h2>Diagram</h2>\n<p class=\"muted\">Mermaid source — paste it into a Markdown view that renders Mermaid (GitLab, GitHub). This file carries no scripts, so it renders identically offline.</p>\n<pre>");
    s.push_str(&esc(&erd::render(schema)));
    s.push_str("</pre>\n");

    s.push_str("<h2>Tables</h2>\n");
    for (name, table) in &schema.tables {
        one_table(&mut s, name, table);
    }

    modules_section(&mut s, schema);
    deprecated_section(&mut s, schema);
    graveyard(&mut s, ids);

    s.push_str("</body>\n</html>\n");
    s
}

/// Views, procedures, functions and triggers, with their definitions — which
/// for this family of objects *is* the object (ADR-0002).
fn modules_section(s: &mut String, schema: &Schema) {
    if schema.modules.is_empty() {
        return;
    }
    s.push_str("<h2>Views, procedures, functions and triggers</h2>\n");
    for (name, m) in &schema.modules {
        let _ = writeln!(
            s,
            "<h3 id=\"{}\">{}</h3>",
            esc(&name.to_string()),
            esc(&name.to_string())
        );
        let on = name
            .attached_to()
            .map(|t| format!(" on {}", esc(&t.to_string())))
            .unwrap_or_default();
        let _ = writeln!(s, "<p class=\"muted\">{}{on}</p>", m.kind);
        if let Some(d) = &m.description {
            let _ = writeln!(s, "<p>{}</p>", esc(d));
        }
        let _ = writeln!(s, "<pre>{}</pre>", esc(m.definition.trim_end()));
    }
}

fn one_table(s: &mut String, name: &TableName, table: &Table) {
    let _ = writeln!(
        s,
        "<h3 id=\"{}\">{}</h3>",
        esc(&name.to_string()),
        code(&name.to_string())
    );
    if let Some(d) = &table.description {
        let _ = writeln!(s, "<p>{}</p>", esc(d));
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
            c.default.as_deref().map(code).unwrap_or_else(dash),
            c.description.as_deref().map(esc).unwrap_or_else(dash)
        );
    }
    s.push_str("</table>\n");

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
            code(&fk.references_table.to_string()),
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
                    format!("{} desc", code(&c.name))
                } else {
                    code(&c.name)
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
                rows.push((format!("{name}.{col_name}"), reason.clone()));
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
            esc(&reason)
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

fn columns(cols: &[String]) -> String {
    cols.iter().map(|c| code(c)).collect::<Vec<_>>().join(", ")
}

fn dash() -> String {
    "&mdash;".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, ColumnType};

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
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

    #[test]
    fn rendering_is_deterministic() {
        let (s, ids) = (schema_with_description("x"), IdsFile::default());
        let first = render(&s, &ids, "Schema");
        for _ in 0..10 {
            assert_eq!(render(&s, &ids, "Schema"), first);
        }
    }
}
