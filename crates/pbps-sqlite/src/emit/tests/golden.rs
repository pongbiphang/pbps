//! Goldens for each in-place statement the emitter writes (#2950): the exact
//! text, then the same statements run on a real SQLite and the catalog compared
//! with a database created directly in the target shape, in both directions
//! where the change has an inverse.

use pbps_dialect::{Created, Released};
use pbps_model::{
    Change, CheckConstraint, ColumnChange, ForeignKey, Index, IndexChange, IndexColumn,
    ModuleChange, ModuleKind, ObjectLocalName, PrimaryKey, ReferentialAction, Table, TableChange,
    UniqueConstraint,
};

use super::*;
use crate::dialect::Sqlite;

pub(crate) trait WithDefault {
    fn with_default(self, text: &str) -> Self;
}

impl WithDefault for Column {
    fn with_default(mut self, text: &str) -> Self {
        self.value_source = pbps_model::ValueSource::Default(text.to_owned());
        self
    }
}

pub(crate) fn uid(kind: UidKind, seed: &str) -> Uid {
    Uid::derived(kind, seed, 0)
}

fn names(columns: &[&str]) -> Vec<pbps_model::ColumnName> {
    columns.iter().map(|c| (*c).into()).collect()
}

pub(crate) fn index(columns: &[&str], unique: bool, filter: Option<&str>) -> Index {
    Index {
        columns: columns.iter().map(|c| IndexColumn::column(*c)).collect(),
        include: Vec::new(),
        unique,
        filter: filter.map(str::to_owned),
        method: Default::default(),
        storage_parameters: Default::default(),
        filegroup: None,
    }
}

pub(super) fn create_table_change(name: &str, table: &Table) -> Change {
    Change::Table(TableChange::CreateTable {
        uid: uid(UidKind::Table, name),
        name: tn(name),
        table: Box::new(table.clone()),
        beside_default: None,
        moves_at_most: None,
    })
}

pub(super) fn add_column(table: &str, name: &str, column: Column) -> Change {
    Change::Column(ColumnChange::AddColumn {
        uid: uid(UidKind::Column, &[table, ".", name].concat()),
        table: tn(table),
        name: name.into(),
        column: Box::new(column),
    })
}

fn sqlite() -> Sqlite {
    Sqlite::default()
}

fn sql_of(change: Change) -> Vec<String> {
    statements(&sqlite(), change)
        .expect("emits")
        .into_iter()
        .map(|s| s.sql)
        .collect()
}

/// A table with every kind of constraint, and two indexes.
fn rich() -> Table {
    let mut t = table(&[
        ("id", col("integer").not_null()),
        ("owner", col("integer").not_null()),
        ("name", col("text").with_default("'x'")),
        ("qty", col("integer")),
    ]);
    t.primary_key = Some(PrimaryKey {
        name: None,
        columns: names(&["id"]),
        storage_parameters: Default::default(),
        filegroup: None,
    });
    t.unique.insert(
        ObjectLocalName::new("u_name"),
        UniqueConstraint {
            columns: names(&["owner", "name"]),
            storage_parameters: Default::default(),
            filegroup: None,
        },
    );
    t.checks.insert(
        ObjectLocalName::new("c_qty"),
        CheckConstraint {
            expression: "qty >= 0".into(),
        },
    );
    t.foreign_keys.insert(
        ObjectLocalName::new("f_owner"),
        ForeignKey {
            columns: names(&["owner"]),
            references_table: tn("owners"),
            references_columns: names(&["id"]),
            on_delete: ReferentialAction::Cascade,
            on_update: ReferentialAction::NoAction,
        },
    );
    t.indexes.insert(
        ObjectLocalName::new("i_name"),
        index(&["name"], false, None),
    );
    t.indexes.insert(
        ObjectLocalName::new("i_qty"),
        index(&["owner", "qty"], true, Some("qty IS NOT NULL")),
    );
    t
}

