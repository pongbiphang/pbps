//! What SQLite will refuse, or this dialect does not build yet, in a
//! declaration (#2950).
//!
//! Three words for a refusal, as the other dialects use them. `Unsupported`:
//! SQLite has no such thing (each one is shown to be a syntax error, or to be
//! absent, on the engine). `NotBuilt`: valid SQLite that version 0 does not
//! build yet (generated columns, collation, expression indexes, triggers,
//! declared rows). `Invalid`: SQLite has the feature and the declaration uses
//! it wrongly. Valid SQLite refused on purpose is listed as a gap in
//! DEC-2950.2 and pinned by a test that names it.

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::DialectError;
use pbps_model::escape::Escaped;
use pbps_model::{
    ColumnName, IndexKey, IndexMethod, Module, ModuleId, ModuleKind, ModuleOptions,
    ObjectLocalName, Role, Schema, Table, TableName, ValueSource,
};

use crate::types::DIALECT;
use crate::{ident, lexis, names, types};

/// The only schema: SQLite's own name for the database a connection opened.
/// Attached databases are out of scope, and a schema-qualified parent is a
/// syntax error in `REFERENCES` anyway.
pub const SCHEMA: &str = "main";

pub(crate) fn invalid(message: String) -> DialectError {
    DialectError::Invalid {
        dialect: DIALECT,
        message,
    }
}

/// The refusal of a ledger name. Tables, views and indexes share one
/// namespace, so the ledger's two tables take their names from all three.
fn ledger_name(what: &str) -> DialectError {
    invalid(format!(
        "{what} has the name of a ledger table this tool owns (SPEC §8.1), and SQLite keeps \
         tables, views and indexes in one namespace; choose another name"
    ))
}

/// The refusal of a name kept for a rename's middle step: a table renamed to
/// its own name in another ASCII case passes through `__pbps_rename_<uid>`, and
/// the step fails if anything holds that name.
pub(crate) fn intermediate_name(what: &str) -> DialectError {
    invalid(format!(
        "{what} starts with `{}`, which this tool keeps for the name a table holds while it is \
         renamed to its own name in another case; choose another name",
        names::RENAME_PREFIX
    ))
}

/// The refusal of a constraint name this tool makes for an unnamed one.
fn synthetic_name(what: &str) -> DialectError {
    invalid(format!(
        "{what} starts with `{}`, which this tool keeps for the names it gives constraints a file \
         leaves unnamed; choose another name",
        names::UNNAMED_PREFIX
    ))
}

pub(crate) fn unsupported(feature: impl Into<String>) -> DialectError {
    DialectError::Unsupported {
        dialect: DIALECT,
        feature: feature.into(),
    }
}

pub(crate) fn not_built(part: impl Into<String>) -> DialectError {
    DialectError::NotBuilt {
        dialect: DIALECT,
        part: part.into(),
    }
}

/// A table or view name: in `main`, writable, not SQLite's own, and not one
/// of the ledger's, which are taken in the namespace both share.
fn object(kind: &str, schema: &str, local: &str, shown: &str, found: &mut Vec<DialectError>) {
    if schema != SCHEMA {
        found.push(unsupported(format!(
            "a schema other than `{SCHEMA}` (the {kind} `{shown}` is in `{}`); declare it as `{SCHEMA}.<name>`",
            Escaped(schema)
        )));
    }
    if let Err(error) = ident::quote(local) {
        found.push(error);
    }
    if ident::has_reserved_prefix(local) {
        found.push(invalid(format!(
            "the {kind} `{shown}` starts with `sqlite_`, which SQLite keeps for its own objects"
        )));
    }
    if names::is_ledger(local) {
        found.push(ledger_name(&format!("the {kind} `{shown}`")));
    }
    if names::is_intermediate(local) {
        found.push(intermediate_name(&format!("the {kind} `{shown}`")));
    }
}

/// Columns a key, constraint or index names: each declared, none twice. The
/// engine takes a duplicate and a case-variant (`A` for `a`); both are
/// refused as meaningless and as two spellings of one column (DEC-2950.2).
fn columns_of(what: &str, columns: &[ColumnName], table: &Table, found: &mut Vec<DialectError>) {
    if columns.is_empty() {
        found.push(invalid(format!("{what} names no column")));
    }
    let mut seen = BTreeSet::new();
    for column in columns {
        if !table.columns.contains_key(column) {
            found.push(invalid(format!(
                "{what} names the column `{}`, which the table does not declare",
                column.shown()
            )));
        } else if !seen.insert(column) {
            found.push(invalid(format!(
                "{what} names the column `{}` twice",
                column.shown()
            )));
        }
    }
}

/// A constraint's name: one SQLite can be given, and not one of the names this
/// tool makes for unnamed constraints.
fn constraint_name(what: &str, name: &str, found: &mut Vec<DialectError>) {
    local_name(what, name, found);
    if names::is_synthetic(name) {
        found.push(synthetic_name(&format!("{what} `{}`", Escaped(name))));
    }
}

fn local_name(what: &str, name: &str, found: &mut Vec<DialectError>) {
    if ident::quote(name).is_err() {
        found.push(invalid(format!(
            "{what} `{}` is not a name SQLite can be given",
            Escaped(name)
        )));
    }
}

/// Whether `text` is one expression SQLite reads whole: something there, its
/// parentheses closed, no `;`, and no subquery, which SQLite has in none of
/// the places this is asked about (a check, an index filter, a default).
/// `SELECT` is reserved, so the bare word is always one. Read with SQLite's
/// own lexis (`lexis`).
///
/// This reads structure and nothing here parses SQL. What an expression may
/// mean (a default that names a column, a filter that calls a function that
/// is not deterministic, a name that does not exist) is the engine's to
/// refuse, at apply and inside the plan's transaction.
fn expression(text: &str) -> bool {
    lexis::has_token(text)
        && lexis::is_one_expression(text)
        && !lexis::has_bare_word(text, "select")
}

/// Whether `text` is exactly one default SQLite reads as a value, and nothing
/// after it. SQLite takes `DEFAULT 5 NOT NULL` (a constraint), `DEFAULT 5, b
/// int` (a second column) and `DEFAULT 'a' COLLATE nocase`, so the text must
/// be one form: a signed number, a complete string, a blob literal, `NULL`,
/// `TRUE`, `FALSE`, `CURRENT_TIME`, `CURRENT_DATE`, `CURRENT_TIMESTAMP`, or an
/// expression in parentheses. Valid SQLite refused on purpose, named in
/// DEC-2950.2: a bare word or a quoted name (read as the string it spells),
/// a signed string or NULL, and a comment after the form.
#[must_use]
pub fn default_value(text: &str) -> bool {
    let text = text.trim_matches(lexis::is_space);
    if literal(text)
        || matches!(
            text.to_ascii_lowercase().as_str(),
            "current_time" | "current_date" | "current_timestamp"
        )
    {
        return true;
    }
    match text
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
    {
        Some(inner) => expression(inner),
        None => false,
    }
}

/// A signed number, a complete string, a blob literal, `NULL`, `TRUE` or
/// `FALSE`, and nothing else.
fn literal(text: &str) -> bool {
    if types::signed_number(text) {
        return true;
    }
    if matches!(
        text.to_ascii_lowercase().as_str(),
        "null" | "true" | "false"
    ) {
        return true;
    }
    let (blob, literal) = match text.strip_prefix(['x', 'X']) {
        Some(rest) if rest.starts_with('\'') => (true, rest),
        _ => (false, text),
    };
    if literal.starts_with('\'') {
        let inside = literal
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .unwrap_or("");
        let hex = inside.len() % 2 == 0 && inside.chars().all(|c| c.is_ascii_hexdigit());
        return lexis::is_one_string(literal) && (!blob || hex);
    }
    false
}

/// The constant a default that [`default_value`] accepts is, if it is one: a number, a
/// string, a blob, `NULL`, `TRUE` or `FALSE`. `ALTER TABLE .. ADD COLUMN`
/// refuses the rest (`CURRENT_TIME`, `CURRENT_DATE`, `CURRENT_TIMESTAMP` and
/// every computed expression, even `(1+2)`), measured on 3.54.1. Parentheses
/// around a constant, nested or not, leave it a constant (`(1)`, `((-1))`,
/// `('a')`), measured too, and so does a comment beside it (`(1 /* unit */)`):
/// the constant is read from the code, so a comment counts as the white space
/// it is and a string is never read into. The engine also folds a sign outside
/// the parentheses (`(-(1))`); that one stays the rebuild's.
#[must_use]
pub(crate) fn default_constant(text: &str) -> Option<String> {
    let code: String = lexis::pieces(text)
        .into_iter()
        .map(|piece| match piece {
            lexis::Piece::Code(run) | lexis::Piece::Quoted(run) => run,
            lexis::Piece::Comment(_) => " ",
        })
        .collect();
    let mut text = code.trim_matches(lexis::is_space);
    while let Some(inner) = text
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
    {
        text = inner.trim_matches(lexis::is_space);
    }
    literal(text).then(|| text.to_owned())
}

