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

fn transition(
    base: Side<'_>,
    desired: Side<'_>,
    root: &RebuildPartitionedTable,
) -> Result<pbps_model::ValidIds, RebuildError> {
    super::target_identities(base, desired, root, &pbps_dialect::MinimalDialect)
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

fn reason() -> Reason {
    "maintenance".parse().unwrap()
}

fn diff(base: Side<'_>, desired: Side<'_>) -> crate::Diffed {
    crate::diff_partial(
        base,
        desired,
        &pbps_dialect::MinimalDialect,
        &Default::default(),
    )
}

fn first_rebuild_error(diffed: &crate::Diffed) -> String {
    match &diffed.errors[..] {
        [crate::DiffError::RebuildTable { why, .. }] => why.clone(),
        other => panic!("expected one rebuild refusal, got {other:?}"),
    }
}

#[test]
fn approval_installs_the_replacements_and_retires_the_sources_in_one_write() {
    let (mut base, _, mut desired) = fixture();
    let other = TableName::new("app", "other");
    base.tables.insert(other.clone(), Table::default());
    desired.tables.insert(other.clone(), Table::default());
    let ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let approval = recorded(&base, &ids, &desired);

    let record = &approval.rebuilds[ids.table_uid(&table()).unwrap()];
    assert!(
        record
            .source
            .tables
            .keys()
            .all(|u| ids.tables.contains_key(u))
    );
    assert_eq!(record.source.tables.len(), 2);
    assert_eq!(record.replacement.tables.len(), 2);
    assert_eq!(
        record.replacement.columns.len(),
        2,
        "inherited columns own no identity"
    );
    assert_eq!(
        (record.max_rows, record.operator.as_str()),
        (20, "operator")
    );
    for (uid, name) in &record.replacement.tables {
        assert_eq!(approval.tables.get(uid), Some(name), "live");
    }
    for uid in record
        .source
        .tables
        .keys()
        .chain(record.source.columns.keys())
    {
        let stone = &approval.tombstones[uid];
        assert_eq!(stone.reason, "maintenance");
        assert_eq!(
            (stone.operator.as_str(), stone.dropped_at.as_str()),
            ("operator", "2026-10-23")
        );
        assert!(!approval.tables.contains_key(uid) && !approval.columns.contains_key(uid));
    }
    assert_eq!(
        approval.table_uid(&other),
        ids.table_uid(&other),
        "unrelated identities stay"
    );
    assert_ne!(approval.table_uid(&table()), ids.table_uid(&table()));
    approval.validate().unwrap();
    approval.check_held_by(&desired).unwrap();
}

#[test]
fn repeating_an_approval_keeps_the_record_and_only_new_terms_refresh_its_audit() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let key = ids.table_uid(&table()).unwrap();
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
        &reason(),
        &later,
    )
    .unwrap();
    assert_eq!(
        again, approval,
        "same terms change nothing, not even the audit fields"
    );
    let raised = record(
        side(&base, &ids),
        &desired,
        &approval,
        &table(),
        21,
        &reason(),
        &later,
    )
    .unwrap();
    let (old, new) = (&approval.rebuilds[key], &raised.rebuilds[key]);
    assert_eq!(
        (new.max_rows, new.operator.as_str(), new.allowed_at.as_str()),
        (21, "later", "2026-10-24")
    );
    assert_eq!(
        (&new.source, &new.replacement),
        (&old.source, &old.replacement)
    );
    assert_eq!(raised.tables, approval.tables);
    assert_eq!(raised.tombstones, approval.tombstones);
    let error = record(
        side(&base, &ids),
        &desired,
        &approval,
        &table(),
        0,
        &reason(),
        &later,
    )
    .unwrap_err();
    assert!(error.why.contains("max_rows"), "{error}");
}