#[test]
fn create_table_writes_every_constraint_inline_and_each_index_after_it() {
    let sql = sql_of(create_table_change("t", &rich()));
    assert_eq!(sql.len(), 3, "{sql:#?}");
    assert_eq!(
        sql[0],
        "CREATE TABLE main.\"t\" (\n    \"id\" integer NOT NULL,\n    \"owner\" integer NOT NULL,\n    \"name\" text DEFAULT 'x',\n    \"qty\" integer,\n    PRIMARY KEY (\"id\"),\n    CONSTRAINT \"u_name\" UNIQUE (\"owner\", \"name\"),\n    CONSTRAINT \"c_qty\" CHECK (qty >= 0),\n    CONSTRAINT \"f_owner\" FOREIGN KEY (\"owner\") REFERENCES \"owners\" (\"id\") ON DELETE CASCADE ON UPDATE NO ACTION\n);"
    );
    assert_eq!(sql[1], "CREATE INDEX main.\"i_name\" ON \"t\" (\"name\");");
    assert_eq!(
        sql[2],
        "CREATE UNIQUE INDEX main.\"i_qty\" ON \"t\" (\"owner\", \"qty\") WHERE qty IS NOT NULL;"
    );
}

#[test]
fn create_table_builds_the_table_a_hand_written_statement_builds() {
    let statements = statements(&sqlite(), create_table_change("t", &rich())).expect("emits");
    let mut made = Fresh::memory();
    made.connection()
        .execute_batch("CREATE TABLE owners(id integer primary key)")
        .expect("parent");
    run(made.connection(), &statements).expect("runs");
    let mut by_hand = built(
        "CREATE TABLE owners(id integer primary key);
         CREATE TABLE t(id integer not null, owner integer not null, name text default 'x', qty integer,
           primary key (id), constraint u_name unique (owner, name), constraint c_qty check (qty >= 0),
           constraint f_owner foreign key (owner) references owners(id) on delete cascade on update no action);
         CREATE INDEX i_name ON t(name);
         CREATE UNIQUE INDEX i_qty ON t(owner, qty) WHERE qty IS NOT NULL",
    );
    assert_same(
        &catalog_of(&mut made),
        &catalog_of(&mut by_hand),
        "create table",
    );
}

#[test]
fn create_table_records_the_table_it_creates() {
    let created = statements(&sqlite(), create_table_change("t", &rich())).expect("emits");
    assert_eq!(created[0].creates, vec![Created::Table(tn("t"))]);
    assert!(created[1].creates.is_empty() && created[2].creates.is_empty());
}

#[test]
fn drop_table_and_its_inverse_round_trip_through_the_catalog() {
    let t = table(&[("a", col("integer"))]);
    let drop = Change::Table(TableChange::DropTable {
        uid: uid(UidKind::Table, "t"),
        name: tn("t"),
        detach_from: None,
    });
    assert_eq!(sql_of(drop.clone()), vec!["DROP TABLE main.\"t\";"]);
    let mut db = built("CREATE TABLE t(a integer)");
    run(
        db.connection(),
        &statements(&sqlite(), drop).expect("emits"),
    )
    .expect("runs");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built("")),
        "drop table",
    );
    run(
        db.connection(),
        &statements(&sqlite(), create_table_change("t", &t)).expect("emits"),
    )
    .expect("runs");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built("CREATE TABLE t(a integer)")),
        "its inverse",
    );
}

fn rename_table(from: &str, to: &str) -> Change {
    Change::Table(TableChange::RenameTable {
        uid: uid(UidKind::Table, from),
        from: tn(from),
        to: tn(to),
        defaults: Vec::new(),
    })
}

#[test]
fn rename_table_writes_the_new_name_bare_and_records_the_rename() {
    let out = statements(&sqlite(), rename_table("t", "u")).expect("emits");
    assert_eq!(out[0].sql, "ALTER TABLE main.\"t\" RENAME TO \"u\";");
    assert_eq!(out[0].renames.len(), 1);
    let mut db = built(
        "CREATE TABLE t(a integer primary key); CREATE TABLE c(a integer references t(a)); CREATE INDEX i ON t(a)",
    );
    run(db.connection(), &out).expect("runs");
    let mut direct = built(
        "CREATE TABLE u(a integer primary key); CREATE TABLE c(a integer references u(a)); CREATE INDEX i ON u(a)",
    );
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut direct),
        "rename: indexes and foreign keys follow",
    );
    run(
        db.connection(),
        &statements(&sqlite(), rename_table("u", "t")).expect("emits"),
    )
    .expect("back");
    let mut original = built(
        "CREATE TABLE t(a integer primary key); CREATE TABLE c(a integer references t(a)); CREATE INDEX i ON t(a)",
    );
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut original),
        "rename and back",
    );
}

