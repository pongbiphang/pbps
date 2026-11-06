//! The reader of a stored view rule admits one plain `SELECT` of expressions
//! over one relation and nothing else (DEC-2563.8). The rules are measured ones,
//! printed by 16.15 and 18.6 for views over a table with a function that takes
//! its row type.

use super::*;

const PLAIN: [(u32, &str); 2] = [
    (16, include_str!("fixtures/view-plain-16.nodes")),
    (18, include_str!("fixtures/view-plain-18.nodes")),
];
const SUBLINK: [(u32, &str); 2] = [
    (16, include_str!("fixtures/view-sublink-16.nodes")),
    (18, include_str!("fixtures/view-sublink-18.nodes")),
];
const JOIN: [(u32, &str); 2] = [
    (16, include_str!("fixtures/view-join-16.nodes")),
    (18, include_str!("fixtures/view-join-18.nodes")),
];

#[test]
fn a_plain_select_of_expressions_is_read_entry_by_entry() {
    for (major, text) in PLAIN {
        let bound = bound_view(text, major).unwrap_or_else(|e| panic!("PG{major}: {e}"));
        // `app.visible(t) IS TRUE` calls one routine and no operator;
        // `(t.id + 1) IS NOT NULL` uses an operator and its function. Each entry
        // binds its own objects, none shared.
        assert_eq!(bound.len(), 2, "PG{major}");
        assert_eq!(bound[0].routines.len(), 1, "PG{major}");
        assert!(bound[0].operators.is_empty(), "PG{major}");
        assert_eq!(bound[1].operators.len(), 1, "PG{major}");
        assert!(
            bound[0].routines.is_disjoint(&bound[1].routines),
            "PG{major}"
        );
        assert!(!bound[1].reads_session, "PG{major}");
    }
}

#[test]
fn a_view_that_is_more_than_expressions_over_one_relation_is_refused() {
    for (major, text) in SUBLINK {
        let why = bound_view(text, major).expect_err("a sublink is never read");
        assert!(
            why.contains("more than a list of plain expressions"),
            "PG{major}: {why}"
        );
    }
    for (major, text) in JOIN {
        let why = bound_view(text, major).expect_err("a second relation is never read");
        assert!(why.contains("more or less than one"), "PG{major}: {why}");
    }
    for (major, text) in PLAIN {
        // Each is the plain view with one field set, so the refusal is that field's.
        for (from, to, needle) in [
            (
                ":quals <>",
                ":quals {CONST :consttype 16 :consttypmod -1 :constcollid 0 :constlen 1 \
                 :constbyval true :constisnull false :location -1 :constvalue 1 [ 1 0 0 0 0 0 0 0 ]}",
                "WHERE clause",
            ),
            (
                ":hasAggs false",
                ":hasAggs true",
                "more than a list of plain expressions",
            ),
            (
                ":rtekind 0",
                ":rtekind 1",
                "something other than a relation",
            ),
            (
                ":securityQuals <>",
                ":securityQuals (1)",
                "more than a plain table",
            ),
        ] {
            assert!(text.contains(from), "PG{major}: the fixture lacks {from}");
            let why = bound_view(&text.replacen(from, to, 1), major)
                .expect_err(&format!("PG{major}: {from} -> {to}"));
            assert!(why.contains(needle), "PG{major}: {from}: {why}");
        }
    }
}

#[test]
fn a_view_is_read_only_on_the_majors_it_was_measured_on() {
    for (_, text) in PLAIN {
        for major in [15, 17, 19] {
            assert!(bound_view(text, major).is_err(), "PG{major}");
        }
    }
    assert!(bound_view("(not a node tree", 18).is_err());
}
