//! The rehearsal's engine half against a real SQLite: the copy, the plan run on
//! it as an apply runs it, and what is asked of it afterwards.
//!
//! The statement loop and the comparison are the command line's and are driven by
//! hand here: the differ's own route makes the statements, a transaction of the
//! apply's framing runs them, and the reader reads the copy back.

use std::time::{Duration, Instant};

use pbps_db::DbError;
use pbps_db::catalog::Pulled;
use pbps_db::sqlite::Connection;
use pbps_dialect::Statement;
use pbps_model::{CheckConstraint, IdsFile, Schema};

use super::{
    About, Detail, Renamed, Resolution, SCRATCH_LIMIT, Unresolved, explain_copy, resolution,
    scratch, scratch_within,
};
use crate::TRANSACTION;
use crate::catalog::introspect;
use crate::dialect::Sqlite;
use crate::emit::tests::golden::{WithDefault, index};
use crate::emit::tests::{col, ids, plan, table, tn};
use crate::tests::Fresh;

/// (what, the database, the declarations, the declarations' identity seeds)
type Case<'a> = (&'a str, &'a str, &'a str, &'a [(&'a str, &'a str)]);

/// A statement the engine refused: its place in the plan (from 1), its text, and
/// the engine's answer. The copy is returned too, so a test can look at what a
/// refused plan left.
#[derive(Debug)]
struct Refused {
    at: usize,
    sql: String,
    error: DbError,
    copy: Connection,
}

/// A rehearsal that ran to its end.
struct Ran {
    statements: Vec<Statement>,
    read_back: Pulled,
    new: Vec<String>,
}

fn declared_from(sql: &str) -> Schema {
    let mut fresh = Fresh::memory();
    fresh.connection().execute_batch(sql).expect("declared");
    introspect(fresh.connection()).expect("reads").schema
}

/// A file database made by `sql`.
fn file(sql: &str) -> Fresh {
    let mut fresh = Fresh::file();
    fresh.connection().execute_batch(sql).expect("a database");
    fresh
}

/// Runs the plan from the file's own schema to `declared` on a copy of the file,
/// as an apply runs it: one `BEGIN IMMEDIATE`, each statement, the commit.
fn rehearse(
    db: &mut Fresh,
    declared: &Schema,
    declared_ids: &IdsFile,
) -> Result<Ran, Box<Refused>> {
    let base = introspect(db.connection()).expect("reads");
    let statements = plan(
        (&base.schema, &ids(&base.schema, &[])),
        (declared, declared_ids),
        &Sqlite::default(),
    )
    .expect("plans");
    let mut copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    let before = resolution(&copy).expect("asked");
    copy.execute_batch(&TRANSACTION.begin).expect("begins");
    for (index, statement) in statements.iter().enumerate() {
        if let Err(error) = copy.execute_batch(&statement.sql) {
            let _ = copy.execute_batch(&TRANSACTION.rollback);
            return Err(Box::new(Refused {
                at: index + 1,
                sql: statement.sql.clone(),
                error,
                copy,
            }));
        }
    }
    copy.execute_batch(&TRANSACTION.commit).expect("commits");
    let after = resolution(&copy).expect("asked");
    let read_back = introspect(&mut copy).expect("reads");
    Ok(Ran {
        statements,
        read_back,
        new: after.new_since(&before, &Renamed::default()),
    })
}

/// Whether the differ finds nothing left to do between the copy as read back and
/// the declarations, both under the identities their names give.
fn converged(ran: &Ran, declared: &Schema) -> bool {
    plan(
        (&ran.read_back.schema, &ids(&ran.read_back.schema, &[])),
        (declared, &ids(declared, &[])),
        &Sqlite::default(),
    )
    .expect("plans")
    .is_empty()
}

fn count(c: &Connection, sql: &str) -> i64 {
    c.query(sql, &[]).expect("query")[0]
        .integer(0)
        .expect("integer")
}

fn refusal(result: Result<Ran, Box<Refused>>) -> Box<Refused> {
    match result {
        Err(refused) => refused,
        Ok(ran) => panic!("expected the engine to refuse: {:#?}", ran.statements),
    }
}

