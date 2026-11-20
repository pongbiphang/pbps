//! What surrounds the file's schema: other databases on the connection, views
//! that no longer work, the read's own transaction, and objects the engine will
//! not give back.

use pbps_db::DbError;
use pbps_db::sqlite::{Connection, OpenMode};
use pbps_model::{ModuleId, ObjectName};

use super::super::{assemble, introspect, introspect_within_transaction, read_raw};
use super::{limits, read, read_sql, table, warned};
use crate::tests::Fresh;
use crate::{READ, TRANSACTION};

#[test]
fn an_attached_database_and_the_temporary_objects_are_left_out_by_name_and_not_silently() {
    let mut fresh = Fresh::memory();
    fresh
        .connection()
        .execute_batch(
            "CREATE TABLE main_t(a int); \
             ATTACH DATABASE ':memory:' AS other; \
             CREATE TABLE other.x(a); CREATE VIEW other.vx AS SELECT 1; \
             CREATE TEMP TABLE tt(a); CREATE TEMP VIEW tv AS SELECT 1; \
             CREATE INDEX temp.ti ON tt(a); \
             CREATE TEMP TRIGGER trg AFTER INSERT ON tt BEGIN SELECT 1; END",
        )
        .expect("setup");
    let pulled = read(&mut fresh);
    let names: Vec<_> = pulled
        .schema
        .tables
        .keys()
        .map(|n| n.name.as_str())
        .collect();
    assert_eq!(names, ["main_t"]);
    assert!(pulled.schema.modules.is_empty() && pulled.limitations.is_empty());
    for note in [
        "a database attached as `other`",
        "the table `x` is in the attached `other` database, not in `main`",
        "the view `vx` is in the attached `other` database",
        "the table `tt` is in the temporary database",
        "the view `tv` is in the temporary database",
        "the index `ti` is in the temporary database",
        "the trigger `trg` is in the temporary database",
    ] {
        assert!(warned(&pulled, note), "{note}: {:#?}", pulled.warnings);
    }
    assert_eq!(pulled.warnings.len(), 7, "{:#?}", pulled.warnings);
    // Negative: with nothing elsewhere, nothing is said, and `sqlite_temp_schema`
    // itself is nobody's table.
    let (_, plain) = read_sql("CREATE TABLE t(a int)");
    assert!(plain.warnings.is_empty());
}

#[test]
fn a_view_that_no_longer_works_is_read_as_text_and_its_columns_are_never_asked_for() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch(
        "CREATE TABLE t(a int, b int); CREATE VIEW v AS SELECT a FROM t; \
         CREATE VIEW w AS SELECT b FROM t; DROP TABLE t; \
         CREATE VIEW u AS SELECT * FROM nowhere",
    )
    .expect("setup");
    // The engine's side: the columns of such a view are an error, which is why
    // the reader does not ask.
    for view in ["v", "w", "u"] {
        c.query(&format!("SELECT * FROM pragma_table_xinfo('{view}')"), &[])
            .expect_err("an invalid view has no columns");
    }
    let before = introspect(c).expect("reads");
    let ids: Vec<_> = before.schema.modules.keys().cloned().collect();
    let expected: Vec<_> = ["u", "v", "w"]
        .iter()
        .map(|n| ModuleId::Named(ObjectName::new("main", *n)))
        .collect();
    assert_eq!(ids, expected);
    assert!(before.warnings.is_empty() && before.limitations.is_empty());
    // The schema the engine holds is not damaged by the read: the views work
    // again when their table does, and a second read is the same.
    c.execute_batch("CREATE TABLE t(a int, b int); CREATE TABLE nowhere(x int)")
        .expect("recreate");
    for view in ["v", "w", "u"] {
        c.query(&format!("SELECT * FROM {view}"), &[])
            .expect("the view works");
    }
    let integrity = c.query("PRAGMA integrity_check", &[]).expect("check");
    assert_eq!(integrity[0].text(0).expect("text"), "ok");
    let after = introspect(c).expect("reads");
    assert_eq!(after.schema.modules, before.schema.modules);
}

