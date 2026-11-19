//! What the emitter answers where it writes no statement (#2950). The match in
//! `emit` names every variant of every family with no wildcard, so a new kind of
//! change cannot arrive unanswered; these tests pin the answers that carry a
//! decision: each is an error that says which part, and none is an empty
//! success.

use pbps_model::{
    CheckConstraint, ColumnChange, ConstraintChange, ForeignKey, ObjectLocalName, PrimaryKey,
    ReferentialAction, RoleChange, UniqueConstraint,
};

use super::golden::{index, uid};
use super::*;

#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// The table rebuild (item 8).
    Rebuild,
    /// The next part of this item.
    Later,
    /// Refused by validation; the emitter answers too.
    Unsupported,
}

fn answer(change: Change) -> Answer {
    match statements(&Sqlite::default(), change) {
        Ok(out) => panic!("a statement was written: {:?}", sql_text(&out)),
        Err(DialectError::NotBuilt { part, .. }) if part.contains("table rebuild") => {
            Answer::Rebuild
        }
        Err(DialectError::NotBuilt { part, .. }) if part.contains("next part of item 3") => {
            Answer::Later
        }
        Err(DialectError::Unsupported { .. }) => Answer::Unsupported,
        Err(other) => panic!("unexpected answer: {other:?}"),
    }
}