#[test]
fn a_plan_the_engine_takes_ends_on_the_declarations_and_leaves_the_file_alone() {
    // (what, the database, the declarations, the declarations' identity seeds)
    let cases: [Case; 8] = [
        (
            "add a column",
            "CREATE TABLE t(a integer); INSERT INTO t VALUES (1)",
            "CREATE TABLE t(a integer, b text)",
            &[],
        ),
        (
            "drop a column",
            "CREATE TABLE t(a integer, b text); INSERT INTO t VALUES (1, 'x')",
            "CREATE TABLE t(a integer)",
            &[],
        ),
        (
            "add an index",
            "CREATE TABLE t(a integer); INSERT INTO t VALUES (1)",
            "CREATE TABLE t(a integer); CREATE INDEX i ON t(a)",
            &[],
        ),
        (
            "add a view",
            "CREATE TABLE t(a integer)",
            "CREATE TABLE t(a integer); CREATE VIEW v AS SELECT a FROM t",
            &[],
        ),
        (
            "rename a column with a view that follows it",
            "CREATE TABLE t(a integer, b text); CREATE VIEW v AS SELECT b FROM t",
            "CREATE TABLE t(a integer, c text); CREATE VIEW v AS SELECT c FROM t",
            &[("t.c", "t.b")],
        ),
        (
            "drop a table",
            "CREATE TABLE t(a integer); CREATE TABLE u(a integer); INSERT INTO u VALUES (1)",
            "CREATE TABLE t(a integer)",
            &[],
        ),
        (
            "add a check the rows satisfy",
            "CREATE TABLE t(a integer); INSERT INTO t VALUES (1), (2)",
            "CREATE TABLE t(a integer, CONSTRAINT k CHECK (a > 0))",
            &[],
        ),
        (
            "drop a check",
            "CREATE TABLE t(a integer, CONSTRAINT k CHECK (a > 0)); INSERT INTO t VALUES (1)",
            "CREATE TABLE t(a integer)",
            &[],
        ),
    ];
    for (what, database, declared_sql, seeds) in cases {
        let mut db = file(database);
        let before = std::fs::read(db.path()).expect("bytes");
        let declared = declared_from(declared_sql);
        let ran = rehearse(&mut db, &declared, &ids(&declared, seeds))
            .unwrap_or_else(|refused| panic!("{what}: statement {}: {}", refused.at, refused.sql));
        assert!(
            !ran.statements.is_empty(),
            "{what}: there was something to do"
        );
        assert!(converged(&ran, &declared), "{what}");
        assert!(ran.new.is_empty(), "{what}: {:?}", ran.new);
        assert_eq!(std::fs::read(db.path()).expect("bytes"), before, "{what}");

        // Negative: a plan cut short does not end there, so the comparison says something.
        let base = introspect(db.connection()).expect("reads");
        let all = plan(
            (&base.schema, &ids(&base.schema, &[])),
            (&declared, &ids(&declared, seeds)),
            &Sqlite::default(),
        )
        .expect("plans");
        let mut copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
        for statement in &all[..all.len() - 1] {
            copy.execute_batch(&statement.sql).expect("runs");
        }
        let partial = introspect(&mut copy).expect("reads");
        let short = Ran {
            statements: all,
            read_back: partial,
            new: Vec::new(),
        };
        assert!(!converged(&short, &declared), "{what}");
    }
}

#[test]
fn what_depends_on_the_rows_is_refused_by_the_engine_on_the_copy_and_leaves_nothing() {
    // (what, the rows that break it, rows that do not, the declarations)
    let cases = [
        (
            "a check some row violates",
            "INSERT INTO t VALUES (-1), (2)",
            "INSERT INTO t VALUES (1), (2)",
            "CREATE TABLE t(a integer, CONSTRAINT k CHECK (a > 0))",
        ),
        (
            "a column that holds a null made not null",
            "INSERT INTO t VALUES (NULL), (2)",
            "INSERT INTO t VALUES (1), (2)",
            "CREATE TABLE t(a integer NOT NULL)",
        ),
        (
            "a unique index over duplicates",
            "INSERT INTO t VALUES (1), (1)",
            "INSERT INTO t VALUES (1), (2)",
            "CREATE TABLE t(a integer); CREATE UNIQUE INDEX u ON t(a)",
        ),
    ];
    for (what, bad, good, declared_sql) in cases {
        let declared = declared_from(declared_sql);
        let declared_ids = ids(&declared, &[]);
        let mut db = file(&format!("CREATE TABLE t(a integer); {bad}"));
        let bytes = std::fs::read(db.path()).expect("bytes");
        let schema_before: i64 = count(db.connection(), "SELECT count(*) FROM sqlite_schema");
        let refused = refusal(rehearse(&mut db, &declared, &declared_ids));
        assert!(
            refused.error.to_string().contains("constraint failed"),
            "{what}: {}",
            refused.error
        );
        assert!(refused.at >= 1 && !refused.sql.is_empty(), "{what}");
        // The apply's transaction is rolled back: the copy is as it was copied, the
        // file is untouched, and no transaction is left open anywhere.
        assert!(!refused.copy.in_transaction(), "{what}");
        assert_eq!(
            count(&refused.copy, "SELECT count(*) FROM sqlite_schema"),
            schema_before,
            "{what}: the copy holds nothing of the refused plan"
        );
        assert_eq!(std::fs::read(db.path()).expect("bytes"), bytes, "{what}");

        // Negative: the same plan on rows that satisfy it is taken.
        let mut fine = file(&format!("CREATE TABLE t(a integer); {good}"));
        let ran = rehearse(&mut fine, &declared, &declared_ids)
            .unwrap_or_else(|r| panic!("{what}: statement {}: {}: {}", r.at, r.sql, r.error));
        assert!(converged(&ran, &declared), "{what}");
    }
}

