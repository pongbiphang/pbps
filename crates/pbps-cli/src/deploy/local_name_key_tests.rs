use super::*;
use pbps_model::{Change, ChangeSet, Index, IndexColumn, PlannedChange};

#[test]
fn qualified_index_claims_and_constraint_reads_preserve_raw_local_names() {
    const RAW: &str = "part\nline\\slash\u{200b}";
    const SHOWN: &str = r"part\nline\\slash\u{200b}";
    let table = TableName::new("app", "t");
    let index = Index {
        columns: vec![IndexColumn::column("id")],
        include: vec![],
        unique: false,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
    };
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::SetPrimaryKey {
                table: table.clone(),
                from: None,
                to: Some(pbps_model::PrimaryKey {
                    name: Some(format!("pk{RAW}").into()),
                    columns: vec!["id".into()],
                    storage_parameters: Default::default(),
                }),
                nonclustered: false,
            }),
            PlannedChange::new(Change::AddUnique {
                table: table.clone(),
                name: format!("uq{RAW}").into(),
                constraint: pbps_model::UniqueConstraint {
                    columns: vec!["id".into()],
                    storage_parameters: Default::default(),
                },
                clustered: false,
            }),
            PlannedChange::new(Change::AddForeignKey {
                table: table.clone(),
                name: format!("fk{RAW}").into(),
                constraint: Box::new(pbps_model::ForeignKey {
                    columns: vec!["id".into()],
                    references_table: TableName::new("app", "parent"),
                    references_columns: vec!["id".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                }),
            }),
            PlannedChange::new(Change::AddCheck {
                table: table.clone(),
                name: format!("ck{RAW}").into(),
                constraint: pbps_model::CheckConstraint {
                    expression: "id > 0".into(),
                },
            }),
            PlannedChange::new(Change::AddIndex {
                table: table.clone(),
                name: format!("ix{RAW}").into(),
                index: Box::new(index),
                clustered: false,
            }),
        ],
    };
    let ids = IdsFile {
        tables: [("t_aaaaaa".parse().unwrap(), table.clone())].into(),
        ..Default::default()
    };
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, &ids).unwrap();
    let claims: BTreeSet<_> = created_index_names(&changes).into_iter().collect();
    assert_eq!(
        claims,
        ["pk", "uq", "ix"]
            .into_iter()
            .map(|prefix| TableName::new("app", format!("{prefix}{RAW}")))
            .collect()
    );
    assert!(!claims.contains(&TableName::new("app", format!("ix{SHOWN}"))));
    let (reads, parents) = object_reads(&changes, &names);
    assert_eq!(
        reads.into_iter().collect::<BTreeSet<_>>(),
        ["pk", "uq", "fk", "ck"]
            .into_iter()
            .map(|prefix| TableName::new("app", format!("{prefix}{RAW}")))
            .collect()
    );
    assert_eq!(parents, vec![table]);
}

#[test]
fn catalog_prose_carried_report_shows_the_description_without_changing_it() {
    let raw = "view a\n\u{1b}\u{202e}\\n\\u{1b}é*/";
    let report = pbps_db::impact::ImpactReport {
        carried: vec![pbps_db::impact::Referrer {
            kind: "view".into(),
            name: raw.into(),
            detail: None,
            removed_with: None,
        }],
        ..Default::default()
    };
    assert_eq!(
        impact_lines(&report, &[]),
        [r"  view a\n\u{1b}\u{202e}\\n\\u{1b}é*/ — carried into the new name, keeps working"]
    );
    assert_eq!(report.carried[0].name, raw);
    assert!(impact_lines(&Default::default(), &[]).is_empty());
    assert_eq!(
        impact_lines(
            &pbps_db::impact::ImpactReport {
                carried: vec![pbps_db::impact::Referrer {
                    kind: "view".into(),
                    name: "view app.plain".into(),
                    detail: None,
                    removed_with: None,
                }],
                ..Default::default()
            },
            &[]
        ),
        ["  view app.plain — carried into the new name, keeps working"]
    );
}