#[test]
fn add_column_forms_and_their_catalogs() {
    let cases: [(&str, Column, &str, &str); 4] = [
        (
            "a nullable column",
            col("text"),
            "ALTER TABLE main.\"t\" ADD COLUMN \"x\" text;",
            "x text",
        ),
        (
            "a NOT NULL column with a default",
            col("integer").not_null().with_default("0"),
            "ALTER TABLE main.\"t\" ADD COLUMN \"x\" integer NOT NULL DEFAULT 0;",
            "x integer not null default 0",
        ),
        (
            "a nullable column with a default",
            col("text").with_default("'d'"),
            "ALTER TABLE main.\"t\" ADD COLUMN \"x\" text DEFAULT 'd';",
            "x text default 'd'",
        ),
        (
            "a boolean default",
            col("boolean").not_null().with_default("TRUE"),
            "ALTER TABLE main.\"t\" ADD COLUMN \"x\" boolean NOT NULL DEFAULT TRUE;",
            "x boolean not null default TRUE",
        ),
    ];
    for (what, column, golden, hand) in cases {
        let out = statements(&sqlite(), add_column("t", "x", column)).expect(what);
        assert_eq!(out[0].sql, golden, "{what}");
        assert_eq!(
            out[0].creates,
            vec![Created::Column(tn("t"), "x".into())],
            "{what}"
        );
        let mut db = built("CREATE TABLE t(a integer); INSERT INTO t VALUES (1)");
        run(db.connection(), &out).unwrap_or_else(|e| panic!("{what}: {e}"));
        let mut direct = built(
            &[
                "CREATE TABLE t(a integer, ",
                hand,
                "); INSERT INTO t(a) VALUES (1)",
            ]
            .concat(),
        );
        assert_same(&catalog_of(&mut db), &catalog_of(&mut direct), what);
    }
}

#[test]
fn rename_column_rewrites_what_names_it() {
    let change = Change::Column(ColumnChange::RenameColumn {
        uid: uid(UidKind::Column, "t.b"),
        table: tn("t"),
        from: "b".into(),
        to: "c".into(),
        table_was: None,
    });
    let out = statements(&sqlite(), change).expect("emits");
    assert_eq!(
        out[0].sql,
        "ALTER TABLE main.\"t\" RENAME COLUMN \"b\" TO \"c\";"
    );
    let setup = |name: &str| {
        [
            "CREATE TABLE t(a integer, ",
            name,
            " text, CONSTRAINT k CHECK (",
            name,
            " <> 'z')); CREATE INDEX i ON t(",
            name,
            ") WHERE ",
            name,
            " > 'a'; CREATE VIEW v AS SELECT ",
            name,
            " FROM t",
        ]
        .concat()
    };
    let mut db = built(&setup("b"));
    run(db.connection(), &out).expect("runs");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built(&setup("c"))),
        "rename column",
    );
}

#[test]
fn index_create_and_drop_are_each_others_inverse() {
    let add = Change::Index(IndexChange::AddIndex {
        table: tn("t"),
        name: ObjectLocalName::new("i"),
        index: Box::new(index(&["a", "b"], true, Some("b IS NOT NULL"))),
        clustered: false,
    });
    let drop = Change::Index(IndexChange::DropIndex {
        table: tn("t"),
        name: ObjectLocalName::new("i"),
    });
    let created = sql_of(add.clone());
    assert_eq!(
        created,
        vec!["CREATE UNIQUE INDEX main.\"i\" ON \"t\" (\"a\", \"b\") WHERE b IS NOT NULL;"]
    );
    assert_eq!(sql_of(drop.clone()), vec!["DROP INDEX main.\"i\";"]);
    let mut db = built("CREATE TABLE t(a integer, b integer)");
    run(db.connection(), &statements(&sqlite(), add).expect("emits")).expect("create");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built(
            "CREATE TABLE t(a integer, b integer); CREATE UNIQUE INDEX i ON t(a, b) WHERE b IS NOT NULL",
        )),
        "create index",
    );
    run(
        db.connection(),
        &statements(&sqlite(), drop).expect("emits"),
    )
    .expect("drop");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built("CREATE TABLE t(a integer, b integer)")),
        "drop index",
    );
}