#[test]
fn the_three_declarations_the_dialect_cannot_judge_are_refused_naming_the_statement() {
    let database = "CREATE TABLE t(a integer, b integer); INSERT INTO t VALUES (1, 2)";
    let base = declared_from(database);
    // A default that names a column, a check that names a column that is not
    // there, and a partial index's filter that is not deterministic.
    let default = {
        let mut declared = base.clone();
        declared.tables.insert(
            tn("y"),
            table(&[
                ("x", col("integer")),
                ("z", col("integer").with_default("(x)")),
            ]),
        );
        declared
    };
    let check = {
        let mut declared = base.clone();
        declared.tables.get_mut(&tn("t")).expect("t").checks.insert(
            "k".into(),
            CheckConstraint {
                expression: "missing > 0".into(),
            },
        );
        declared
    };
    let filter = {
        let mut declared = base.clone();
        declared
            .tables
            .get_mut(&tn("t"))
            .expect("t")
            .indexes
            .insert("i".into(), index(&["a"], false, Some("random() > 0")));
        declared
    };
    for (what, declared, statement, engine) in [
        (
            "a default naming a column",
            default,
            "CREATE TABLE",
            "is not constant",
        ),
        (
            "a check naming a missing column",
            check,
            "ADD CONSTRAINT",
            "no such column: missing",
        ),
        (
            "a non-deterministic filter",
            filter,
            "CREATE INDEX",
            "non-deterministic functions",
        ),
    ] {
        let mut db = file(database);
        let bytes = std::fs::read(db.path()).expect("bytes");
        let refused = refusal(rehearse(&mut db, &declared, &ids(&declared, &[])));
        assert!(refused.sql.contains(statement), "{what}: {}", refused.sql);
        assert!(
            refused.error.to_string().contains(engine),
            "{what}: {}",
            refused.error
        );
        assert_eq!(std::fs::read(db.path()).expect("bytes"), bytes, "{what}");
    }
    // Negative: the same three, written so the engine takes them.
    let fine = {
        let mut declared = base.clone();
        declared.tables.insert(
            tn("y"),
            table(&[
                ("x", col("integer")),
                ("z", col("integer").with_default("5")),
            ]),
        );
        let t = declared.tables.get_mut(&tn("t")).expect("t");
        t.checks.insert(
            "k".into(),
            CheckConstraint {
                expression: "a > 0".into(),
            },
        );
        t.indexes
            .insert("i".into(), index(&["a"], false, Some("a > 0")));
        declared
    };
    let mut db = file(database);
    let ran = rehearse(&mut db, &fine, &ids(&fine, &[])).unwrap_or_else(|r| {
        panic!("statement {}: {}: {}", r.at, r.sql, r.error);
    });
    assert!(converged(&ran, &fine));
}

#[test]
fn a_plan_that_replaces_a_tables_only_column_runs_and_converges() {
    // The differ adds the replacement before it drops the column (#2963), so the
    // table is never left with none.
    let mut db = file("CREATE TABLE t(a integer)");
    let declared = declared_from("CREATE TABLE t(b integer)");
    let ran = rehearse(&mut db, &declared, &ids(&declared, &[])).unwrap_or_else(|r| {
        panic!("statement {}: {}: {}", r.at, r.sql, r.error);
    });
    assert!(converged(&ran, &declared));
    // Negative: the engine's refusal of a table left with no column still stops
    // a rehearsal, at the statement and in the engine's words.
    let copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    let error = copy
        .execute_batch("ALTER TABLE t DROP COLUMN a")
        .expect_err("the only column");
    assert!(
        error.to_string().contains("no other columns exist"),
        "{error}"
    );
}

