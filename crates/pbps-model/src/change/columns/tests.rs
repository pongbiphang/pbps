use super::*;
use crate::{Column, OwnedSequence};

#[test]
fn sequence_lifetimes_and_retunes_are_statement_properties() {
    use Attribute::*;
    use Lifetime::*;
    let sequence = ValueSource::Sequence(OwnedSequence::serial());
    for (from, to, expected) in [
        (
            ValueSource::None,
            sequence.clone(),
            vec![(Default, Retain), (Numbering, Create)],
        ),
        (
            sequence.clone(),
            ValueSource::None,
            vec![(Default, Retain), (Numbering, Retire)],
        ),
        (sequence.clone(), sequence, vec![(Numbering, Retain)]),
    ] {
        let change = Change::AlterColumnNumbering {
            uid: "c_aaaaaa".parse().unwrap(),
            column: "app.p.d".parse().unwrap(),
            ty: "integer".parse().unwrap(),
            from,
            to,
        };
        assert_eq!(change.parent_column_uid().unwrap().as_str(), "c_aaaaaa");
        assert_eq!(
            change
                .column_transitions()
                .into_iter()
                .map(|t| (t.property, t.lifetime))
                .collect::<Vec<_>>(),
            expected
                .into_iter()
                .map(|(a, l)| (ColumnProperty::Column(a), l))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn recollation_does_not_invent_type_default_or_nullability_events() {
    let mut change = Change::AlterColumnType {
        uid: "c_aaaaaa".parse().unwrap(),
        column: "app.p.d".parse().unwrap(),
        from: "text".parse().unwrap(),
        to: "text".parse().unwrap(),
        from_nullable: true,
        to_nullable: true,
        from_collation: None,
        to_collation: Some(Collation::new("pg_catalog.C")),
        through_external_partitions: false,
        using: None,
    };
    let transitions = change.column_transitions();
    assert_eq!(transitions.len(), 1);
    assert_eq!(
        transitions[0].property,
        ColumnProperty::Column(Attribute::Collation)
    );
    if let Change::AlterColumnType { to_collation, .. } = &mut change {
        *to_collation = None;
    }
    assert!(change.column_transitions().is_empty());
}

#[test]
fn partition_local_changes_do_not_acquire_parent_writes() {
    let change = Change::SetPartitionDefault {
        uid: "t_bbbbbb".parse().unwrap(),
        table: "app.a".parse().unwrap(),
        parent: "app.p".parse().unwrap(),
        column: "d".into(),
        from: None,
        to: None,
        fallback: None,
    };
    assert!(change.parent_column_uid().is_none());
    assert!(change.column_transitions().is_empty());
    let change = Change::AddColumn {
        uid: "c_aaaaaa".parse().unwrap(),
        table: "app.p".parse().unwrap(),
        name: "d".into(),
        column: Box::new(Column::new("integer".parse().unwrap())),
    };
    assert_eq!(change.column_transitions().len(), 6);
}
