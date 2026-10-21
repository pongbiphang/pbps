use pbps_dialect::Dialect;
use pbps_model::{Change, ChangeSet, Index, IndexColumn, PlannedChange, Row, RowKey, TableName};

fn index(unique: bool, filtered: bool) -> Change {
    Change::AddIndex {
        table: TableName::new("app", "items"),
        name: "ix_items".into(),
        index: Box::new(Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column("id".into()),
                descending: false,
                opclass: None,
            }],
            include: vec![],
            unique,
            filter: filtered.then(|| "id IS NOT NULL".into()),
            method: Default::default(),
            storage_parameters: Default::default(),
        }),
        clustered: false,
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
        &pbps_mssql::Mssql as &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
        &pbps_pg::Postgres::new(),
    ] {
        let report = dialect.fixture_preflight(&plan(vec![inserted(), index(true, true)]));
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
        &pbps_mssql::Mssql as &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
        &pbps_pg::Postgres::new(),
    ] {
        let ordinary = dialect.fixture_preflight(&plan(vec![index(true, false)]));
        assert_eq!(ordinary.probes.len(), 1, "{ordinary:?}");
        assert!(ordinary.unchecked.is_empty(), "{ordinary:?}");
        let no_check = dialect.fixture_preflight(&plan(vec![inserted(), index(false, true)]));
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
                storage_parameters: Default::default(),
            },
            clustered: false,
        },
        Change::SetPrimaryKey {
            table: table.clone(),
            from: None,
            to: Some(PrimaryKey {
                name: Some("pk".into()),
                columns: vec!["value".into()],
                storage_parameters: Default::default(),
            }),
            nonclustered: false,
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
    for (dialect, checks_collations) in [
        (
            &pbps_mssql::Mssql as &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
            false,
        ),
        (&pbps_pg::Postgres::new(), true),
    ] {
        for constraint in &constraints {
            let report = dialect.fixture_preflight(&plan(vec![row.clone(), constraint.clone()]));
            // Unknown row values do not hide PostgreSQL's independent
            // metadata check for a planned foreign key.
            let expected = usize::from(
                checks_collations && matches!(constraint, Change::AddForeignKey { .. }),
            );
            assert_eq!(report.probes.len(), expected, "{constraint:?}: {report:?}");
            assert!(
                report
                    .probes
                    .iter()
                    .all(|p| p.description.contains("incompatible collations")),
                "{report:?}"
            );
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
        beside_default: None,
        moves_at_most: None,
    };
    for dialect in [
        &pbps_mssql::Mssql as &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
        &pbps_pg::Postgres::new(),
    ] {
        let report = dialect.fixture_preflight(&plan(vec![create.clone(), index(true, false)]));
        assert_eq!(report.probes.len(), 1, "{report:?}");
        assert!(report.unchecked.is_empty(), "{report:?}");
    }
}

mod plan_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pbps-dialect/tests/support/preflight.rs"
    ));
}
use plan_fixture::FixturePreflight as _;

#[test]
fn probes_use_original_table_and_column_names_after_multiple_renames() {
    use pbps_model::{IdsFile, Uid, plan_names::PlanNames};
    let table_uid: Uid = "t_aaaaaa".parse().unwrap();
    let column_uid: Uid = "c_aaaaaa".parse().unwrap();
    let table = |name| TableName::new("app", name);
    let rename_table = |from, to| Change::RenameTable {
        uid: table_uid.clone(),
        from: table(from),
        to: table(to),
        defaults: vec![],
    };
    let rename_column = |from: &str, to: &str| Change::RenameColumn {
        uid: column_uid.clone(),
        table: table("final_table"),
        from: from.into(),
        to: to.into(),
        table_was: Some(table("original_table")),
    };
    let changes = plan(vec![
        rename_table("original_table", "temporary_table"),
        rename_table("temporary_table", "final_table"),
        rename_column("original_column", "temporary_column"),
        rename_column("temporary_column", "final_column"),
        Change::AlterColumnNullability {
            uid: column_uid.clone(),
            column: table("final_table").column("final_column"),
            ty: "int".parse().unwrap(),
            to_nullable: false,
            through_external_partitions: false,
            collation: None,
        },
    ]);
    let ids = IdsFile {
        tables: [(table_uid, table("final_table"))].into(),
        columns: [(column_uid, table("final_table").column("final_column"))].into(),
        ..IdsFile::default()
    };
    let names = PlanNames::of(&changes.changes, &ids).unwrap();
    for dialect in [
        &pbps_mssql::Mssql as &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
        &pbps_pg::Postgres::new(),
    ] {
        let report = dialect.preflight(&names);
        assert!(report.unchecked.is_empty(), "{report:?}");
        let probe = report
            .probes
            .iter()
            .find(|p| p.description.contains("NULL"))
            .expect("the tightening has a probe");
        assert!(probe.sql.contains("original_table"), "{probe:?}");
        assert!(probe.sql.contains("original_column"), "{probe:?}");
        assert!(!probe.sql.contains("temporary_"), "{probe:?}");
        assert!(!probe.sql.contains("final_"), "{probe:?}");
    }
}
