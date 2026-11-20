//! What the model cannot hold, and what the reader cannot read: every case is
//! a warning and a limitation that name the object, and the rest of the file
//! is read around it.

use pbps_db::catalog::{LimitationTarget, Pulled};
use pbps_model::{ModuleId, ObjectLocalName, ObjectName, TableName};

use super::super::{RawCatalog, RawView, assemble, read_raw};
use super::{limits, read_sql, table, warned};
use crate::tests::{Fresh, accepts};

/// `name` was not read, and one limitation and a warning say why.
fn refused(pulled: &Pulled, name: &str, why: &str) {
    assert!(
        !pulled
            .schema
            .tables
            .contains_key(&TableName::new("main", name)),
        "`{name}` was read"
    );
    let found = limits(pulled, name);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(found[0].contains(why), "{} does not say `{why}`", found[0]);
    assert!(warned(pulled, why), "{:#?}", pulled.warnings);
}

const KEPT: &str = "CREATE TABLE ok(a integer PRIMARY KEY); ";

/// A table that is refused and the one beside it that is not.
fn beside(sql: &str) -> Pulled {
    assert!(accepts(sql), "the engine takes {sql}");
    let (_, pulled) = read_sql(&[KEPT, sql].concat());
    assert!(
        pulled
            .schema
            .tables
            .contains_key(&TableName::new("main", "ok"))
    );
    pulled
}

#[test]
fn text_the_tokenizer_does_not_understand_leaves_the_table_unread() {
    // A bare identifier as a default is valid SQLite outside the subset.
    let pulled = beside("CREATE TABLE t(a int DEFAULT foo)");
    refused(&pulled, "t", "it is not read");
    assert!(limits(&pulled, "t")[0].contains("table `t`"));
    assert_eq!(pulled.schema.tables.len(), 1);
}

#[test]
fn a_disagreement_between_the_text_and_the_pragmas_leaves_the_table_unread() {
    let mut fresh = Fresh::memory();
    fresh
        .connection()
        .execute_batch(&[KEPT, "CREATE TABLE t(a int, b int NOT NULL)"].concat())
        .expect("tables");
    let mut raw = read_raw(fresh.connection()).expect("reads");
    assert!(
        assemble(&raw)
            .schema
            .tables
            .contains_key(&TableName::new("main", "t"))
    );
    // The engine's side said something the text does not: `a` is NOT NULL.
    let t = raw.tables.iter_mut().find(|t| t.name == "t").expect("t");
    t.facts.columns[0].not_null = true;
    let pulled = assemble(&raw);
    refused(&pulled, "t", "its text and the catalog disagree");
    assert!(
        limits(&pulled, "t")[0].contains("column `a`"),
        "{:?}",
        limits(&pulled, "t")
    );
    assert!(
        pulled
            .schema
            .tables
            .contains_key(&TableName::new("main", "ok"))
    );
}

#[test]
fn a_quoted_empty_type_name_is_a_disagreement_naming_the_column() {
    for declaration in ["a \"\"", "a ''", "a []", "a ``"] {
        let pulled = beside(&format!("CREATE TABLE t(b int, {declaration})"));
        refused(&pulled, "t", "column `a`");
        assert!(
            limits(&pulled, "t")[0].contains("type words"),
            "{declaration}"
        );
    }
    // Negative: no type is the reserved type and is read.
    let (_, pulled) = read_sql("CREATE TABLE t(a)");
    assert_eq!(table(&pulled, "t").columns["a"].ty, super::ty(""));
    assert!(pulled.warnings.is_empty());
}

#[test]
fn a_column_really_declared_untyped_or_blank_is_a_table_not_read_with_the_column_named() {
    let pulled = beside("CREATE TABLE t(b int, a untyped)");
    refused(&pulled, "t", "column `a`");
    assert!(
        limits(&pulled, "t")[0].contains("reserves"),
        "{:?}",
        limits(&pulled, "t")
    );
    let pulled = beside("CREATE TABLE t(b int, a \" \")");
    refused(&pulled, "t", "column `a`");
    assert!(
        limits(&pulled, "t")[0].contains("only white space"),
        "{:?}",
        limits(&pulled, "t")
    );
    // Negative: the word inside a type is an ordinary type, and a type the
    // dialect does not know is refused by name rather than read as another.
    let (_, pulled) = read_sql("CREATE TABLE t(a untyped(5), b untyped int)");
    assert!(pulled.warnings.is_empty(), "{:#?}", pulled.warnings);
}