#[test]
fn reapproving_changed_declarations_rewrites_the_record_only_once_identity_is_resolved() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let key = ids.table_uid(&table()).unwrap();
    let grown = with_additions(desired.clone(), true, true);

    // Unresolved additions have no identity: refuse by name, change nothing.
    let error = record(
        side(&base, &ids),
        &grown,
        &approval,
        &table(),
        20,
        &reason(),
        &ctx(),
    )
    .unwrap_err();
    assert!(error.why.contains("resolve identity"), "{error}");

    let resolved = crate::resolve(&grown, &approval, &[], &ctx()).unwrap().ids;
    let refreshed = record(
        side(&base, &ids),
        &grown,
        &resolved,
        &table(),
        20,
        &reason(),
        &ctx(),
    )
    .unwrap();
    let (old, new) = (&approval.rebuilds[key], &refreshed.rebuilds[key]);
    assert_eq!(
        new.source, old.source,
        "the source is what the environments hold"
    );
    assert_eq!(new.replacement.tables.len(), 3);
    assert_ne!(new.target_checksum, old.target_checksum);
    qualify(side(&base, &ids), side(&grown, &refreshed), &table()).unwrap();

    // A member that is still recorded but no longer declared must be retired first.
    let mut shrunk = desired.clone();
    shrunk.tables.remove(&TableName::new("app", "p1"));
    let error = record(
        side(&base, &ids),
        &shrunk,
        &approval,
        &table(),
        20,
        &reason(),
        &ctx(),
    )
    .unwrap_err();
    assert!(error.why.contains("no longer declared"), "{error}");
}

#[test]
fn a_new_key_change_against_a_committed_approval_starts_the_next_link() {
    let (base, ids, desired) = fixture();
    let first = recorded(&base, &ids, &desired);
    let mut widened = desired.clone();
    widened
        .tables
        .get_mut(&table())
        .unwrap()
        .columns
        .get_mut("bucket")
        .unwrap()
        .ty = "bigint".parse().unwrap();
    let second = record(
        side(&desired, &first),
        &widened,
        &first,
        &table(),
        30,
        &reason(),
        &ctx(),
    )
    .unwrap();
    second.validate().unwrap();
    assert_eq!(second.rebuilds.len(), 2, "links are never merged");
    let head = ids.table_uid(&table()).unwrap();
    assert_eq!(
        second.rebuilds[first.table_uid(&table()).unwrap()]
            .source
            .tables
            .len(),
        2
    );
    assert!(
        first
            .rebuilds
            .values()
            .next()
            .unwrap()
            .replacement
            .tables
            .keys()
            .all(|u| second.tombstones.contains_key(u)),
        "the first link's replacements are the second's sources"
    );
    // The same call against a baseline that lacks the first approval is a
    // repeat of it, not a second link.
    let repeated = record(
        side(&base, &ids),
        &widened,
        &first,
        &table(),
        30,
        &reason(),
        &ctx(),
    )
    .unwrap();
    assert_eq!(repeated.rebuilds.len(), 1);
    assert_eq!(repeated.rebuilds[head].source, first.rebuilds[head].source);
}

#[test]
fn every_approved_member_change_resolves_to_the_switched_file() {
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
            &reason(),
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
            assert_eq!(
                resolution,
                crate::Resolution {
                    ids: approval.clone(),
                    ..Default::default()
                },
                "{}",
                case.name
            );
            let payload = qualify(
                side(&base, &ids),
                side(&case.desired, &approval),
                &member_cases::root(),
            )
            .expect(&case.name);
            assert_eq!(payload.before, base.tables, "{}", case.name);
            assert_eq!(payload.after, case.desired.tables, "{}", case.name);
            let target =
                transition(side(&base, &ids), side(&case.desired, &approval), &payload).unwrap();
            assert_eq!(*target, approval, "{}", case.name);
            assert!(
                payload
                    .intent
                    .source
                    .tables
                    .keys()
                    .chain(payload.intent.source.columns.keys())
                    .all(|u| target.tombstones.contains_key(u)),
                "{}",
                case.name
            );
        }
    }
    assert!(unresolved.is_empty(), "{}", unresolved.join("\n"));
}

