//! The statement pragmas against a real SQLite (#2994): what they answer beside
//! an object named like their table-valued function, which schema they are
//! asked of, how a name is written into them, and what no answer means.

use pbps_db::DbError;
use pbps_db::sqlite::{Connection, Record};

use super::{
    answered, by_name, columns, databases, foreign_key_list, foreign_keys, index_columns,
    index_list, indexes, sorted, table, table_list, table_xinfo, tables, tables_everywhere,
};
use crate::tests::Fresh;

/// A schema with something for every pragma to say.
const SCHEMA: &str = "\
    CREATE TABLE p(id INTEGER PRIMARY KEY, u TEXT UNIQUE); \
    CREATE TABLE t(a INT DEFAULT (1+1), b TEXT NOT NULL DEFAULT 'x', c, \
        g INT GENERATED ALWAYS AS (a + 1) STORED, \
        p_id REFERENCES p, pu TEXT REFERENCES p(u) ON DELETE CASCADE, \
        PRIMARY KEY (b, a)) WITHOUT ROWID; \
    CREATE INDEX t_i ON t(c DESC, lower(b) COLLATE nocase) WHERE c IS NOT NULL; \
    CREATE TABLE s(x ANY) STRICT; \
    CREATE VIEW v AS SELECT a FROM t";

/// An object of each pragma function's name: tables with the function's own
/// columns and a row that is not the catalog's, a view, and one of a shape that
/// has nothing to do with the function.
const NAMESAKES: &str = "\
    CREATE TABLE pragma_table_list(schema, name, type, ncol, wr, strict); \
    INSERT INTO pragma_table_list VALUES ('main', 'ghost', 'table', 1, 1, 1); \
    CREATE TABLE pragma_table_xinfo(cid, name, type, \"notnull\", dflt_value, pk, hidden); \
    INSERT INTO pragma_table_xinfo VALUES (0, 'ghost', 'int', 1, '7', 1, 0); \
    CREATE VIEW pragma_index_list AS SELECT 0 AS seq, 'ghost' AS name, 1 AS \"unique\", \
        'c' AS origin, 0 AS partial; \
    CREATE TABLE pragma_index_xinfo(a); \
    CREATE TABLE pragma_foreign_key_list(id, seq, \"table\", \"from\", \"to\", on_update, \
        on_delete, match); \
    INSERT INTO pragma_foreign_key_list VALUES (0, 0, 'ghost', 'a', NULL, 'x', 'x', 'x'); \
    CREATE TABLE pragma_database_list(seq, name, file); \
    INSERT INTO pragma_database_list VALUES (9, 'ghost', '')";

fn database(sql: &str) -> Fresh {
    let mut fresh = Fresh::memory();
    fresh.connection().execute_batch(sql).expect("setup");
    fresh
}

fn shown(rows: &[Record]) -> Vec<String> {
    rows.iter().map(|row| format!("{row:?}")).collect()
}

/// Everything the pragmas say about [`SCHEMA`]'s own objects, in a fixed order.
fn said(c: &Connection) -> Vec<String> {
    let mut all = Vec::new();
    let listed = by_name(tables(c).expect("listed"), table_list::NAME).expect("named");
    for row in &listed {
        let name = row.text(table_list::NAME).expect("a name");
        if name.starts_with("pragma_") {
            continue;
        }
        all.push(format!("{row:?}"));
        all.extend(shown(&table(c, name).expect("table")));
        all.extend(shown(&columns(c, name).expect("columns")));
        all.extend(shown(&foreign_keys(c, name).expect("keys")));
        for index in indexes(c, name).expect("indexes") {
            all.push(format!("{index:?}"));
            let index = index.text(index_list::NAME).expect("a name");
            all.extend(shown(&index_columns(c, index).expect("index columns")));
        }
    }
    all
}

