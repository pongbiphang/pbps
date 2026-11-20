//! What a file says, read as the model: the structure the pragmas report and
//! the text only the stored `CREATE` holds, put together.

use pbps_model::{
    ColumnName, ModuleId, ObjectLocalName, ObjectName, ReferentialAction, TableName, ValueSource,
};

use super::{limits, read, read_sql, table, ty, warned};
use crate::tests::Fresh;

pub(super) const FILE: &str = "
CREATE TABLE owners(id integer PRIMARY KEY, name text NOT NULL);
CREATE TABLE t(
    id integer NOT NULL,
    Owner integer NOT NULL,
    name text DEFAULT 'x',
    qty integer,
    other integer,
    PRIMARY KEY (id, owner),
    CONSTRAINT u_name UNIQUE (owner, NAME),
    CONSTRAINT c_qty CHECK (qty >= 0),
    CONSTRAINT f_owner FOREIGN KEY (owner) REFERENCES owners(id) ON DELETE CASCADE ON UPDATE SET NULL,
    CONSTRAINT f_other FOREIGN KEY (other) REFERENCES owners
);
CREATE INDEX i_name ON t(name);
CREATE UNIQUE INDEX i_qty ON t(owner, qty DESC) WHERE qty IS NOT NULL;
CREATE VIEW v AS SELECT id FROM t;
";

#[test]
fn a_file_reads_as_the_model_it_says() {
    let (_, pulled) = read_sql(FILE);
    assert!(pulled.warnings.is_empty(), "{:#?}", pulled.warnings);
    assert!(pulled.limitations.is_empty() && pulled.unmanaged_modules.is_empty());
    assert!(pulled.catalog_notices.is_empty());
    let t = table(&pulled, "t");
    // Columns in the order written, spelled as the table spells them (a clause
    // may spell a name in another case), with their types, nullability and defaults.
    let columns: Vec<_> = t.columns.keys().map(ColumnName::raw).collect();
    assert_eq!(columns, ["id", "Owner", "name", "qty", "other"]);
    assert_eq!(t.columns["name"].ty, ty("text"));
    assert!(!t.columns["id"].nullable && !t.columns["Owner"].nullable);
    assert!(t.columns["qty"].nullable && t.columns["other"].nullable);
    assert_eq!(
        t.columns["name"].value_source,
        ValueSource::Default("'x'".into())
    );
    assert_eq!(t.columns["qty"].value_source, ValueSource::None);
    let pk = t.primary_key.as_ref().expect("a primary key");
    assert_eq!(pk.name, None);
    assert_eq!(pk.columns, ["id".into(), "Owner".into()]);
    let u = &t.unique[&ObjectLocalName::new("u_name")];
    assert_eq!(u.columns, ["Owner".into(), "name".into()]);
    assert_eq!(
        t.checks[&ObjectLocalName::new("c_qty")].expression,
        "qty >= 0"
    );
    let f = &t.foreign_keys[&ObjectLocalName::new("f_owner")];
    assert_eq!(f.columns, ["Owner".into()]);
    assert_eq!(f.references_table, TableName::new("main", "owners"));
    assert_eq!(f.references_columns, ["id".into()]);
    assert_eq!(
        (f.on_delete, f.on_update),
        (ReferentialAction::Cascade, ReferentialAction::SetNull)
    );
    // A reference that names no parent column is the parent's primary key.
    let f = &t.foreign_keys[&ObjectLocalName::new("f_other")];
    assert_eq!(f.references_columns, ["id".into()]);
    assert_eq!(
        (f.on_delete, f.on_update),
        (ReferentialAction::NoAction, ReferentialAction::NoAction)
    );
    let i = &t.indexes[&ObjectLocalName::new("i_name")];
    assert!(!i.unique && i.filter.is_none() && i.columns.len() == 1);
    let i = &t.indexes[&ObjectLocalName::new("i_qty")];
    assert!(i.unique && i.columns[1].descending && !i.columns[0].descending);
    assert_eq!(i.filter.as_deref(), Some("qty IS NOT NULL"));
    let v = &pulled.schema.modules[&ModuleId::Named(ObjectName::new("main", "v"))];
    assert_eq!(v.definition, "SELECT id FROM t");
    // The parent was read as well.
    let owners = table(&pulled, "owners");
    assert_eq!(
        owners.primary_key.as_ref().expect("a key").columns,
        ["id".into()]
    );
    assert!(owners.indexes.is_empty());
}