#[test]
fn identity_intents_on_an_approved_table_follow_the_ordinary_rules() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let child = TableName::new("app", "p1");
    let mut without_child = desired.clone();
    without_child.tables.remove(&child);
    // No pending state freezes the approved members any more: a drop with a
    // reason resolves at once, and tombstones the replacement like any other.
    let dropped = crate::resolve(
        &without_child,
        &approval,
        &[pbps_model::Intent::DropTable {
            table: child.clone(),
            reason: "retired".parse().unwrap(),
        }],
        &ctx(),
    )
    .unwrap();
    assert_eq!(dropped.dropped_tables.len(), 1);
    dropped.ids.validate().unwrap();
    assert!(
        dropped
            .ids
            .rebuilds
            .values()
            .next()
            .unwrap()
            .replacement
            .tables
            .keys()
            .all(|u| dropped.ids.contains_uid(u))
    );
    // And the ordinary requirement stays: no reason, no drop.
    let blockers = crate::resolve(&without_child, &approval, &[], &ctx()).unwrap_err();
    assert!(
        matches!(&blockers[..], [crate::Blocker::DropTableNeedsReason { table }] if table == &child)
    );
    // A rename of a column is an ordinary rename of the replacement's column.
    let mut renamed = desired.clone();
    let parent = renamed.tables.get_mut(&table()).unwrap();
    let column = parent.columns.shift_remove("bucket").unwrap();
    parent.columns.insert("slot".into(), column);
    parent.partition_by.as_mut().unwrap().columns = vec!["slot".into()];
    let resolved = crate::resolve(
        &renamed,
        &approval,
        &[pbps_model::Intent::RenameColumn {
            table: table(),
            from: "bucket".into(),
            to: "slot".into(),
        }],
        &ctx(),
    )
    .unwrap();
    assert_eq!(resolved.renamed_columns.len(), 1);
    resolved.ids.validate().unwrap();
}

#[test]
fn members_added_after_the_approval_are_ordinary_additions_that_stale_it() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let grown = with_additions(desired, true, true);
    let resolved = crate::resolve(&grown, &approval, &[], &ctx()).unwrap();
    assert_eq!(resolved.created_tables.len(), 1);
    assert_eq!(resolved.added_columns.len(), 1);
    resolved.ids.validate().unwrap();
    assert_eq!(
        resolved.ids.rebuilds, approval.rebuilds,
        "the record is history, not rewritten"
    );
    let error = qualify(side(&base, &ids), side(&grown, &resolved.ids), &table()).unwrap_err();
    assert!(
        error
            .why
            .contains("re-approve it with `pbps rebuild-table app.p --max-rows 20"),
        "{error}"
    );
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
        // An approval must not pass just because it is already recorded.
        for current in [&ids, &approval] {
            let error = record(
                side(&before, &ids),
                &after,
                current,
                &table(),
                20,
                &reason(),
                &ctx(),
            )
            .unwrap_err();
            assert_eq!(error.table, table());
            assert!(error.why.contains("app.p1.k"), "{location}: {error}");
            assert!(error.why.contains("partition column override"), "{error}");
        }
        let error = qualify(side(&before, &ids), side(&after, &approval), &table()).unwrap_err();
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
        &reason(),
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
fn pulled_type_resolution_keeps_the_switched_identities() {
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
    assert_eq!(
        (&added.tables, &added.columns, &added.rebuilds),
        (&approval.tables, &approval.columns, &approval.rebuilds)
    );
    added.validate().unwrap();
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
    assert_eq!((payload.before.len(), payload.after.len()), (1, 1));
    assert!(
        record(
            side(&base, &ids),
            &base,
            &ids,
            &table(),
            20,
            &reason(),
            &ctx()
        )
        .is_err()
    );
}

#[test]
fn a_recorded_move_into_the_tree_refuses_the_approval() {
    let (base, mut ids, desired) = fixture();
    let child = ids.table_uid(&TableName::new("app", "p1")).unwrap().clone();
    ids.moves.insert(
        child,
        pbps_model::Move {
            max_rows: 5,
            allowed_at: "2026-10-01".into(),
            reason: "move".into(),
            operator: "o".into(),
        },
    );
    let error = record(
        side(&base, &ids),
        &desired,
        &ids,
        &table(),
        20,
        &reason(),
        &ctx(),
    )
    .unwrap_err();
    assert!(
        error.why.contains("move into app.p1 would be lost"),
        "{error}"
    );
}