#[test]
fn an_object_named_like_a_pragma_function_changes_nothing_a_statement_answers() {
    let mut plain = database(SCHEMA);
    let wanted = said(plain.connection());
    // Not vacuous: the schema makes every pragma answer.
    for needle in ["sqlite_autoindex_t_1", "CASCADE", "nocase", "1+1"] {
        assert!(wanted.iter().any(|line| line.contains(needle)), "{needle}");
    }
    let temp = NAMESAKES.replace("CREATE TABLE ", "CREATE TEMP TABLE ");
    let attached = format!(
        "ATTACH ':memory:' AS \"o\"\"x\"; {}",
        NAMESAKES
            .replace("CREATE TABLE ", "CREATE TABLE \"o\"\"x\".")
            .replace("CREATE VIEW ", "CREATE VIEW \"o\"\"x\".")
            .replace("INSERT INTO ", "INSERT INTO \"o\"\"x\".")
    );
    let mixed = NAMESAKES.replace("pragma_", "PRAGMA_");
    for namesakes in [
        NAMESAKES,
        &temp.replace("CREATE VIEW ", "CREATE TEMP VIEW "),
        &attached,
        &mixed,
    ] {
        let mut beside = database(SCHEMA);
        let c = beside.connection();
        c.execute_batch(namesakes)
            .unwrap_or_else(|e| panic!("{namesakes}: {e}"));
        // The function form is taken over, which is what the statements avoid.
        let taken = c
            .query("SELECT name FROM pragma_database_list", &[])
            .expect("the namesake answers");
        assert_eq!(taken[0].text(0).expect("text"), "ghost", "{namesakes}");
        // `said` skips only names that start `pragma_` in this case, so the
        // mixed-case objects are compared apart below.
        if namesakes != mixed {
            assert_eq!(said(c), wanted, "{namesakes}");
        }
        let names: Vec<String> = databases(c)
            .expect("databases")
            .iter()
            .map(|row| {
                row.text(super::database_list::NAME)
                    .expect("text")
                    .to_owned()
            })
            .collect();
        assert!(!names.iter().any(|name| name == "ghost"), "{names:?}");
        for (pragma, rows) in [
            ("table", table(c, "ghost")),
            ("columns", columns(c, "ghost")),
            ("indexes", indexes(c, "t")),
            ("keys", foreign_keys(c, "t")),
        ] {
            let rows = shown(&rows.expect(pragma));
            assert!(
                !rows.iter().any(|row| row.contains("ghost")),
                "{pragma}: {rows:?}"
            );
        }
    }
    // The namesakes are objects like any other, and are read as themselves.
    let mut beside = database(&format!("{SCHEMA}; {NAMESAKES}"));
    let c = beside.connection();
    let own = columns(c, "pragma_table_xinfo").expect("columns");
    assert_eq!(own.len(), 7);
    assert_eq!(own[3].text(table_xinfo::NAME).expect("text"), "notnull");
    let view = table(c, "PRAGMA_Index_List").expect("table");
    assert_eq!(view[0].text(table_list::KIND).expect("text"), "view");
}

#[test]
fn a_pragma_is_asked_of_main_and_never_of_a_temporary_table_of_the_name() {
    let mut fresh = database(
        "CREATE TABLE t(a INT, b INT REFERENCES p); CREATE TABLE p(id INTEGER PRIMARY KEY); \
         CREATE INDEX t_a ON t(a); CREATE TEMP TABLE t(only_temp)",
    );
    let c = fresh.connection();
    // Unqualified, the statement itself answers for the temporary table.
    let unqualified = c.query("PRAGMA table_xinfo(\"t\")", &[]).expect("asked");
    assert_eq!(unqualified[0].text(1).expect("text"), "only_temp");
    let main = columns(c, "t").expect("columns");
    assert_eq!(main.len(), 2);
    assert_eq!(main[0].text(table_xinfo::NAME).expect("text"), "a");
    assert_eq!(indexes(c, "t").expect("indexes").len(), 1);
    assert_eq!(foreign_keys(c, "t").expect("keys").len(), 1);
    assert_eq!(table(c, "t").expect("table").len(), 1);
    // `tables` is `main`'s, and `tables_everywhere` every schema's.
    let schemas = |rows: Vec<Record>| -> Vec<String> {
        let mut found: Vec<String> = rows
            .iter()
            .filter(|row| row.text(table_list::NAME).expect("text") == "t")
            .map(|row| row.text(table_list::SCHEMA).expect("text").to_owned())
            .collect();
        found.sort();
        found
    };
    assert_eq!(schemas(tables(c).expect("listed")), ["main"]);
    assert_eq!(
        schemas(tables_everywhere(c).expect("listed")),
        ["main", "temp"]
    );
    // Negative: a table only `temp` holds is not `main`'s.
    c.execute_batch("CREATE TEMP TABLE only_here(a)")
        .expect("temp");
    assert!(table(c, "only_here").expect("table").is_empty());
    assert!(columns(c, "only_here").expect("columns").is_empty());
}