#[test]
fn a_descending_key_is_written_and_kept() {
    let mut i = index(&["a"], false, None);
    i.columns[0].descending = true;
    let add = Change::Index(IndexChange::AddIndex {
        table: tn("t"),
        name: ObjectLocalName::new("i"),
        index: Box::new(i),
        clustered: false,
    });
    let out = statements(&sqlite(), add).expect("emits");
    assert_eq!(out[0].sql, "CREATE INDEX main.\"i\" ON \"t\" (\"a\" DESC);");
    let mut db = built("CREATE TABLE t(a integer)");
    run(db.connection(), &out).expect("runs");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built(
            "CREATE TABLE t(a integer); CREATE INDEX i ON t(a DESC)",
        )),
        "descending",
    );
}

fn module_change(kind: &str, body: &str) -> Change {
    let (id, module) = view("v", body);
    Change::Module(match kind {
        "create" => ModuleChange::CreateModule {
            id,
            module: Box::new(module),
        },
        "alter" => ModuleChange::AlterModule {
            id,
            module: Box::new(module),
        },
        _ => ModuleChange::DropModule {
            id,
            kind: ModuleKind::View,
        },
    })
}

#[test]
fn views_are_created_replaced_by_drop_and_create_and_dropped() {
    assert_eq!(
        sql_of(module_change("create", "SELECT a FROM t")),
        vec!["CREATE VIEW main.\"v\" AS SELECT a FROM t;"]
    );
    assert_eq!(
        sql_of(module_change("alter", "SELECT b FROM t")),
        vec![
            "DROP VIEW main.\"v\";",
            "CREATE VIEW main.\"v\" AS SELECT b FROM t;"
        ]
    );
    assert_eq!(
        sql_of(module_change("drop", "")),
        vec!["DROP VIEW main.\"v\";"]
    );
    let dropped = statements(&sqlite(), module_change("drop", "")).expect("emits");
    assert_eq!(dropped[0].releases.len(), 1);
    let altered = statements(&sqlite(), module_change("alter", "SELECT b FROM t")).expect("emits");
    assert!(
        matches!(altered[0].releases[0], Released::Module(_)),
        "the drop releases the name the create takes"
    );
    let mut db = built("CREATE TABLE t(a integer, b integer); CREATE VIEW v AS SELECT a FROM t");
    run(db.connection(), &altered).expect("alter");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built(
            "CREATE TABLE t(a integer, b integer); CREATE VIEW v AS SELECT b FROM t",
        )),
        "alter view",
    );
    run(
        db.connection(),
        &statements(&sqlite(), module_change("drop", "")).expect("emits"),
    )
    .expect("drop");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built("CREATE TABLE t(a integer, b integer)")),
        "drop view",
    );
}

#[test]
fn a_view_body_ending_in_a_line_comment_does_not_swallow_the_terminator() {
    let out = sql_of(module_change("create", "SELECT 1 -- note"));
    assert_eq!(out, vec!["CREATE VIEW main.\"v\" AS SELECT 1 -- note\n;"]);
    let mut db = built("");
    db.connection().execute_batch(&out[0]).expect("runs");
}

#[test]
fn the_deprecation_marker_is_declaration_metadata_and_emits_nothing() {
    let change = Change::Column(ColumnChange::SetColumnDeprecated {
        uid: uid(UidKind::Column, "t.a"),
        column: pbps_model::ColumnRef {
            table: tn("t"),
            name: "a".into(),
        },
        reason: None,
    });
    assert_eq!(statements(&sqlite(), change), Ok(Vec::new()));
}