#[test]
fn two_constraints_named_alike_in_any_ascii_case_leave_the_table_unread() {
    let pulled =
        beside("CREATE TABLE t(a int, CONSTRAINT k CHECK (a > 0), CONSTRAINT K CHECK (a < 9))");
    refused(&pulled, "t", "two of its constraints are named `K`");
    // Negative: names that differ by more than ASCII case are two names.
    let (_, pulled) = read_sql(
        "CREATE TABLE t(a int, CONSTRAINT \"é\" CHECK (a > 0), CONSTRAINT \"É\" CHECK (a < 9))",
    );
    assert_eq!(table(&pulled, "t").checks.len(), 2);
}

#[test]
fn a_name_no_declaration_can_write_leaves_its_table_unread() {
    for sql in [
        "CREATE TABLE t(a int, \"\" int)",
        "CREATE TABLE t(a int, CONSTRAINT \"\" CHECK (a > 0))",
        "CREATE TABLE \"\"(a int)",
    ] {
        let pulled = beside(sql);
        let name = if sql.contains("TABLE \"\"") { "" } else { "t" };
        refused(&pulled, name, "no declaration can write");
    }
    let pulled = beside("CREATE TABLE t(a int); CREATE INDEX \"\" ON t(a)");
    assert!(table(&pulled, "t").indexes.is_empty());
    assert!(limits(&pulled, "t")[0].contains("no declaration can write"));
    let pulled = beside("CREATE VIEW \"\" AS SELECT 1");
    assert!(pulled.schema.modules.is_empty());
    assert_eq!(pulled.unmanaged_modules.len(), 1);
}

#[test]
fn a_virtual_table_is_named_and_not_read_and_its_shadow_tables_are_not_reported() {
    let pulled = beside("CREATE VIRTUAL TABLE r USING rtree(id, x0, x1)");
    refused(&pulled, "r", "virtual table");
    assert!(
        !warned(&pulled, "r_node") && !warned(&pulled, "r_rowid"),
        "{:#?}",
        pulled.warnings
    );
    assert_eq!(pulled.schema.tables.len(), 1);
}

#[test]
fn a_foreign_key_that_cannot_be_read_is_left_out_and_the_table_is_read() {
    for (sql, why) in [
        // Its parent is a table that is not read.
        (
            "CREATE TABLE bad(a int DEFAULT foo); CREATE TABLE t(a int, CONSTRAINT f FOREIGN KEY (a) REFERENCES bad(a))",
            "`bad`, which is not read",
        ),
        // Its parent is not there at all (SQLite takes it, and enforces it later).
        (
            "CREATE TABLE t(a int, CONSTRAINT f FOREIGN KEY (a) REFERENCES nowhere(a))",
            "`nowhere`, which is not read",
        ),
        // No parent column is named and the parent has no primary key.
        (
            "CREATE TABLE p(x int); CREATE TABLE t(a int, CONSTRAINT f FOREIGN KEY (a) REFERENCES p)",
            "that table has no primary key",
        ),
        // A parent column the parent does not have.
        (
            "CREATE TABLE p(x int); CREATE TABLE t(a int, CONSTRAINT f FOREIGN KEY (a) REFERENCES p(zz))",
            "a column that `p` does not have",
        ),
    ] {
        let pulled = beside(sql);
        let t = table(&pulled, "t");
        assert!(t.foreign_keys.is_empty(), "{sql}");
        assert!(t.columns.contains_key("a"));
        let found = limits(&pulled, "t");
        assert_eq!(found.len(), 1, "{sql}: {found:#?}");
        assert!(
            found[0].contains("is left out") && found[0].contains(why),
            "{sql}: {found:?}"
        );
    }
    // Negative: a parent that is read is a foreign key.
    let (_, pulled) = read_sql(
        "CREATE TABLE p(x integer PRIMARY KEY); CREATE TABLE t(a int, CONSTRAINT f FOREIGN KEY (a) REFERENCES p)",
    );
    assert_eq!(table(&pulled, "t").foreign_keys.len(), 1);
    assert!(pulled.limitations.is_empty());
}

