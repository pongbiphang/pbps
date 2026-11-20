//! What the engine's pragmas say about a table, and the comparison with what
//! the tokenizer read from the stored text: the production comparison
//! (`catalog::facts`) re-exported for the oracles, and the helpers only the
//! tests need.

use pbps_db::sqlite::{Connection, Value};

use crate::ident;

use super::super::{Kind, Place, Reaction, TableText, ViewText};
use crate::catalog::facts::read;
pub(in super::super) use crate::catalog::facts::{Facts, agree, agree_index};

pub(in super::super) fn facts(c: &Connection, table: &str) -> Facts {
    read(c, table).expect("the pragmas").expect("the table")
}

fn int(row: &pbps_db::sqlite::Record, i: usize) -> i64 {
    row.integer(i).expect("an integer")
}

fn text(row: &pbps_db::sqlite::Record, i: usize) -> String {
    row.text(i).expect("text").to_owned()
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

/// The same for a view: a column list agrees with the columns the engine finds.
pub(in super::super) fn agree_view(text: &ViewText, c: &Connection) -> Result<(), String> {
    // A view that names what is not there has no columns to count: the engine
    // creates it all the same, and there is nothing to compare.
    let Some(list) = &text.column_list else {
        return Ok(());
    };
    let Ok(columns) = c.query(
        "SELECT count(*) FROM pragma_table_xinfo(?1)",
        &[Value::Text(text.name.clone())],
    ) else {
        return Ok(());
    };
    let found = int(&columns[0], 0);
    if list.len() as i64 == found {
        Ok(())
    } else {
        Err(format!(
            "a view column list of {} names, {found} columns in the engine",
            list.len()
        ))
    }
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