/// What is wrong with one column's type, default, value source and
/// collation: shared by a table's validation and by the emitter's `ADD COLUMN`.
pub(crate) fn column_errors(
    shown: &str,
    column: &ColumnName,
    def: &pbps_model::Column,
    found: &mut Vec<DialectError>,
) {
    if let Err(error) = types::normalize(&def.ty) {
        found.push(error);
    }
    match &def.value_source {
        ValueSource::None => {}
        ValueSource::Default(text) => {
            if !default_value(text) {
                found.push(invalid(format!(
                    "the default of `{shown}.{}` is not one value SQLite reads whole: a number, a string, \
                     `NULL`, `TRUE`, `FALSE`, `CURRENT_TIME`, `CURRENT_DATE`, `CURRENT_TIMESTAMP`, or an \
                     expression in parentheses",
                    column.shown()
                )));
            }
        }
        ValueSource::Identity(_) => found.push(unsupported(format!(
            "identity columns (`{shown}.{}`); declare an `integer` primary key, which numbers itself",
            column.shown()
        ))),
        ValueSource::Sequence(_) => found.push(unsupported(format!(
            "a column owning a sequence (`{shown}.{}`)",
            column.shown()
        ))),
        ValueSource::Generated(_) => found.push(not_built(format!(
            "generated columns (`{shown}.{}`)",
            column.shown()
        ))),
    }
    if def.collation.is_some() {
        found.push(not_built(format!(
            "column collation (`{shown}.{}`)",
            column.shown()
        )));
    }
}

/// Constraint names of one table that differ only in ASCII case. The shared
/// rule (`Table::constraint_name_conflicts`, called in `table`) compares names
/// exactly and reports the pairs that are letter for letter alike; SQLite folds
/// ASCII case, which is what this adds. SQLite takes a `CREATE TABLE` that
/// gives two constraints the same name, and then `DROP CONSTRAINT` drops the
/// first of them whatever its kind and `ADD CONSTRAINT` refuses the name
/// (measured), so a declaration that does it could never be changed in place.
fn constraint_names_by_case(shown: &str, table: &Table, found: &mut Vec<DialectError>) {
    let named = table
        .primary_key
        .iter()
        .filter_map(|key| key.name.as_ref())
        .map(|name| (name, "primary key"))
        .chain(table.unique.keys().map(|name| (name, "unique constraint")))
        .chain(table.foreign_keys.keys().map(|name| (name, "foreign key")))
        .chain(table.checks.keys().map(|name| (name, "check constraint")));
    let mut seen: Vec<(&ObjectLocalName, &str)> = Vec::new();
    for (name, kind) in named {
        if let Some((_, was)) = seen
            .iter()
            .find(|(other, _)| other.raw() != name.raw() && ident::same(other.raw(), name.raw()))
        {
            found.push(invalid(format!(
                "the {was} and the {kind} of `{shown}` are named alike in another ASCII case (`{}`); \
                 SQLite folds the case, so it drops whichever comes first and refuses to add a second",
                name.shown()
            )));
        }
        seen.push((name, kind));
    }
}

/// What is wrong with one declared check: a name SQLite cannot be given, or an
/// expression it would not read whole.
pub(crate) fn check_errors(
    shown: &str,
    constraint: &ObjectLocalName,
    check: &pbps_model::CheckConstraint,
    found: &mut Vec<DialectError>,
) {
    constraint_name("check constraint", constraint.raw(), found);
    if !expression(&check.expression) {
        found.push(invalid(format!(
            "check constraint `{}` of `{shown}` is not one expression SQLite reads whole (empty, \
             unbalanced parentheses, or a `;`)",
            constraint.shown()
        )));
    }
}

/// What is wrong with one declared index. The columns are held to the table's
/// only when the table is at hand: an emitter given just the index cannot.
pub(crate) fn index_errors(
    shown: &str,
    index: &ObjectLocalName,
    def: &pbps_model::Index,
    table: Option<&Table>,
    found: &mut Vec<DialectError>,
) {
    let what = format!("index `{}` of `{shown}`", index.shown());
    local_name("index", index.raw(), found);
    if ident::has_reserved_prefix(index.raw()) {
        found.push(invalid(format!(
            "{what} starts with `sqlite_`, which SQLite keeps for its own objects"
        )));
    }
    if names::is_ledger(index.raw()) {
        found.push(ledger_name(&what));
    }
    if names::is_intermediate(index.raw()) {
        found.push(intermediate_name(&what));
    }
    let mut keys = Vec::new();
    for key in &def.columns {
        match &key.key {
            IndexKey::Column(column) => keys.push(column.clone()),
            IndexKey::Expression(_) => {
                found.push(not_built(format!("expression indexes ({what})")))
            }
        }
        if key.opclass.is_some() || !key.opclass_parameters.is_empty() {
            found.push(unsupported(format!("operator classes ({what})")));
        }
        if key.collation.is_some() {
            found.push(not_built(format!("collation in an index ({what})")));
        }
    }
    if def.columns.is_empty() {
        found.push(invalid(format!("{what} names no column")));
    }
    // Only the keys that are columns are held to the table's columns. An
    // index of expressions alone names none and is not empty for that.
    if !keys.is_empty()
        && let Some(table) = table
    {
        columns_of(&what, &keys, table, found);
    }
    if !def.include.is_empty() {
        found.push(unsupported(format!("included columns ({what})")));
    }
    if def.method != IndexMethod::Btree {
        found.push(unsupported(format!(
            "an index method other than the default ({what})"
        )));
    }
    if !def.storage_parameters.is_empty() || def.filegroup.is_some() {
        found.push(unsupported(format!("storage options on {what}")));
    }
    if let Some(filter) = &def.filter
        && !expression(filter)
    {
        found.push(invalid(format!(
            "the filter of {what} is not one expression SQLite reads whole (empty, unbalanced \
             parentheses, or a `;`)"
        )));
    }
}

/// Everything wrong with a declared table.
#[must_use]
pub fn table(name: &TableName, table: &Table) -> Vec<DialectError> {
    let mut found = Vec::new();
    let shown = name.shown().to_string();
    object("table", &name.schema, &name.name, &shown, &mut found);

    if table.columns.is_empty() {
        found.push(invalid(format!(
            "table `{shown}` declares no column; SQLite needs at least one"
        )));
    }
    let mut folded: BTreeMap<String, &ColumnName> = BTreeMap::new();
    for (column, def) in &table.columns {
        if ident::quote(column.raw()).is_err() {
            found.push(invalid(format!(
                "column `{}` of `{shown}` is not a name SQLite can be given",
                column.shown()
            )));
        }
        if let Some(first) = folded.insert(ident::fold(column.raw()).into_owned(), column)
            && first != column
        {
            found.push(invalid(format!(
                "columns `{}` and `{}` of `{shown}` differ only in ASCII case, which SQLite holds as one name",
                first.shown(),
                column.shown()
            )));
        }
        column_errors(&shown, column, def, &mut found);
    }
    if !table.computed.is_empty() {
        found.push(not_built(format!("computed columns (table `{shown}`)")));
    }

    let features: [(bool, &str); 10] = [
        (table.filegroup.is_some(), "a filegroup"),
        (table.clustered.is_some(), "a clustered key"),
        (table.replica_identity.is_some(), "a replica identity"),
        (
            !table.storage_parameters.is_empty(),
            "table storage parameters",
        ),
        (table.unlogged, "unlogged tables"),
        (
            table.row_security.enabled || table.row_security.forced,
            "row security",
        ),
        (!table.row_policies.is_empty(), "row policies"),
        (table.system_time.is_some(), "system-versioned tables"),
        (
            table.partition_by.is_some() || table.partition_of.is_some(),
            "partitions",
        ),
        (!table.exclude.is_empty(), "exclusion constraints"),
    ];
    for (declared, feature) in features {
        if declared {
            found.push(unsupported(format!("{feature} (table `{shown}`)")));
        }
    }
    if table.data.is_some() {
        found.push(not_built(format!("declared rows (table `{shown}`)")));
    }

    if let Some(key) = &table.primary_key {
        columns_of(
            &format!("the primary key of `{shown}`"),
            &key.columns,
            table,
            &mut found,
        );
        if let Some(constraint) = &key.name {
            constraint_name("primary key", constraint.raw(), &mut found);
        }
        if !key.storage_parameters.is_empty() || key.filegroup.is_some() {
            found.push(unsupported(format!(
                "storage options on the primary key of `{shown}`"
            )));
        }
    }
    for (constraint, unique) in &table.unique {
        let what = format!("unique constraint `{}` of `{shown}`", constraint.shown());
        columns_of(&what, &unique.columns, table, &mut found);
        constraint_name("unique constraint", constraint.raw(), &mut found);
        if !unique.storage_parameters.is_empty() || unique.filegroup.is_some() {
            found.push(unsupported(format!("storage options on {what}")));
        }
    }
    for (constraint, fk) in &table.foreign_keys {
        let what = format!("foreign key `{}` of `{shown}`", constraint.shown());
        columns_of(&what, &fk.columns, table, &mut found);
        constraint_name("foreign key", constraint.raw(), &mut found);
        if fk.references_table.schema != SCHEMA {
            found.push(unsupported(format!(
                "a schema other than `{SCHEMA}` ({what} references `{}`)",
                fk.references_table.shown()
            )));
        }
        if fk.references_columns.len() != fk.columns.len() {
            found.push(invalid(format!(
                "{what} has {} column(s) and references {}; SQLite needs the same number",
                fk.columns.len(),
                fk.references_columns.len()
            )));
        }
    }
    for (constraint, check) in &table.checks {
        check_errors(&shown, constraint, check, &mut found);
    }
    found.extend(table.constraint_name_conflicts().into_iter().map(invalid));
    constraint_names_by_case(&shown, table, &mut found);
    for (index, def) in &table.indexes {
        index_errors(&shown, index, def, Some(table), &mut found);
    }
    found
}