#[test]
fn an_index_the_model_cannot_hold_is_left_out_with_its_reason_and_the_table_is_read() {
    for (index, why) in [
        ("CREATE INDEX i ON t(lower(b))", "an expression as a key"),
        (
            "CREATE INDEX i ON t(b COLLATE nocase)",
            "the collation `nocase`",
        ),
        ("CREATE INDEX i ON t(a, (a + 1))", "an expression as a key"),
    ] {
        let pulled = beside(&format!("CREATE TABLE t(a int, b text); {index}"));
        assert!(table(&pulled, "t").indexes.is_empty(), "{index}");
        let found = limits(&pulled, "t");
        assert_eq!(found.len(), 1, "{index}: {found:#?}");
        assert!(
            found[0].contains("index `i` is left out") && found[0].contains(why),
            "{index}: {found:?}"
        );
    }
    // Negative: a plain key, a descending one and a `BINARY` collation are read.
    let (_, pulled) =
        read_sql("CREATE TABLE t(a int, b text); CREATE INDEX i ON t(a DESC, b COLLATE binary)");
    assert_eq!(table(&pulled, "t").indexes.len(), 1);
    assert!(pulled.limitations.is_empty());
}

/// One feature the model has no field for: the table is read, one note says
/// where the feature is, and a constraint whose meaning it would change is out.
#[test]
fn each_marker_is_a_warning_and_a_limitation_and_the_table_is_read_around_it() {
    type Check = fn(&pbps_model::Table);
    const P: &str = "CREATE TABLE p(id integer PRIMARY KEY); ";
    let cases: [(&str, &str, &str, Check); 12] = [
        (
            "",
            "CREATE TABLE t(a int) STRICT",
            "the table is STRICT",
            |t| assert!(t.columns.contains_key("a")),
        ),
        (
            "",
            "CREATE TABLE t(a int PRIMARY KEY) WITHOUT ROWID",
            "the table is WITHOUT ROWID",
            |t| assert!(t.primary_key.is_some()),
        ),
        (
            "",
            "CREATE TABLE t(a integer PRIMARY KEY AUTOINCREMENT)",
            "the primary key (a) has AUTOINCREMENT",
            |t| assert!(t.primary_key.is_some()),
        ),
        (
            "",
            "CREATE TABLE t(a text COLLATE nocase)",
            "column `a` has a collation",
            |t| assert!(t.columns.contains_key("a")),
        ),
        (
            "",
            "CREATE TABLE t(a int, b int AS (a + 1))",
            "column `b` is a generated column",
            |t| assert!(t.columns.contains_key("b")),
        ),
        (
            "",
            "CREATE TABLE t(a text, CONSTRAINT u UNIQUE (a COLLATE nocase))",
            "has a collation on a key column; it is left out",
            |t| assert!(t.unique.is_empty()),
        ),
        (
            "",
            "CREATE TABLE t(a int, b int, PRIMARY KEY (a DESC, b))",
            "has a direction on a key column; it is left out",
            |t| assert!(t.primary_key.is_none()),
        ),
        (
            "",
            "CREATE TABLE t(a int, CONSTRAINT u UNIQUE (a) ON CONFLICT REPLACE)",
            "has an ON CONFLICT clause; it is left out",
            |t| assert!(t.unique.is_empty()),
        ),
        (
            P,
            "CREATE TABLE t(a int, CONSTRAINT f FOREIGN KEY (a) REFERENCES p DEFERRABLE)",
            "is DEFERRABLE or NOT DEFERRABLE; it is left out",
            |t| assert!(t.foreign_keys.is_empty()),
        ),
        (
            P,
            "CREATE TABLE t(a int, CONSTRAINT f FOREIGN KEY (a) REFERENCES p DEFERRABLE INITIALLY DEFERRED)",
            "has INITIALLY; it is left out",
            |t| assert!(t.foreign_keys.is_empty()),
        ),
        (
            P,
            "CREATE TABLE t(a int, CONSTRAINT f FOREIGN KEY (a) REFERENCES p MATCH FULL)",
            "has MATCH; it is left out",
            |t| assert!(t.foreign_keys.is_empty()),
        ),
        (
            "",
            "CREATE TABLE t(a int NOT NULL ON CONFLICT IGNORE)",
            // A NOT NULL is a fact of its column and is still read: the note
            // must not say it was left out.
            "has an ON CONFLICT clause; the model has no field for it",
            |t| assert!(!t.columns["a"].nullable),
        ),
    ];
    for (setup, sql, note, check) in cases {
        let (_, pulled) = read_sql(&[setup, sql].concat());
        check(table(&pulled, "t"));
        let found = limits(&pulled, "t");
        // One note for each marker: a `DEFERRABLE INITIALLY DEFERRED` has two.
        let count = if sql.contains("INITIALLY") { 2 } else { 1 };
        assert_eq!(found.len(), count, "{sql}: {found:#?}");
        assert!(found.iter().any(|f| f.contains(note)), "{sql}: {found:?}");
        assert!(warned(&pulled, note), "{sql}: {:#?}", pulled.warnings);
    }
}

