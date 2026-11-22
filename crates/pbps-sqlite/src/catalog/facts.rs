//! What the pragmas say about one table, and the comparison with its text
//! (#2950, item 4).
//!
//! Every fact a pragma reports is read from the pragma. [`agree`] then checks it
//! against what the tokenizer read from the stored `CREATE` text, in both
//! directions, on everything both can say: nothing the text says may differ
//! from the pragmas, and nothing the pragmas report may be unaccounted for by
//! the text. A table on which they disagree is not read (the text was not
//! understood), never read from one side.
//!
//! A NULL is read as absent and a value of another type as an error, so no
//! unreadable value can read as "nothing there". The pragmas are asked as
//! statements ([`crate::pragma`]), which no table or view of the database can
//! answer in place of.

use std::cmp::Reverse;

use pbps_db::DbError;
use pbps_db::sqlite::{Connection, Record};

use super::text::{Constraint, IndexText, Kind, Marker, Place, Reaction, TableText, Target};
use crate::ident;
use crate::pragma::{self, foreign_key_list, index_list, index_xinfo, table_list, table_xinfo};

/// One column, as `PRAGMA table_xinfo` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Col {
    pub name: String,
    /// The declared type as stored; empty for a column with no type.
    pub ty: String,
    pub not_null: bool,
    pub default: Option<String>,
    /// The position in the primary key, 1-based, or 0.
    pub pk: i64,
    /// 2 virtual and 3 stored generated; 1 hidden to virtual tables.
    pub hidden: i64,
}

/// One column of one foreign key, as `PRAGMA foreign_key_list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForeignRow {
    pub id: i64,
    pub seq: i64,
    /// The parent as the clause spells it.
    pub table: String,
    pub from: String,
    /// `None` where the clause names no parent column.
    pub to: Option<String>,
    pub on_update: String,
    pub on_delete: String,
}

