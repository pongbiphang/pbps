//! Shared fixtures for the emitter's tests (#2950): builders, the differ's own
//! route to a plan, and the comparison of two catalogs.
//!
//! "The same catalog" is compared on facts, never on `sqlite_schema.sql`, which
//! keeps the text a statement was written in (`ADD COLUMN` appends the new
//! column's text verbatim, a rename adds quotes, `CREATE TABLE main.t` stores
//! `CREATE TABLE "t"`): names and kinds; every column's position, declared type,
//! `NOT NULL`, default and key ordinal; each index's flags and key columns; each
//! foreign key; and, for the facts only the text holds, a canonical form of each
//! CHECK expression (keyed by its constraint name), each partial index
//! predicate and each view body. The canonical form drops comments, plain
//! identifier quotes and case outside strings, and collapses ASCII white space.
//! The splitter that cuts the checks out of the table text is scaffolding for
//! these tests: the catalog reader replaces it.

use pbps_db::sqlite::{Connection, Value};
use pbps_dialect::lower::{lower, lower_all};
use pbps_dialect::{DialectError, Statement};
use pbps_diff::order::PlanView;
use pbps_diff::order::deployment::Sources;
use pbps_diff::order::deployment::schedule::{Declarations, Facts, schedule};
use pbps_model::{
    Change, Column, ColumnType, Hints, IdsFile, Module, ModuleId, ModuleKind, ObjectName, PlanMode,
    PlannedChange, Schema, Table, TableName, Uid, UidKind,
};

use crate::dialect::Sqlite;
use crate::lexis::{self, Piece};
use crate::tests::Fresh;

mod differential;
mod golden;
mod names;
mod plan_dialect;
mod refusals;
mod rename_order;
mod versions;

pub(super) fn ty(text: &str) -> ColumnType {
    text.parse().expect("a type")
}

pub(super) fn col(text: &str) -> Column {
    Column::new(ty(text))
}

pub(super) fn tn(name: &str) -> TableName {
    TableName::new("main", name)
}

pub(super) fn table(columns: &[(&str, Column)]) -> Table {
    let mut table = Table::default();
    for (name, column) in columns {
        table.columns.insert((*name).into(), column.clone());
    }
    table
}

pub(super) fn view(name: &str, body: &str) -> (ModuleId, Module) {
    (
        ModuleId::Named(ObjectName::new("main", name)),
        Module {
            kind: ModuleKind::View,
            description: None,
            options: Default::default(),
            definition: body.to_owned(),
        },
    )
}

pub(super) fn schema(tables: &[(&str, Table)], views: &[(&str, &str)]) -> Schema {
    let mut schema = Schema::default();
    for (name, table) in tables {
        schema.tables.insert(tn(name), table.clone());
    }
    for (name, body) in views {
        let (id, module) = view(name, body);
        schema.modules.insert(id, module);
    }
    schema
}

/// Identities for every table and column of a schema. An object keeps its
/// identity across a rename by taking the same seed: `seeds` maps the key a
/// table (`t`) or a column (`t.b`) has in this schema to the seed it was first
/// given.
pub(super) fn ids(schema: &Schema, seeds: &[(&str, &str)]) -> IdsFile {
    let seed = |key: &str| {
        seeds
            .iter()
            .find(|(k, _)| *k == key)
            .map_or_else(|| key.to_owned(), |(_, s)| (*s).to_owned())
    };
    let mut ids = IdsFile::default();
    for (name, table) in &schema.tables {
        let table_seed = seed(&name.name);
        ids.tables
            .insert(Uid::derived(UidKind::Table, &table_seed, 0), name.clone());
        for column in table.columns.keys() {
            let key = [name.name.as_str(), ".", column.raw()].concat();
            let seeded = seed(&key);
            // A column's seed is its table's seed and its own name: a renamed
            // table keeps its columns.
            let column_seed = if seeded == key {
                [table_seed.as_str(), ".", column.raw()].concat()
            } else {
                seeded
            };
            ids.columns.insert(
                Uid::derived(UidKind::Column, &column_seed, 0),
                name.column(column.raw()),
            );
        }
    }
    ids
}

/// The differ's own route from a base to a declared schema: diff, schedule,
/// order, lower. What a plan would run, statement by statement.
pub(super) fn plan(
    base: (&Schema, &IdsFile),
    declared: (&Schema, &IdsFile),
    dialect: &Sqlite,
) -> Result<Vec<Statement>, String> {
    let before = pbps_diff::Side {
        schema: base.0,
        ids: base.1,
    };
    let hints = Hints::default();
    let planned = pbps_diff::diff(
        before,
        pbps_diff::Side {
            schema: declared.0,
            ids: declared.1,
        },
        dialect,
        &hints,
    )
    .map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let scheduled = schedule(
        planned,
        &Declarations {
            schema: declared.0,
            sources: Sources {
                before,
                after: declared.1,
            },
            annotations: &hints.annotations,
        },
        &Facts::default(),
        dialect,
    )
    .map_err(|error| error.to_string())?;
    let ordered = scheduled.changes.ordered(PlanMode::Transactional, dialect);
    let lowered =
        lower_all(&ordered as &dyn PlanView<Sqlite>, dialect).map_err(|error| error.to_string())?;
    Ok(lowered.statements().to_vec())
}