#[test]
fn a_read_is_one_snapshot_that_a_writer_cannot_change_under_it() {
    let mut fresh = Fresh::file();
    fresh
        .connection()
        .execute_batch("CREATE TABLE t(a int)")
        .expect("t");
    let second = Connection::open(fresh.path(), OpenMode::ExistingOnly).expect("second connection");
    let seen = fresh
        .connection()
        .transaction(&READ, |tx| {
            let first = assemble(&read_raw(tx)?);
            // The reader holds the snapshot: a writer is refused, not interleaved.
            let busy = second
                .execute_batch("CREATE TABLE z(a int)")
                .expect_err("locked");
            assert!(busy.to_string().contains("locked"), "{busy}");
            // The reader holds no write lock, which `BEGIN IMMEDIATE` would: another
            // connection can still begin a write transaction (it cannot commit one).
            second
                .execute_batch("BEGIN IMMEDIATE")
                .expect("no write lock is held");
            second.execute_batch("ROLLBACK").expect("rollback");
            let again = assemble(&read_raw(tx)?);
            assert_eq!(first, again);
            Ok(first)
        })
        .expect("reads");
    assert_eq!(seen.schema.tables.len(), 1);
    // `introspect` is a deferred read: it goes on while another connection holds
    // the write lock, which a `BEGIN IMMEDIATE` would wait for in vain.
    second
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the write lock");
    let during = introspect(fresh.connection()).expect("a read beside a writer");
    assert_eq!(during.schema.tables.len(), 1);
    second.execute_batch("ROLLBACK").expect("rollback");
    // The read took no write lock and holds none after it: the writer goes on.
    second
        .execute_batch("CREATE TABLE z(a int)")
        .expect("a writer after the read");
    assert_eq!(
        introspect(fresh.connection())
            .expect("reads")
            .schema
            .tables
            .len(),
        2
    );
    // The framing is a deferred transaction, and an apply's is not.
    assert_eq!(READ.begin, "BEGIN");
    assert_ne!(READ.begin, TRANSACTION.begin);
}

#[test]
fn a_read_inside_a_transaction_the_caller_holds_sees_what_that_transaction_wrote() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    // Outside one, it is refused rather than read without a snapshot.
    let refused = introspect_within_transaction(c).expect_err("no transaction");
    assert!(matches!(refused, DbError::Refused(_)), "{refused:?}");
    let inside = c
        .transaction(&TRANSACTION, |tx| {
            tx.execute_batch("CREATE TABLE made(a int)")?;
            introspect_within_transaction(tx)
        })
        .expect("reads");
    assert!(inside.schema.tables.keys().any(|n| n.name == "made"));
    assert_eq!(inside, introspect(c).expect("reads"));
    // Rolled back, the table is not there to be read.
    let rolled: Result<(), DbError> = c.transaction(&TRANSACTION, |tx| {
        tx.execute_batch("CREATE TABLE gone(a int)")?;
        assert!(
            introspect_within_transaction(tx)?
                .schema
                .tables
                .keys()
                .any(|n| n.name == "gone")
        );
        Err(DbError::Refused("stop".into()))
    });
    assert!(rolled.is_err());
    assert!(
        !introspect(c)
            .expect("reads")
            .schema
            .tables
            .keys()
            .any(|n| n.name == "gone")
    );
}

#[test]
fn text_that_is_not_utf8_is_an_error_and_never_a_missing_object() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch("CREATE TABLE t(a int)").expect("t");
    // Only a damaged schema holds it: SQLite stores any bytes as text, and the
    // driver refuses them, because replacing them would name another object.
    c.execute_batch(
        "PRAGMA writable_schema = ON; \
         INSERT INTO sqlite_schema(type, name, tbl_name, rootpage, sql) \
         VALUES ('view', 'bad', 'bad', 0, CAST(x'4352454154452056494557206261642041532053454c4543542027ff27' AS TEXT)); \
         PRAGMA writable_schema = OFF",
    )
    .expect("a damaged schema");
    let error = introspect(c).expect_err("an unreadable object is an error");
    assert!(error.to_string().contains("UTF-8"), "{error}");
}

