use super::*;
use pbps_model::{BoundDatum, Column, PartitionBound, PartitionBy, PartitionOf};

fn record(
    base: Side<'_>,
    desired: &Schema,
    ids: &IdsFile,
    table: &TableName,
    max_rows: u64,
    reason: &Reason,
    ctx: &Context,
) -> Result<IdsFile, RebuildError> {
    super::record(
        base,
        desired,
        ids,
        table,
        max_rows,
        reason,
        ctx,
        &pbps_dialect::MinimalDialect,
    )
}

fn qualify(
    base: Side<'_>,
    desired: Side<'_>,
    table: &TableName,
) -> Result<RebuildPartitionedTable, RebuildError> {
    super::qualify(base, desired, table, &pbps_dialect::MinimalDialect)
}

fn side<'a>(schema: &'a Schema, ids: &'a IdsFile) -> Side<'a> {
    Side { schema, ids }
}

fn table() -> TableName {
    TableName::new("app", "p")
}
fn ctx() -> Context {
    Context {
        operator: "operator".into(),
        today: "2026-10-23".into(),
    }
}
fn fixture() -> (Schema, IdsFile, Schema) {
    let mut base = Schema::default();
    let parent = Table {
        columns: [
            ("k".into(), Column::new("integer".parse().unwrap())),
            ("bucket".into(), Column::new("integer".parse().unwrap())),
        ]
        .into_iter()
        .collect(),
        partition_by: Some(PartitionBy {
            columns: vec!["k".into()],
            strategy: PartitionStrategy::Range,
            external: false,
        }),
        ..Table::default()
    };
    base.tables.insert(table(), parent);
    base.tables.insert(
        TableName::new("app", "p1"),
        Table {
            partition_of: Some(PartitionOf {
                parent: table(),
                bound: PartitionBound::Range {
                    from: vec![BoundDatum::Value("0".into())],
                    to: vec![BoundDatum::Value("10".into())],
                },
                columns: Default::default(),
            }),
            ..Table::default()
        },
    );
    let ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let mut desired = base.clone();
    desired
        .tables
        .get_mut(&table())
        .unwrap()
        .partition_by
        .as_mut()
        .unwrap()
        .columns = vec!["bucket".into()];
    (base, ids, desired)
}
fn recorded(base: &Schema, ids: &IdsFile, desired: &Schema) -> IdsFile {
    record(
        Side { schema: base, ids },
        desired,
        ids,
        &table(),
        20,
        &"maintenance".parse().unwrap(),
        &ctx(),
    )
    .unwrap()
}

#[test]
fn pending_rebuilds_refuse_covered_intents_and_implicit_uid_retirement() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let child = TableName::new("app", "p1");
    let reason: Reason = "retired".parse().unwrap();
    for intent in [
        pbps_model::Intent::RenameTable {
            from: table(),
            to: TableName::new("app", "q"),
        },
        pbps_model::Intent::DropTable {
            table: child.clone(),
            reason: reason.clone(),
        },
        pbps_model::Intent::RenameColumn {
            table: table(),
            from: "k".into(),
            to: "old_k".into(),
        },
        pbps_model::Intent::DropColumn {
            column: table().column("k"),
            reason,
        },
        pbps_model::Intent::MoveRows {
            table: child.clone(),
            max_rows: 10,
            reason: "move".into(),
        },
    ] {
        for annotations in [0, 1] {
            let errors = crate::resolve_with_annotations(
                &desired,
                &approval,
                std::slice::from_ref(&intent),
                annotations,
                &ctx(),
            )
            .unwrap_err();
            assert!(
                matches!(&errors[..], [crate::Blocker::PendingRebuild { table: root, .. }] if root == &table())
            );
        }
    }
    // Surrendering own column identities on attach needs no explicit intent.
    let mut attached = desired.clone();
    let outer = TableName::new("app", "outer");
    attached
        .tables
        .insert(outer.clone(), desired.tables[&table()].clone());
    let mut of = base.tables[&child].partition_of.clone().unwrap();
    of.parent = outer;
    attached.tables.get_mut(&table()).unwrap().partition_of = Some(of);
    let errors = crate::resolve(&attached, &approval, &[], &ctx()).unwrap_err();
    assert!(errors.iter().any(|b| matches!(b, crate::Blocker::PendingRebuild { object, .. } if object == "app.p.k" || object == "app.p.bucket")));
    // Unrelated identities remain resolvable.
    let mut unrelated = desired.clone();
    unrelated
        .tables
        .insert(TableName::new("app", "other"), Table::default());
    crate::resolve(&unrelated, &approval, &[], &ctx())
        .unwrap()
        .ids
        .validate()
        .unwrap();
}