#[test]
fn the_read_does_not_depend_on_the_order_things_were_made_in() {
    let (_, forward) = read_sql(FILE);
    let (_, again) = {
        let mut fresh = Fresh::memory();
        fresh.connection().execute_batch(FILE).expect("file");
        let pulled = read(&mut fresh);
        (fresh, pulled)
    };
    assert_eq!(forward, again, "reading twice");
    // The parents first and the objects that name them after: the same read.
    let (_, reordered) = read_sql(
        "CREATE TABLE a(id integer PRIMARY KEY); CREATE TABLE b(x integer REFERENCES a(id) ON DELETE CASCADE); \
         CREATE VIEW v AS SELECT 1; CREATE INDEX ib ON b(x)",
    );
    let (_, other) = read_sql(
        "CREATE VIEW v AS SELECT 1; CREATE TABLE b(x integer CONSTRAINT __pbps_x REFERENCES a(id) ON DELETE CASCADE); \
         CREATE TABLE a(id integer PRIMARY KEY); CREATE INDEX ib ON b(x)",
    );
    assert_eq!(reordered.schema.modules, other.schema.modules);
    assert_eq!(table(&reordered, "a"), table(&other, "a"));
    assert_eq!(table(&reordered, "b").columns, table(&other, "b").columns);
}

#[test]
fn an_empty_database_is_an_empty_read_and_not_an_error() {
    let (_, pulled) = read_sql("");
    assert_eq!(pulled, pbps_db::catalog::Pulled::default());
}

#[test]
fn a_clause_that_spells_a_name_in_another_case_is_read_as_the_columns_own_name() {
    let (_, pulled) = read_sql(
        "CREATE TABLE P(Id integer PRIMARY KEY); \
         CREATE TABLE t(Aa integer, b integer, CONSTRAINT u UNIQUE (aA, B), CONSTRAINT f FOREIGN KEY (AA) REFERENCES p(ID))",
    );
    let t = table(&pulled, "t");
    assert_eq!(
        t.unique[&ObjectLocalName::new("u")].columns,
        ["Aa".into(), "b".into()]
    );
    let f = &t.foreign_keys[&ObjectLocalName::new("f")];
    assert_eq!(f.columns, ["Aa".into()]);
    assert_eq!(f.references_table, TableName::new("main", "P"));
    assert_eq!(f.references_columns, ["Id".into()]);
}

#[test]
fn text_is_returned_as_the_engine_stored_it_after_a_rename() {
    let (mut fresh, before) =
        read_sql("CREATE TABLE \"t\" (\"qty\" integer, CONSTRAINT \"c\" CHECK (qty >= 0))");
    assert_eq!(
        table(&before, "t").checks[&ObjectLocalName::new("c")].expression,
        "qty >= 0"
    );
    fresh
        .connection()
        .execute_batch("ALTER TABLE t RENAME COLUMN qty TO \"quantity\"")
        .expect("rename");
    let after = read(&mut fresh);
    // The engine rewrote the name in the stored text and quoted it; the reader
    // returns what is stored and does not undo that.
    let rows = fresh
        .connection()
        .query("SELECT sql FROM sqlite_schema WHERE name = 't'", &[])
        .expect("stored");
    let stored = rows[0].text(0).expect("text").to_owned();
    assert!(stored.contains("CHECK (\"quantity\" >= 0)"), "{stored}");
    assert_eq!(
        table(&after, "t").checks[&ObjectLocalName::new("c")].expression,
        "\"quantity\" >= 0"
    );
}

#[test]
fn an_index_filter_and_a_view_body_keep_the_comment_the_engine_kept() {
    let (_, pulled) = read_sql(
        "CREATE TABLE t(a integer, b integer); CREATE INDEX i ON t(a) WHERE b > 0 -- why\n; \
         CREATE VIEW v AS SELECT a FROM t -- note\n",
    );
    let i = &table(&pulled, "t").indexes[&ObjectLocalName::new("i")];
    // A line comment at the end is closed with a line feed, so what a plan
    // writes after it is not read as part of it.
    assert_eq!(i.filter.as_deref(), Some("b > 0 -- why\n"));
    let v = &pulled.schema.modules[&ModuleId::Named(ObjectName::new("main", "v"))];
    assert!(v.definition.ends_with("-- note"), "{:?}", v.definition);
}

#[test]
fn the_ledger_and_the_engines_own_tables_are_not_the_files_schema_and_are_not_a_warning() {
    let (_, pulled) = read_sql(
        "CREATE TABLE __pbps_state(a); CREATE TABLE __pbps_lock(a); \
         CREATE TABLE s(id integer PRIMARY KEY AUTOINCREMENT); INSERT INTO s DEFAULT VALUES",
    );
    let names: Vec<_> = pulled
        .schema
        .tables
        .keys()
        .map(|n| n.name.as_str())
        .collect();
    assert_eq!(names, ["s"]);
    // `sqlite_sequence` exists now and is nobody's table; AUTOINCREMENT is the
    // only thing said.
    assert!(!warned(&pulled, "sqlite_sequence") && !warned(&pulled, "__pbps"));
    assert!(warned(&pulled, "AUTOINCREMENT"));
    assert_eq!(limits(&pulled, "s").len(), 1);
}
