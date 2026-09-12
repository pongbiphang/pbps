use pbps_dialect::Dialect;
use pbps_model::{Change, ChangeSet, Index, IndexColumn, PlannedChange, Row, RowKey, TableName};

fn index(unique: bool, filtered: bool) -> Change {
    Change::AddIndex {
        table: TableName::new("app", "items"),
        name: "ix_items".into(),
        index: Box::new(Index {
            columns: vec![IndexColumn {
                name: "id".into(),
                descending: false,
            }],
            include: vec![],
            unique,
            filter: filtered.then(|| "id IS NOT NULL".into()),
        }),
    }
}

fn inserted() -> Change {
    Change::InsertRow {
        table: TableName::new("app", "items"),
        key_column: "id".into(),
        identity_key: false,
        key: RowKey::from("1"),
        row: Row(Default::default()),
        defaults: Default::default(),
        types: Default::default(),
    }
}

fn plan(changes: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    }
}

#[test]
fn deliberately_unbuilt_checks_keep_their_name_and_reason_on_both_engines() {
    for dialect in [
        &pbps_mssql::Mssql as &dyn Dialect,
        &pbps_pg::Postgres::new(),
    ] {
        let report = dialect.preflight(&plan(vec![inserted(), index(true, true)]));
        assert!(report.probes.is_empty(), "{report:?}");
        assert_eq!(report.unchecked.len(), 1, "{report:?}");
        assert!(
            report.unchecked[0].description.contains("ix_items"),
            "{report:?}"
        );
        assert!(
            report.unchecked[0].description.contains("app.items"),
            "{report:?}"
        );
        assert!(
            report.unchecked[0].reason.contains("cannot be evaluated"),
            "{report:?}"
        );
    }
}

#[test]
fn ordinary_checks_and_changes_needing_no_check_are_not_unchecked() {
    for dialect in [
        &pbps_mssql::Mssql as &dyn Dialect,
        &pbps_pg::Postgres::new(),
    ] {
        let ordinary = dialect.preflight(&plan(vec![index(true, false)]));
        assert_eq!(ordinary.probes.len(), 1, "{ordinary:?}");
        assert!(ordinary.unchecked.is_empty(), "{ordinary:?}");
        let no_check = dialect.preflight(&plan(vec![inserted(), index(false, true)]));
        assert!(no_check.probes.is_empty(), "{no_check:?}");
        assert!(no_check.unchecked.is_empty(), "{no_check:?}");
    }
}

#[test]
fn unspellable_constraint_values_and_check_predicates_are_reported() {
    use pbps_model::{CheckConstraint, ForeignKey, PrimaryKey, UniqueConstraint};
    let table = TableName::new("app", "items");
    let constraints = vec![
        Change::AddUnique {
            table: table.clone(),
            name: "uq".into(),
            constraint: UniqueConstraint {
                columns: vec!["value".into()],
            },
        },
        Change::SetPrimaryKey {
            table: table.clone(),
            from: None,
            to: Some(PrimaryKey {
                name: Some("pk".into()),
                columns: vec!["value".into()],
            }),
        },
        Change::AddForeignKey {
            table: table.clone(),
            name: "fk".into(),
            constraint: Box::new(ForeignKey {
                columns: vec!["value".into()],
                references_table: TableName::new("app", "parents"),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            }),
        },
        Change::AddCheck {
            table,
            name: "ck".into(),
            constraint: CheckConstraint {
                expression: "value > 0".into(),
            },
        },
    ];
    let mut row = inserted();
    if let Change::InsertRow { defaults, .. } = &mut row {
        defaults.insert("value".into(), "unpredictable()".into());
    }
    for dialect in [
        &pbps_mssql::Mssql as &dyn Dialect,
        &pbps_pg::Postgres::new(),
    ] {
        for constraint in &constraints {
            let report = dialect.preflight(&plan(vec![row.clone(), constraint.clone()]));
            assert!(report.probes.is_empty(), "{constraint:?}: {report:?}");
            assert_eq!(report.unchecked.len(), 1, "{constraint:?}: {report:?}");
            assert!(!report.unchecked[0].reason.is_empty());
        }
    }
}

#[test]
fn an_empty_new_table_has_an_answer_and_is_not_an_unchecked_key() {
    let mut table = pbps_model::Table::default();
    table
        .columns
        .insert("id".into(), pbps_model::Column::new("int".parse().unwrap()));
    let create = Change::CreateTable {
        uid: "t_a1b2c3".parse().unwrap(),
        name: TableName::new("app", "items"),
        table: Box::new(table),
    };
    for dialect in [
        &pbps_mssql::Mssql as &dyn Dialect,
        &pbps_pg::Postgres::new(),
    ] {
        let report = dialect.preflight(&plan(vec![create.clone(), index(true, false)]));
        assert_eq!(report.probes.len(), 1, "{report:?}");
        assert!(report.unchecked.is_empty(), "{report:?}");
    }
}