#[test]
fn empty_range_parents_qualify_but_unchanged_keys_do_not() {
    let (mut base, _, mut desired) = fixture();
    let child = TableName::new("app", "p1");
    base.tables.remove(&child);
    desired.tables.remove(&child);
    let ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let approval = recorded(&base, &ids, &desired);
    let payload = qualify(side(&base, &ids), side(&desired, &approval), &table()).unwrap();
    assert_eq!(payload.before.len(), 1);
    assert_eq!(payload.after.len(), 1);
    assert!(
        record(
            side(&base, &ids),
            &base,
            &ids,
            &table(),
            20,
            &"maintenance".parse().unwrap(),
            &ctx()
        )
        .is_err()
    );
}

#[test]
fn snapshot_omissions_discard_the_whole_pending_rebuild() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    assert_eq!(approval.without(&[]), approval);
    assert_eq!(
        approval.without(&[TableName::new("app", "unrelated")]),
        approval
    );
    for missing in [table(), TableName::new("app", "p1")] {
        let projected = approval.without(&[missing]);
        assert!(projected.rebuilds.is_empty());
        projected.validate().unwrap();
    }
    let mut missing_column = approval.clone();
    missing_column
        .columns
        .remove(ids.column_uid(&table().column("k")).unwrap());
    let projected = missing_column.without(&[]);
    assert!(projected.rebuilds.is_empty());
    projected.validate().unwrap();
}

#[test]
fn rebuild_intent_reserves_lineage_and_converges_without_replacing_live_ids() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    assert_eq!(approval.tables, ids.tables);
    assert_eq!(approval.columns, ids.columns);
    let intent = &approval.rebuilds[ids.table_uid(&table()).unwrap()];
    assert_eq!(intent.source.as_ids().tables, ids.tables);
    assert_eq!(intent.replacement.tables.len(), 2);
    assert_eq!(
        intent.replacement.columns.len(),
        2,
        "inherited columns have no independent identity"
    );
    assert!(
        intent
            .replacement
            .tables
            .keys()
            .all(|u| !ids.contains_uid(u) && approval.contains_uid(u))
    );
    let later = Context {
        operator: "later".into(),
        today: "2026-10-24".into(),
    };
    let again = record(
        side(&base, &ids),
        &desired,
        &approval,
        &table(),
        20,
        &intent.reason,
        &later,
    )
    .unwrap();
    assert_eq!(again, approval);
    let changed = record(
        side(&base, &ids),
        &desired,
        &approval,
        &table(),
        21,
        &intent.reason,
        &later,
    )
    .unwrap();
    let updated = &changed.rebuilds[ids.table_uid(&table()).unwrap()];
    assert_eq!(updated.replacement, intent.replacement);
    assert_eq!(updated.operator, "later");
    let payload = qualify(side(&base, &ids), side(&desired, &approval), &table()).unwrap();
    assert_eq!(payload.before[&table()], base.tables[&table()]);
    assert_eq!(payload.after[&table()], desired.tables[&table()]);
}

#[test]
fn qualification_refuses_missing_stale_and_foreign_lineage() {
    let (base, ids, mut desired) = fixture();
    assert!(
        qualify(side(&base, &ids), side(&desired, &ids), &table())
            .unwrap_err()
            .why
            .contains("no explicit")
    );
    let approval = recorded(&base, &ids, &desired);
    desired.tables.get_mut(&table()).unwrap().description = Some("changed".into());
    assert!(qualify(side(&base, &ids), side(&desired, &approval), &table()).is_err());
    desired.tables.get_mut(&table()).unwrap().description = None;
    let mut foreign = ids.clone();
    let old = foreign.table_uid(&table()).unwrap().clone();
    foreign.tables.remove(&old);
    foreign
        .tables
        .insert(Uid::generate(UidKind::Table), table());
    assert!(qualify(side(&base, &foreign), side(&desired, &approval), &table()).is_err());
    let mut incomplete = approval.clone();
    let column = incomplete.column_uid(&table().column("k")).unwrap().clone();
    incomplete.columns.remove(&column);
    incomplete
        .rebuilds
        .values_mut()
        .next()
        .unwrap()
        .source
        .columns
        .remove(&column);
    let mut missing = ids.clone();
    missing.columns.remove(&column);
    assert!(qualify(side(&base, &missing), side(&desired, &incomplete), &table()).is_err());
}

