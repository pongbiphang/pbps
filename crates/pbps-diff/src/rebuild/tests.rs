use super::*;
use pbps_model::{BoundDatum, Column, PartitionBound, PartitionBy, PartitionColumn, PartitionOf};

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

fn with_additions(mut desired: Schema, column: bool, partition: bool) -> Schema {
    if column {
        desired
            .tables
            .get_mut(&table())
            .unwrap()
            .columns
            .insert("extra".into(), Column::new("integer".parse().unwrap()));
    }
    if partition {
        let mut leaf = desired.tables[&TableName::new("app", "p1")].clone();
        leaf.partition_of.as_mut().unwrap().bound = PartitionBound::Range {
            from: vec![BoundDatum::Value("10".into())],
            to: vec![BoundDatum::Value("20".into())],
        };
        desired.tables.insert(TableName::new("app", "p2"), leaf);
    }
    desired
}

fn check_replacement_reservations(column: bool, partition: bool) {
    let (base, ids, desired) = fixture();
    let desired = with_additions(desired, column, partition);
    let approval = recorded(&base, &ids, &desired);
    for resolution in [
        crate::resolve(&desired, &approval, &[], &ctx()),
        crate::resolve_with_annotations(&desired, &approval, &[], 0, &ctx()),
        crate::resolve_pulled(&desired, &approval, &BTreeMap::new(), &ctx()),
    ] {
        let resolved = resolution.unwrap();
        assert_eq!(resolved.ids, approval);
        assert!(resolved.created_tables.is_empty());
        assert!(resolved.added_columns.is_empty());
        resolved.ids.validate().unwrap();
        let payload = qualify(side(&base, &ids), side(&desired, &resolved.ids), &table()).unwrap();
        assert_eq!(
            payload.intent.replacement,
            approval.rebuilds.values().next().unwrap().replacement
        );
        assert_eq!(
            crate::resolve(&desired, &resolved.ids, &[], &ctx())
                .unwrap()
                .ids,
            approval
        );
    }
}

#[test]
fn added_parent_columns_keep_their_rebuild_reservations() {
    check_replacement_reservations(true, false);
}

#[test]
fn added_partitions_keep_their_rebuild_reservations() {
    check_replacement_reservations(false, true);
}

#[test]
fn added_partitions_and_columns_keep_their_rebuild_reservations() {
    check_replacement_reservations(true, true);
}

#[test]
fn unreserved_names_still_receive_live_identities() {
    let (base, ids, desired) = fixture();
    let mut desired = with_additions(desired, true, true);
    let approval = recorded(&base, &ids, &desired);
    let other = TableName::new("app", "other");
    desired
        .tables
        .insert(other.clone(), base.tables[&table()].clone());
    desired
        .tables
        .get_mut(&table())
        .unwrap()
        .columns
        .insert("unreserved".into(), Column::new("integer".parse().unwrap()));
    // The rebuild never reserves type identities; new managed types must
    // remain resolvable while a table replacement is pending as well.
    let ty: pbps_model::TypeRef = "app.count".parse().unwrap();
    desired.types.insert(
        ty.clone(),
        pbps_model::ManagedType::Enum {
            values: vec!["one".into()],
        },
    );
    for resolution in [
        crate::resolve(&desired, &approval, &[], &ctx()),
        crate::resolve_with_annotations(&desired, &approval, &[], 0, &ctx()),
        crate::resolve_pulled(&desired, &approval, &BTreeMap::new(), &ctx()),
    ] {
        let resolved = resolution.unwrap().ids;
        assert!(resolved.table_uid(&other).is_some());
        assert!(resolved.column_uid(&other.column("k")).is_some());
        assert!(resolved.column_uid(&table().column("unreserved")).is_some());
        assert!(resolved.type_uid(&ty).is_some());
        assert!(resolved.table_uid(&TableName::new("app", "p2")).is_none());
        assert!(resolved.column_uid(&table().column("extra")).is_none());
        assert_eq!(resolved.rebuilds, approval.rebuilds);
        resolved.validate().unwrap();
    }
}