#[test]
fn a_table_named_like_a_pragma_function_refuses_the_resolution() {
    let mut db = file("CREATE TABLE t(a integer); CREATE VIEW v AS SELECT a FROM t");
    let copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    assert!(resolution(&copy).expect("asked").is_empty());
    // An empty namesake would answer "no views, no tables": everything resolves.
    copy.execute_batch("DROP TABLE t; CREATE TABLE pragma_table_list(schema, name, type)")
        .expect("the engine takes it");
    let refused = resolution(&copy).expect_err("not asked of a namesake");
    assert!(matches!(refused, DbError::Refused(_)), "{refused:?}");
    assert!(
        refused.to_string().contains("pragma_table_list"),
        "{refused}"
    );
}

#[test]
fn a_view_a_plan_leaves_broken_passes_every_statement_and_the_read_back_and_only_the_resolution_sees_it()
 {
    let mut db = file("CREATE TABLE t(a integer); CREATE VIEW v AS SELECT a FROM t");
    let mut copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    let before = resolution(&copy).expect("asked");
    assert!(before.is_empty());
    copy.execute_batch("DROP TABLE t")
        .expect("the engine takes it");
    // The reader never selects from a view, so it reads what is left without a word.
    assert!(introspect(&mut copy).is_ok());
    let after = resolution(&copy).expect("asked");
    let lines = after.new_since(&before, &Renamed::default());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("view `v`") && lines[0].contains("no such table"),
        "{lines:?}"
    );
}

#[test]
fn what_failed_the_same_way_before_the_plan_ran_is_not_the_plans_fault() {
    // A view that is already broken, a default calling a function that does not
    // exist, and a foreign key to a column that is no key: all three exist in the
    // file the plan starts from.
    let mut db = file(
        "CREATE VIEW v AS SELECT zz FROM nope; \
         CREATE TABLE d(a DEFAULT (nosuchfn())); \
         CREATE TABLE p(x); CREATE TABLE c(y REFERENCES p(x))",
    );
    let copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    let before = resolution(&copy).expect("asked");
    assert!(!before.is_empty());
    // A plan that touches none of them: nothing is new.
    copy.execute_batch("CREATE TABLE z(a)").expect("runs");
    let after = resolution(&copy).expect("asked");
    assert!(
        after.new_since(&before, &Renamed::default()).is_empty(),
        "{:?}",
        after.new_since(&before, &Renamed::default())
    );
    // Negative: the same view failing for a different reason is the plan's.
    copy.execute_batch("CREATE TABLE nope(a)").expect("runs");
    let changed = resolution(&copy)
        .expect("asked")
        .new_since(&before, &Renamed::default());
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert!(
        changed[0].contains("view `v`") && changed[0].contains("no such column"),
        "{changed:?}"
    );
    // Negative: a new default of the same kind is the plan's.
    copy.execute_batch("CREATE TABLE e(a DEFAULT (nosuchfn()))")
        .expect("runs");
    let grown = resolution(&copy)
        .expect("asked")
        .new_since(&before, &Renamed::default());
    assert!(grown.iter().any(|line| line.contains("`e.a`")), "{grown:?}");
    assert!(
        !grown.iter().any(|line| line.contains("`d.a`")),
        "{grown:?}"
    );
}

#[test]
fn what_failed_before_a_rename_is_not_new_under_the_new_name() {
    // Orphan rows and a default that fail already; the plan renames the parent,
    // the child, and the table and the column with the default. (A trigger that
    // does not compile is not here: the engine refuses a rename beside one.)
    let mut db = file(
        "CREATE TABLE p(x PRIMARY KEY); CREATE TABLE c(y REFERENCES p(x)); \
         CREATE TABLE d(a DEFAULT (nosuchfn()))",
    );
    db.connection()
        .execute_batch("PRAGMA foreign_keys = OFF; INSERT INTO c VALUES (7), (8)")
        .expect("orphans");
    let copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    let before = resolution(&copy).expect("asked");
    assert!(!before.is_empty());
    copy.execute_batch(
        "ALTER TABLE p RENAME TO P2; ALTER TABLE c RENAME TO c2; \
         ALTER TABLE d RENAME TO d2; ALTER TABLE d2 RENAME COLUMN a TO b",
    )
    .expect("the renames");
    let after = resolution(&copy).expect("asked");
    let renamed = Renamed {
        tables: vec![
            ("p".into(), "p2".into()),
            ("C".into(), "c2".into()),
            ("d".into(), "d2".into()),
        ],
        columns: vec![("d2".into(), "a".into(), "b".into())],
    };
    let new = after.new_since(&before, &renamed);
    assert!(new.is_empty(), "{new:?}");
    // Negative: without the plan's renames every one of them reads as new, and
    // a rename the plan did not make does not excuse a finding.
    let unknown = after.new_since(&before, &Renamed::default());
    assert_eq!(unknown.len(), before.0.len(), "{unknown:?}");
    let other = Renamed {
        tables: vec![("p".into(), "elsewhere".into())],
        columns: Vec::new(),
    };
    assert!(!after.new_since(&before, &other).is_empty());
    // And rows added under the new names are still counted, the old ones not.
    copy.execute_batch("PRAGMA foreign_keys = OFF; INSERT INTO c2 VALUES (9)")
        .expect("one more");
    let grown = resolution(&copy)
        .expect("asked")
        .new_since(&before, &renamed);
    assert_eq!(grown.len(), 1, "{grown:?}");
    assert!(grown[0].starts_with("1 more row(s)"), "{grown:?}");
}