#[test]
fn absent_empty_and_unreadable_are_three_answers() {
    // Nothing there is an empty read.
    let (_, empty) = read_sql("");
    assert_eq!(empty, pbps_db::catalog::Pulled::default());
    // The pure function agrees: an empty catalog is an empty result.
    assert_eq!(
        assemble(&super::super::RawCatalog::default()),
        pbps_db::catalog::Pulled::default()
    );
    // A table with nothing in it is a table with no columns refused by the
    // engine, so the smallest table is read whole, and nothing is said.
    let (_, one) = read_sql("CREATE TABLE t(a)");
    assert_eq!(table(&one, "t").columns.len(), 1);
    assert!(limits(&one, "t").is_empty());
    // A table the pragmas do not know is `None`, its own answer: not a table
    // with no columns, and not "not STRICT, has a rowid". The reader never gets
    // one for a table it just listed, in one snapshot, and would say so as an
    // error if it did.
    let mut fresh = Fresh::memory();
    fresh
        .connection()
        .execute_batch("CREATE TABLE t(a)")
        .expect("t");
    assert!(
        crate::catalog::facts::read(fresh.connection(), "missing")
            .expect("reads")
            .is_none()
    );
    assert!(read_raw(fresh.connection()).expect("reads").tables.len() == 1);
}

/// What reading a database set up by `sql` says, as a refusal.
fn refusal(sql: &str) -> String {
    let mut fresh = Fresh::memory();
    fresh.connection().execute_batch(sql).expect("setup");
    match introspect(fresh.connection()) {
        Err(DbError::Refused(m)) => m,
        other => panic!(
            "{sql}: expected a refusal, got {:?}",
            other.map(|p| p.warnings)
        ),
    }
}

#[test]
fn an_object_named_like_a_pragma_function_refuses_the_read_and_is_named() {
    // Each of these takes the place of the catalog function in `FROM pragma_x`
    // (measured: the rows of the user's table are answered, with no error).
    for (sql, named) in [
        (
            "CREATE TABLE pragma_table_list(schema, name, type)",
            "`pragma_table_list` in `main`",
        ),
        (
            "CREATE TEMP TABLE pragma_table_list(schema, name, type)",
            "`pragma_table_list` in `temp`",
        ),
        (
            "CREATE VIEW pragma_table_xinfo AS SELECT 1 AS a",
            "`pragma_table_xinfo` in `main`",
        ),
        (
            "CREATE TABLE \"PRAGMA_Table_List\"(a)",
            "`PRAGMA_Table_List` in `main`",
        ),
        (
            "ATTACH DATABASE ':memory:' AS \"o\"\"x\"; CREATE TABLE \"o\"\"x\".pragma_table_list(a)",
            "`pragma_table_list` in `o\"x`",
        ),
    ] {
        let message = refusal(sql);
        assert!(message.contains(named), "{sql}: {message}");
        assert!(message.contains("rename or drop it"), "{message}");
    }
}

#[test]
fn a_name_that_only_looks_like_a_pragma_function_reads_normally() {
    // Negative: the prefix is `pragma_` at the start, and only a table or view
    // can take a function's place.
    let (_, pulled) = read_sql(
        "CREATE TABLE pragmatic(a int); CREATE TABLE xpragma_table_list(a int); \
         CREATE TABLE pragma(a int); CREATE TABLE t(a int); CREATE INDEX pragma_i ON t(a); \
         ATTACH DATABASE ':memory:' AS o; CREATE TABLE o.other(a)",
    );
    for name in ["pragmatic", "xpragma_table_list", "pragma", "t"] {
        table(&pulled, name);
    }
}
