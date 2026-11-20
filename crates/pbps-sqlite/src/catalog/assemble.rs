//! From what was read to the model (#2950, item 4): pure, every decision a
//! function of the [`RawCatalog`], the same split `pbps-pg` makes.
//!
//! The rules, in the order they apply:
//!
//! 1. **A table that cannot be read whole is not read.** Its text is not
//!    understood, or disagrees with the pragmas, or a column's type cannot be
//!    read back (a column really declared `untyped`, or a type the dialect
//!    refuses), or two of its constraints share a name. It is left out, a
//!    warning and a limitation name it and why, and every foreign key of another
//!    table that names it is left out too. This is decided before anything that
//!    names a table is built, so no map describes a table that was refused.
//! 2. **What the model cannot hold is a warning and a limitation**, and the
//!    table is read around it: a column's collation, a generated column,
//!    `AUTOINCREMENT`, `STRICT`, `WITHOUT ROWID`. A constraint or an index
//!    whose meaning would change without the feature (a conflict clause, a
//!    direction or a collation on a key, `DEFERRABLE`, `MATCH`, an expression
//!    key) is left out, never read as something it is not.
//! 3. **A constraint the file leaves unnamed** is read under the stable name
//!    `__pbps_unnamed_<kind>_<digest>`, with a warning, because the model keys
//!    a check, a unique constraint and a foreign key by name. The digest is a
//!    function of what the constraint is (its kind and its normalized content),
//!    never of where it stands, so adding another constraint renames nothing.
//!    Identical ones (SQLite keeps both a repeated `CHECK` and a repeated
//!    foreign key, and one index for a repeated `UNIQUE`) take `_2`, `_3` in
//!    the order written: nothing is dropped, so a later rebuild keeps what the
//!    file had.
//! 4. **Text is returned as stored.** A rename's added quotes are not undone.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::catalog::{LimitationTarget, Pulled, UnmanagedModule};
use pbps_db::pull::record_limitation;
use pbps_model::escape::Escaped;
use pbps_model::{
    CheckConstraint, Column, ColumnName, ColumnType, ForeignKey, Index, IndexColumn, Module,
    ModuleId, ModuleKind, ObjectLocalName, ObjectName, PrimaryKey, ReferentialAction, Table,
    TableName, UniqueConstraint, ValueSource,
};

use super::facts::{Col, Facts, agree, agree_index};
use super::text::{self, Constraint, Kind, Marker, Place, Reaction, TableText, Target};
use super::{RawCatalog, RawTable};
use crate::emit::closed;
use crate::{ident, lexis, names, types};

/// A table that can be read: its text, its facts and its column types.
struct Parsed<'a> {
    raw: &'a RawTable,
    text: TableText,
    types: Vec<ColumnType>,
}

fn name_of(name: &str) -> TableName {
    TableName::new("main", name)
}

/// A warning and a limitation on a table.
fn note(pulled: &mut Pulled, table: &str, detail: String) {
    let detail = format!("table `{}`: {detail}", Escaped(table));
    record_limitation(
        &mut pulled.warnings,
        &mut pulled.limitations,
        Some(LimitationTarget::Relation(name_of(table))),
        detail,
    );
}

fn refuse(pulled: &mut Pulled, table: &str, why: &str) {
    note(pulled, table, format!("it is not read: {why}"));
}

/// The constraint's own name, if the file gave one.
fn declared_names(text: &TableText) -> impl Iterator<Item = &str> {
    text.constraints.iter().filter_map(|c| c.name.as_deref())
}

fn parse(raw: &RawTable) -> Result<Parsed<'_>, String> {
    let text = text::table(&raw.name, &raw.sql).map_err(|unread| unread.to_string())?;
    agree(&text, &raw.facts).map_err(|why| format!("its text and the catalog disagree ({why})"))?;
    let mut types = Vec::new();
    for col in &raw.facts.columns {
        types.push(
            types::read_back(&col.ty)
                .map_err(|why| format!("column `{}`: {why}", Escaped(&col.name)))?,
        );
    }
    // A name no plan can write (the empty one, or one with a NUL) is valid
    // SQLite and cannot be declared: the table is not read, rather than read
    // as something a declaration is then refused for.
    for name in std::iter::once(&raw.name)
        .chain(&text.columns)
        .map(String::as_str)
        .chain(declared_names(&text))
    {
        if ident::quote(name).is_err() {
            return Err(format!(
                "the name `{}` is one SQLite takes and no declaration can write",
                Escaped(name)
            ));
        }
    }
    let mut seen: Vec<&str> = Vec::new();
    for name in declared_names(&text) {
        // The names this tool gives an unnamed constraint are its own: a file's
        // constraint under one could not be told from them.
        if names::is_synthetic(name) {
            return Err(format!(
                "the constraint `{}` is named with `{}`, which this tool keeps for the names it gives \
                 constraints a file leaves unnamed",
                Escaped(name),
                names::UNNAMED_PREFIX
            ));
        }
        if seen.iter().any(|other| ident::same(other, name)) {
            return Err(format!(
                "two of its constraints are named `{}` (names compare with ASCII case folded), and \
                 SQLite would drop whichever comes first",
                Escaped(name)
            ));
        }
        seen.push(name);
    }
    Ok(Parsed { raw, text, types })
}

