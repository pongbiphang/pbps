//! The emitter's accept and refuse answers for `ADD COLUMN` against the
//! engine's, both ways (#2950). The engine's answer is measured on a table that
//! already holds a row, which is the case a plan has to be safe for (SQLite takes
//! a `NOT NULL` column with no default only on an empty table, and a plan cannot
//! know that).

use super::golden::{WithDefault, add_column};
use super::*;

struct Case {
    name: &'static str,
    ty: &'static str,
    not_null: bool,
    default: Option<&'static str>,
    /// The column as the `ALTER` would spell it, so the engine can be asked
    /// about the statement the emitter declined to write.
    hand: &'static str,
    /// Whether the emitter writes the statement.
    emitted: bool,
}

const CASES: &[Case] = &[
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(1 /* unit */)"),
        hand: "x integer NOT NULL DEFAULT (1 /* unit */)",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(/* unit */ 1)"),
        hand: "x integer NOT NULL DEFAULT (/* unit */ 1)",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(1 -- unit\n)"),
        hand: "x integer NOT NULL DEFAULT (1 -- unit\n)",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("( /* a */ ( 'x' /* b */ ) )"),
        hand: "x integer NOT NULL DEFAULT ( /* a */ ( 'x' /* b */ ) )",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(- /* s */ 1)"),
        hand: "x integer NOT NULL DEFAULT (- /* s */ 1)",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("('/* not a comment */')"),
        hand: "x integer NOT NULL DEFAULT ('/* not a comment */')",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(1 /* + 1 */)"),
        hand: "x integer NOT NULL DEFAULT (1 /* + 1 */)",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(1 /* unit */ + 1)"),
        hand: "x integer NOT NULL DEFAULT (1 /* unit */ + 1)",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(NULL /* none */)"),
        hand: "x integer NOT NULL DEFAULT (NULL /* none */)",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: false,
        default: Some("(/* none */ NULL)"),
        hand: "x integer DEFAULT (/* none */ NULL)",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(1)"),
        hand: "x integer NOT NULL DEFAULT (1)",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(-1)"),
        hand: "x integer NOT NULL DEFAULT (-1)",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("((1))"),
        hand: "x integer NOT NULL DEFAULT ((1))",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("( 1 )"),
        hand: "x integer NOT NULL DEFAULT ( 1 )",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("('hi')"),
        hand: "x integer NOT NULL DEFAULT ('hi')",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(x'00')"),
        hand: "x integer NOT NULL DEFAULT (x'00')",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(TRUE)"),
        hand: "x integer NOT NULL DEFAULT (TRUE)",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(1+1)"),
        hand: "x integer NOT NULL DEFAULT (1+1)",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(CURRENT_TIME)"),
        hand: "x integer NOT NULL DEFAULT (CURRENT_TIME)",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("((1)+1)"),
        hand: "x integer NOT NULL DEFAULT ((1)+1)",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("(NULL)"),
        hand: "x integer NOT NULL DEFAULT (NULL)",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: false,
        default: Some("(NULL)"),
        hand: "x integer DEFAULT (NULL)",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "text",
        not_null: false,
        default: None,
        hand: "x text",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: None,
        hand: "x integer NOT NULL",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("0"),
        hand: "x integer NOT NULL DEFAULT 0",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: true,
        default: Some("NULL"),
        hand: "x integer NOT NULL DEFAULT NULL",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: false,
        default: Some("NULL"),
        hand: "x integer DEFAULT NULL",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "text",
        not_null: false,
        default: Some("'d'"),
        hand: "x text DEFAULT 'd'",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: false,
        default: Some("-1"),
        hand: "x integer DEFAULT -1",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "real",
        not_null: true,
        default: Some("1.5e3"),
        hand: "x real NOT NULL DEFAULT 1.5e3",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "blob",
        not_null: true,
        default: Some("x'ab'"),
        hand: "x blob NOT NULL DEFAULT x'ab'",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "boolean",
        not_null: true,
        default: Some("TRUE"),
        hand: "x boolean NOT NULL DEFAULT TRUE",
        emitted: true,
    },
    Case {
        name: "x",
        ty: "text",
        not_null: false,
        default: Some("CURRENT_TIMESTAMP"),
        hand: "x text DEFAULT CURRENT_TIMESTAMP",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "text",
        not_null: true,
        default: Some("current_date"),
        hand: "x text NOT NULL DEFAULT current_date",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "text",
        not_null: false,
        default: Some("CURRENT_TIME"),
        hand: "x text DEFAULT CURRENT_TIME",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "integer",
        not_null: false,
        default: Some("(1 + 1)"),
        hand: "x integer DEFAULT (1 + 1)",
        emitted: false,
    },
    Case {
        name: "x",
        ty: "text",
        not_null: false,
        default: Some("(lower('A'))"),
        hand: "x text DEFAULT (lower('A'))",
        emitted: false,
    },
    Case {
        name: "rowid",
        ty: "integer",
        not_null: false,
        default: None,
        hand: "rowid integer",
        emitted: false,
    },
    Case {
        name: "_ROWID_",
        ty: "integer",
        not_null: false,
        default: None,
        hand: "_ROWID_ integer",
        emitted: false,
    },
    Case {
        name: "Oid",
        ty: "integer",
        not_null: false,
        default: None,
        hand: "Oid integer",
        emitted: false,
    },
    Case {
        name: "a b",
        ty: "text",
        not_null: false,
        default: None,
        hand: "\"a b\" text",
        emitted: true,
    },
    Case {
        name: "a\"b",
        ty: "text",
        not_null: false,
        default: None,
        hand: "\"a\"\"b\" text",
        emitted: true,
    },
];

