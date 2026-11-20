//! What the engine's pragmas say about a table, and the
//! comparison with what the tokenizer read from the stored text.
//!
//! `agree` is the O1 oracle: both directions, on everything both sources can
//! say. Nothing the tokenizer read may differ from the pragmas, and nothing
//! the pragmas report may be unaccounted for by the text. The reader that uses
//! the tokenizer runs the same comparison on every table and does not read one
//! on which it fails; it lives here until that reader exists.

use pbps_db::sqlite::{Connection, Record, Value};

use super::super::{Constraint, Kind, Marked, Marker, Place, Reaction, TableText};
use crate::ident;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in super::super) struct Col {
    pub name: String,
    pub ty: String,
    pub not_null: bool,
    pub default: Option<String>,
    pub pk: i64,
    pub hidden: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in super::super) struct ForeignRow {
    pub id: i64,
    pub seq: i64,
    pub table: String,
    pub from: String,
    pub to: Option<String>,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in super::super) struct IndexRow {
    pub name: String,
    pub unique: bool,
    pub origin: String,
    pub partial: bool,
    /// The key columns (`key = 1`), by position: name (None for an
    /// expression), descending, collation.
    pub keys: Vec<(Option<String>, bool, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in super::super) struct Facts {
    pub columns: Vec<Col>,
    pub indexes: Vec<IndexRow>,
    pub foreign: Vec<ForeignRow>,
    pub without_rowid: bool,
    pub strict: bool,
}

fn cell(row: &Record, i: usize) -> Option<String> {
    row.text(i)
        .map(str::to_owned)
        .or_else(|_| row.integer(i).map(|n| n.to_string()))
        .ok()
}

fn int(row: &Record, i: usize) -> i64 {
    row.integer(i).expect("an integer")
}

fn text(row: &Record, i: usize) -> String {
    row.text(i).expect("text").to_owned()
}

pub(in super::super) fn facts(c: &Connection, table: &str) -> Facts {
    let arg = [Value::Text(table.to_owned())];
    let columns = c
        .query(
            "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1) ORDER BY cid",
            &arg,
        )
        .expect("table_xinfo")
        .iter()
        .map(|r| Col {
            name: text(r, 0),
            ty: text(r, 1),
            not_null: int(r, 2) == 1,
            default: cell(r, 3),
            pk: int(r, 4),
            hidden: int(r, 5),
        })
        .collect();
    let mut indexes = Vec::new();
    for r in c
        .query(
            "SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1) ORDER BY name",
            &arg,
        )
        .expect("index_list")
    {
        let name = text(&r, 0);
        let keys = c
            .query(
                "SELECT name, \"desc\", coll FROM pragma_index_xinfo(?1) WHERE key = 1 ORDER BY seqno",
                &[Value::Text(name.clone())],
            )
            .expect("index_xinfo")
            .iter()
            .map(|k| (cell(k, 0), int(k, 1) == 1, text(k, 2)))
            .collect();
        indexes.push(IndexRow {
            name,
            unique: int(&r, 1) == 1,
            origin: text(&r, 2),
            partial: int(&r, 3) == 1,
            keys,
        });
    }
    let foreign = c
        .query(
            "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete FROM pragma_foreign_key_list(?1) ORDER BY id DESC, seq",
            &arg,
        )
        .expect("foreign_key_list")
        .iter()
        .map(|r| ForeignRow {
            id: int(r, 0),
            seq: int(r, 1),
            table: text(r, 2),
            from: text(r, 3),
            to: cell(r, 4),
            on_update: text(r, 5),
            on_delete: text(r, 6),
        })
        .collect();
    let flags = c
        .query(
            "SELECT wr, strict FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
            &arg,
        )
        .expect("table_list");
    let without_rowid = flags.first().is_some_and(|r| int(r, 0) == 1);
    let strict = flags.first().is_some_and(|r| int(r, 1) == 1);
    Facts {
        columns,
        indexes,
        foreign,
        without_rowid,
        strict,
    }
}

/// The stored text and name of every object of `kind` that has text, by name.
pub(in super::super) fn stored(c: &Connection, kind: &str) -> Vec<(String, String)> {
    c.query(
        "SELECT name, sql FROM sqlite_schema WHERE type = ?1 AND sql IS NOT NULL ORDER BY name",
        &[Value::Text(kind.to_owned())],
    )
    .expect("sqlite_schema")
    .iter()
    .map(|r| (text(r, 0), text(r, 1)))
    .collect()
}

fn action_word(action: Option<Reaction>) -> &'static str {
    match action {
        None | Some(Reaction::NoAction) => "NO ACTION",
        Some(Reaction::SetNull) => "SET NULL",
        Some(Reaction::SetDefault) => "SET DEFAULT",
        Some(Reaction::Cascade) => "CASCADE",
        Some(Reaction::Restrict) => "RESTRICT",
    }
}