/// Where a marker is, in words.
fn place(text: &TableText, target: Target) -> String {
    match target {
        Target::Table => "the table".to_owned(),
        Target::Column(at) => format!(
            "column `{}`",
            Escaped(text.columns.get(at).map_or("?", String::as_str))
        ),
        Target::Constraint(at) => text
            .constraints
            .get(at)
            .map_or_else(|| "a constraint".to_owned(), label),
    }
}

fn label(c: &Constraint) -> String {
    let (kind, columns) = match &c.kind {
        Kind::PrimaryKey { columns } => ("primary key", columns.join(", ")),
        Kind::Unique { columns } => ("unique constraint", columns.join(", ")),
        Kind::ForeignKey {
            columns, parent, ..
        } => ("foreign key", format!("{} to {parent}", columns.join(", "))),
        Kind::Check { .. } => ("check", String::new()),
        Kind::NotNull | Kind::Null | Kind::Default { .. } => ("column constraint", String::new()),
    };
    let named = c
        .name
        .as_deref()
        .map_or_else(String::new, |n| format!(" `{}`", Escaped(n)));
    format!("the {kind}{named} ({})", Escaped(&columns))
}

/// What a marker says, and whether the thing it is on cannot be read at all
/// without it.
fn describe(marker: &Marker) -> (&'static str, bool) {
    match marker {
        Marker::Strict => ("is STRICT", false),
        Marker::WithoutRowid => ("is WITHOUT ROWID", false),
        Marker::Autoincrement => ("has AUTOINCREMENT", false),
        Marker::Collate { .. } => ("has a collation", false),
        Marker::Generated { .. } => ("is a generated column", false),
        Marker::KeyCollate { .. } => ("has a collation on a key column", true),
        Marker::KeyDirection { .. } => ("has a direction on a key column", true),
        Marker::Conflict(_) => ("has an ON CONFLICT clause", true),
        Marker::Deferrable { .. } => ("is DEFERRABLE or NOT DEFERRABLE", true),
        Marker::Initially(_) => ("has INITIALLY", true),
        Marker::Match { .. } => ("has MATCH", true),
    }
}

fn reaction(action: Option<Reaction>) -> ReferentialAction {
    match action {
        None | Some(Reaction::NoAction) => ReferentialAction::NoAction,
        Some(Reaction::SetNull) => ReferentialAction::SetNull,
        Some(Reaction::SetDefault) => ReferentialAction::SetDefault,
        Some(Reaction::Cascade) => ReferentialAction::Cascade,
        Some(Reaction::Restrict) => ReferentialAction::Restrict,
    }
}

/// A foreign key's action as the text of its digest: no clause and `NO ACTION`
/// are one constraint.
fn word(action: Option<Reaction>) -> &'static str {
    match action {
        None | Some(Reaction::NoAction) => "no action",
        Some(Reaction::SetNull) => "set null",
        Some(Reaction::SetDefault) => "set default",
        Some(Reaction::Cascade) => "cascade",
        Some(Reaction::Restrict) => "restrict",
    }
}

/// The column of `cols` a clause's name means (ASCII case folded).
fn actual<'a>(cols: &'a [Col], name: &str) -> Option<&'a str> {
    cols.iter()
        .find(|c| ident::same(&c.name, name))
        .map(|c| c.name.as_str())
}

fn actuals(cols: &[Col], names: &[String]) -> Option<Vec<ColumnName>> {
    names
        .iter()
        .map(|n| actual(cols, n).map(ColumnName::from))
        .collect()
}