#[test]
fn the_renames_of_a_plan_are_read_from_its_changes() {
    let from = declared_from("CREATE TABLE t(a integer)");
    let to = declared_from("CREATE TABLE u(b integer)");
    let base_ids = ids(&from, &[]);
    let hints = pbps_model::Hints::default();
    // The same identities under new names: a table rename and a column rename.
    let to_ids = ids(&to, &[("u", "t"), ("u.b", "t.a")]);
    let plan = pbps_diff::diff(
        pbps_diff::Side {
            schema: &from,
            ids: &base_ids,
        },
        pbps_diff::Side {
            schema: &to,
            ids: &to_ids,
        },
        &Sqlite::default(),
        &hints,
    )
    .expect("plans");
    let renamed = Renamed::of(&plan);
    assert_eq!(renamed.tables, vec![("t".to_owned(), "u".to_owned())]);
    assert_eq!(
        renamed.columns,
        vec![("u".to_owned(), "a".to_owned(), "b".to_owned())]
    );
    // Negative: a plan that renames nothing names nothing.
    assert_eq!(
        Renamed::of(&pbps_model::ChangeSet {
            changes: Vec::new()
        }),
        Renamed::default()
    );
}

#[test]
fn foreign_key_rows_count_only_the_ones_the_plan_added() {
    let rows = |n: u64| {
        Resolution(vec![Unresolved {
            what: "foreign key",
            name: "c -> p".into(),
            about: vec![About::Table("c".into()), About::Table("p".into())],
            detail: Detail::Rows(n),
        }])
    };
    assert!(rows(2).new_since(&rows(2), &Renamed::default()).is_empty());
    assert!(
        rows(1).new_since(&rows(2), &Renamed::default()).is_empty(),
        "fewer is not worse"
    );
    let more = rows(5).new_since(&rows(2), &Renamed::default());
    assert_eq!(more.len(), 1);
    assert!(more[0].starts_with("3 more row(s)"), "{more:?}");
    let fresh = rows(4).new_since(&Resolution::default(), &Renamed::default());
    assert!(fresh[0].starts_with("4 more row(s)"), "{fresh:?}");
}

#[test]
fn the_orphan_rows_a_file_already_had_are_found_on_the_copy_and_are_not_the_plans() {
    let mut db = file("CREATE TABLE p(x PRIMARY KEY); CREATE TABLE c(y REFERENCES p(x))");
    db.connection()
        .execute_batch(
            "PRAGMA foreign_keys = OFF; INSERT INTO c VALUES (9); PRAGMA foreign_keys = ON",
        )
        .expect("an orphan the file already had");
    let copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    let before = resolution(&copy).expect("asked");
    assert!(!before.is_empty());
    assert!(
        resolution(&copy)
            .expect("asked")
            .new_since(&before, &Renamed::default())
            .is_empty()
    );
}

#[test]
fn a_database_over_the_limit_is_refused_in_the_rehearsals_words_with_the_way_out() {
    let mut db = file("CREATE TABLE t(a integer); INSERT INTO t VALUES (1)");
    let size = db.connection().size().expect("size");
    let bytes = std::fs::read(db.path()).expect("bytes");
    let refused = scratch(db.connection(), size - 1).expect_err("over the limit");
    let DbError::Refused(message) = &refused else {
        panic!("expected a refusal: {refused:?}");
    };
    let library = db.connection().version().expect("version").to_string();
    assert!(message.contains(&format!("SQLite {library}")), "{message}");
    assert!(
        message.contains("in memory") && message.contains("MiB"),
        "{message}"
    );
    assert!(message.contains("`--no-rehearse`"), "{message}");
    // The size was read first: this is the over-the-limit sentence and not the one
    // for a database that grew during the copy.
    assert!(message.contains("a copy may be"), "{message}");
    assert!(!message.contains("grew"), "{message}");
    assert_eq!(std::fs::read(db.path()).expect("bytes"), bytes);
    // Negative: at the limit it is copied.
    assert!(scratch(db.connection(), size).is_ok());
}