#[test]
fn a_name_is_written_into_the_statement_whatever_it_holds() {
    let names = [
        "a\"b",
        "a b",
        "select",
        "1",
        "",
        "é",
        "a'b",
        "a]b",
        "a`b",
        "main",
        "temp",
        "on",
        "default",
        "a.b",
        "a;b",
        "-1",
        "a\nb",
        "a)b",
        "a(b",
        "a--b",
        "a/*b",
        "x\"\"y",
        "true",
        "0x10",
        "+",
        "pragma_table_xinfo",
    ];
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    for (n, name) in names.iter().enumerate() {
        let quoted = format!("\"{}\"", name.replace('"', "\"\""));
        c.execute_batch(&format!(
            "CREATE TABLE {quoted}(c{n} INT REFERENCES {quoted}); CREATE INDEX i{n} ON {quoted}(c{n})"
        ))
        .unwrap_or_else(|e| panic!("{name:?}: {e}"));
    }
    for (n, name) in names.iter().enumerate() {
        let row = table(c, name).expect("table");
        assert_eq!(row.len(), 1, "{name:?}");
        assert_eq!(row[0].text(table_list::NAME).expect("text"), *name);
        let own = columns(c, name).expect("columns");
        assert_eq!(own.len(), 1, "{name:?}");
        assert_eq!(
            own[0].text(table_xinfo::NAME).expect("text"),
            format!("c{n}")
        );
        assert_eq!(indexes(c, name).expect("indexes").len(), 1, "{name:?}");
        let keys = foreign_keys(c, name).expect("keys");
        assert_eq!(keys[0].text(foreign_key_list::TABLE).expect("text"), *name);
    }
    // A name is compared as SQLite compares names: ASCII letters fold, others
    // do not.
    assert_eq!(table(c, "SELECT").expect("table").len(), 1);
    assert_eq!(columns(c, "MAIN").expect("columns").len(), 1);
    assert!(table(c, "É").expect("table").is_empty());
    // Negative: a name nothing has answers no row, and a name that cannot be
    // written is an error and not a question about another table.
    assert!(table(c, "nope").expect("table").is_empty());
    assert!(columns(c, "nope").expect("columns").is_empty());
    assert!(indexes(c, "nope").expect("indexes").is_empty());
    assert!(index_columns(c, "nope").expect("index columns").is_empty());
    assert!(foreign_keys(c, "nope").expect("keys").is_empty());
    for ask in [table, columns, indexes, index_columns, foreign_keys] {
        match ask(c, "a\0b") {
            Err(DbError::BadRow(message)) => {
                assert!(message.contains("cannot be written"), "{message}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }
}

#[test]
fn no_row_from_a_pragma_asked_with_no_argument_is_an_error_and_not_an_empty_catalog() {
    // A database with nothing in it still answers each of them.
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let listed = tables(c).expect("listed");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].text(table_list::NAME).expect("text"),
        "sqlite_schema"
    );
    assert!(!tables_everywhere(c).expect("listed").is_empty());
    assert!(!databases(c).expect("databases").is_empty());
    // What a library that does not know a pragma does (measured): no row, no
    // error.
    let unknown = c.query("PRAGMA main.no_such_pragma", &[]).expect("asked");
    assert!(unknown.is_empty());
    match answered("main.no_such_pragma", unknown) {
        Err(DbError::BadRow(message)) => {
            assert!(
                message.contains("`PRAGMA main.no_such_pragma`"),
                "{message}"
            );
            assert!(message.contains("not an empty database"), "{message}");
        }
        other => panic!("expected an error, got {other:?}"),
    }
    // Negative: rows are passed on as they are.
    assert_eq!(answered("main.table_list", listed).expect("rows").len(), 1);
}

#[test]
fn sorted_orders_by_the_key_and_keeps_the_engines_order_within_one() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let rows = || {
        c.query("VALUES (2, 'b'), (1, 'z'), (2, 'a'), (1, NULL)", &[])
            .expect("rows")
    };
    let seconds = |rows: &[Record]| -> Vec<Option<String>> {
        rows.iter()
            .map(|row| row.optional_text(1).expect("text").map(str::to_owned))
            .collect()
    };
    let by_number = sorted(rows(), |row| row.integer(0)).expect("sorted");
    assert_eq!(
        seconds(&by_number),
        [
            Some("z".to_owned()),
            None,
            Some("b".to_owned()),
            Some("a".to_owned())
        ]
    );
    let descending = sorted(rows(), |row| row.integer(0).map(std::cmp::Reverse)).expect("sorted");
    assert_eq!(
        seconds(&descending),
        [
            Some("b".to_owned()),
            Some("a".to_owned()),
            Some("z".to_owned()),
            None
        ]
    );
    // Byte order, as `BINARY` has it: upper case first, and no folding.
    let names = c
        .query("VALUES ('b'), ('B'), ('a'), ('é'), ('_')", &[])
        .expect("rows");
    let ordered: Vec<String> = by_name(names, 0)
        .expect("sorted")
        .iter()
        .map(|row| row.text(0).expect("text").to_owned())
        .collect();
    assert_eq!(ordered, ["B", "_", "a", "b", "é"]);
    // Negative: a key that cannot be read is an error, never a place in the
    // order.
    assert!(matches!(by_name(rows(), 1), Err(DbError::BadRow(_))));
}