/// The 64-bit FNV-1a of `text`, in 16 hex digits. The digest of an unnamed
/// constraint is part of the name a declaration will carry, so this function
/// does not change (a test pins its values).
fn digest(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Names as the table spells them, joined: a clause's case changes nothing.
fn spelled(cols: &[Col], names: &[String]) -> String {
    names
        .iter()
        .map(|n| actual(cols, n).unwrap_or(n.as_str()))
        .collect::<Vec<_>>()
        .join("\u{1}")
}

/// The stable names of a table's unnamed constraints, by constraint position.
fn synthetic(p: &Parsed<'_>, by_name: &BTreeMap<&str, &Parsed<'_>>) -> BTreeMap<usize, String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for (at, c) in p.text.constraints.iter().enumerate() {
        if c.name.is_some() {
            continue;
        }
        let own = &p.raw.facts.columns;
        let (kind, content) = match &c.kind {
            Kind::Unique { columns } => ("unique", spelled(own, columns)),
            Kind::Check { expression } => ("check", lexis::canonical(expression)),
            Kind::ForeignKey {
                columns,
                parent,
                parent_columns,
                on_delete,
                on_update,
            } => {
                let target = by_name.values().find(|t| ident::same(&t.raw.name, parent));
                let theirs = target.map_or(&[][..], |t| t.raw.facts.columns.as_slice());
                let parent_name = target.map_or(parent.as_str(), |t| t.raw.name.as_str());
                (
                    "foreign",
                    [
                        spelled(own, columns),
                        parent_name.to_owned(),
                        spelled(theirs, parent_columns),
                        format!("{}/{}", word(*on_delete), word(*on_update)),
                    ]
                    .join("\u{2}"),
                )
            }
            Kind::PrimaryKey { .. } | Kind::NotNull | Kind::Null | Kind::Default { .. } => continue,
        };
        let base = format!(
            "{}{kind}_{}",
            names::UNNAMED_PREFIX,
            digest(&format!("{kind}\0{content}"))
        );
        // Counted by the name, not by the content: identical constraints share
        // one, and so would two whose digests collide, which must stay two.
        let n = seen.entry(base.clone()).or_insert(0);
        *n += 1;
        let suffix = if *n > 1 {
            format!("_{n}")
        } else {
            String::new()
        };
        out.insert(at, format!("{base}{suffix}"));
    }
    out
}

/// A foreign key as the model holds it, or why it cannot be read.
fn foreign_key(
    p: &Parsed<'_>,
    by_name: &BTreeMap<&str, &Parsed<'_>>,
    kind: &Kind,
) -> Result<ForeignKey, String> {
    let Kind::ForeignKey {
        columns,
        parent,
        parent_columns,
        on_delete,
        on_update,
    } = kind
    else {
        return Err("not a foreign key".into());
    };
    let Some(parent_table) = by_name.values().find(|t| ident::same(&t.raw.name, parent)) else {
        return Err(format!(
            "it references `{}`, which is not read",
            Escaped(parent)
        ));
    };
    let parent_cols = &parent_table.raw.facts.columns;
    let references = if parent_columns.is_empty() {
        // The parent's primary key, in key order.
        let mut key: Vec<(i64, &str)> = parent_cols
            .iter()
            .filter(|c| c.pk > 0)
            .map(|c| (c.pk, c.name.as_str()))
            .collect();
        key.sort_unstable();
        if key.is_empty() {
            return Err(format!(
                "it references `{}` without naming columns, and that table has no primary key",
                Escaped(parent)
            ));
        }
        key.into_iter().map(|(_, n)| ColumnName::from(n)).collect()
    } else {
        actuals(parent_cols, parent_columns).ok_or_else(|| {
            format!(
                "it references a column that `{}` does not have",
                Escaped(parent)
            )
        })?
    };
    let child = actuals(&p.raw.facts.columns, columns)
        .ok_or("it names a column the table does not have")?;
    Ok(ForeignKey {
        columns: child,
        references_table: name_of(&parent_table.raw.name),
        references_columns: references,
        on_delete: reaction(*on_delete),
        on_update: reaction(*on_update),
    })
}

