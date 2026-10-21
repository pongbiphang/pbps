use pbps_dialect::Dialect;
use pbps_model::{Column, PolicyCommand, PolicyRoles, RowPolicy, Table, TableName};

fn table() -> Table {
    Table {
        columns: [("id".into(), Column::new("integer".parse().unwrap()))].into(),
        ..Default::default()
    }
}

#[test]
fn row_security_validation_checks_programmatic_policies_and_names() {
    let pg = pbps_pg::Postgres::new();
    let name = TableName::new("app", "t");
    let mut t = table();
    t.checks.insert(
        "p".into(),
        pbps_model::CheckConstraint {
            expression: "id > 0".into(),
        },
    );
    t.row_policies.insert("p".into(), RowPolicy::default());
    assert!(pg.validate_table(&name, &t).is_empty());
    for policy in [
        RowPolicy {
            command: PolicyCommand::Select,
            with_check: Some("true".into()),
            ..Default::default()
        },
        RowPolicy {
            command: PolicyCommand::Insert,
            using: Some("true".into()),
            ..Default::default()
        },
        RowPolicy {
            to: PolicyRoles::Roles(Default::default()),
            ..Default::default()
        },
        RowPolicy {
            to: PolicyRoles::Roles(["public".into()].into()),
            ..Default::default()
        },
        RowPolicy {
            to: PolicyRoles::Roles(["x".repeat(64).into()].into()),
            ..Default::default()
        },
    ] {
        t.row_policies.insert("p".into(), policy);
        assert!(!pg.validate_table(&name, &t).is_empty());
    }
    t.row_policies.clear();
    t.row_policies.insert("p".repeat(64), RowPolicy::default());
    assert!(!pg.validate_table(&name, &t).is_empty());
}

#[test]
fn row_security_validation_refuses_data_and_partition_trees() {
    let pg = pbps_pg::Postgres::new();
    let name = TableName::new("app", "t");
    for shape in 0..3 {
        let mut t = table();
        match shape {
            0 => t.row_security.enabled = true,
            1 => t.row_security.forced = true,
            _ => {
                t.row_policies.insert("p".into(), RowPolicy::default());
            }
        }
        assert!(pg.validate_table(&name, &t).is_empty());
        t.data = Some(pbps_model::TableData {
            mode: pbps_model::DataMode::Ensure,
            rows: Default::default(),
        });
        assert!(
            pg.validate_table(&name, &t)
                .iter()
                .any(|e| e.to_string().contains("`data:` cannot"))
        );
        t.data = None;
        t.partition_by = Some(pbps_model::PartitionBy {
            strategy: Default::default(),
            columns: vec!["id".into()],
            external: false,
        });
        assert!(
            pg.validate_table(&name, &t)
                .iter()
                .any(|e| e.to_string().contains("partition tree"))
        );
        t.partition_by = None;
        t.partition_of = Some(pbps_model::PartitionOf {
            parent: name.clone(),
            bound: pbps_model::PartitionBound::Default,
            columns: Default::default(),
        });
        assert!(
            pg.validate_table(&name, &t)
                .iter()
                .any(|e| e.to_string().contains("partition tree"))
        );
        assert!(
            pg.validate_partition(&name, &t, &table())
                .iter()
                .any(|e| e.to_string().contains("partition tree"))
        );
        t.row_security = Default::default();
        t.row_policies.clear();
        assert!(pg.validate_partition(&name, &t, &table()).is_empty());
    }
}
