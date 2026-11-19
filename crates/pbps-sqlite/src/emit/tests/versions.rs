//! The version floor and what it gates (#2950). The floor is the oldest SQLite
//! an application opens the file with. It gates text written into the file and
//! kept there (`DEFAULT TRUE`); the in-place `ALTER` forms are run by the library
//! pbps links and leave text any release since 3.35 reads, so today they are not
//! gated. `allows_under` is the one place that decision lives, and these tests
//! run each form under both gates so the other one is a one-line change.

use pbps_model::ColumnChange;

use super::golden::{WithDefault, add_column, uid};
use super::*;
use crate::Version;
use crate::emit::{BOOLEAN_DEFAULT, Form, GATE, Gate, allows_under, emit_under};

#[test]
fn the_arrival_table_is_the_releases_that_brought_each_form() {
    let expected = [
        (Form::RenameColumn, (3, 25, 0)),
        (Form::DropColumn, (3, 35, 0)),
        (Form::SetNotNull, (3, 53, 0)),
        (Form::DropNotNull, (3, 53, 0)),
        (Form::AddCheck, (3, 53, 0)),
        (Form::DropCheck, (3, 53, 0)),
        (Form::DropForeignKey, (3, 53, 0)),
    ];
    assert_eq!(expected.len(), Form::ALL.len());
    for (form, (major, minor, patch)) in expected {
        assert_eq!(
            form.arrival(),
            Version::new(major, minor, patch),
            "{form:?}"
        );
    }
}

#[test]
fn the_text_gate_allows_every_form_and_the_form_gate_follows_the_arrival_table() {
    for form in Form::ALL {
        for floor in [
            Version::new(3, 8, 0),
            Version::new(3, 35, 0),
            Version::new(3, 54, 1),
        ] {
            assert!(allows_under(Gate::Text, floor, form), "{form:?} at {floor}");
            assert_eq!(
                allows_under(Gate::EveryForm, floor, form),
                floor >= form.arrival(),
                "{form:?} at {floor}"
            );
        }
    }
    // The setting in force is the text gate (the decision's one line).
    assert_eq!(GATE, Gate::Text);
}

fn rename_column() -> Change {
    Change::Column(ColumnChange::RenameColumn {
        uid: uid(UidKind::Column, "t.b"),
        table: tn("t"),
        from: "b".into(),
        to: "c".into(),
        table_was: None,
    })
}

#[test]
fn rename_column_under_both_gates_below_and_at_its_arrival() {
    let old = Sqlite::with_floor(Version::new(3, 24, 0));
    let new = Sqlite::with_floor(Version::new(3, 25, 0));
    for dialect in [old, new] {
        assert!(emit_under(Gate::Text, &dialect, &rename_column()).is_ok());
    }
    assert!(emit_under(Gate::EveryForm, &new, &rename_column()).is_ok());
    match emit_under(Gate::EveryForm, &old, &rename_column()) {
        Err(DialectError::NotBuilt { part, .. }) => {
            assert!(part.contains("table rebuild"), "{part}");
            assert!(part.contains("3.25.0"), "{part}");
        }
        other => panic!("a form below its release must be the rebuild's, got {other:?}"),
    }
}

fn boolean_default() -> Change {
    add_column("t", "x", col("boolean").not_null().with_default("TRUE"))
}

#[test]
fn a_boolean_default_is_text_kept_in_the_file_so_the_floor_gates_it_under_either_gate() {
    let before = Sqlite::with_floor(Version::new(3, 22, 0));
    let at = Sqlite::with_floor(BOOLEAN_DEFAULT);
    for gate in [Gate::Text, Gate::EveryForm] {
        assert!(
            emit_under(gate, &at, &boolean_default()).is_ok(),
            "{gate:?}"
        );
        match emit_under(gate, &before, &boolean_default()) {
            Err(DialectError::NotBuilt { part, .. }) => assert!(part.contains("3.23.0"), "{part}"),
            other => panic!("{gate:?}: {other:?}"),
        }
    }
    // The same text in a table, which is where the gate matters most.
    let mut t = table(&[("x", col("boolean").with_default("false"))]);
    t.columns.insert("y".into(), col("integer"));
    assert!(
        emit_under(
            Gate::Text,
            &before,
            &super::golden::create_table_change("t", &t)
        )
        .is_err()
    );
    assert!(
        emit_under(
            Gate::Text,
            &at,
            &super::golden::create_table_change("t", &t)
        )
        .is_ok()
    );
}

#[test]
fn the_boolean_default_means_a_constant_only_from_its_release() {
    // The engine measured on the bundled release: `TRUE` is a constant. On a
    // release before 3.23 it is a name, which this crate cannot run, so the
    // arrival is pinned from the release notes and the bundled behaviour here.
    let mut db = built(
        "CREATE TABLE t(a integer DEFAULT 1, b boolean DEFAULT TRUE); INSERT INTO t(a) VALUES (1)",
    );
    let rows = db
        .connection()
        .query("SELECT b FROM t", &[])
        .expect("reads");
    assert_eq!(rows[0].integer(0).expect("an integer, not text"), 1);
    assert_eq!(BOOLEAN_DEFAULT, Version::new(3, 23, 0));
}