#[test]
fn qualification_follows_what_the_baseline_holds() {
    let (base, ids, mut desired) = fixture();
    assert!(
        qualify(side(&base, &ids), side(&desired, &ids), &table())
            .unwrap_err()
            .why
            .contains("no explicit"),
        "an unapproved table has no lineage"
    );
    let approval = recorded(&base, &ids, &desired);
    let payload = qualify(side(&base, &ids), side(&desired, &approval), &table()).unwrap();
    assert_eq!(payload.before[&table()], base.tables[&table()]);
    assert_eq!(payload.after[&table()], desired.tables[&table()]);
    // An environment that already holds the replacement has nothing to rebuild.
    assert!(
        qualify(
            side(&desired, &approval),
            side(&desired, &approval),
            &table()
        )
        .is_err()
    );
    // The declarations moved on after the approval: re-approve, by name.
    desired.tables.get_mut(&table()).unwrap().description = Some("changed".into());
    let stale = qualify(side(&base, &ids), side(&desired, &approval), &table()).unwrap_err();
    assert!(
        stale
            .why
            .contains("pbps rebuild-table app.p --max-rows 20 --reason <why>"),
        "{stale}"
    );
    desired.tables.get_mut(&table()).unwrap().description = None;
    // A baseline whose parent has another identity, or lacks a column's, is foreign.
    let mut foreign = ids.clone();
    let old = foreign.table_uid(&table()).unwrap().clone();
    foreign.tables.remove(&old);
    foreign
        .tables
        .insert(Uid::generate(UidKind::Table), table());
    assert!(
        qualify(side(&base, &foreign), side(&desired, &approval), &table())
            .unwrap_err()
            .why
            .contains("lineage")
    );
    let mut missing = ids.clone();
    missing
        .columns
        .remove(ids.column_uid(&table().column("k")).unwrap());
    assert!(qualify(side(&base, &missing), side(&desired, &approval), &table()).is_err());
    // Identities beyond the tree must match too: a baseline with another table
    // is not the environment the files were approved against.
    let mut stray = ids.clone();
    stray.tables.insert(
        Uid::generate(UidKind::Table),
        TableName::new("app", "stray"),
    );
    assert!(qualify(side(&base, &stray), side(&desired, &approval), &table()).is_err());
    // A member re-created under a new identity after the approval is not the
    // replacement the record names, whatever its declaration says.
    let mut swapped = approval.clone();
    let leaf = TableName::new("app", "p1");
    let old_leaf = swapped.table_uid(&leaf).unwrap().clone();
    swapped.tables.remove(&old_leaf);
    swapped.tombstones.insert(
        old_leaf,
        pbps_model::Tombstone {
            was: "app.p1".into(),
            dropped_at: "2026-10-24".into(),
            reason: "re-created".into(),
            operator: "o".into(),
        },
    );
    swapped
        .tables
        .insert(Uid::generate(UidKind::Table), leaf.clone());
    swapped.validate().unwrap();
    let error = qualify(side(&base, &ids), side(&desired, &swapped), &table()).unwrap_err();
    assert!(error.why.contains("lineage"), "{error}");
    // And a declared member with no identity at all is not rebuildable yet.
    let mut unidentified = swapped.clone();
    unidentified.tables.retain(|_, name| name != &leaf);
    unidentified.validate().unwrap();
    let error = qualify(side(&base, &ids), side(&desired, &unidentified), &table()).unwrap_err();
    assert!(error.why.contains("lineage"), "{error}");
}

#[test]
fn unsupported_shapes_and_unchanged_keys_refuse_the_approval() {
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
            &reason(),
            &ctx()
        )
        .is_err()
    );
}