/// A default's text as the pragma gives it: one outer pair of parentheses and
/// the white space at the edges removed.
fn as_pragma(default: &str) -> String {
    let trimmed = default.trim_matches(crate::lexis::is_space);
    match trimmed
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
    {
        Some(inner) => inner.trim_matches(crate::lexis::is_space).to_owned(),
        None => trimmed.to_owned(),
    }
}

/// The columns of the keys the text declares, in text order.
fn keys_of(text: &TableText, primary: bool) -> Vec<&Vec<String>> {
    text.constraints
        .iter()
        .filter_map(|c| match &c.kind {
            Kind::PrimaryKey { columns } if primary => Some(columns),
            Kind::Unique { columns } if !primary => Some(columns),
            Kind::PrimaryKey { .. }
            | Kind::Unique { .. }
            | Kind::Check { .. }
            | Kind::ForeignKey { .. }
            | Kind::NotNull
            | Kind::Null
            | Kind::Default { .. } => None,
        })
        .collect()
}

fn markers_on(text: &TableText, target: super::super::Target) -> Vec<&Marker> {
    text.markers
        .iter()
        .filter(|m: &&Marked| m.target == target)
        .map(|m| &m.marker)
        .collect()
}

/// Whether the tokenizer's reading of a table's text and the engine's pragmas
/// say the same thing wherever both can say it.
pub(in super::super) fn agree(text: &TableText, facts: &Facts) -> Result<(), String> {
    // 1. The columns, in order.
    let names: Vec<&str> = facts.columns.iter().map(|c| c.name.as_str()).collect();
    let written: Vec<&str> = text.columns.iter().map(String::as_str).collect();
    if names != written {
        return Err(format!(
            "columns: the text has {written:?}, the engine {names:?}"
        ));
    }
    // 2. The primary key: its columns, in key order, are the `pk` ordinals.
    let mut ordinals: Vec<(i64, &str)> = facts
        .columns
        .iter()
        .filter(|c| c.pk > 0)
        .map(|c| (c.pk, c.name.as_str()))
        .collect();
    ordinals.sort_unstable();
    let from_engine: Vec<&str> = ordinals.iter().map(|(_, n)| *n).collect();
    let primary = keys_of(text, true);
    if primary.len() > 1 {
        return Err("two primary keys in the text".into());
    }
    let from_text: Vec<&str> = primary
        .first()
        .map(|k| k.iter().map(String::as_str).collect())
        .unwrap_or_default();
    if from_text != from_engine {
        return Err(format!(
            "primary key: the text has {from_text:?}, the engine {from_engine:?}"
        ));
    }
    // 3. The unique constraints. Identical ones, and one equal to the primary
    // key, share an index, so a text constraint is accounted for by an index
    // or by an equal key; an index is accounted for by a constraint.
    let engine_unique: Vec<Vec<&str>> = facts
        .indexes
        .iter()
        .filter(|i| i.origin == "u")
        .map(|i| i.keys.iter().filter_map(|k| k.0.as_deref()).collect())
        .collect();
    let text_unique: Vec<Vec<&str>> = keys_of(text, false)
        .into_iter()
        .map(|k| k.iter().map(String::as_str).collect())
        .collect();
    for index in &engine_unique {
        if !text_unique.contains(index) {
            return Err(format!(
                "a unique index over {index:?} that no constraint of the text explains"
            ));
        }
    }
    for unique in &text_unique {
        if !engine_unique.contains(unique) && *unique != from_engine {
            return Err(format!(
                "the text declares UNIQUE {unique:?}, which the engine holds no index for"
            ));
        }
    }
    // 4. The foreign keys, in text order against the ids in reverse.
    let text_fks: Vec<&Constraint> = text
        .constraints
        .iter()
        .filter(|c| matches!(c.kind, Kind::ForeignKey { .. }))
        .collect();
    let mut groups: Vec<Vec<&ForeignRow>> = Vec::new();
    for row in &facts.foreign {
        match groups.last_mut() {
            Some(group) if group[0].id == row.id => group.push(row),
            _ => groups.push(vec![row]),
        }
    }
    if groups.len() != text_fks.len() {
        return Err(format!(
            "{} foreign keys in the text, {} in the engine",
            text_fks.len(),
            groups.len()
        ));
    }
    for (clause, rows) in text_fks.iter().zip(&groups) {
        let Kind::ForeignKey {
            columns,
            parent,
            parent_columns,
            on_delete,
            on_update,
        } = &clause.kind
        else {
            continue;
        };
        let engine_from: Vec<&str> = rows.iter().map(|r| r.from.as_str()).collect();
        let text_from: Vec<&str> = columns.iter().map(String::as_str).collect();
        if engine_from != text_from {
            return Err(format!(
                "a foreign key over {text_from:?} is {engine_from:?} in the engine"
            ));
        }
        if !ident::same(&rows[0].table, parent) {
            return Err(format!(
                "a foreign key to `{parent}` is to `{}` in the engine",
                rows[0].table
            ));
        }
        let engine_to: Vec<Option<&str>> = rows.iter().map(|r| r.to.as_deref()).collect();
        let text_to: Vec<Option<&str>> = if parent_columns.is_empty() {
            vec![None; rows.len()]
        } else {
            parent_columns.iter().map(|c| Some(c.as_str())).collect()
        };
        if engine_to != text_to {
            return Err(format!(
                "a foreign key to {text_to:?} is to {engine_to:?} in the engine"
            ));
        }
        if rows[0].on_delete != action_word(*on_delete)
            || rows[0].on_update != action_word(*on_update)
        {
            return Err(format!(
                "foreign key actions: the text has {}/{}, the engine {}/{}",
                action_word(*on_delete),
                action_word(*on_update),
                rows[0].on_delete,
                rows[0].on_update
            ));
        }
    }
    // 5, 6 and 7. A column's default, NOT NULL and generated flag.
    for (at, col) in facts.columns.iter().enumerate() {
        let place = Place::Column(at);
        let defaults: Vec<&str> = text
            .constraints
            .iter()
            .filter(|c| c.place == place)
            .filter_map(|c| match &c.kind {
                Kind::Default { text } => Some(text.as_str()),
                Kind::PrimaryKey { .. }
                | Kind::Unique { .. }
                | Kind::Check { .. }
                | Kind::ForeignKey { .. }
                | Kind::NotNull
                | Kind::Null => None,
            })
            .collect();
        let generated = markers_on(text, super::super::Target::Column(at))
            .into_iter()
            .any(|m| matches!(m, Marker::Generated { .. }));
        let expected = defaults.last().map(|d| as_pragma(d));
        if !generated && expected != col.default {
            return Err(format!(
                "column `{}`: the default is {expected:?} in the text, {:?} in the engine",
                col.name, col.default
            ));
        }
        let not_null = text
            .constraints
            .iter()
            .any(|c| c.place == place && matches!(c.kind, Kind::NotNull));
        let implied = (facts.without_rowid || facts.strict) && col.pk > 0;
        if !implied && not_null != col.not_null {
            return Err(format!(
                "column `{}`: NOT NULL is {not_null} in the text, {} in the engine",
                col.name, col.not_null
            ));
        }
        if generated != (col.hidden == 2 || col.hidden == 3) {
            return Err(format!(
                "column `{}`: generated is {generated} in the text, hidden is {} in the engine",
                col.name, col.hidden
            ));
        }
    }
    Ok(())
}

