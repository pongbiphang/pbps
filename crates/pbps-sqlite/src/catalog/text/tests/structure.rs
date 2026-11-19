//! What the tokenizer reads, stated exactly. Each test names a property of
//! the subset and compares the whole result, so a field that moves is seen.

use super::super::{
    Conflict, Constraint, Deferral, Kind, Marked, Marker, Place, Reaction, TableText, Target, table,
};

fn t(sql: &str) -> TableText {
    table("t", sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn names(columns: &[&str]) -> Vec<String> {
    columns.iter().map(|c| (*c).to_owned()).collect()
}

fn cons(name: Option<&str>, kind: Kind, place: Place) -> Constraint {
    Constraint {
        name: name.map(str::to_owned),
        kind,
        place,
    }
}

fn marked(marker: Marker, target: Target) -> Marked {
    Marked { marker, target }
}

#[test]
fn a_table_is_its_name_its_columns_in_order_and_nothing_it_does_not_hold() {
    let read = t("CREATE TABLE t(a int, \"b c\" text, [d] , `e`)");
    assert_eq!(read.name, "t");
    assert_eq!(read.columns, names(&["a", "b c", "d", "e"]));
    assert!(read.constraints.is_empty() && read.markers.is_empty());
}

#[test]
fn every_quoted_form_is_unquoted_for_the_table_a_column_and_a_constraint() {
    let read = t(
        "CREATE TABLE \"a\"\"b\"(`c``d` int, [e f] int, 'g''h' int, CONSTRAINT \"k\"\"1\" CHECK (1))",
    );
    assert_eq!(read.name, "a\"b");
    assert_eq!(read.columns, names(&["c`d", "e f", "g'h"]));
    assert_eq!(read.constraints[0].name.as_deref(), Some("k\"1"));
}

#[test]
fn the_type_is_skipped_whatever_it_holds_and_the_constraints_after_it_are_found() {
    for ty in [
        "",
        "int",
        "VARCHAR( 10 )",
        "DECIMAL(10, 2)",
        "UNSIGNED BIG INT",
        "\"my type\"",
        "[t]",
        "`x y`",
        "unsigned big int",
        "int(5)",
        "numeric(+3, -2)",
    ] {
        let read = t(&["CREATE TABLE t(a ", ty, " NOT NULL DEFAULT 5)"].concat());
        assert_eq!(read.columns, names(&["a"]), "{ty}");
        assert_eq!(
            read.constraints,
            vec![
                cons(None, Kind::NotNull, Place::Column(0)),
                cons(None, Kind::Default { text: "5".into() }, Place::Column(0)),
            ],
            "{ty}"
        );
    }
}

#[test]
fn a_column_named_like_a_keyword_sqlite_takes_bare_is_a_column() {
    let read = t(
        "CREATE TABLE t(key int, action int NOT NULL, generated int, match int, strict int, \"unique\" int)",
    );
    assert_eq!(
        read.columns,
        names(&["key", "action", "generated", "match", "strict", "unique"])
    );
    assert_eq!(read.constraints.len(), 1);
}

#[test]
fn column_level_constraints_are_found_with_their_names_in_the_order_written() {
    let read = t(
        "CREATE TABLE t(a int CONSTRAINT nn NOT NULL CONSTRAINT dd DEFAULT 5 CONSTRAINT ck CHECK (a > 0) CONSTRAINT uq UNIQUE CONSTRAINT rf REFERENCES p(id) ON DELETE CASCADE, b int PRIMARY KEY)",
    );
    assert_eq!(
        read.constraints,
        vec![
            cons(Some("nn"), Kind::NotNull, Place::Column(0)),
            cons(
                Some("dd"),
                Kind::Default { text: "5".into() },
                Place::Column(0)
            ),
            cons(
                Some("ck"),
                Kind::Check {
                    expression: "a > 0".into()
                },
                Place::Column(0)
            ),
            cons(
                Some("uq"),
                Kind::Unique {
                    columns: names(&["a"])
                },
                Place::Column(0)
            ),
            cons(
                Some("rf"),
                Kind::ForeignKey {
                    columns: names(&["a"]),
                    parent: "p".into(),
                    parent_columns: names(&["id"]),
                    on_delete: Some(Reaction::Cascade),
                    on_update: None,
                },
                Place::Column(0)
            ),
            cons(
                None,
                Kind::PrimaryKey {
                    columns: names(&["b"])
                },
                Place::Column(1)
            ),
        ]
    );
}

#[test]
fn table_level_constraints_follow_the_columns_and_keep_their_key_order() {
    let read = t(
        "CREATE TABLE t(a int, b int, CONSTRAINT pk PRIMARY KEY (b, a), UNIQUE (a, b), CONSTRAINT c CHECK (a < b), FOREIGN KEY (a, b) REFERENCES q)",
    );
    assert_eq!(
        read.constraints,
        vec![
            cons(
                Some("pk"),
                Kind::PrimaryKey {
                    columns: names(&["b", "a"])
                },
                Place::Table
            ),
            cons(
                None,
                Kind::Unique {
                    columns: names(&["a", "b"])
                },
                Place::Table
            ),
            cons(
                Some("c"),
                Kind::Check {
                    expression: "a < b".into()
                },
                Place::Table
            ),
            cons(
                None,
                Kind::ForeignKey {
                    columns: names(&["a", "b"]),
                    parent: "q".into(),
                    parent_columns: vec![],
                    on_delete: None,
                    on_update: None,
                },
                Place::Table
            ),
        ]
    );
}

#[test]
fn an_unnamed_constraint_has_no_name_and_the_tokenizer_invents_none() {
    let read =
        t("CREATE TABLE t(a int UNIQUE CHECK (a > 0) REFERENCES p, CHECK (a < 9), UNIQUE (a, a))");
    assert!(
        read.constraints.iter().all(|c| c.name.is_none()),
        "{read:?}"
    );
    assert_eq!(read.constraints.len(), 5);
}

#[test]
fn an_expression_is_its_text_between_the_parentheses_verbatim_with_its_edges_trimmed() {
    let read =
        t("CREATE TABLE t(a text, CHECK (  a <> ')' AND /* ) */ (a IN ('x', \"y\")) -- )\n ))");
    assert_eq!(
        read.constraints[0].kind,
        Kind::Check {
            expression: "a <> ')' AND /* ) */ (a IN ('x', \"y\")) -- )".into()
        }
    );
    // A comment ends at its first `*/`: what follows is code.
    let read = t("CREATE TABLE t(a int, CHECK (a /* /* */ > 0))");
    assert_eq!(
        read.constraints[0].kind,
        Kind::Check {
            expression: "a /* /* */ > 0".into()
        }
    );
}

#[test]
fn a_default_is_the_text_after_default_in_one_of_the_listed_forms() {
    for default in [
        "1",
        "(1)",
        "-5",
        "+5",
        "- 5",
        "1.5e3",
        "1e+5",
        "0x1F",
        ".5",
        "'it''s'",
        "x'ab'",
        "NULL",
        "true",
        "CURRENT_TIMESTAMP",
        "( 'a' || 'b' )",
        "('x)' )",
        "((1))",
        "(1 /* ) */)",
    ] {
        let read = t(&["CREATE TABLE t(a int DEFAULT ", default, " NOT NULL)"].concat());
        assert_eq!(
            read.constraints,
            vec![
                cons(
                    None,
                    Kind::Default {
                        text: default.to_owned()
                    },
                    Place::Column(0)
                ),
                cons(None, Kind::NotNull, Place::Column(0)),
            ],
            "{default}"
        );
    }
}

#[test]
fn a_foreign_key_clause_gives_its_parent_columns_and_actions() {
    let read = t(
        "CREATE TABLE t(a int, b int, c int, FOREIGN KEY (a) REFERENCES \"p\"(\"id\") ON UPDATE NO ACTION ON DELETE SET DEFAULT, FOREIGN KEY (b) REFERENCES p ON DELETE SET NULL, FOREIGN KEY (c) REFERENCES p(id) ON DELETE RESTRICT ON UPDATE CASCADE)",
    );
    let actions: Vec<_> = read
        .constraints
        .iter()
        .map(|c| match &c.kind {
            Kind::ForeignKey {
                on_delete,
                on_update,
                parent_columns,
                ..
            } => (*on_delete, *on_update, parent_columns.len()),
            other @ (Kind::PrimaryKey { .. }
            | Kind::Unique { .. }
            | Kind::Check { .. }
            | Kind::NotNull
            | Kind::Null
            | Kind::Default { .. }) => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        actions,
        vec![
            (Some(Reaction::SetDefault), Some(Reaction::NoAction), 1),
            (Some(Reaction::SetNull), None, 0),
            (Some(Reaction::Restrict), Some(Reaction::Cascade), 1),
        ]
    );
}

#[test]
fn every_feature_the_model_cannot_hold_is_a_marker_that_says_where_it_is() {
    let read = t(
        "CREATE TABLE t(a text COLLATE nocase, b int NOT NULL ON CONFLICT IGNORE, c integer PRIMARY KEY DESC ON CONFLICT FAIL AUTOINCREMENT, d int, e int GENERATED ALWAYS AS (d + 1) STORED, f int AS (d) VIRTUAL, g int AS (d), h int REFERENCES p(id) MATCH FULL NOT DEFERRABLE INITIALLY DEFERRED, UNIQUE (a COLLATE rtrim DESC, d) ON CONFLICT REPLACE, FOREIGN KEY (d) REFERENCES p DEFERRABLE INITIALLY IMMEDIATE) STRICT, WITHOUT ROWID",
    );
    let ids: Vec<usize> = (0..read.constraints.len()).collect();
    assert_eq!(ids.len(), 5, "{:?}", read.constraints);
    // The constraints, to say which marker is on which.
    let by_kind = |wanted: &str| {
        read.constraints
            .iter()
            .position(|c| match (&c.kind, wanted) {
                (Kind::NotNull, "not null") | (Kind::PrimaryKey { .. }, "primary") => true,
                (Kind::ForeignKey { columns, .. }, "fk-column") => columns == &names(&["h"]),
                (Kind::ForeignKey { columns, .. }, "fk-table") => columns == &names(&["d"]),
                (Kind::Unique { .. }, "unique") => true,
                _ => false,
            })
            .unwrap_or_else(|| panic!("{wanted}"))
    };
    let (not_null, primary, fk_column, fk_table, unique) = (
        by_kind("not null"),
        by_kind("primary"),
        by_kind("fk-column"),
        by_kind("fk-table"),
        by_kind("unique"),
    );
    let expected = vec![
        marked(
            Marker::Collate {
                name: "nocase".into(),
            },
            Target::Column(0),
        ),
        marked(
            Marker::Conflict(Conflict::Ignore),
            Target::Constraint(not_null),
        ),
        marked(
            Marker::KeyDirection {
                position: 0,
                descending: true,
            },
            Target::Constraint(primary),
        ),
        marked(
            Marker::Conflict(Conflict::Fail),
            Target::Constraint(primary),
        ),
        marked(Marker::Autoincrement, Target::Constraint(primary)),
        marked(Marker::Generated { stored: Some(true) }, Target::Column(4)),
        marked(
            Marker::Generated {
                stored: Some(false),
            },
            Target::Column(5),
        ),
        marked(Marker::Generated { stored: None }, Target::Column(6)),
        marked(
            Marker::Match {
                name: "FULL".into(),
            },
            Target::Constraint(fk_column),
        ),
        marked(
            Marker::Deferrable { not: true },
            Target::Constraint(fk_column),
        ),
        marked(
            Marker::Initially(Deferral::Deferred),
            Target::Constraint(fk_column),
        ),
        marked(
            Marker::Deferrable { not: false },
            Target::Constraint(fk_table),
        ),
        marked(
            Marker::Initially(Deferral::Immediate),
            Target::Constraint(fk_table),
        ),
        marked(
            Marker::KeyCollate {
                position: 0,
                name: "rtrim".into(),
            },
            Target::Constraint(unique),
        ),
        marked(
            Marker::KeyDirection {
                position: 0,
                descending: true,
            },
            Target::Constraint(unique),
        ),
        marked(
            Marker::Conflict(Conflict::Replace),
            Target::Constraint(unique),
        ),
        marked(Marker::Strict, Target::Table),
        marked(Marker::WithoutRowid, Target::Table),
    ];
    let mut found = read.markers.clone();
    let mut wanted = expected;
    // Order is by position in the text, which the vectors already follow, but
    // a table constraint's markers come after its siblings': compare as sets.
    let key = |m: &Marked| format!("{m:?}");
    found.sort_by_key(key);
    wanted.sort_by_key(key);
    assert_eq!(found, wanted);
}

#[test]
fn a_not_deferrable_foreign_key_and_a_deferrable_one_are_two_markers_not_none() {
    let read = t("CREATE TABLE t(a int REFERENCES p NOT DEFERRABLE, b int REFERENCES p)");
    assert_eq!(
        read.markers,
        vec![marked(
            Marker::Deferrable { not: true },
            Target::Constraint(0)
        )]
    );
}

#[test]
fn what_the_engine_rewrites_is_in_the_subset() {
    let read = t(
        "CREATE TABLE \"t\" (\n    \"id\" integer NOT NULL,\n    \"quantity\" integer, \"z\" integer NOT NULL DEFAULT 0,\n    PRIMARY KEY (\"id\"),\n    CONSTRAINT \"c_qty\" CHECK (\"quantity\" >= 0)\n, CONSTRAINT \"c_z\" CHECK (z >= 0))",
    );
    assert_eq!(read.columns, names(&["id", "quantity", "z"]));
    let checks: Vec<_> = read
        .constraints
        .iter()
        .filter_map(|c| match &c.kind {
            Kind::Check { expression } => {
                Some((c.name.clone().expect("named"), expression.clone()))
            }
            Kind::PrimaryKey { .. }
            | Kind::Unique { .. }
            | Kind::ForeignKey { .. }
            | Kind::NotNull
            | Kind::Null
            | Kind::Default { .. } => None,
        })
        .collect();
    assert_eq!(
        checks,
        vec![
            ("c_qty".to_owned(), "\"quantity\" >= 0".to_owned()),
            ("c_z".to_owned(), "z >= 0".to_owned()),
        ]
    );
}

#[test]
fn generated_and_always_without_an_as_are_words_of_a_type_and_not_a_generated_column() {
    // Neither is reserved, so `generated always` ends a type until an `AS`
    // follows; the engine cuts them from the declared type, and no marker is
    // made for what is not there.
    let read =
        t("CREATE TABLE t(a int GENERATED ALWAYS, b generated, c int GENERATED ALWAYS AS (a))");
    assert_eq!(read.columns, names(&["a", "b", "c"]));
    assert_eq!(
        read.markers,
        vec![marked(
            Marker::Generated { stored: None },
            Target::Column(2)
        )]
    );
}