#[test]
fn an_environment_is_read_as_source_replacement_neither_or_partial() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let (key, record) = approval.rebuilds.iter().next().unwrap();
    assert_eq!(holding(&ids, key, record), Holding::Source);
    assert_eq!(holding(&approval, key, record), Holding::Replacement);
    assert_eq!(holding(&IdsFile::default(), key, record), Holding::Neither);
    // Parts of both, or a parent without its members, are never "nothing to do".
    let mut both = ids.clone();
    both.tables
        .insert(record.replacement_root(key).unwrap().clone(), table());
    assert_eq!(holding(&both, key, record), Holding::Partial);
    let mut orphan = IdsFile::default();
    orphan.columns.insert(
        record.source.columns.keys().next().unwrap().clone(),
        table().column("k"),
    );
    assert_eq!(holding(&orphan, key, record), Holding::Partial);
    let mut headless = approval.clone();
    headless
        .tables
        .insert(key.clone(), TableName::new("app", "stale"));
    headless
        .tables
        .remove(record.replacement_root(key).unwrap());
    assert_eq!(holding(&headless, key, record), Holding::Partial);
}

#[test]
fn a_plan_follows_what_its_environment_holds_of_the_lineage() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let target = side(&desired, &approval);

    // Holding the replacement: nothing to rebuild, and no drop and create.
    let done = diff(target, target);
    assert!(
        done.errors.is_empty() && done.changes.changes.is_empty(),
        "{:?}",
        done.errors
    );

    // Holding the source: the record supplies the rebuild, which is still refused.
    let pending = diff(side(&base, &ids), target);
    assert!(first_rebuild_error(&pending).contains("not available yet"));
    assert!(pending.changes.changes.is_empty());

    // The declarations moved since the approval: refused by name with the command.
    let mut moved = desired.clone();
    moved.tables.get_mut(&table()).unwrap().description = Some("changed".into());
    let stale = diff(side(&base, &ids), side(&moved, &approval));
    assert!(first_rebuild_error(&stale).contains("re-approve it with `pbps rebuild-table app.p"));

    // Holding neither (a new environment): the replacement is simply created.
    let fresh = diff(side(&Schema::default(), &IdsFile::default()), target);
    assert!(fresh.errors.is_empty(), "{:?}", fresh.errors);
    let created = fresh
        .changes
        .changes
        .iter()
        .filter(|c| matches!(c.change, pbps_model::Change::CreateTable { .. }))
        .count();
    assert_eq!(created, 2);
    assert!(
        !fresh
            .changes
            .changes
            .iter()
            .any(|c| matches!(c.change, pbps_model::Change::DropTable { .. }))
    );

    // Part of the lineage, never read as "nothing to rebuild".
    let record = approval.rebuilds.values().next().unwrap();
    let mut partial = ids.clone();
    partial.columns.insert(
        record.replacement.columns.keys().next().unwrap().clone(),
        record.replacement.columns.values().next().unwrap().clone(),
    );
    assert!(first_rebuild_error(&diff(side(&base, &partial), target)).contains("only part of"));
}

#[test]
fn a_chain_is_applied_link_by_link_and_never_merged() {
    let (base, ids, desired) = fixture();
    let first = recorded(&base, &ids, &desired);
    let mut widened = desired.clone();
    widened
        .tables
        .get_mut(&table())
        .unwrap()
        .columns
        .get_mut("bucket")
        .unwrap()
        .ty = "bigint".parse().unwrap();
    let second = record(
        side(&desired, &first),
        &widened,
        &first,
        &table(),
        30,
        &reason(),
        &ctx(),
    )
    .unwrap();
    let head = side(&widened, &second);
    // An environment holding the very first source cannot jump to the end.
    let behind = diff(side(&base, &ids), head);
    assert!(first_rebuild_error(&behind).contains("links are never merged"));
    // One that holds the first replacement rebuilds the last link, as recorded.
    let current = diff(side(&desired, &first), head);
    assert!(first_rebuild_error(&current).contains("not available yet"));
    // One that holds the final replacement has nothing to do.
    let done = diff(head, head);
    assert!(done.errors.is_empty() && done.changes.changes.is_empty());
}