#[test]
fn a_view_with_a_column_list_or_text_that_is_not_understood_is_inventoried_and_not_read() {
    let (_, pulled) =
        read_sql("CREATE TABLE t(a int, b int); CREATE VIEW v(x, y) AS SELECT a, b FROM t");
    let id = ModuleId::Named(ObjectName::new("main", "v"));
    assert!(pulled.schema.modules.is_empty());
    assert_eq!(pulled.unmanaged_modules.len(), 1);
    assert_eq!(pulled.unmanaged_modules[0].kind, "view");
    assert_eq!(
        pulled.unmanaged_modules[0].target,
        LimitationTarget::module(id)
    );
    assert!(warned(&pulled, "view `v` has a column list"));
    assert_eq!(limits(&pulled, "v").len(), 1);
    // The tokenizer refuses what is not a view's stored text; the engine never
    // stores such text, so this is the pure function's own answer.
    let raw = RawCatalog {
        views: vec![RawView {
            name: "w".into(),
            sql: "CREATE VIEW w SELECT 1".into(),
        }],
        ..RawCatalog::default()
    };
    let pulled = assemble(&raw);
    assert!(pulled.schema.modules.is_empty());
    assert_eq!(pulled.unmanaged_modules.len(), 1);
    assert!(
        pulled.unmanaged_modules[0].why.starts_with("view "),
        "{}",
        pulled.unmanaged_modules[0].why
    );
    assert_eq!(limits(&pulled, "w").len(), 1);
}

#[test]
fn a_trigger_is_inventoried_by_its_parent_and_not_read() {
    let (_, pulled) =
        read_sql("CREATE TABLE t(a int); CREATE TRIGGER g AFTER INSERT ON t BEGIN SELECT 1; END");
    assert_eq!(pulled.unmanaged_modules.len(), 1);
    let id = ModuleId::Trigger {
        on: ObjectName::new("main", "t"),
        name: "g".into(),
    };
    assert_eq!(
        pulled.unmanaged_modules[0].target,
        LimitationTarget::module(id)
    );
    assert!(warned(&pulled, "trigger `g` on `t`"));
    assert!(table(&pulled, "t").columns.contains_key("a"));
}

#[test]
fn the_name_a_case_only_rename_passes_through_is_a_notice_and_is_not_read() {
    let (_, pulled) = read_sql("CREATE TABLE __pbps_rename_abc(a int); CREATE TABLE t(a int)");
    assert_eq!(pulled.catalog_notices.len(), 1);
    assert!(pulled.catalog_notices[0].contains("__pbps_rename_abc"));
    assert!(pulled.warnings.is_empty() && pulled.limitations.is_empty());
    assert_eq!(pulled.schema.tables.len(), 1);
}

#[test]
fn an_index_named_for_a_unique_constraint_is_the_constraints_and_not_an_index() {
    let (_, pulled) = read_sql("CREATE TABLE t(a int, CONSTRAINT u UNIQUE (a))");
    let t = table(&pulled, "t");
    assert!(t.indexes.is_empty(), "{:?}", t.indexes);
    assert!(t.unique.contains_key(&ObjectLocalName::new("u")));
}

/// A key over a column with its own collation compares by it, and the model has
/// no field for the collation: the key is left out, never read as an ordinary
/// one. `COLLATE BINARY` is the ordinary one.
#[test]
fn a_key_over_a_collated_column_is_left_out() {
    // The engine's side: the collation is the key's.
    assert!(!accepts(
        "CREATE TABLE t(a TEXT COLLATE NOCASE UNIQUE); INSERT INTO t VALUES ('a'), ('A')"
    ));
    let (_, pulled) = read_sql(
        "CREATE TABLE t(a TEXT COLLATE NOCASE UNIQUE, b TEXT COLLATE nocase, c TEXT COLLATE BINARY, \
         PRIMARY KEY (b, c), CONSTRAINT u UNIQUE (c))",
    );
    let t = table(&pulled, "t");
    assert!(t.primary_key.is_none());
    assert_eq!(t.unique.keys().map(|k| k.raw()).collect::<Vec<_>>(), ["u"]);
    assert!(warned(
        &pulled,
        "is over a column with a collation; it is left out"
    ));
    assert!(!warned(&pulled, "with no name"), "{:#?}", pulled.warnings);
}