/// Foreign keys of `table` whose parent columns are not a key of the parent.
/// SQLite creates such a child and then refuses every write to it (`foreign
/// key mismatch`, measured), so nothing at `CREATE` or at apply says so; the
/// declaration has to. The parent columns must be its primary key, or be held
/// unique as a set by an index over all of its rows; their order is free.
///
/// A parent the schema does not declare is the shared checks' to report.
#[must_use]
pub fn foreign_key_parents(name: &TableName, table: &Table, schema: &Schema) -> Vec<DialectError> {
    let mut found = Vec::new();
    for (constraint, fk) in &table.foreign_keys {
        let parent = if fk.references_table == *name {
            table
        } else if let Some(parent) = schema.tables.get(&fk.references_table) {
            parent
        } else {
            continue;
        };
        let wanted: BTreeSet<&ColumnName> = fk.references_columns.iter().collect();
        // A parent column named twice matches no key, however many columns
        // the key has: the sets alone would take it.
        let distinct = wanted.len() == fk.references_columns.len();
        let is = |columns: &mut dyn Iterator<Item = &ColumnName>| {
            distinct && columns.collect::<BTreeSet<_>>() == wanted
        };
        let primary = parent
            .primary_key
            .as_ref()
            .is_some_and(|key| is(&mut key.columns.iter()));
        let unique = parent
            .unique
            .values()
            .any(|unique| is(&mut unique.columns.iter()));
        let indexed = parent.indexes.values().any(|index| {
            let plain: Option<Vec<&ColumnName>> = index
                .columns
                .iter()
                .map(|key| match &key.key {
                    IndexKey::Column(column) if key.collation.is_none() => Some(column),
                    IndexKey::Column(_) | IndexKey::Expression(_) => None,
                })
                .collect();
            index.unique
                && index.filter.is_none()
                && plain.is_some_and(|columns| is(&mut columns.into_iter()))
        });
        if !(primary || unique || indexed) {
            found.push(invalid(format!(
                "foreign key `{}` of `{}` references columns of `{}` that are not its primary key \
                 and are not held unique together; SQLite would create the table and then refuse \
                 every write to it (foreign key mismatch)",
                constraint.shown(),
                name.shown(),
                fk.references_table.shown()
            )));
        }
    }
    found
}

/// Everything wrong with a declared module. A view is a name in `main` and one
/// `SELECT`, `WITH` or `VALUES` statement; SQLite has no procedures or
/// functions, and triggers are not built yet.
#[must_use]
pub fn module(id: &ModuleId, module: &Module) -> Vec<DialectError> {
    let name = id.object_name();
    let shown = name.shown().to_string();
    match module.kind {
        ModuleKind::Procedure => vec![unsupported(format!("procedures (`{shown}`)"))],
        ModuleKind::Function => vec![unsupported(format!(
            "functions (`{shown}`); application-defined functions live in the host process"
        ))],
        ModuleKind::Trigger => vec![not_built(format!("triggers (`{shown}`)"))],
        ModuleKind::View => {
            let mut found = Vec::new();
            object("view", &name.schema, &name.name, &shown, &mut found);
            if id.args().is_some() {
                found.push(invalid(format!("the view `{shown}` is declared with an argument list; SQLite identifies a view by name")));
            }
            if module.options != ModuleOptions::default() {
                found.push(unsupported(format!(
                    "view options such as `schemabinding` or view indexes (`{shown}`)"
                )));
            }
            let body = &module.definition;
            if !lexis::has_token(body) {
                found.push(invalid(format!(
                    "the view `{shown}` has an empty definition"
                )));
            } else if !matches!(
                lexis::first_word(body).as_deref(),
                Some("select" | "with" | "values")
            ) {
                found.push(invalid(format!(
                    "the definition of the view `{shown}` must be a `SELECT`, `WITH` or `VALUES` statement"
                )));
            }
            if lexis::has_second_statement(body) {
                found.push(invalid(format!(
                    "the definition of the view `{shown}` holds a second statement after a `;`; a view is one statement"
                )));
            }
            found
        }
    }
}