/// One change as a one-operation plan, through `PlanDialect::emit`.
pub(super) fn statements(dialect: &Sqlite, change: Change) -> Result<Vec<Statement>, DialectError> {
    let ordered =
        pbps_diff::order::singleton(PlannedChange::new(change), PlanMode::Transactional, dialect);
    lower(&ordered as &dyn PlanView<Sqlite>, 0, dialect).map(|lowered| lowered.into_statements())
}

/// Runs statements in order on a connection.
pub(super) fn run(c: &Connection, statements: &[Statement]) -> Result<(), String> {
    for statement in statements {
        c.execute_batch(&statement.sql)
            .map_err(|error| [statement.sql.as_str(), ": ", &error.to_string()].concat())?;
    }
    Ok(())
}

pub(super) fn sql_text(statements: &[Statement]) -> Vec<&str> {
    statements.iter().map(|s| s.sql.as_str()).collect()
}

/// The facts two catalogs are compared on (see the module documentation).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Catalog {
    objects: Vec<String>,
    columns: Vec<String>,
    indexes: Vec<String>,
    foreign_keys: Vec<String>,
    checks: Vec<String>,
    predicates: Vec<String>,
    views: Vec<String>,
}

fn rows(c: &Connection, sql: &str, params: &[Value]) -> Vec<Vec<String>> {
    c.query(sql, params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .iter()
        .map(|row| {
            (0..row.len())
                .map(|i| {
                    row.text(i)
                        .map(str::to_owned)
                        .or_else(|_| row.integer(i).map(|n| n.to_string()))
                        .unwrap_or_else(|_| "NULL".into())
                })
                .collect()
        })
        .collect()
}

/// The comparison form of SQLite text: comments dropped, plain identifier
/// quotes dropped, case folded outside strings, ASCII white space collapsed.
pub(super) fn canonical(text: &str) -> String {
    let mut out = String::new();
    for piece in lexis::pieces(text) {
        match piece {
            Piece::Comment(_) => out.push(' '),
            Piece::Code(code) => out.push_str(&code.to_ascii_lowercase()),
            Piece::Quoted(run) => {
                let inner = &run[1..run.len().saturating_sub(1).max(1)];
                let plain = run.starts_with('"')
                    && !inner.is_empty()
                    && inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if plain {
                    out.push_str(&inner.to_ascii_lowercase());
                } else {
                    out.push_str(run);
                }
            }
        }
    }
    out.split(lexis::is_space)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The top-level clauses of a `CREATE TABLE` body, cut at the commas of its
/// outermost parentheses.
fn clauses(create: &str) -> Vec<String> {
    let mut depth = 0usize;
    let mut current = String::new();
    let mut out = Vec::new();
    for piece in lexis::pieces(create) {
        match piece {
            Piece::Comment(_) => {}
            Piece::Quoted(run) => {
                if depth >= 1 {
                    current.push_str(run);
                }
            }
            Piece::Code(code) => {
                for ch in code.chars() {
                    match ch {
                        '(' => {
                            depth += 1;
                            if depth > 1 {
                                current.push(ch);
                            }
                        }
                        ')' => {
                            if depth > 1 {
                                current.push(ch);
                            }
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                out.push(std::mem::take(&mut current));
                            }
                        }
                        ',' if depth == 1 => out.push(std::mem::take(&mut current)),
                        _ if depth >= 1 => current.push(ch),
                        _ => {}
                    }
                }
            }
        }
    }
    out.into_iter().filter(|c| !c.trim().is_empty()).collect()
}

/// `(constraint name, canonical expression)` of each table-level CHECK.
fn checks_of(create: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for clause in clauses(create) {
        let text = canonical(&clause);
        let (name, rest) = match text.strip_prefix("constraint ") {
            Some(after) => match after.split_once(' ') {
                Some((name, rest)) => (name.to_owned(), rest.to_owned()),
                None => continue,
            },
            None => ("-".to_owned(), text.clone()),
        };
        if let Some(expression) = rest.strip_prefix("check") {
            found.push((name, expression.trim_start().to_owned()));
        }
    }
    found
}

pub(super) fn catalog(c: &Connection) -> Catalog {
    let mut catalog = Catalog {
        objects: Vec::new(),
        columns: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        checks: Vec::new(),
        predicates: Vec::new(),
        views: Vec::new(),
    };
    for row in rows(
        c,
        "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name",
        &[],
    ) {
        let (kind, name, owner, sql) = (&row[0], &row[1], &row[2], &row[3]);
        catalog
            .objects
            .push([kind.as_str(), " ", name, " on ", owner].join(""));
        match kind.as_str() {
            "table" => {
                for (constraint, expression) in checks_of(sql) {
                    catalog
                        .checks
                        .push([name.as_str(), " ", &constraint, " ", &expression].join(""));
                }
                let t = Value::Text(name.clone());
                for r in rows(
                    c,
                    "SELECT cid, name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1) ORDER BY cid",
                    std::slice::from_ref(&t),
                ) {
                    catalog
                        .columns
                        .push([name.as_str(), ": ", &r.join(" | ")].concat());
                }
                for r in rows(
                    c,
                    "SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1) ORDER BY name",
                    std::slice::from_ref(&t),
                ) {
                    catalog
                        .indexes
                        .push([name.as_str(), " list ", &r.join(" | ")].concat());
                }
                for r in rows(
                    c,
                    "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete, match FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
                    std::slice::from_ref(&t),
                ) {
                    catalog
                        .foreign_keys
                        .push([name.as_str(), ": ", &r.join(" | ")].concat());
                }
            }
            "index" => {
                let i = Value::Text(name.clone());
                for r in rows(
                    c,
                    "SELECT seqno, cid, name, desc, coll FROM pragma_index_xinfo(?1) WHERE key = 1 ORDER BY seqno",
                    std::slice::from_ref(&i),
                ) {
                    catalog
                        .indexes
                        .push([name.as_str(), " key ", &r.join(" | ")].concat());
                }
                let text = canonical(sql);
                if let Some((_, predicate)) = text.split_once(" where ") {
                    catalog
                        .predicates
                        .push([name.as_str(), " ", predicate].concat());
                }
            }
            "view" => {
                let text = canonical(sql);
                let body = text
                    .split_once(" as ")
                    .map_or(text.as_str(), |(_, body)| body);
                catalog.views.push([name.as_str(), " ", body].concat());
            }
            _ => {}
        }
    }
    catalog
}

/// A database built by running `sql` on a fresh in-memory engine.
pub(super) fn built(sql: &str) -> Fresh {
    let mut fresh = Fresh::memory();
    fresh
        .connection()
        .execute_batch(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    fresh
}

pub(super) fn catalog_of(fresh: &mut Fresh) -> Catalog {
    catalog(fresh.connection())
}

/// The catalogs agree, or this says where they differ.
pub(super) fn assert_same(left: &Catalog, right: &Catalog, what: &str) {
    assert_eq!(left, right, "{what}");
}

#[test]
fn the_catalog_comparison_sees_each_kind_of_difference() {
    let base = "CREATE TABLE t(a int NOT NULL, b text, CONSTRAINT k CHECK (a > 0)); CREATE INDEX i ON t(b) WHERE b > 'x'; CREATE VIEW v AS SELECT a FROM t";
    let mut reference = built(base);
    let reference = catalog_of(&mut reference);
    let mut same = built(&base.replace("CHECK (a > 0)", "Check /* c */ (a  >  0)"));
    assert_same(
        &catalog_of(&mut same),
        &reference,
        "text differences that are not facts",
    );
    for (what, changed) in [
        (
            "a column's NOT NULL",
            base.replace("a int NOT NULL", "a int"),
        ),
        ("a column's type", base.replace("b text", "b varchar(5)")),
        ("a check's expression", base.replace("a > 0", "a > 1")),
        (
            "a check's name",
            base.replace("CONSTRAINT k", "CONSTRAINT j"),
        ),
        ("a partial predicate", base.replace("b > 'x'", "b > 'y'")),
        (
            "a view's body",
            base.replace("SELECT a FROM t", "SELECT b FROM t"),
        ),
        ("an index's key", base.replace("ON t(b)", "ON t(a)")),
        (
            "a column's default",
            base.replace("b text,", "b text DEFAULT 'd',"),
        ),
        (
            "a column's position",
            base.replace("a int NOT NULL, b text", "b text, a int NOT NULL"),
        ),
    ] {
        let mut other = built(&changed);
        assert_ne!(catalog_of(&mut other), reference, "{what} must be seen");
    }
    // A foreign key.
    let mut with_fk = built(
        "CREATE TABLE p(id integer primary key); CREATE TABLE c(a int, FOREIGN KEY (a) REFERENCES p(id))",
    );
    let mut without = built("CREATE TABLE p(id integer primary key); CREATE TABLE c(a int)");
    assert_ne!(catalog_of(&mut with_fk), catalog_of(&mut without));
}