#[test]
fn every_approved_member_change_resolves_without_other_identity_intent() {
    let base = member_cases::source();
    let ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let mut unresolved = Vec::new();
    for case in member_cases::cases() {
        let result = record(
            side(&base, &ids),
            &case.desired,
            &ids,
            &member_cases::root(),
            20,
            &"maintenance".parse().unwrap(),
            &ctx(),
        );
        if let Some(why) = case.refusal {
            let error = result.expect_err(&case.name);
            assert!(error.why.contains(why), "{}: {error}", case.name);
            continue;
        }
        let approval = result.expect(&case.name);
        for resolution in [
            crate::resolve(&case.desired, &approval, &[], &ctx()),
            crate::resolve_with_annotations(&case.desired, &approval, &[], 0, &ctx()),
            crate::resolve_pulled(&case.desired, &approval, &BTreeMap::new(), &ctx()),
        ] {
            let resolution = match resolution {
                Ok(resolution) => resolution,
                Err(blockers) => {
                    unresolved.push(format!("{}: {blockers:?}", case.name));
                    continue;
                }
            };
            assert_eq!(resolution.ids, approval, "{}", case.name);
            assert_eq!(
                resolution,
                crate::Resolution {
                    ids: approval.clone(),
                    ..Default::default()
                },
                "{}",
                case.name
            );
            resolution.ids.validate().unwrap();
            let payload = qualify(
                side(&base, &ids),
                side(&case.desired, &resolution.ids),
                &member_cases::root(),
            )
            .expect(&case.name);
            assert_eq!(payload.before, base.tables, "{}", case.name);
            assert_eq!(payload.after, case.desired.tables, "{}", case.name);
            assert_eq!(
                crate::resolve(&case.desired, &resolution.ids, &[], &ctx()).unwrap(),
                resolution,
                "{}",
                case.name
            );
        }
    }
    assert!(unresolved.is_empty(), "{}", unresolved.join("\n"));
}

#[test]
fn only_recorded_members_are_exempt_from_ordinary_drop_intent() {
    let (mut base, _, mut desired) = fixture();
    let other = TableName::new("app", "other");
    base.tables
        .insert(other.clone(), base.tables[&table()].clone());
    desired
        .tables
        .insert(other.clone(), base.tables[&other].clone());
    let ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let approval = recorded(&base, &ids, &desired);
    for drop_table in [false, true] {
        let mut omitted = desired.clone();
        if drop_table {
            omitted.tables.remove(&other);
        } else {
            omitted
                .tables
                .get_mut(&other)
                .unwrap()
                .columns
                .shift_remove("bucket");
        }
        let blockers = crate::resolve(&omitted, &approval, &[], &ctx()).unwrap_err();
        assert!(
            matches!(&blockers[..], [crate::Blocker::DropTableNeedsReason { table: name }] if drop_table && name == &other)
                || matches!(&blockers[..], [crate::Blocker::DropColumnNeedsReason { column }] if !drop_table && column == &other.column("bucket")),
            "{blockers:?}"
        );
    }
    // Removing the recorded permission restores the ordinary drop requirements.
    let mut withdrawn = approval;
    withdrawn.rebuilds.clear();
    desired.tables.remove(&TableName::new("app", "p1"));
    desired
        .tables
        .get_mut(&table())
        .unwrap()
        .columns
        .shift_remove("k");
    let blockers = crate::resolve(&desired, &withdrawn, &[], &ctx()).unwrap_err();
    assert!(blockers.iter().any(|b| matches!(b, crate::Blocker::DropTableNeedsReason { table: name } if name == &TableName::new("app", "p1"))));
    assert!(blockers.iter().any(|b| matches!(b, crate::Blocker::DropColumnNeedsReason { column } if column == &table().column("k"))));
}