/// A name written back as one quoted name.
fn q(name: &str) -> String {
    ["\"", &name.replace('"', "\"\""), "\""].concat()
}

/// `CREATE TABLE` rebuilt from the pieces the tokenizer read and the types
/// the pragmas gave, in a canonical layout, for a table with no marker. The
/// engine must read it as the table the original text made.
pub(in super::super) fn rebuilt(text: &TableText, facts: &Facts) -> String {
    let mut items: Vec<String> = Vec::new();
    for (at, col) in facts.columns.iter().enumerate() {
        let mut item = q(&col.name);
        if !col.ty.is_empty() {
            item.push(' ');
            // The pragma gives a quoted type without its quotes.
            let plain = col.ty.chars().all(|c| {
                ident::identifier_continues(c)
                    || matches!(c, ' ' | '(' | ')' | ',' | '+' | '-' | '.')
            });
            item.push_str(&if plain { col.ty.clone() } else { q(&col.ty) });
        }
        let place = Place::Column(at);
        for c in text.constraints.iter().filter(|c| c.place == place) {
            match &c.kind {
                Kind::NotNull => item.push_str(" NOT NULL"),
                Kind::Default { text } => {
                    item.push_str(" DEFAULT ");
                    item.push_str(text);
                }
                Kind::PrimaryKey { .. }
                | Kind::Unique { .. }
                | Kind::Check { .. }
                | Kind::ForeignKey { .. }
                | Kind::Null => {}
            }
        }
        items.push(item);
    }
    for c in &text.constraints {
        let mut clause = String::new();
        if let Some(name) = &c.name {
            clause.push_str("CONSTRAINT ");
            clause.push_str(&q(name));
            clause.push(' ');
        }
        let list = |names: &[String]| {
            [
                "(",
                &names.iter().map(|n| q(n)).collect::<Vec<_>>().join(", "),
                ")",
            ]
            .concat()
        };
        match &c.kind {
            Kind::PrimaryKey { columns } => {
                clause.push_str("PRIMARY KEY ");
                clause.push_str(&list(columns));
            }
            Kind::Unique { columns } => {
                clause.push_str("UNIQUE ");
                clause.push_str(&list(columns));
            }
            Kind::Check { expression } => {
                clause.push_str("CHECK (");
                clause.push_str(expression);
                clause.push_str("\n)");
            }
            Kind::ForeignKey {
                columns,
                parent,
                parent_columns,
                on_delete,
                on_update,
            } => {
                clause.push_str("FOREIGN KEY ");
                clause.push_str(&list(columns));
                clause.push_str(" REFERENCES ");
                clause.push_str(&q(parent));
                if !parent_columns.is_empty() {
                    clause.push(' ');
                    clause.push_str(&list(parent_columns));
                }
                clause.push_str(" ON DELETE ");
                clause.push_str(action_word(*on_delete));
                clause.push_str(" ON UPDATE ");
                clause.push_str(action_word(*on_update));
            }
            Kind::NotNull | Kind::Null | Kind::Default { .. } => continue,
        }
        items.push(clause);
    }
    [
        "CREATE TABLE ",
        &q(&text.name),
        " (\n  ",
        &items.join(",\n  "),
        "\n)",
    ]
    .concat()
}