/// A foreign key is read only when the columns it references are a key the
/// model holds for the parent: SQLite refuses every write to a child whose
/// parent has no such key, and a declaration may not say one.
#[test]
fn a_foreign_key_to_a_key_that_is_left_out_is_left_out() {
    let (_, pulled) = read_sql(
        "CREATE TABLE p(a TEXT, b int, PRIMARY KEY (a COLLATE NOCASE), CONSTRAINT u UNIQUE (b)); \
         CREATE TABLE c(x REFERENCES p, y int, CONSTRAINT kept FOREIGN KEY (y) REFERENCES p (b), \
         CONSTRAINT loose FOREIGN KEY (y) REFERENCES p (a))",
    );
    assert!(table(&pulled, "p").primary_key.is_none());
    let c = table(&pulled, "c");
    assert_eq!(
        c.foreign_keys.keys().map(|k| k.raw()).collect::<Vec<_>>(),
        ["kept"]
    );
    assert!(warned(&pulled, "are not a key of their table that is read"));
    // The unnamed one that was left out is not listed as read under a name.
    assert!(!warned(&pulled, "with no name"), "{:#?}", pulled.warnings);
    // What is read is a schema the declaration's own rules take.
    let found: Vec<_> = pulled
        .schema
        .tables
        .iter()
        .flat_map(|(n, t)| crate::validate::foreign_key_parents(n, t, &pulled.schema))
        .collect();
    assert!(found.is_empty(), "{found:?}");
}

/// The text is read for what the pragmas do not say, so it has to be this
/// object's: a schema table written by hand can hold another's text, or a
/// table's kind in one place and not the other.
#[test]
fn text_that_is_another_objects_or_another_kind_leaves_the_object_unread() {
    let mut fresh = Fresh::memory();
    fresh
        .connection()
        .execute_batch(
            &[
                KEPT,
                "CREATE TABLE t(a int); CREATE TABLE s(a int PRIMARY KEY) STRICT, WITHOUT ROWID; \
                 CREATE VIEW v AS SELECT 1",
            ]
            .concat(),
        )
        .expect("objects");
    let read = |c: &mut Fresh| read_raw(c.connection()).expect("reads");
    let whole = assemble(&read(&mut fresh));
    assert_eq!(whole.schema.tables.len(), 3);
    assert_eq!(whole.schema.modules.len(), 1);

    let mut raw = read(&mut fresh);
    let t = raw.tables.iter_mut().find(|t| t.name == "t").expect("t");
    t.sql = "CREATE TABLE x(a int)".into();
    refused(&assemble(&raw), "t", "the text creates `x`");

    for flip in [true, false] {
        let mut raw = read(&mut fresh);
        let s = raw.tables.iter_mut().find(|t| t.name == "s").expect("s");
        if flip {
            s.facts.strict = false;
        } else {
            s.facts.without_rowid = false;
        }
        refused(&assemble(&raw), "s", "STRICT or WITHOUT ROWID is in one");
    }

    let mut raw = read(&mut fresh);
    raw.views[0].sql = "CREATE VIEW w AS SELECT 1".into();
    let pulled = assemble(&raw);
    assert!(pulled.schema.modules.is_empty());
    assert!(warned(&pulled, "its text creates `w`"));
}

/// An index or a view under a name this tool keeps for its own tables is valid
/// SQLite and no declaration: it is left out by name, never read as the file's.
#[test]
fn an_index_or_a_view_under_a_name_the_tool_keeps_is_left_out_by_name() {
    let pulled = beside(
        "CREATE TABLE t(a int); CREATE INDEX __pbps_state ON t(a); CREATE INDEX __pbps_rename_i ON t(a); \
         CREATE INDEX kept ON t(a); CREATE VIEW __pbps_rename_v AS SELECT 1; CREATE VIEW seen AS SELECT 1",
    );
    let t = table(&pulled, "t");
    assert_eq!(
        t.indexes.keys().map(|k| k.raw()).collect::<Vec<_>>(),
        ["kept"]
    );
    assert_eq!(limits(&pulled, "t").len(), 2);
    assert!(warned(
        &pulled,
        "its name is one this tool keeps for its own tables"
    ));
    assert_eq!(pulled.schema.modules.len(), 1);
    assert_eq!(pulled.unmanaged_modules.len(), 1);
    // A view under one of the ledger's names is the file's own object, and is
    // said so; only a table there is the ledger.
    let (_, pulled) = read_sql("CREATE VIEW __pbps_state AS SELECT 1");
    assert!(pulled.schema.modules.is_empty());
    assert_eq!(pulled.unmanaged_modules.len(), 1);
}