#[test]
fn a_busy_database_is_the_busy_code_with_a_sentence_and_never_a_hang() {
    let mut db = file("CREATE TABLE t(a integer)");
    let c = db.connection();
    c.execute_batch("BEGIN IMMEDIATE")
        .expect("this connection writes");
    let wait = Duration::from_millis(150);
    let started = Instant::now();
    let error = scratch_within(c, SCRATCH_LIMIT, wait).expect_err("busy");
    assert!(started.elapsed() >= wait);
    assert!(started.elapsed() < Duration::from_secs(60));
    assert_eq!(error.server_error_code().as_deref(), Some("5"), "{error}");
    let sentence = error.to_string();
    assert!(
        sentence.contains("write transaction") && sentence.contains("`--no-rehearse`"),
        "{sentence}"
    );
    c.execute_batch("ROLLBACK").expect("ended");
    // Negative: free, it is copied.
    assert!(scratch_within(c, SCRATCH_LIMIT, wait).is_ok());
}

#[test]
fn the_source_connection_is_exactly_as_it_was_after_a_rehearsal_and_a_refusal() {
    let mut db = file("CREATE TABLE t(a integer); INSERT INTO t VALUES (1)");
    let path = db.path().to_path_buf();
    let listing = |dir: &std::path::Path| {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("a directory")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let files = listing(path.parent().expect("a directory"));
    let bytes = std::fs::read(&path).expect("bytes");
    let c = db.connection();
    c.set_busy_timeout(Duration::from_millis(1234))
        .expect("set");
    scratch(c, SCRATCH_LIMIT).expect("copied");
    scratch(c, 1).expect_err("refused");
    assert_eq!(count(c, "PRAGMA busy_timeout"), 1234);
    assert_eq!(count(c, "SELECT foreign_keys FROM pragma_foreign_keys"), 1);
    assert_eq!(
        count(
            c,
            "SELECT legacy_alter_table FROM pragma_legacy_alter_table"
        ),
        0
    );
    assert!(!c.in_transaction());
    assert_eq!(std::fs::read(&path).expect("bytes"), bytes);
    assert_eq!(listing(path.parent().expect("a directory")), files);
}

#[test]
fn the_copy_holds_the_ledger_as_the_file_does_and_nothing_of_it_is_the_files() {
    use crate::state::{ensure_tables, lock, lock_holder};
    let mut db = Fresh::file();
    let path = db.path().to_path_buf();
    ensure_tables(db.connection()).expect("a ledger");
    let held = lock(db.connection(), "me [host h, pid 1]").expect("locked");
    let files = std::fs::read_dir(path.parent().expect("a directory"))
        .expect("a directory")
        .count();
    let copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    // The tables and the lock row are in the copy, because the pages are.
    assert_eq!(
        count(
            &copy,
            "SELECT count(*) FROM sqlite_schema WHERE name LIKE '__pbps_%' AND type = 'table'"
        ),
        3
    );
    assert!(lock_holder(&copy).expect("read").is_some());
    // But the copy has no sidecar and made none, and dropping it frees nothing of
    // the file's: the lock is still held, and a second connection is still refused.
    assert_eq!(
        std::fs::read_dir(path.parent().expect("a directory"))
            .expect("a directory")
            .count(),
        files
    );
    drop(copy);
    let other = Connection::open(&path, pbps_db::sqlite::OpenMode::ExistingOnly).expect("opens");
    assert!(lock(&other, "you [host h, pid 2]").is_err());
    drop(held);
}

#[test]
fn a_rename_leaves_no_spelling_difference_because_the_differ_drops_and_recreates_what_names_the_column()
 {
    // The measurement of 2.4 of the plan. After `RENAME COLUMN` the engine writes
    // the new name quoted into every text that named the old one, so a rename on
    // its own leaves `"c" <> ''` where the declarations say `c <> ''`.
    let database = "CREATE TABLE t(a integer, b text, CONSTRAINT k CHECK (b <> '')); \
                    CREATE INDEX i ON t(a) WHERE b > ''; \
                    CREATE VIEW v AS SELECT a, b FROM t WHERE b > ''";
    let declared = declared_from(
        "CREATE TABLE t(a integer, c text, CONSTRAINT k CHECK (c <> '')); \
         CREATE INDEX i ON t(a) WHERE c > ''; CREATE VIEW v AS SELECT a, c FROM t WHERE c > ''",
    );
    let seeds = [("t.c", "t.b")];
    let mut db = file(database);
    let mut copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    copy.execute_batch("ALTER TABLE main.\"t\" RENAME COLUMN \"b\" TO \"c\"")
        .expect("a bare rename");
    let stored: String = copy
        .query(
            "SELECT sql FROM sqlite_schema WHERE name = 'k' OR name = 't'",
            &[],
        )
        .expect("query")[0]
        .text(0)
        .expect("text")
        .to_owned();
    assert!(
        stored.contains("\"c\" <> ''"),
        "the engine quotes the rewritten name: {stored}"
    );
    let bare = Ran {
        statements: Vec::new(),
        read_back: introspect(&mut copy).expect("reads"),
        new: Vec::new(),
    };
    assert!(
        !converged(&bare, &declared),
        "a rename alone is a spelling difference the differ sees"
    );
    // The plan the differ makes tears down what names the column before the rename
    // and writes it again, as declared, after it, so nothing is left to see.
    let ran = rehearse(&mut db, &declared, &ids(&declared, &seeds)).unwrap_or_else(|r| {
        panic!("statement {}: {}: {}", r.at, r.sql, r.error);
    });
    let sql: Vec<&str> = ran.statements.iter().map(|s| s.sql.as_str()).collect();
    let at = |needle: &str| sql.iter().position(|s| s.contains(needle)).expect(needle);
    assert!(
        at("DROP CONSTRAINT \"k\"") < at("RENAME COLUMN"),
        "{sql:#?}"
    );
    assert!(at("DROP INDEX") < at("RENAME COLUMN"), "{sql:#?}");
    assert!(
        at("RENAME COLUMN") < at("ADD CONSTRAINT \"k\" CHECK (c <> '')"),
        "{sql:#?}"
    );
    assert!(at("RENAME COLUMN") < at("CREATE VIEW"), "{sql:#?}");
    assert!(converged(&ran, &declared), "{sql:#?}");
    assert!(ran.new.is_empty(), "{:?}", ran.new);
}

#[test]
fn a_copy_that_failed_is_told_as_a_database_that_grew_or_one_that_stayed_busy() {
    let what = "the rehearsal runs the plan on a copy";
    let wait = Duration::from_millis(1500);
    let grew = explain_copy(DbError::Refused("x".into()), what, 1 << 20, wait);
    let DbError::Refused(message) = &grew else {
        panic!("expected a refusal: {grew:?}");
    };
    assert!(
        message.contains("grew past 1.0 MiB") && message.contains("`--no-rehearse`"),
        "{message}"
    );
    let busy = DbError::Driver {
        message: "database is locked".into(),
        code: Some("5".into()),
        diagnostic: None,
    };
    let told = explain_copy(busy, what, 1 << 20, wait);
    assert_eq!(
        told.server_error_code().as_deref(),
        Some("5"),
        "the code survives"
    );
    assert!(told.to_string().contains("1.5 s") && told.to_string().contains("`--no-rehearse`"));
    // Negative: any other failure is the engine's own.
    let other = DbError::BadRow("unreadable".into());
    assert!(matches!(
        explain_copy(other, what, 1 << 20, wait),
        DbError::BadRow(_)
    ));
}

#[test]
fn a_writer_that_leaves_is_waited_for_and_not_refused() {
    use std::sync::mpsc;
    let mut db = file("CREATE TABLE t(a integer)");
    let path = db.path().to_path_buf();
    let (held, told) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let second =
            Connection::open(&path, pbps_db::sqlite::OpenMode::ExistingOnly).expect("opens");
        second.execute_batch("BEGIN EXCLUSIVE").expect("held");
        held.send(()).expect("told");
        std::thread::sleep(Duration::from_millis(200));
        second.execute_batch("ROLLBACK").expect("released");
    });
    told.recv().expect("the writer holds the lock");
    // A wait far longer than the writer stays, so it is outlasted however loaded
    // the machine is; this connection's own wait is zero and is put back.
    let copy = scratch_within(db.connection(), SCRATCH_LIMIT, Duration::from_secs(60));
    assert!(copy.is_ok(), "{:?}", copy.err());
    assert_eq!(count(db.connection(), "PRAGMA busy_timeout"), 0);
    writer.join().expect("the writer ended");
}