/// SQLite has neither roles nor grants: the file's permissions are the only
/// control.
#[must_use]
pub fn role(name: &pbps_model::RoleName, _role: &Role) -> Vec<DialectError> {
    vec![unsupported(format!(
        "roles and grants (`{}`); SQLite has no users, and the file's permissions are the only control",
        name.shown()
    ))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{accepts, refusal};
    use pbps_model::{
        CheckConstraint, Collation, Column, ComputedColumn, Filegroup, ForeignKey, Generated,
        Identity, Index, IndexColumn, PartitionBound, PartitionBy, PartitionOf, PartitionStrategy,
        PrimaryKey, ReferentialAction, ReplicaIdentity, RowPolicy, SystemTime, UniqueConstraint,
    };

    fn column() -> Column {
        Column::new("int".parse().expect("type"))
    }

    fn key(name: &str) -> IndexColumn {
        IndexColumn {
            key: IndexKey::Column(name.into()),
            descending: false,
            opclass: None,
            collation: None,
            opclass_parameters: Default::default(),
        }
    }

    fn index(columns: &[&str]) -> Index {
        Index {
            columns: columns.iter().map(|c| key(c)).collect(),
            include: Vec::new(),
            unique: false,
            filter: None,
            method: IndexMethod::Btree,
            storage_parameters: Default::default(),
            filegroup: None,
        }
    }

    fn primary(columns: &[&str]) -> PrimaryKey {
        PrimaryKey {
            name: None,
            columns: columns.iter().map(|c| (*c).into()).collect(),
            storage_parameters: Default::default(),
            filegroup: None,
        }
    }

    /// `main.t` with one column `c`, which is valid.
    fn base() -> Table {
        let mut table = Table::default();
        table.columns.insert("c".into(), column());
        table
    }

    fn errors(table: &Table) -> Vec<DialectError> {
        self::table(&TableName::new("main", "t"), table)
    }

    fn said(errors: &[DialectError]) -> String {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" | ")
    }

    fn is_unsupported(error: &DialectError) -> bool {
        matches!(
            error,
            DialectError::Unsupported {
                dialect: "sqlite",
                ..
            }
        )
    }

    fn is_not_built(error: &DialectError) -> bool {
        matches!(
            error,
            DialectError::NotBuilt {
                dialect: "sqlite",
                ..
            }
        )
    }

    fn is_invalid(error: &DialectError) -> bool {
        matches!(
            error,
            DialectError::Invalid {
                dialect: "sqlite",
                ..
            }
        )
    }

    /// Defaults, as `(text, the engine takes it, this takes it)`. Where the
    /// engine takes more than this, the row is a gap named in DEC-2950.2.
    const DEFAULTS: &[(&str, bool, bool)] = &[
        ("0", true, true),
        ("-5", true, true),
        ("+5", true, true),
        ("- 5", true, true),
        ("1.5", true, true),
        ("1e3", true, true),
        ("0x10", true, true),
        (".5", true, true),
        ("5.", true, true),
        ("'x'", true, true),
        ("'it''s'", true, true),
        ("''", true, true),
        ("x'ab'", true, true),
        ("X'ab'", true, true),
        ("NULL", true, true),
        ("null", true, true),
        ("TRUE", true, true),
        ("FALSE", true, true),
        ("CURRENT_TIME", true, true),
        ("CURRENT_DATE", true, true),
        ("CURRENT_TIMESTAMP", true, true),
        ("current_timestamp", true, true),
        ("(1+2)", true, true),
        ("(abs(1))", true, true),
        ("( 'a' || 'b' )", true, true),
        ("(random())", true, true),
        ("(CURRENT_TIMESTAMP)", true, true),
        ("(')')", true, true),
        ("(1 /* ) */)", true, true),
        ("(1 -- )\n)", true, true),
        ("()", false, false),
        ("1+2", false, false),
        ("abs(1)", false, false),
        ("'unterminated", false, false),
        ("( 1", false, false),
        ("1)", false, false),
        ("(1))", false, false),
        ("5)", false, false),
        ("5 --c", false, false),
        ("random()", false, false),
        ("CURRENT_TIMESTAMP + 1", false, false),
        ("1e", false, false),
        ("0x", false, false),
        ("'a' 'b'", false, false),
        ("5 6", false, false),
        ("+TRUE", false, false),
        ("(1)(2)", false, false),
        ("", false, false),
        // Valid SQLite refused on purpose: a name read as the string it spells, a signed string or NULL.
        ("\"dq\"", true, false),
        ("dq", true, false),
        ("x", true, false),
        ("[x]", true, false),
        ("`x`", true, false),
        ("+'x'", true, false),
        ("-'x'", true, false),
        ("-NULL", true, false),
        // Valid SQLite that says more than a default: a constraint, a column, a trailing comment.
        ("5 NOT NULL", true, false),
        ("5, b int", true, false),
        ("'a' COLLATE nocase", true, false),
        ("(1) CHECK (a>0)", true, false),
    ];

    #[test]
    fn a_default_is_one_value_judged_like_the_engine_in_both_directions() {
        for &(text, engine, here) in DEFAULTS {
            let ddl = ["CREATE TABLE t(a int DEFAULT ", text, ")"].concat();
            assert_eq!(accepts(&ddl), engine, "the engine on {text:?}");
            assert_eq!(default_value(text), here, "default_value on {text:?}");
            // Soundness: nothing is accepted here that the engine refuses.
            assert!(!here || engine, "{text:?}");
        }
        // Layout around the one form is layout; a comment is not.
        assert!(
            default_value("  5\n")
                && !default_value("5 /*c*/")
                && accepts("CREATE TABLE t(a int DEFAULT 5 /*c*/)")
        );
        assert!(default_value("x''") == accepts("CREATE TABLE t(a int DEFAULT x'')"));
        assert!(!default_value("x'a'") && !accepts("CREATE TABLE t(a int DEFAULT x'a')"));
        assert!(!default_value("x'zz'") && !accepts("CREATE TABLE t(a int DEFAULT x'zz')"));
    }

    /// Everything SQLite has no syntax for, each shown to be refused by the
    /// engine and refused here as `Unsupported`.
    #[test]
    fn what_sqlite_lacks_is_refused_as_unsupported_and_the_engine_refuses_it_too() {
        type Make = fn(&mut Table);
        let rows: Vec<(&str, &str, Make)> = vec![
            ("a filegroup", "CREATE TABLE t(c) ON [PRIMARY]", |t| {
                t.filegroup = Some(Filegroup::new("fg"))
            }),
            (
                "a clustered key",
                "CREATE TABLE t(c PRIMARY KEY CLUSTERED)",
                |t| t.clustered = Some(pbps_model::Clustered::Heap),
            ),
            (
                "a replica identity",
                "CREATE TABLE t(c); ALTER TABLE t REPLICA IDENTITY FULL",
                |t| t.replica_identity = Some(ReplicaIdentity::Full),
            ),
            (
                "table storage parameters",
                "CREATE TABLE t(c) WITH (fillfactor=70)",
                |t| {
                    t.storage_parameters
                        .insert("fillfactor".into(), "70".into());
                },
            ),
            ("unlogged tables", "CREATE UNLOGGED TABLE t(c)", |t| {
                t.unlogged = true
            }),
            (
                "row security",
                "CREATE TABLE t(c); ALTER TABLE t ENABLE ROW LEVEL SECURITY",
                |t| t.row_security.enabled = true,
            ),
            (
                "row policies",
                "CREATE TABLE t(c); CREATE POLICY p ON t",
                |t| {
                    t.row_policies.insert(
                        "p".into(),
                        RowPolicy {
                            command: Default::default(),
                            restrictive: false,
                            to: Default::default(),
                            using: None,
                            with_check: None,
                        },
                    );
                },
            ),
            (
                "system-versioned tables",
                "CREATE TABLE t(c, PERIOD FOR SYSTEM_TIME (c, c))",
                |t| {
                    t.system_time = Some(SystemTime {
                        start: "c".into(),
                        end: "c".into(),
                        hidden: false,
                        versioning: None,
                    });
                },
            ),
            (
                "partitions",
                "CREATE TABLE t(c) PARTITION BY RANGE (c)",
                |t| {
                    t.partition_by = Some(PartitionBy {
                        strategy: PartitionStrategy::Range,
                        columns: vec!["c".into()],
                        external: false,
                    });
                },
            ),
            (
                "partitions",
                "CREATE TABLE p PARTITION OF t FOR VALUES FROM (1) TO (2)",
                |t| {
                    t.partition_of = Some(PartitionOf {
                        parent: TableName::new("main", "p"),
                        bound: PartitionBound::Default,
                        columns: Default::default(),
                    });
                },
            ),
            (
                "exclusion constraints",
                "CREATE TABLE t(c, EXCLUDE USING gist (c WITH =))",
                |t| {
                    t.exclude.insert(
                        "x".into(),
                        pbps_model::ExclusionConstraint {
                            method: IndexMethod::Gist,
                            keys: vec![pbps_model::ExclusionKey {
                                key: key("c"),
                                operator: "=".parse().expect("operator"),
                            }],
                            include: Vec::new(),
                            filter: None,
                            storage_parameters: Default::default(),
                            timing: Default::default(),
                        },
                    );
                },
            ),
            (
                "references `s.p`",
                "CREATE TABLE t(c, FOREIGN KEY (c) REFERENCES s.p(c))",
                |t| {
                    let mut f = fk(&["c"], &["c"]);
                    f.references_table = TableName::new("s", "p");
                    t.foreign_keys.insert("f".into(), f);
                },
            ),
            (
                "storage options on the primary key",
                "CREATE TABLE t(c PRIMARY KEY WITH (fillfactor=70))",
                |t| {
                    let mut key = primary(&["c"]);
                    key.storage_parameters
                        .insert("fillfactor".into(), "70".into());
                    t.primary_key = Some(key);
                },
            ),
            (
                "storage options on unique constraint",
                "CREATE TABLE t(c UNIQUE USING INDEX TABLESPACE x)",
                |t| {
                    t.unique.insert(
                        "u".into(),
                        UniqueConstraint {
                            columns: vec!["c".into()],
                            storage_parameters: Default::default(),
                            filegroup: Some(Filegroup::new("fg")),
                        },
                    );
                },
            ),
            (
                "included columns",
                "CREATE TABLE t(c); CREATE INDEX i ON t(c) INCLUDE (c)",
                |t| {
                    let mut i = index(&["c"]);
                    i.include = vec!["c".into()];
                    t.indexes.insert("i".into(), i);
                },
            ),
            (
                "an index method other than the default",
                "CREATE TABLE t(c); CREATE INDEX i ON t USING gin (c)",
                |t| {
                    let mut i = index(&["c"]);
                    i.method = IndexMethod::Gin;
                    t.indexes.insert("i".into(), i);
                },
            ),
            (
                "storage options on index",
                "CREATE TABLE t(c); CREATE INDEX i ON t(c) WITH (fillfactor=70)",
                |t| {
                    let mut i = index(&["c"]);
                    i.storage_parameters
                        .insert("fillfactor".into(), "70".into());
                    t.indexes.insert("i".into(), i);
                },
            ),
            (
                "operator classes",
                "CREATE TABLE t(c); CREATE INDEX i ON t(c text_pattern_ops)",
                |t| {
                    let mut i = index(&["c"]);
                    i.columns[0].opclass = Some("text_pattern_ops".parse().expect("opclass"));
                    t.indexes.insert("i".into(), i);
                },
            ),
            (
                "identity columns",
                "CREATE TABLE t(c int GENERATED ALWAYS AS IDENTITY)",
                |t| {
                    t.columns.get_mut("c").expect("c").value_source =
                        ValueSource::Identity(Identity {
                            seed: 1,
                            increment: 1,
                            options: Default::default(),
                        });
                },
            ),
            ("is in `s`", "CREATE TABLE s.t(c)", |_| {}),
        ];
        for (feature, sql, make) in rows {
            assert!(!accepts(sql), "the engine refuses {feature}: {sql}");
            let mut table = base();
            make(&mut table);
            let name = if feature == "is in `s`" {
                TableName::new("s", "t")
            } else {
                TableName::new("main", "t")
            };
            let found = self::table(&name, &table);
            assert!(
                found
                    .iter()
                    .any(|e| is_unsupported(e) && e.to_string().contains(feature)),
                "{feature}: {}",
                said(&found)
            );
        }
        assert!(
            errors(&base()).is_empty(),
            "negative: the base table is valid"
        );
    }

    /// Valid SQLite that version 0 does not build yet: the engine takes each,
    /// and the refusal says "not built", never "SQLite has no such thing".
    #[test]
    fn valid_sqlite_not_built_yet_is_refused_as_not_built_and_the_engine_takes_it() {
        type Make = fn(&mut Table);
        let rows: Vec<(&str, Make)> = vec![
            ("CREATE TABLE t(c, b AS (c + 1))", |t| {
                t.columns.insert("b".into(), {
                    let mut b = column();
                    b.value_source = ValueSource::Generated(Generated {
                        expression: "c + 1".into(),
                        stored: false,
                    });
                    b
                });
            }),
            ("CREATE TABLE t(c, b AS (c + 1))", |t| {
                t.computed.insert(
                    "b".into(),
                    ComputedColumn {
                        expression: "c + 1".into(),
                        persisted: false,
                        not_null: false,
                    },
                );
            }),
            ("CREATE TABLE t(c COLLATE nocase)", |t| {
                t.columns.get_mut("c").expect("c").collation = Some(Collation::new("nocase"))
            }),
            ("CREATE TABLE t(c); CREATE INDEX i ON t(lower(c))", |t| {
                let mut i = index(&["c"]);
                i.columns[0].key = IndexKey::Expression("lower(c)".into());
                t.indexes.insert("i".into(), i);
            }),
            (
                "CREATE TABLE t(c); CREATE INDEX i ON t(c COLLATE nocase)",
                |t| {
                    let mut i = index(&["c"]);
                    i.columns[0].collation = Some("nocase".into());
                    t.indexes.insert("i".into(), i);
                },
            ),
        ];
        for (sql, make) in rows {
            assert!(accepts(sql), "the engine takes {sql}");
            let mut table = base();
            make(&mut table);
            let found = errors(&table);
            assert!(
                // Valid SQLite that is not built is said once, as that: it
                // is neither absent from SQLite nor a mistake.
                found.iter().all(is_not_built) && !found.is_empty(),
                "{sql}: {}",
                said(&found)
            );
        }
        // Declared rows have no DDL of their own.
        let mut table = base();
        table.data = Some(pbps_model::TableData {
            mode: pbps_model::DataMode::Exact,
            rows: Default::default(),
        });
        assert!(errors(&table).iter().any(is_not_built));
    }

    /// A declaration that uses a feature SQLite has wrongly: refused here as
    /// `Invalid`, and the engine refuses the DDL it would be written as.
    #[test]
    fn declarations_the_engine_would_refuse_are_invalid() {
        type Make = fn(&mut Table);
        let named = |name: &str| TableName::new("main", name);
        // (what, table name, DDL the engine refuses, a change to the base table)
        let rows: Vec<(&str, TableName, &str, Make)> = vec![
            (
                "a reserved prefix",
                named("sqlite_t"),
                "CREATE TABLE sqlite_t(c)",
                |_| {},
            ),
            (
                "a reserved prefix in another case",
                named("SQLite_T"),
                "CREATE TABLE SQLite_T(c)",
                |_| {},
            ),
            (
                "the ledger name in another case",
                named("__PBPS_STATE"),
                "CREATE TABLE __pbps_state(c); CREATE TABLE __PBPS_STATE(c)",
                |_| {},
            ),
            ("no column", named("t"), "CREATE TABLE t()", |t| {
                t.columns.clear()
            }),
            (
                "two columns of one name",
                named("t"),
                "CREATE TABLE t(c, C)",
                |t| {
                    t.columns.insert("C".into(), column());
                },
            ),
            (
                "a primary key over a missing column",
                named("t"),
                "CREATE TABLE t(c, PRIMARY KEY (nosuch))",
                |t| t.primary_key = Some(primary(&["nosuch"])),
            ),
            (
                "a unique constraint over a missing column",
                named("t"),
                "CREATE TABLE t(c, UNIQUE (nosuch))",
                |t| {
                    t.unique.insert(
                        "u".into(),
                        UniqueConstraint {
                            columns: vec!["nosuch".into()],
                            storage_parameters: Default::default(),
                            filegroup: None,
                        },
                    );
                },
            ),
            (
                "a foreign key over a missing column",
                named("t"),
                "CREATE TABLE t(c, FOREIGN KEY (nosuch) REFERENCES t(c))",
                |t| {
                    t.foreign_keys.insert("f".into(), fk(&["nosuch"], &["c"]));
                },
            ),
            (
                "a foreign key with unequal column counts",
                named("t"),
                "CREATE TABLE t(c, d, FOREIGN KEY (c, d) REFERENCES t(c))",
                |t| {
                    t.columns.insert("d".into(), column());
                    t.foreign_keys.insert("f".into(), fk(&["c", "d"], &["c"]));
                },
            ),
            (
                "an index over a missing column",
                named("t"),
                "CREATE TABLE t(c); CREATE INDEX i ON t(nosuch)",
                |t| {
                    t.indexes.insert("i".into(), index(&["nosuch"]));
                },
            ),
            (
                "an index named with the reserved prefix",
                named("t"),
                "CREATE TABLE t(c); CREATE INDEX sqlite_i ON t(c)",
                |t| {
                    t.indexes.insert("sqlite_i".into(), index(&["c"]));
                },
            ),
            (
                "an index with no column",
                named("t"),
                "CREATE TABLE t(c); CREATE INDEX i ON t()",
                |t| {
                    let mut i = index(&["c"]);
                    i.columns.clear();
                    t.indexes.insert("i".into(), i);
                },
            ),
            (
                "an empty check",
                named("t"),
                "CREATE TABLE t(c, CHECK ())",
                |t| {
                    t.checks.insert(
                        "k".into(),
                        CheckConstraint {
                            expression: String::new(),
                        },
                    );
                },
            ),
            (
                "a check with a loose parenthesis",
                named("t"),
                "CREATE TABLE t(c, CHECK (c > 0) OR (c < 9))",
                |t| {
                    t.checks.insert(
                        "k".into(),
                        CheckConstraint {
                            expression: "c > 0) OR (c < 9".into(),
                        },
                    );
                },
            ),
            (
                "a check holding a second statement",
                named("t"),
                "CREATE TABLE t(c, CHECK (c > 0; SELECT 1))",
                |t| {
                    t.checks.insert(
                        "k".into(),
                        CheckConstraint {
                            expression: "c > 0; SELECT 1".into(),
                        },
                    );
                },
            ),
            (
                "an empty filter",
                named("t"),
                "CREATE TABLE t(c); CREATE INDEX i ON t(c) WHERE ()",
                |t| {
                    let mut i = index(&["c"]);
                    i.filter = Some("/* none */".into());
                    t.indexes.insert("i".into(), i);
                },
            ),
            (
                "a bad default",
                named("t"),
                "CREATE TABLE t(c int DEFAULT abs(1))",
                |t| {
                    t.columns.get_mut("c").expect("c").value_source =
                        ValueSource::Default("abs(1)".into());
                },
            ),
        ];
        for (what, name, sql, make) in rows {
            assert!(!accepts(sql), "the engine refuses {what}: {sql}");
            let mut table = base();
            make(&mut table);
            let found = self::table(&name, &table);
            assert!(found.iter().any(is_invalid), "{what}: {}", said(&found));
        }
    }

    /// A column's type is judged by the type table: a name that promises numbering
    /// is refused (the engine takes it and numbers nothing), an unlisted name is
    /// written.
    #[test]
    fn a_column_type_is_judged_by_the_type_table() {
        let mut table = base();
        table.columns.get_mut("c").expect("c").ty = "serial".parse().expect("type");
        let found = errors(&table);
        assert!(
            found
                .iter()
                .any(|e| is_invalid(e) && e.to_string().contains("cannot be a column type here")),
            "{}",
            said(&found)
        );
        assert!(accepts("CREATE TABLE t(c serial)"));
        table.columns.get_mut("c").expect("c").ty = "uuid".parse().expect("type");
        assert!(errors(&table).is_empty());
    }

    fn fk(columns: &[&str], references: &[&str]) -> ForeignKey {
        ForeignKey {
            columns: columns.iter().map(|c| (*c).into()).collect(),
            references_table: TableName::new("main", "t"),
            references_columns: references.iter().map(|c| (*c).into()).collect(),
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        }
    }

    /// Valid SQLite refused on purpose, each pinned: the engine takes it and
    /// this says why not. Named as gaps in DEC-2950.2.
    #[test]
    fn valid_sqlite_refused_on_purpose_is_pinned() {
        type Make = fn(&mut Table);
        let rows: Vec<(&str, &str, Make)> = vec![
            (
                "a primary key naming a column twice",
                "CREATE TABLE t(c, PRIMARY KEY (c, c))",
                |t| t.primary_key = Some(primary(&["c", "c"])),
            ),
            (
                "a primary key naming a column in another case",
                "CREATE TABLE t(c, PRIMARY KEY (C))",
                |t| t.primary_key = Some(primary(&["C"])),
            ),
            (
                "an index naming a column twice",
                "CREATE TABLE t(c); CREATE INDEX i ON t(c, c)",
                |t| {
                    t.indexes.insert("i".into(), index(&["c", "c"]));
                },
            ),
            ("an empty column name", "CREATE TABLE t(\"\")", |t| {
                t.columns.clear();
                t.columns.insert("".into(), column());
            }),
            (
                "an empty constraint name",
                "CREATE TABLE t(c, CONSTRAINT \"\" CHECK (c > 0))",
                |t| {
                    t.checks.insert(
                        "".into(),
                        CheckConstraint {
                            expression: "c > 0".into(),
                        },
                    );
                },
            ),
        ];
        for (what, sql, make) in rows {
            assert!(accepts(sql), "the engine takes {what}: {sql}");
            let mut table = base();
            make(&mut table);
            assert!(errors(&table).iter().any(is_invalid), "{what}");
        }
        // The same database written `MAIN` is `main` to the engine; only the one spelling is declared.
        assert!(accepts("CREATE TABLE MAIN.t(c)"));
        assert!(
            table(&TableName::new("MAIN", "t"), &base())
                .iter()
                .any(is_unsupported)
        );
    }

    /// A valid table is accepted, with a key, a unique constraint, a foreign
    /// key, a check and a partial descending index, and the DDL it stands for
    /// runs on the engine.
    #[test]
    fn a_valid_table_is_accepted_and_its_ddl_runs() {
        let mut table = base();
        table.columns.insert("d".into(), {
            let mut d = column();
            d.value_source = ValueSource::Default("0".into());
            d.nullable = false;
            d
        });
        table.primary_key = Some(primary(&["c"]));
        table.unique.insert(
            "u".into(),
            UniqueConstraint {
                columns: vec!["d".into()],
                storage_parameters: Default::default(),
                filegroup: None,
            },
        );
        table.foreign_keys.insert("f".into(), fk(&["d"], &["c"]));
        table.checks.insert(
            "k".into(),
            CheckConstraint {
                expression: "d >= 0".into(),
            },
        );
        let mut i = index(&["d"]);
        i.columns[0].descending = true;
        i.unique = true;
        i.filter = Some("d > 1".into());
        table.indexes.insert("i".into(), i);
        assert!(errors(&table).is_empty(), "{}", said(&errors(&table)));
        assert!(accepts(
            "CREATE TABLE \"t\"(\"c\" int, \"d\" int NOT NULL DEFAULT 0, CONSTRAINT \"pk\" PRIMARY KEY (\"c\"), \
             CONSTRAINT \"u\" UNIQUE (\"d\"), CONSTRAINT \"f\" FOREIGN KEY (\"d\") REFERENCES \"t\"(\"c\"), \
             CONSTRAINT \"k\" CHECK (d >= 0)); CREATE UNIQUE INDEX \"i\" ON \"t\"(\"d\" DESC) WHERE d > 1"
        ));
    }

    /// Views, as `(body, the engine takes it as a view, this takes it)`.
    const VIEWS: &[(&str, bool, bool)] = &[
        ("SELECT 1", true, true),
        ("select 1", true, true),
        ("WITH x AS (SELECT 1) SELECT * FROM x", true, true),
        ("VALUES (1)", true, true),
        ("/* c */ SELECT 1", true, true),
        ("-- c\nSELECT 1", true, true),
        ("SELECT 1 -- c", true, true),
        ("SELECT 1 UNION SELECT 2", true, true),
        ("SELECT ';'", true, true),
        ("SELECT 1;", true, true),
        ("SELECT 1;;", true, true),
        ("SELECT 1 ; /* c */ ", true, true),
        ("(SELECT 1)", false, false),
        ("INSERT INTO t VALUES (1)", false, false),
        ("DELETE FROM t", false, false),
        ("PRAGMA user_version", false, false),
        ("EXPLAIN SELECT 1", false, false),
        // The first word is a whole name, by SQLite's own rule for one.
        ("SELECT$foo 1", false, false),
        ("SELECT\u{e9} 1", false, false),
        ("SELECT1", false, false),
        ("WITH_x AS (SELECT 1) SELECT 1", false, false),
        ("SELECT\"a\" FROM (SELECT 1 AS a)", true, true),
        ("SELECT(1)", true, true),
        ("", false, false),
        ("   ", false, false),
        // Valid SQLite that is two statements: refused, a view is one.
        ("SELECT 1; SELECT 2", true, false),
    ];

    fn view(body: &str) -> (ModuleId, Module) {
        (
            ModuleId::Named(pbps_model::ObjectName::new("main", "v")),
            Module {
                kind: ModuleKind::View,
                description: None,
                options: ModuleOptions::default(),
                definition: body.to_owned(),
            },
        )
    }

    #[test]
    fn a_view_is_judged_like_the_engine_in_both_directions() {
        for &(body, engine, here) in VIEWS {
            assert_eq!(
                accepts(&["CREATE VIEW v AS ", body].concat()),
                engine,
                "the engine on {body:?}"
            );
            let (id, module) = view(body);
            assert_eq!(
                super::module(&id, &module).is_empty(),
                here,
                "validate on {body:?}: {}",
                said(&super::module(&id, &module))
            );
            assert!(!here || engine, "{body:?}");
        }
    }

    /// The limit of a lexical check, said: nothing here parses SQL, so a body
    /// that starts right and is one statement but is not a whole query (a bare
    /// `SELECT`) is accepted, and the engine refuses it when the plan runs, in
    /// the plan's transaction.
    #[test]
    fn a_view_body_that_is_not_a_whole_query_is_left_to_the_engine() {
        // The second is the nested-comment text: the engine ends the comment at the
        // first `*/`, so what follows is code it cannot parse.
        for body in ["SELECT", "SELECT 1 /* a /* b */ c */"] {
            let (id, module) = view(body);
            assert!(super::module(&id, &module).is_empty(), "{body:?}");
            assert!(!accepts(&["CREATE VIEW v AS ", body].concat()), "{body:?}");
        }
    }

    /// A second statement hidden where only SQLite's own lexis finds it.
    #[test]
    fn a_view_cannot_carry_a_statement_behind_a_comment_the_shared_scanner_nests() {
        let (id, module) = view("SELECT 1 /* /* */ ; CREATE TABLE smuggled(a) /* */");
        assert!(
            super::module(&id, &module)
                .iter()
                .any(|e| e.to_string().contains("second statement"))
        );
    }

    #[test]
    fn views_in_another_schema_with_options_or_named_like_sqlite_s_own_are_refused() {
        let mut other = view("SELECT 1");
        other.0 = ModuleId::Named(pbps_model::ObjectName::new("s", "v"));
        assert!(super::module(&other.0, &other.1).iter().any(is_unsupported));
        assert!(!accepts("CREATE VIEW s.v AS SELECT 1"));
        let mut options = view("SELECT 1");
        options.1.options.schemabinding = true;
        assert!(
            super::module(&options.0, &options.1)
                .iter()
                .any(is_unsupported)
        );
        let mut reserved = view("SELECT 1");
        reserved.0 = ModuleId::Named(pbps_model::ObjectName::new("main", "sqlite_v"));
        assert!(
            super::module(&reserved.0, &reserved.1)
                .iter()
                .any(is_invalid)
        );
        assert!(refusal("CREATE VIEW sqlite_v AS SELECT 1").contains("reserved"));
    }

    /// Expressions of a check, as `(text, the engine takes it inside
    /// `CHECK (…)`, this takes it)`. Nothing is taken here that the engine
    /// refuses; where the engine takes more, the row says why it is refused.
    const EXPRESSIONS: &[(&str, bool, bool)] = &[
        ("c > 0", true, true),
        ("(c > 0)", true, true),
        ("c > 0 AND (c < 9 OR c IS NULL)", true, true),
        ("random() > 0", true, true),
        ("abs ( c ) > (0)", true, true),
        ("c IN ()", true, true),
        ("c > 0 /* why */", true, true),
        ("c > 0 -- why\n", true, true),
        ("c <> ')'", true, true),
        ("\"c\" > 0", true, true),
        // A subquery: SQLite has none in a check, a default or an index filter.
        ("c IN (SELECT 1)", false, false),
        ("c > (select max(1))", false, false),
        ("EXISTS (SELECT 1)", false, false),
        ("c <> \"select\"", true, true),
        ("c <> 'select'", true, true),
        ("c > 0 /* select */", true, true),
        // An empty group is no expression.
        ("()", false, false),
        ("(())", false, false),
        ("c > ()", false, false),
        ("c > 0 AND ( /* nothing */ )", false, false),
        ("c > 0 OR ()", false, false),
        ("NOT ()", false, false),
        ("c IN /* none */ ()", true, true),
        ("abs /* of */ (c) > 0", true, true),
        ("0 AND()", false, false),
        ("", false, false),
        // A run that never closes swallows the parenthesis that ends the check.
        ("c > 0 /*", false, false),
        ("c > 0 /* *", false, false),
        ("c > 0 -- why", false, false),
        ("c > 'a", false, false),
        ("c > \"a", false, false),
        ("c > `a", false, false),
        ("c > [a", false, false),
        ("c > 0)", false, false),
        ("(c > 0", false, false),
        ("c > 0; SELECT 1", false, false),
    ];

    #[test]
    fn a_check_expression_is_never_taken_where_the_engine_refuses_it() {
        let (mut taken, mut refused) = (0, 0);
        for &(text, engine, here) in EXPRESSIONS {
            let ddl = ["CREATE TABLE t(c, CHECK (", text, "))"].concat();
            assert_eq!(accepts(&ddl), engine, "the engine on {text:?}");
            assert_eq!(expression(text), here, "the check on {text:?}");
            assert!(
                engine || !here,
                "{text:?} is taken and the engine refuses it"
            );
            taken += usize::from(here);
            refused += usize::from(!here);
        }
        assert!(taken > 5 && refused > 5);
    }

    /// A foreign key's parent columns must be the parent's primary key or be
    /// held unique as a set. SQLite creates the child either way and refuses
    /// every write to it afterwards (`foreign key mismatch`), so the engine's
    /// answer here is a write, not the `CREATE`.
    #[test]
    fn a_foreign_key_must_reference_a_key_of_its_parent() {
        type Make = fn(&mut Table);
        // (what, DDL added to the parent, the same in the model, parent columns referenced)
        let rows: Vec<(&str, &str, Make, &[&str])> = vec![
            ("the primary key", "", |_| {}, &["id"]),
            ("a column that is no key", "", |_| {}, &["a"]),
            ("the primary key named twice", "", |_| {}, &["id", "id"]),
            (
                "a unique index",
                "CREATE UNIQUE INDEX u ON p(a)",
                |p| {
                    let mut i = index(&["a"]);
                    i.unique = true;
                    p.indexes.insert("u".into(), i);
                },
                &["a"],
            ),
            (
                "a unique constraint of the table",
                "CREATE UNIQUE INDEX u ON p(b)",
                |p| {
                    p.unique.insert(
                        "u".into(),
                        pbps_model::UniqueConstraint {
                            columns: vec!["b".into()],
                            storage_parameters: Default::default(),
                            filegroup: None,
                        },
                    );
                },
                &["b"],
            ),
            (
                "an index that is not unique",
                "CREATE INDEX u ON p(a)",
                |p| {
                    p.indexes.insert("u".into(), index(&["a"]));
                },
                &["a"],
            ),
            (
                "a unique index over part of the rows",
                "CREATE UNIQUE INDEX u ON p(a) WHERE a > 0",
                |p| {
                    let mut i = index(&["a"]);
                    i.unique = true;
                    i.filter = Some("a > 0".into());
                    p.indexes.insert("u".into(), i);
                },
                &["a"],
            ),
            (
                "both columns of a unique pair",
                "CREATE UNIQUE INDEX u ON p(a, b)",
                |p| {
                    let mut i = index(&["a", "b"]);
                    i.unique = true;
                    p.indexes.insert("u".into(), i);
                },
                &["a", "b"],
            ),
            (
                "a unique pair in the other order",
                "CREATE UNIQUE INDEX u ON p(a, b)",
                |p| {
                    let mut i = index(&["a", "b"]);
                    i.unique = true;
                    p.indexes.insert("u".into(), i);
                },
                &["b", "a"],
            ),
            (
                "one column of a unique pair",
                "CREATE UNIQUE INDEX u ON p(a, b)",
                |p| {
                    let mut i = index(&["a", "b"]);
                    i.unique = true;
                    p.indexes.insert("u".into(), i);
                },
                &["a"],
            ),
        ];
        let (mut kept, mut refused) = (0, 0);
        for (what, extra, make, references) in rows {
            let value = |column: &str| match column {
                "id" => "1",
                "a" => "2",
                _ => "3",
            };
            let children: Vec<String> = (0..references.len()).map(|n| format!("x{n}")).collect();
            let ddl = [
                "CREATE TABLE p(id integer primary key, a int, b int); ",
                extra,
                "; CREATE TABLE c(",
                &children
                    .iter()
                    .map(|c| [c.as_str(), " int"].concat())
                    .collect::<Vec<_>>()
                    .join(", "),
                ", FOREIGN KEY (",
                &children.join(", "),
                ") REFERENCES p (",
                &references.join(", "),
                "))",
            ]
            .concat();
            assert!(accepts(&ddl), "{what}: the engine creates the child");
            let write = [
                ddl.as_str(),
                "; INSERT INTO p VALUES (1, 2, 3); INSERT INTO c VALUES (",
                &references
                    .iter()
                    .map(|c| value(c))
                    .collect::<Vec<_>>()
                    .join(", "),
                ")",
            ]
            .concat();
            let engine = accepts(&write);

            let mut parent = Table::default();
            for name in ["id", "a", "b"] {
                parent.columns.insert(name.into(), column());
            }
            parent.columns.get_mut("id").expect("id").ty = "integer".parse().expect("type");
            parent.primary_key = Some(pbps_model::PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
                filegroup: None,
            });
            make(&mut parent);
            let mut child = Table::default();
            for name in &children {
                child.columns.insert(name.as_str().into(), column());
            }
            let mut key = fk(
                &children.iter().map(String::as_str).collect::<Vec<_>>(),
                references,
            );
            key.references_table = TableName::new("main", "p");
            child.foreign_keys.insert("f".into(), key);
            let mut schema = pbps_model::Schema::default();
            schema.tables.insert(TableName::new("main", "p"), parent);
            let found = foreign_key_parents(&TableName::new("main", "c"), &child, &schema);
            assert_eq!(found.is_empty(), engine, "{what}: {}", said(&found));
            kept += usize::from(engine);
            refused += usize::from(!engine);
        }
        assert!(kept >= 3 && refused >= 3, "{kept} kept, {refused} refused");
    }

    /// A subquery is refused in the two other places an expression stands, as
    /// the engine refuses it there.
    #[test]
    fn a_subquery_is_refused_in_a_default_and_in_an_index_filter() {
        assert!(!accepts("CREATE TABLE t(c DEFAULT ((SELECT 1)))"));
        assert!(!accepts(
            "CREATE TABLE t(c); CREATE INDEX i ON t(c) WHERE c IN (SELECT 1)"
        ));
        assert!(!default_value("((SELECT 1))"));
        assert!(!default_value("(1 + (select 2))"));
        assert!(!expression("c IN (SELECT 1)"));
        // Negative: the same shapes without one.
        assert!(accepts("CREATE TABLE t(c DEFAULT ((1)))") && default_value("((1))"));
        assert!(accepts(
            "CREATE TABLE t(c); CREATE INDEX i ON t(c) WHERE c IN (1)"
        ));
    }

    /// Where the check stops, pinned: it reads structure, and nothing here
    /// parses SQL. What an expression may mean is the engine's to refuse, at
    /// apply and inside the plan's transaction. Each row is taken here and
    /// refused by the engine.
    #[test]
    fn what_an_expression_means_is_left_to_the_engine() {
        for (sql, text, is_default) in [
            // A default may not name a column.
            ("CREATE TABLE t(c, d DEFAULT (c))", "(c)", true),
            // A partial index's filter may not call a function that is not deterministic.
            (
                "CREATE TABLE t(c); CREATE INDEX i ON t(c) WHERE random() > 0",
                "random() > 0",
                false,
            ),
            // A check may not name a column the table does not have.
            (
                "CREATE TABLE t(c, CHECK (missing > 0))",
                "missing > 0",
                false,
            ),
        ] {
            assert!(!accepts(sql), "the engine refuses {sql}");
            let taken = if is_default {
                default_value(text)
            } else {
                expression(text)
            };
            assert!(taken, "{text} is structure this check takes");
        }
    }

    /// A case-only rename passes through `__pbps_rename_<uid>`, and SQLite
    /// refuses that step when a table, a view or an index holds the name. No
    /// declared object may therefore start with the prefix, in any case.
    #[test]
    fn a_name_kept_for_a_rename_s_middle_step_is_refused() {
        // The engine's side: each holder of the name blocks the step.
        for holder in [
            "CREATE TABLE __pbps_rename_x(a)",
            "CREATE VIEW __PBPS_RENAME_x AS SELECT 1",
            "CREATE TABLE o(c); CREATE INDEX __pbps_rename_x ON o(c)",
        ] {
            let step = "; CREATE TABLE t(a); ALTER TABLE main.t RENAME TO __pbps_rename_x";
            assert!(accepts(holder), "{holder}");
            assert!(!accepts(&[holder, step].concat()), "{holder}");
        }
        for name in ["__pbps_rename_x", "__PBPS_RENAME_", "__Pbps_Rename_tbl_1"] {
            assert!(!self::table(&TableName::new("main", name), &base()).is_empty());
            let mut table = base();
            table.indexes.insert(name.into(), index(&["c"]));
            assert!(!errors(&table).is_empty(), "an index named {name}");
        }
        // Negative: near the prefix, and the prefix inside a name.
        for name in ["__pbps_rename", "__pbps_renamed", "x__pbps_rename_x"] {
            assert!(self::table(&TableName::new("main", name), &base()).is_empty());
            let mut table = base();
            table.indexes.insert(name.into(), index(&["c"]));
            assert!(errors(&table).is_empty(), "an index named {name}");
        }
    }

    /// The ledger's two names are taken in the one namespace tables, views
    /// and indexes share, so none of the three may carry one, in any case.
    #[test]
    fn a_view_or_an_index_named_like_a_ledger_table_is_refused() {
        // The engine's side: with the ledger in place, each is refused.
        for sql in [
            "CREATE VIEW __pbps_state AS SELECT 1",
            "CREATE VIEW __PBPS_LOCK AS SELECT 1",
            "CREATE TABLE t(c); CREATE INDEX __pbps_lock ON t(c)",
            "CREATE TABLE t(c); CREATE INDEX __Pbps_State ON t(c)",
        ] {
            let ledger = "CREATE TABLE __pbps_state(a); CREATE TABLE __pbps_lock(a); ";
            assert!(!accepts(&[ledger, sql].concat()), "{sql}");
            assert!(accepts(sql), "without the ledger the engine takes {sql}");
        }
        for name in ["__pbps_state", "__PBPS_LOCK"] {
            let mut named = view("SELECT 1");
            named.0 = ModuleId::Named(pbps_model::ObjectName::new("main", name));
            let found = super::module(&named.0, &named.1);
            assert!(
                found.iter().any(is_invalid),
                "view {name}: {}",
                said(&found)
            );
        }
        for name in ["__pbps_lock", "__Pbps_State"] {
            let mut table = base();
            table.indexes.insert(name.into(), index(&["c"]));
            let found = errors(&table);
            assert!(
                found.iter().any(is_invalid),
                "index {name}: {}",
                said(&found)
            );
        }
        // Negative: a name that only starts like one is free.
        let mut near = view("SELECT 1");
        near.0 = ModuleId::Named(pbps_model::ObjectName::new("main", "__pbps_states"));
        assert!(super::module(&near.0, &near.1).is_empty());
        let mut table = base();
        table.indexes.insert("__pbps_locks".into(), index(&["c"]));
        assert!(errors(&table).is_empty(), "{}", said(&errors(&table)));
    }

    /// What SQLite has no object for, and the trigger it has and this does not build.
    #[test]
    fn routines_and_roles_are_refused_as_unsupported_and_triggers_as_not_built() {
        for (kind, sql) in [
            (ModuleKind::Procedure, "CREATE PROCEDURE p() BEGIN END"),
            (ModuleKind::Function, "CREATE FUNCTION f() RETURNS int"),
        ] {
            assert!(!accepts(sql), "{sql}");
            let (id, mut module) = view("SELECT 1");
            module.kind = kind;
            assert!(
                super::module(&id, &module).iter().all(is_unsupported)
                    && !super::module(&id, &module).is_empty(),
                "{kind}"
            );
        }
        assert!(accepts(
            "CREATE TABLE t(a); CREATE TRIGGER g AFTER INSERT ON t BEGIN SELECT 1; END"
        ));
        let (id, mut module) = view("BEGIN SELECT 1; END");
        module.kind = ModuleKind::Trigger;
        assert!(super::module(&id, &module).iter().all(is_not_built));
        assert!(!accepts("CREATE ROLE r") && !accepts("GRANT SELECT ON t TO r"));
        let role = pbps_model::Role::default();
        assert!(is_unsupported(
            &super::role(&pbps_model::RoleName::new("r"), &role)[0]
        ));
    }

    /// The reader gives a constraint the file leaves unnamed the name
    /// `__pbps_unnamed_<kind>_<digest>`; a declared constraint may not take one,
    /// or a declaration would name a constraint the file has not got.
    #[test]
    fn a_constraint_named_like_one_the_reader_makes_is_refused() {
        // The engine's side: it takes any such name, so the rule is ours.
        for sql in [
            "CREATE TABLE t(c, CONSTRAINT __pbps_unnamed_check_x CHECK (c > 0))",
            "CREATE TABLE t(c, CONSTRAINT __PBPS_UNNAMED_unique_x UNIQUE (c))",
            "CREATE TABLE t(c, CONSTRAINT __pbps_unnamed_foreign_x FOREIGN KEY (c) REFERENCES t(c))",
        ] {
            assert!(accepts(sql), "{sql}");
        }
        type Add = fn(&mut Table, &str);
        let kinds: [(&str, Add); 4] = [
            ("check", |t, n| {
                t.checks.insert(
                    n.into(),
                    CheckConstraint {
                        expression: "c > 0".into(),
                    },
                );
            }),
            ("unique", |t, n| {
                t.unique.insert(
                    n.into(),
                    UniqueConstraint {
                        columns: vec!["c".into()],
                        storage_parameters: Default::default(),
                        filegroup: None,
                    },
                );
            }),
            ("foreign key", |t, n| {
                t.foreign_keys.insert(n.into(), fk(&["c"], &["c"]));
            }),
            ("primary key", |t, n| {
                let mut key = primary(&["c"]);
                key.name = Some(n.into());
                t.primary_key = Some(key);
            }),
        ];
        for (what, add) in kinds {
            for name in [
                "__pbps_unnamed_check_x",
                "__PBPS_UNNAMED_",
                "__Pbps_Unnamed_x_2",
            ] {
                let mut table = base();
                add(&mut table, name);
                let said = said(&errors(&table));
                assert!(said.contains("__pbps_unnamed_"), "{what} {name}: {said}");
            }
            // Negative: near the prefix, and the prefix inside a name.
            for name in [
                "__pbps_unnamed",
                "__pbps_unnamedx",
                "x__pbps_unnamed_x",
                "_pbps_unnamed_x",
            ] {
                let mut table = base();
                add(&mut table, name);
                assert!(errors(&table).is_empty(), "{what} named {name}");
            }
        }
    }
}