fn pk() -> PrimaryKey {
    PrimaryKey {
        name: None,
        columns: vec!["a".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    }
}

fn fk() -> ForeignKey {
    ForeignKey {
        columns: vec!["a".into()],
        references_table: tn("p"),
        references_columns: vec!["id".into()],
        on_delete: ReferentialAction::NoAction,
        on_update: ReferentialAction::NoAction,
    }
}

fn c(name: &str) -> pbps_model::ColumnRef {
    pbps_model::ColumnRef {
        table: tn("t"),
        name: name.into(),
    }
}

#[test]
fn what_sqlite_cannot_change_in_place_is_the_rebuilds() {
    let n = || ObjectLocalName::new("k");
    let cases = [
        (
            "set primary key",
            Change::Constraint(ConstraintChange::SetPrimaryKey {
                table: tn("t"),
                from: None,
                to: Some(pk()),
                nonclustered: false,
            }),
        ),
        (
            "add unique",
            Change::Constraint(ConstraintChange::AddUnique {
                table: tn("t"),
                name: n(),
                constraint: UniqueConstraint {
                    columns: vec!["a".into()],
                    storage_parameters: Default::default(),
                    filegroup: None,
                },
                clustered: false,
            }),
        ),
        (
            "drop unique",
            Change::Constraint(ConstraintChange::DropUnique {
                table: tn("t"),
                name: n(),
            }),
        ),
        (
            "add foreign key",
            Change::Constraint(ConstraintChange::AddForeignKey {
                table: tn("t"),
                name: n(),
                constraint: Box::new(fk()),
            }),
        ),
        (
            "alter type",
            Change::Column(ColumnChange::AlterColumnType {
                uid: uid(UidKind::Column, "t.a"),
                column: c("a"),
                from: ty("integer"),
                to: ty("text"),
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
                using: None,
            }),
        ),
        (
            "alter default",
            Change::Column(ColumnChange::AlterColumnDefault {
                uid: uid(UidKind::Column, "t.a"),
                column: c("a"),
                from: None,
                to: Some("1".into()),
            }),
        ),
    ];
    for (what, change) in cases {
        assert_eq!(answer(change), Answer::Rebuild, "{what}");
    }
}

#[test]
fn the_forms_of_the_next_part_are_said_to_be_so_and_never_written() {
    let n = || ObjectLocalName::new("k");
    let cases = [
        (
            "drop foreign key",
            Change::Constraint(ConstraintChange::DropForeignKey {
                table: tn("t"),
                name: n(),
            }),
        ),
        (
            "add check",
            Change::Constraint(ConstraintChange::AddCheck {
                table: tn("t"),
                name: n(),
                constraint: CheckConstraint {
                    expression: "a > 0".into(),
                },
            }),
        ),
        (
            "drop check",
            Change::Constraint(ConstraintChange::DropCheck {
                table: tn("t"),
                name: n(),
            }),
        ),
        (
            "drop column",
            Change::Column(ColumnChange::DropColumn {
                uid: uid(UidKind::Column, "t.a"),
                column: c("a"),
            }),
        ),
        (
            "nullability",
            Change::Column(ColumnChange::AlterColumnNullability {
                uid: uid(UidKind::Column, "t.a"),
                column: c("a"),
                ty: ty("integer"),
                to_nullable: false,
                collation: None,
                through_external_partitions: false,
            }),
        ),
    ];
    for (what, change) in cases {
        assert_eq!(answer(change), Answer::Later, "{what}");
    }
}

#[test]
fn what_validation_refuses_is_refused_by_the_emitter_too() {
    let role = || pbps_model::RoleName::new("r");
    assert_eq!(
        answer(Change::Role(RoleChange::CreateRole {
            uid: uid(UidKind::Role, "r"),
            name: role()
        })),
        Answer::Unsupported
    );
    assert_eq!(
        answer(Change::Role(RoleChange::DropRole {
            uid: uid(UidKind::Role, "r"),
            name: role(),
            members: Vec::new()
        })),
        Answer::Unsupported
    );
}

#[test]
fn an_index_the_engine_cannot_hold_is_refused_not_written() {
    let mut expression = index(&["a"], false, None);
    expression.columns[0].key = pbps_model::IndexKey::Expression("lower(a)".into());
    let add = |i| {
        Change::Index(pbps_model::IndexChange::AddIndex {
            table: tn("t"),
            name: ObjectLocalName::new("i"),
            index: Box::new(i),
            clustered: false,
        })
    };
    assert!(statements(&Sqlite::default(), add(expression)).is_err());
    assert!(statements(&Sqlite::default(), add(index(&[], false, None))).is_err());
    let clustered = Change::Index(pbps_model::IndexChange::AddIndex {
        table: tn("t"),
        name: ObjectLocalName::new("i"),
        index: Box::new(index(&["a"], false, None)),
        clustered: true,
    });
    assert!(statements(&Sqlite::default(), clustered).is_err());
}

#[test]
fn a_table_the_declaration_could_not_make_is_refused_whole() {
    // A key naming a column the table lacks, and a second schema.
    let mut broken = table(&[("a", col("integer"))]);
    broken.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["zz".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    });
    assert!(
        statements(
            &Sqlite::default(),
            super::golden::create_table_change("t", &broken)
        )
        .is_err()
    );
    let elsewhere = Change::Table(pbps_model::TableChange::DropTable {
        uid: uid(UidKind::Table, "t"),
        name: pbps_model::TableName::new("other", "t"),
        detach_from: None,
    });
    assert!(matches!(
        statements(&Sqlite::default(), elsewhere),
        Err(DialectError::Unsupported { .. })
    ));
}

#[test]
fn a_partition_shaped_table_change_is_refused_and_a_stray_row_limit_is_ignored() {
    let create = |beside: Option<pbps_model::TableName>, limit: Option<u64>| {
        Change::Table(pbps_model::TableChange::CreateTable {
            uid: uid(UidKind::Table, "t"),
            name: tn("t"),
            table: Box::new(table(&[("a", col("integer"))])),
            beside_default: beside,
            moves_at_most: limit,
        })
    };
    assert_eq!(answer(create(Some(tn("rest")), None)), Answer::Unsupported);
    assert!(
        statements(&Sqlite::default(), create(None, Some(5))).is_ok(),
        "a limit with nothing to move is ignored"
    );
    let detach = Change::Table(pbps_model::TableChange::DropTable {
        uid: uid(UidKind::Table, "t"),
        name: tn("t"),
        detach_from: Some(tn("parent")),
    });
    assert_eq!(answer(detach), Answer::Unsupported);
}