/// The reader reads around triggers, and every statement takes a plan that
/// leaves one naming what is gone. Asked with the same text before and after a
/// schema change, as a rehearsal asks.
#[test]
fn a_trigger_a_plan_leaves_broken_is_found_by_compiling_a_write_and_never_running_one() {
    let mut db = file(
        "CREATE TABLE t(a integer, b integer GENERATED ALWAYS AS (a + 1)); \
         CREATE TABLE gone(x); CREATE TABLE kept(x); \
         CREATE VIEW v AS SELECT a FROM t; \
         CREATE TRIGGER on_insert AFTER INSERT ON t BEGIN INSERT INTO gone VALUES (new.a); END; \
         CREATE TRIGGER on_update AFTER UPDATE OF a ON t BEGIN INSERT INTO kept VALUES (new.a); END; \
         CREATE TRIGGER on_view INSTEAD OF DELETE ON v BEGIN DELETE FROM gone; END; \
         INSERT INTO kept VALUES (7)",
    );
    let copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    let before = resolution(&copy).expect("asked");
    // A view takes no insert or update without a trigger for it: that fails
    // before the plan too, in the same words, and is not the plan's.
    copy.execute_batch("DROP TABLE gone")
        .expect("the engine takes it");
    let after = resolution(&copy).expect("asked");
    let lines = after.new_since(&before, &Renamed::default());
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].contains("`t (insert)`") && lines[0].contains("no such table: main.gone"),
        "{lines:?}"
    );
    assert!(lines[1].contains("`v (delete)`"), "{lines:?}");
    // Nothing was written by the asking: no row in `t`, and `kept` as it was.
    assert_eq!(count(&copy, "SELECT count(*) FROM t"), 0);
    assert_eq!(count(&copy, "SELECT count(*) FROM kept"), 1);
    // Negative: a plan that leaves every trigger whole reports nothing.
    let whole = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    let before = resolution(&whole).expect("asked");
    whole
        .execute_batch("CREATE TABLE other(x)")
        .expect("harmless");
    assert!(
        resolution(&whole)
            .expect("asked")
            .new_since(&before, &Renamed::default())
            .is_empty()
    );
}