fn check_partition_override_is_refused(own: PartitionColumn) {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let child = TableName::new("app", "p1");
    for location in ["source", "desired", "both"] {
        let mut before = base.clone();
        let mut after = desired.clone();
        for (schema, side) in [(&mut before, "source"), (&mut after, "desired")] {
            if location == side || location == "both" {
                let member = schema.tables.get_mut(&child).unwrap();
                assert!(member.columns.is_empty());
                member
                    .partition_of
                    .as_mut()
                    .unwrap()
                    .columns
                    .insert("k".into(), own.clone());
            }
        }
        // An approval written by the old predicate must not pass just because
        // its desired fingerprint still matches, or take the re-record shortcut.
        let mut legacy = approval.clone();
        legacy.rebuilds.values_mut().next().unwrap().target_checksum =
            canonical_schema(&after, &pbps_dialect::MinimalDialect).fingerprint();
        legacy.validate().unwrap();
        for current in [&ids, &legacy] {
            let error = record(
                side(&before, &ids),
                &after,
                current,
                &table(),
                20,
                &"maintenance".parse().unwrap(),
                &ctx(),
            )
            .unwrap_err();
            assert_eq!(error.table, table());
            assert!(error.why.contains("app.p1.k"), "{location}: {error}");
            assert!(error.why.contains("partition column override"), "{error}");
        }
        let error = qualify(side(&before, &ids), side(&after, &legacy), &table()).unwrap_err();
        assert!(error.why.contains("app.p1.k"), "{location}: {error}");
        assert!(error.why.contains("partition column override"), "{error}");
    }
}

#[test]
fn partition_default_overrides_refuse_recording_and_qualification() {
    check_partition_override_is_refused(PartitionColumn {
        default: Some("7".into()),
        ..PartitionColumn::default()
    });
}

#[test]
fn partition_not_null_overrides_refuse_recording_and_qualification() {
    check_partition_override_is_refused(PartitionColumn {
        not_null: true,
        ..PartitionColumn::default()
    });
}

#[test]
fn rebuild_approval_requires_committed_and_unchanged_managed_type_identities() {
    let (mut base, ids, mut desired) = fixture();
    let name: pbps_model::TypeRef = "app.count".parse().unwrap();
    let definition = pbps_model::ManagedType::Enum {
        values: vec!["one".into()],
    };
    base.types.insert(name.clone(), definition.clone());
    desired.types.insert(name.clone(), definition);
    let committed = crate::resolve(&base, &ids, &[], &ctx()).unwrap().ids;
    let error = record(
        side(&base, &ids),
        &desired,
        &committed,
        &table(),
        20,
        &"maintenance".parse().unwrap(),
        &ctx(),
    )
    .unwrap_err();
    assert!(
        error.why.contains("commit the source identities"),
        "{error}"
    );

    let approval = recorded(&base, &committed, &desired);
    qualify(side(&base, &committed), side(&desired, &approval), &table()).unwrap();
    let mut changed = approval.clone();
    changed.types.clear();
    changed.types.insert(Uid::generate(UidKind::Type), name);
    changed.validate().unwrap();
    let error = qualify(side(&base, &committed), side(&desired, &changed), &table()).unwrap_err();
    assert!(error.why.contains("lineage"), "{error}");
}

#[test]
fn pulled_type_resolution_preserves_pending_rebuild_source_guards() {
    let (base, ids, mut desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let name: pbps_model::TypeRef = "app.count".parse().unwrap();
    desired.types.insert(
        name.clone(),
        pbps_model::ManagedType::Enum {
            values: vec!["one".into()],
        },
    );
    let added = crate::resolve_pulled(&desired, &approval, &BTreeMap::new(), &ctx())
        .unwrap()
        .ids;
    assert!(added.type_uid(&name).is_some());
    assert_eq!(added.tables, approval.tables);
    assert_eq!(added.columns, approval.columns);
    assert_eq!(added.rebuilds, approval.rebuilds);
    added.validate().unwrap();
    // Pull may mint a type UID, but cannot surrender a covered table's own
    // columns as part of adopting its new partition ownership.
    let child = TableName::new("app", "p1");
    desired.tables.get_mut(&table()).unwrap().partition_of =
        desired.tables[&child].partition_of.clone();
    let blockers = crate::resolve_pulled(&desired, &added, &BTreeMap::new(), &ctx()).unwrap_err();
    assert!(
        blockers
            .iter()
            .any(|b| matches!(b, crate::Blocker::PendingRebuild { .. }))
    );
    desired.types.clear();
    assert!(matches!(
        &crate::resolve_pulled(&desired, &added, &BTreeMap::new(), &ctx()).unwrap_err()[..],
        [crate::Blocker::MissingType { .. }]
    ));
}

#[test]
fn trees_without_partition_overrides_still_record_and_qualify() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let payload = qualify(side(&base, &ids), side(&desired, &approval), &table()).unwrap();
    assert!(
        payload
            .after
            .values()
            .filter_map(|t| t.partition_of.as_ref())
            .all(|of| of.columns.is_empty())
    );
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