/// One index of a table, with its key columns (`key = 1`) by position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexRow {
    pub name: String,
    pub unique: bool,
    /// `c` an index made with `CREATE INDEX`, `u` a `UNIQUE` constraint's,
    /// `pk` a primary key's.
    pub origin: String,
    pub partial: bool,
    /// Name (`None` for an expression), descending, collation.
    pub keys: Vec<(Option<String>, bool, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Facts {
    pub columns: Vec<Col>,
    pub indexes: Vec<IndexRow>,
    /// Ordered by `id` descending, which is the text's order, then `seq`.
    pub foreign: Vec<ForeignRow>,
    pub without_rowid: bool,
    pub strict: bool,
}

fn int(row: &Record, i: usize) -> Result<i64, DbError> {
    row.integer(i)
}

fn text(row: &Record, i: usize) -> Result<String, DbError> {
    row.text(i).map(str::to_owned)
}

/// A nullable text column: NULL is `None`, and a value of another type an error.
fn nullable(row: &Record, i: usize) -> Result<Option<String>, DbError> {
    Ok(row.optional_text(i)?.map(str::to_owned))
}

/// Reads the pragmas for one table of `main`. `None` is a table that is not
/// there, which is not a table with no columns, no keys and no flags: it is
/// asked first, by the name as SQLite compares names (ASCII case folded), so a
/// table found under another spelling of its name keeps its flags.
///
/// The orders are the ones the comparison with the text relies on: columns by
/// `cid`, indexes by name, an index's keys by `seqno`, and foreign keys by `id`
/// descending and then `seq`.
pub(crate) fn read(c: &Connection, table: &str) -> Result<Option<Facts>, DbError> {
    let flags = pragma::table(c, table)?;
    let Some(row) = flags.first() else {
        return Ok(None);
    };
    let (without_rowid, strict) = (
        int(row, table_list::WITHOUT_ROWID)? == 1,
        int(row, table_list::STRICT)? == 1,
    );
    let mut columns = Vec::new();
    for r in pragma::sorted(pragma::columns(c, table)?, |r| int(r, table_xinfo::CID))? {
        columns.push(Col {
            name: text(&r, table_xinfo::NAME)?,
            ty: text(&r, table_xinfo::TYPE)?,
            not_null: int(&r, table_xinfo::NOT_NULL)? == 1,
            default: nullable(&r, table_xinfo::DEFAULT)?,
            pk: int(&r, table_xinfo::PK)?,
            hidden: int(&r, table_xinfo::HIDDEN)?,
        });
    }
    let mut indexes = Vec::new();
    for r in pragma::by_name(pragma::indexes(c, table)?, index_list::NAME)? {
        let name = text(&r, index_list::NAME)?;
        let mut keys = Vec::new();
        for k in pragma::sorted(pragma::index_columns(c, &name)?, |k| {
            int(k, index_xinfo::SEQNO)
        })? {
            if int(&k, index_xinfo::KEY)? == 1 {
                keys.push((
                    nullable(&k, index_xinfo::NAME)?,
                    int(&k, index_xinfo::DESC)? == 1,
                    text(&k, index_xinfo::COLLATION)?,
                ));
            }
        }
        indexes.push(IndexRow {
            name,
            unique: int(&r, index_list::UNIQUE)? == 1,
            origin: text(&r, index_list::ORIGIN)?,
            partial: int(&r, index_list::PARTIAL)? == 1,
            keys,
        });
    }
    let mut foreign = Vec::new();
    for r in pragma::sorted(pragma::foreign_keys(c, table)?, |r| {
        Ok((
            Reverse(int(r, foreign_key_list::ID)?),
            int(r, foreign_key_list::SEQ)?,
        ))
    })? {
        foreign.push(ForeignRow {
            id: int(&r, foreign_key_list::ID)?,
            seq: int(&r, foreign_key_list::SEQ)?,
            table: text(&r, foreign_key_list::TABLE)?,
            from: text(&r, foreign_key_list::FROM)?,
            to: nullable(&r, foreign_key_list::TO)?,
            on_update: text(&r, foreign_key_list::ON_UPDATE)?,
            on_delete: text(&r, foreign_key_list::ON_DELETE)?,
        });
    }
    Ok(Some(Facts {
        columns,
        indexes,
        foreign,
        without_rowid,
        strict,
    }))
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

/// Whether two lists of column names are the same names (SQLite folds ASCII
/// case, and a clause may spell a name another way than the column does).
fn same_names<A: AsRef<str>, B: AsRef<str>>(a: &[A], b: &[B]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| ident::same(x.as_ref(), y.as_ref()))
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

/// Whether the tokenizer's reading of a table's text and the engine's pragmas
/// say the same thing wherever both can say it. The error says what differs.
pub(crate) fn agree(text: &TableText, facts: &Facts) -> Result<(), String> {
    // 1. The columns, in order.
    let names: Vec<&str> = facts.columns.iter().map(|c| c.name.as_str()).collect();
    if !same_names(&text.columns, &names) {
        return Err(format!(
            "columns: the text has {:?}, the engine {names:?}",
            text.columns
        ));
    }
    // The type: the pragma says it, and says nothing for a column with no
    // type and for one declared with a quoted empty name alike. The text tells
    // them apart, so one with type words and an empty type is not understood.
    for (at, col) in facts.columns.iter().enumerate() {
        let words = text.typed.get(at).copied().unwrap_or(false);
        if words == col.ty.is_empty() {
            return Err(format!(
                "column `{}`: the text has {} type words and the engine reports the type `{}`",
                col.name,
                if words { "some" } else { "no" },
                col.ty
            ));
        }
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
    let from_text: &[String] = primary.first().map_or(&[], |k| k.as_slice());
    if !same_names(from_text, &from_engine) {
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
    let text_unique = keys_of(text, false);
    for index in &engine_unique {
        if !text_unique.iter().any(|t| same_names(t, index)) {
            return Err(format!(
                "a unique index over {index:?} that no constraint of the text explains"
            ));
        }
    }
    for unique in &text_unique {
        if !engine_unique.iter().any(|e| same_names(unique, e)) && !same_names(unique, &from_engine)
        {
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
            Some(group) if group.first().is_some_and(|g| g.id == row.id) => group.push(row),
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
        let Some(first) = rows.first() else { continue };
        let engine_from: Vec<&str> = rows.iter().map(|r| r.from.as_str()).collect();
        if !same_names(columns, &engine_from) {
            return Err(format!(
                "a foreign key over {columns:?} is {engine_from:?} in the engine"
            ));
        }
        if !ident::same(&first.table, parent) {
            return Err(format!(
                "a foreign key to `{parent}` is to `{}` in the engine",
                first.table
            ));
        }
        let engine_to: Vec<Option<&str>> = rows.iter().map(|r| r.to.as_deref()).collect();
        let agrees = if parent_columns.is_empty() {
            engine_to.iter().all(Option::is_none)
        } else {
            engine_to.len() == parent_columns.len()
                && engine_to
                    .iter()
                    .zip(parent_columns)
                    .all(|(e, t)| e.is_some_and(|e| ident::same(e, t)))
        };
        if !agrees {
            return Err(format!(
                "a foreign key to {parent_columns:?} is to {engine_to:?} in the engine"
            ));
        }
        if first.on_delete != action_word(*on_delete) || first.on_update != action_word(*on_update)
        {
            return Err(format!(
                "foreign key actions: the text has {}/{}, the engine {}/{}",
                action_word(*on_delete),
                action_word(*on_update),
                first.on_delete,
                first.on_update
            ));
        }
    }
    // 5, 6 and 7. A column's default, NOT NULL and generated flag.
    for (at, col) in facts.columns.iter().enumerate() {
        let place = Place::Column(at);
        let generated = text.markers.iter().any(|m| {
            m.target == Target::Column(at) && matches!(m.marker, Marker::Generated { .. })
        });
        let expected = text
            .constraints
            .iter()
            .filter(|c| c.place == place)
            .filter_map(|c| match &c.kind {
                Kind::Default { text } => Some(as_pragma(text)),
                Kind::PrimaryKey { .. }
                | Kind::Unique { .. }
                | Kind::Check { .. }
                | Kind::ForeignKey { .. }
                | Kind::NotNull
                | Kind::Null => None,
            })
            .next_back();
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
        // A key column of a `STRICT` or `WITHOUT ROWID` table is NOT NULL
        // whatever the text says.
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

/// The same for an index's text and the engine's row for it.
pub(crate) fn agree_index(text: &IndexText, row: &IndexRow, table: &str) -> Result<(), String> {
    if text.name != row.name {
        return Err(format!(
            "the index is `{}` in the text and `{}` in the engine",
            text.name, row.name
        ));
    }
    if !ident::same(&text.table, table) {
        return Err(format!(
            "the index is on `{}` in the text and `{table}` in the engine",
            text.table
        ));
    }
    if text.unique != row.unique
        || text.filter.is_some() != row.partial
        || text.entries != row.keys.len()
    {
        return Err(format!(
            "the index flags or entry count differ: {text:?} against {row:?}"
        ));
    }
    Ok(())
}