fn build(
    pulled: &mut Pulled,
    p: &Parsed<'_>,
    by_name: &BTreeMap<&str, &Parsed<'_>>,
    raw: &RawCatalog,
) {
    let (name, facts, text) = (&p.raw.name, &p.raw.facts, &p.text);
    let mut table = Table::default();
    for (at, (col, ty)) in facts.columns.iter().zip(&p.types).enumerate() {
        let mut column = Column::new(ty.clone());
        column.nullable = !col.not_null;
        if let Some(d) = text
            .constraints
            .iter()
            .filter(|c| c.place == Place::Column(at))
            .filter_map(|c| match &c.kind {
                Kind::Default { text } => Some(text.clone()),
                Kind::PrimaryKey { .. }
                | Kind::Unique { .. }
                | Kind::Check { .. }
                | Kind::ForeignKey { .. }
                | Kind::NotNull
                | Kind::Null => None,
            })
            .next_back()
        {
            column.value_source = ValueSource::Default(d);
        }
        table
            .columns
            .insert(ColumnName::from(col.name.as_str()), column);
    }
    // What the model cannot hold: a note for each, and a constraint it would
    // change the meaning of is left out.
    let mut left_out: BTreeSet<usize> = BTreeSet::new();
    for m in &text.markers {
        let (what, drops) = describe(&m.marker);
        let at = place(text, m.target);
        // Only what the model holds as a constraint can be left out: a NOT
        // NULL or a default with such a clause is a fact of its column, and is
        // read with the note.
        let held = |i: usize| {
            text.constraints.get(i).is_some_and(|c| {
                matches!(
                    c.kind,
                    Kind::PrimaryKey { .. }
                        | Kind::Unique { .. }
                        | Kind::Check { .. }
                        | Kind::ForeignKey { .. }
                )
            })
        };
        let outcome = match (drops, m.target) {
            (true, Target::Constraint(i)) if held(i) => {
                left_out.insert(i);
                "; it is left out"
            }
            _ => "; the model has no field for it",
        };
        note(pulled, name, format!("{at} {what}{outcome}"));
    }
    let unnamed = synthetic(p, by_name);
    // The names given that are in the model: one left out is not read under it.
    let mut given: Vec<&str> = Vec::new();
    for (at, c) in text.constraints.iter().enumerate() {
        if left_out.contains(&at) {
            continue;
        }
        let key = c.name.clone().or_else(|| unnamed.get(&at).cloned());
        let local = key.as_deref().map(ObjectLocalName::new);
        let before = (
            table.unique.len(),
            table.checks.len(),
            table.foreign_keys.len(),
        );
        match (&c.kind, local) {
            (Kind::PrimaryKey { columns }, _) => {
                if let Some(columns) = actuals(&facts.columns, columns) {
                    table.primary_key = Some(PrimaryKey {
                        name: c.name.as_deref().map(ObjectLocalName::new),
                        columns,
                        storage_parameters: BTreeMap::new(),
                        filegroup: None,
                    });
                }
            }
            (Kind::Unique { columns }, Some(local)) => {
                if let Some(columns) = actuals(&facts.columns, columns) {
                    table.unique.insert(
                        local,
                        UniqueConstraint {
                            columns,
                            storage_parameters: BTreeMap::new(),
                            filegroup: None,
                        },
                    );
                }
            }
            (Kind::Check { expression }, Some(local)) => {
                table.checks.insert(
                    local,
                    CheckConstraint {
                        expression: closed(expression),
                    },
                );
            }
            (kind @ Kind::ForeignKey { .. }, Some(local)) => match foreign_key(p, by_name, kind) {
                Ok(fk) => {
                    table.foreign_keys.insert(local, fk);
                }
                Err(why) => note(pulled, name, format!("{} is left out: {why}", label(c))),
            },
            _ => {}
        }
        let after = (
            table.unique.len(),
            table.checks.len(),
            table.foreign_keys.len(),
        );
        if let (None, Some(name), true) = (&c.name, unnamed.get(&at), before != after) {
            given.push(name);
        }
    }
    if !given.is_empty() {
        let list = given;
        pulled.warnings.push(format!(
            "table `{}` has {} constraint(s) with no name in the file, read under the name(s) {}; \
             SQLite can change one only by rebuilding the table",
            Escaped(name),
            list.len(),
            list.iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for index in raw.indexes.iter().filter(|i| ident::same(&i.table, name)) {
        match built_index(index, name, facts) {
            Ok(built) => {
                table
                    .indexes
                    .insert(ObjectLocalName::new(&index.name), built);
            }
            Err(why) => note(
                pulled,
                name,
                format!("index `{}` is left out: {why}", Escaped(&index.name)),
            ),
        }
    }
    pulled.schema.tables.insert(name_of(name), table);
}

fn built_index(raw: &super::RawIndex, table: &str, facts: &Facts) -> Result<Index, String> {
    if ident::quote(&raw.name).is_err() {
        return Err("its name is one SQLite takes and no declaration can write".into());
    }
    let text = text::index(&raw.name, &raw.sql).map_err(|unread| unread.to_string())?;
    let row = facts
        .indexes
        .iter()
        .find(|row| row.name == raw.name)
        .ok_or("the catalog does not list it")?;
    agree_index(&text, row, table)?;
    let mut columns = Vec::new();
    for (key, descending, collation) in &row.keys {
        let Some(key) = key else {
            return Err("it has an expression as a key, which is not read".into());
        };
        if !collation.eq_ignore_ascii_case("BINARY") {
            return Err(format!(
                "its key `{}` has the collation `{}`",
                Escaped(key),
                Escaped(collation)
            ));
        }
        let mut column = IndexColumn::column(ColumnName::from(key.as_str()));
        column.descending = *descending;
        columns.push(column);
    }
    Ok(Index {
        columns,
        include: Vec::new(),
        unique: row.unique,
        filter: text.filter.as_deref().map(closed),
        method: pbps_model::IndexMethod::default(),
        storage_parameters: BTreeMap::new(),
        filegroup: None,
    })
}

/// A module the model cannot hold: inventoried, and a warning and a
/// limitation say why.
fn unmanaged(pulled: &mut Pulled, kind: &'static str, target: LimitationTarget, why: String) {
    pulled.unmanaged_modules.push(UnmanagedModule {
        kind,
        target: target.clone(),
        why: why.clone(),
    });
    record_limitation(
        &mut pulled.warnings,
        &mut pulled.limitations,
        Some(target),
        why,
    );
}

/// Turns one read into a [`Pulled`].
pub(crate) fn assemble(raw: &RawCatalog) -> Pulled {
    let mut pulled = Pulled::default();
    for name in &raw.attached {
        pulled.warnings.push(format!(
            "the connection has a database attached as `{}`; only `main` is read",
            Escaped(name)
        ));
    }
    for (schema, kind, name) in &raw.elsewhere {
        pulled.warnings.push(format!(
            "the {kind} `{}` is in the {} database, not in `main`; it is not read",
            Escaped(name),
            if schema == "temp" {
                "temporary".to_owned()
            } else {
                format!("attached `{}`", Escaped(schema))
            }
        ));
    }
    for name in &raw.intermediates {
        pulled.catalog_notices.push(format!(
            "a table named `{}` stands in the file: it is the name a table passes through when it is \
             renamed to its own name in another case, which a plan never leaves behind; it is not read",
            Escaped(name)
        ));
    }
    let mut readable: BTreeMap<&str, Parsed<'_>> = BTreeMap::new();
    for t in &raw.tables {
        match parse(t) {
            Ok(p) => {
                readable.insert(t.name.as_str(), p);
            }
            Err(why) => refuse(&mut pulled, &t.name, &why),
        }
    }
    for name in &raw.virtuals {
        refuse(
            &mut pulled,
            name,
            "it is a virtual table, which the model has no kind for",
        );
    }
    let by_name: BTreeMap<&str, &Parsed<'_>> = readable.iter().map(|(k, v)| (*k, v)).collect();
    for p in readable.values() {
        build(&mut pulled, p, &by_name, raw);
    }
    for v in &raw.views {
        let id = ModuleId::Named(ObjectName::new("main", &v.name));
        if ident::quote(&v.name).is_err() {
            unmanaged(
                &mut pulled,
                "view",
                LimitationTarget::module(id),
                format!(
                    "view `{}` has a name SQLite takes and no declaration can write",
                    Escaped(&v.name)
                ),
            );
            continue;
        }
        match text::view(&v.name, &v.sql) {
            Ok(view) if view.column_list.is_none() => {
                pulled.schema.modules.insert(
                    id,
                    Module {
                        kind: ModuleKind::View,
                        description: None,
                        options: Default::default(),
                        definition: view.body,
                    },
                );
            }
            Ok(_) => unmanaged(
                &mut pulled,
                "view",
                LimitationTarget::module(id),
                format!(
                    "view `{}` has a column list, which a declaration cannot say",
                    Escaped(&v.name)
                ),
            ),
            Err(unread) => unmanaged(
                &mut pulled,
                "view",
                LimitationTarget::module(id),
                format!("view {unread}"),
            ),
        }
    }
    for t in &raw.triggers {
        let id = ModuleId::Trigger {
            on: ObjectName::new("main", &t.table),
            name: t.name.clone(),
        };
        unmanaged(
            &mut pulled,
            "trigger",
            LimitationTarget::module(id),
            format!(
                "trigger `{}` on `{}` is not read: triggers are not built yet",
                Escaped(&t.name),
                Escaped(&t.table)
            ),
        );
    }
    pulled
}