#[test]
fn qualification_accepts_key_type_changes_and_refuses_unsupported_shapes() {
    let (base, ids, desired) = fixture();
    let mut retyped = base.clone();
    retyped
        .tables
        .get_mut(&table())
        .unwrap()
        .columns
        .get_mut("k")
        .unwrap()
        .ty = "bigint".parse().unwrap();
    recorded(&base, &ids, &retyped);
    for kind in [
        "unchanged",
        "bare_drop",
        "external",
        "subtree",
        "rls",
        "unrelated",
        "ordinary",
    ] {
        let mut bad = desired.clone();
        match kind {
            "unchanged" => bad = base.clone(),
            "bare_drop" => {
                bad = base.clone();
                bad.tables
                    .get_mut(&table())
                    .unwrap()
                    .columns
                    .shift_remove("k");
            }
            "external" => {
                bad.tables
                    .get_mut(&table())
                    .unwrap()
                    .partition_by
                    .as_mut()
                    .unwrap()
                    .external = true
            }
            "subtree" => {
                bad.tables
                    .get_mut(&TableName::new("app", "p1"))
                    .unwrap()
                    .partition_by = base.tables[&table()].partition_by.clone();
            }
            "rls" => bad.tables.get_mut(&table()).unwrap().row_security.enabled = true,
            "unrelated" => {
                bad.tables
                    .insert(TableName::new("app", "other"), Table::default());
            }
            "ordinary" => bad.tables.get_mut(&table()).unwrap().partition_by = None,
            _ => unreachable!(),
        }
        assert!(
            record(
                side(&base, &ids),
                &bad,
                &ids,
                &table(),
                20,
                &"x".parse().unwrap(),
                &ctx()
            )
            .is_err(),
            "{kind}"
        );
    }
    assert!(
        record(
            side(&base, &ids),
            &desired,
            &ids,
            &table(),
            0,
            &"x".parse().unwrap(),
            &ctx()
        )
        .is_err()
    );
}

#[test]
fn pending_rebuilds_never_produce_partial_changes_or_saved_plan_admission() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let diff = crate::diff_partial(
        side(&base, &ids),
        side(&desired, &approval),
        &pbps_dialect::MinimalDialect,
        &Default::default(),
    );
    assert!(diff.changes.changes.is_empty());
    assert!(matches!(
        &diff.errors[..],
        [crate::DiffError::RebuildTable { .. }]
    ));
    let wire = pbps_model::SavedPlan::new(
        pbps_model::PlanOrigin::Database,
        "minimal",
        "now",
        pbps_model::PlanBaseline {
            description: "source".into(),
            checksum: "checksum".into(),
            database_collation: None,
            time_zone: None,
        },
        Default::default(),
        approval,
    );
    assert!(
        crate::order::validate_saved_plan(&wire, &pbps_dialect::MinimalDialect)
            .unwrap_err()
            .to_string()
            .contains("cannot be executed yet")
    );
}

#[test]
fn recorded_format_is_strict_and_old_identity_bytes_stay_unchanged() {
    let old = r#"{"version":1,"tables":{},"columns":{}}"#;
    let ids: IdsFile = serde_json::from_str(old).unwrap();
    assert_eq!(serde_json::to_string(&ids).unwrap(), old);
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    assert_eq!(
        serde_json::from_str::<IdsFile>(&serde_json::to_string(&approval).unwrap()).unwrap(),
        approval
    );
    let key = ids.table_uid(&table()).unwrap();
    let mut value = serde_json::to_value(&approval).unwrap();
    value["rebuilds"][key.to_string()]["extra"] = true.into();
    assert!(serde_json::from_value::<IdsFile>(value).is_err());
    for defect in ["zero", "orphan", "reused", "wrong_kind", "bad_digest"] {
        let mut bad = approval.clone();
        let intent = bad.rebuilds.get_mut(key).unwrap();
        match defect {
            "zero" => intent.max_rows = 0,
            "orphan" => intent.source.tables.clear(),
            "reused" => {
                intent.replacement.tables.insert(key.clone(), table());
            }
            "wrong_kind" => {
                intent
                    .replacement
                    .tables
                    .insert(Uid::generate(UidKind::Column), TableName::new("app", "bad"));
            }
            "bad_digest" => intent.target_checksum = "unreadable".into(),
            _ => unreachable!(),
        }
        assert!(bad.validate().is_err(), "{defect}");
    }
}