#[test]
fn add_column_is_written_exactly_when_the_engine_takes_it_on_a_table_with_rows() {
    for case in CASES {
        let mut column = col(case.ty);
        column.nullable = !case.not_null;
        if let Some(text) = case.default {
            column = column.with_default(text);
        }
        let result = statements(&Sqlite::default(), add_column("t", case.name, column));
        let mut db = built("CREATE TABLE t(a integer); INSERT INTO t VALUES (1)");
        let engine = db
            .connection()
            .execute_batch(&["ALTER TABLE main.t ADD COLUMN ", case.hand].concat());
        assert_eq!(result.is_ok(), case.emitted, "{}: {result:?}", case.hand);
        assert_eq!(
            engine.is_ok(),
            case.emitted,
            "the engine's answer differs from the emitter's for {}: {engine:?}",
            case.hand
        );
        match result {
            Ok(out) => {
                let mut db = built("CREATE TABLE t(a integer); INSERT INTO t VALUES (1)");
                run(db.connection(), &out).unwrap_or_else(|e| panic!("{}: {e}", case.hand));
            }
            Err(DialectError::NotBuilt { part, .. }) => {
                assert!(!part.is_empty(), "{}", case.hand);
            }
            Err(other) => panic!("{}: {other:?}", case.hand),
        }
    }
}

#[test]
fn a_not_null_column_without_a_default_names_both_ways_out() {
    let err = statements(
        &Sqlite::default(),
        add_column("t", "x", col("integer").not_null()),
    )
    .expect_err("refused");
    let text = err.to_string();
    assert!(text.contains("Give the column a default"), "{text}");
    assert!(text.contains("table rebuild"), "{text}");
    assert!(
        text.contains("`t`.`x`") || text.contains("t.x") || text.contains("main.t"),
        "{text}"
    );
}

#[test]
fn a_rebuild_refusal_is_never_an_empty_success() {
    for case in CASES.iter().filter(|c| !c.emitted) {
        let mut column = col(case.ty);
        column.nullable = !case.not_null;
        if let Some(text) = case.default {
            column = column.with_default(text);
        }
        assert!(
            statements(&Sqlite::default(), add_column("t", case.name, column)).is_err(),
            "{}",
            case.hand
        );
    }
}