#[test]
fn an_applied_rewrite_the_files_no_longer_record_is_never_a_drop_and_create() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    // The ledger holds the replacement; the files were withdrawn or are older.
    let withdrawn = withdraw(&approval, &table()).unwrap();
    for files in [side(&base, &withdrawn), side(&base, &ids)] {
        let applied = diff(side(&desired, &approval), files);
        let why = first_rebuild_error(&applied);
        assert!(why.contains("already applied an approved rewrite"), "{why}");
        assert!(applied.changes.changes.is_empty());
    }
    // A deliberate drop of the replacement is an ordinary drop.
    let mut gone = desired.clone();
    gone.tables.clear();
    let dropped_ids = crate::resolve(
        &gone,
        &approval,
        &[
            pbps_model::Intent::DropTable {
                table: TableName::new("app", "p1"),
                reason: "x".parse().unwrap(),
            },
            pbps_model::Intent::DropTable {
                table: table(),
                reason: "x".parse().unwrap(),
            },
        ],
        &ctx(),
    )
    .unwrap()
    .ids;
    assert!(
        lineage_errors(
            side(&desired, &approval),
            side(&gone, &dropped_ids),
            &pbps_dialect::MinimalDialect
        )
        .is_empty()
    );
}

#[test]
fn withdrawing_restores_exactly_the_file_before_the_approval() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    assert_eq!(withdraw(&approval, &table()).unwrap(), ids);
    assert_eq!(
        withdraw(&ids, &table()).unwrap_err().why,
        "no rebuild is recorded for it"
    );
    assert!(withdraw(&approval, &TableName::new("app", "nothing")).is_err());

    let mut widened = desired.clone();
    widened
        .tables
        .get_mut(&table())
        .unwrap()
        .columns
        .get_mut("bucket")
        .unwrap()
        .ty = "bigint".parse().unwrap();
    let second = record(
        side(&desired, &approval),
        &widened,
        &approval,
        &table(),
        30,
        &reason(),
        &ctx(),
    )
    .unwrap();
    // Only the head of a chain withdraws, and it restores the previous link.
    let back = withdraw(&second, &table()).unwrap();
    assert_eq!(back, approval);
    assert_eq!(withdraw(&back, &table()).unwrap(), ids);

    // A replacement dropped since the approval would come back as a source.
    let mut dropped = approval.clone();
    let child = dropped
        .table_uid(&TableName::new("app", "p1"))
        .unwrap()
        .clone();
    dropped.tables.remove(&child);
    dropped.tombstones.insert(
        child,
        pbps_model::Tombstone {
            was: "app.p1".into(),
            dropped_at: "d".into(),
            reason: "r".into(),
            operator: "o".into(),
        },
    );
    assert!(
        withdraw(&dropped, &table())
            .unwrap_err()
            .why
            .contains("git revert")
    );

    // A name the sources need is taken by something new.
    let mut clash = approval.clone();
    clash.rename_table(&table(), &TableName::new("app", "q"));
    clash.tables.insert(Uid::generate(UidKind::Table), table());
    assert!(withdraw(&clash, &table()).is_err());
    assert!(
        withdraw(&clash, &TableName::new("app", "q"))
            .unwrap_err()
            .why
            .contains("cannot be restored")
    );
}