/// One answer for the whole database would hide a second table's unusable key
/// behind a first that the file already had.
#[test]
fn a_key_the_engine_cannot_check_is_found_table_by_table() {
    let mut db = file("CREATE TABLE p(k, z); CREATE TABLE a_old(r REFERENCES p(z))");
    let copy = scratch(db.connection(), SCRATCH_LIMIT).expect("copied");
    let before = resolution(&copy).expect("asked");
    assert!(!before.is_empty(), "the file's own unusable key is seen");
    copy.execute_batch("CREATE TABLE b_new(r REFERENCES p(z))")
        .expect("the engine takes it");
    let lines = resolution(&copy)
        .expect("asked")
        .new_since(&before, &Renamed::default());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("foreign keys of `b_new`") && lines[0].contains("mismatch"),
        "{lines:?}"
    );
    // Negative: a table with a usable key adds nothing.
    let before = resolution(&copy).expect("asked");
    copy.execute_batch("CREATE TABLE q(k PRIMARY KEY); CREATE TABLE c_fine(r REFERENCES q(k))")
        .expect("the engine takes it");
    assert!(
        resolution(&copy)
            .expect("asked")
            .new_since(&before, &Renamed::default())
            .is_empty()
    );
}

/// The size is read before the copy, and a database that is busy for that read
/// is told with the same way out as one that is busy for the copy.
#[test]
fn a_database_busy_for_the_size_read_gets_the_sentence_and_the_way_out_too() {
    let mut db = file("CREATE TABLE t(a integer)");
    let path = db.path().to_path_buf();
    let other = Connection::open(&path, pbps_db::sqlite::OpenMode::ExistingOnly).expect("a second");
    other
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("held by another connection");
    let error = scratch_within(db.connection(), SCRATCH_LIMIT, Duration::from_millis(150))
        .expect_err("busy");
    let sentence = error.to_string();
    assert!(
        sentence.contains("stayed busy") && sentence.contains("`--no-rehearse`"),
        "{sentence}"
    );
    other.execute_batch("ROLLBACK").expect("ended");
    // Negative: free, it is copied.
    assert!(scratch_within(db.connection(), SCRATCH_LIMIT, Duration::from_millis(150)).is_ok());
}