#[test]
fn a_lineage_record_is_whole_or_the_file_is_refused() {
    let old = r#"{"version":1,"tables":{},"columns":{}}"#;
    let plain: IdsFile = serde_json::from_str(old).unwrap();
    assert_eq!(serde_json::to_string(&plain).unwrap(), old);
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
    for defect in [
        "zero",
        "orphan",
        "reused",
        "wrong_kind",
        "bad_digest",
        "source_live",
        "replacement_lost",
    ] {
        let mut bad = approval.clone();
        match defect {
            "zero" => bad.rebuilds.get_mut(key).unwrap().max_rows = 0,
            "orphan" => bad.rebuilds.get_mut(key).unwrap().source.tables.clear(),
            "reused" => {
                bad.rebuilds
                    .get_mut(key)
                    .unwrap()
                    .replacement
                    .tables
                    .insert(key.clone(), table());
            }
            "wrong_kind" => {
                bad.rebuilds
                    .get_mut(key)
                    .unwrap()
                    .replacement
                    .tables
                    .insert(Uid::generate(UidKind::Column), TableName::new("app", "bad"));
            }
            "bad_digest" => {
                bad.rebuilds.get_mut(key).unwrap().target_checksum = "unreadable".into()
            }
            // The half-withdrawn file: a source back without its record dropped.
            "source_live" => {
                bad.tombstones.remove(key);
                bad.tables
                    .insert(key.clone(), TableName::new("app", "stale"));
            }
            // A replacement that is neither live nor retired has been lost.
            "replacement_lost" => {
                let lost = bad.rebuilds[key].replacement.clone();
                bad.tables.retain(|u, _| !lost.tables.contains_key(u));
                bad.columns.retain(|u, _| !lost.columns.contains_key(u));
            }
            _ => unreachable!(),
        }
        assert!(bad.validate().is_err(), "{defect}");
    }
}

#[test]
fn snapshot_omissions_discard_the_whole_lineage() {
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
    let record = approval.rebuilds.values().next().unwrap();
    missing_column
        .columns
        .remove(record.replacement.columns.keys().next().unwrap());
    let projected = missing_column.without(&[]);
    assert!(projected.rebuilds.is_empty());
    projected.validate().unwrap();
}

#[test]
fn saved_plans_carry_the_lineage_but_not_a_broken_one_or_an_unadmitted_rewrite() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let plan = |ids: IdsFile, changes: pbps_model::ChangeSet| {
        pbps_model::SavedPlan::new(
            pbps_model::PlanOrigin::Database,
            "minimal",
            "now",
            pbps_model::PlanBaseline {
                description: "source".into(),
                checksum: "checksum".into(),
                database_collation: None,
                time_zone: None,
            },
            changes,
            ids,
        )
    };
    let admit = |plan: &pbps_model::SavedPlan| {
        crate::order::validate_saved_plan(plan, &pbps_dialect::MinimalDialect)
    };
    // The record outlives the rewrite, so holding one is not a reason to refuse.
    admit(&plan(approval.clone(), Default::default())).unwrap();
    let mut broken = approval.clone();
    broken.tombstones.clear();
    assert!(
        admit(&plan(broken, Default::default()))
            .unwrap_err()
            .to_string()
            .contains("inconsistent")
    );
    let root = qualify(side(&base, &ids), side(&desired, &approval), &table()).unwrap();
    let mut rewrite = pbps_model::ChangeSet::default();
    rewrite.changes.push(pbps_model::PlannedChange::new(
        pbps_model::Change::RebuildPartitionedTable(Box::new(root)),
    ));
    assert!(
        admit(&plan(approval, rewrite))
            .unwrap_err()
            .to_string()
            .contains("cannot be admitted")
    );
}

#[test]
fn proposed_ledger_identities_are_the_files_and_require_the_exact_approval() {
    let (mut base, _, mut desired) = fixture();
    let other = TableName::new("app", "other");
    base.tables.insert(other.clone(), Table::default());
    desired.tables.insert(other.clone(), Table::default());
    let ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let approval = recorded(&base, &ids, &desired);
    let root = qualify(side(&base, &ids), side(&desired, &approval), &table()).unwrap();
    let target = transition(side(&base, &ids), side(&desired, &approval), &root).unwrap();
    assert_eq!(*target, approval, "approval already installed them");
    assert_eq!(target.table_uid(&other), ids.table_uid(&other));
    let mut forged = root.clone();
    forged.intent.max_rows += 1;
    assert!(
        transition(side(&base, &ids), side(&desired, &approval), &forged)
            .unwrap_err()
            .why
            .contains("payload")
    );
    let mut withdrawn = approval.clone();
    withdrawn.rebuilds.clear();
    assert!(transition(side(&base, &ids), side(&desired, &withdrawn), &root).is_err());
    assert_eq!(approval.rebuilds.values().next(), Some(&root.intent));
}
