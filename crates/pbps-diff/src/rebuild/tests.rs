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

/// `MinimalDialect` under PostgreSQL's name: the lineage rules that need an
/// engine able to rebuild read the name, nothing else.
struct Postgres;

impl pbps_dialect::Dialect for Postgres {
    fn name(&self) -> &'static str {
        "postgres"
    }
    fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
        pbps_dialect::MinimalDialect.quote_ident(ident)
    }
    fn intermediate_tables(
        &self,
        change: &pbps_model::Change,
        strategy: pbps_model::Strategy,
    ) -> Result<Vec<TableName>, pbps_dialect::DialectError> {
        pbps_dialect::MinimalDialect.intermediate_tables(change, strategy)
    }
    fn normalize_type(
        &self,
        ty: &pbps_model::ColumnType,
    ) -> Result<pbps_model::ColumnType, pbps_dialect::DialectError> {
        pbps_dialect::MinimalDialect.normalize_type(ty)
    }
    fn type_change_risk(
        &self,
        from: &pbps_model::ColumnType,
        to: &pbps_model::ColumnType,
    ) -> pbps_dialect::TypeChangeRisk {
        pbps_dialect::MinimalDialect.type_change_risk(from, to)
    }
    fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
        pbps_dialect::MinimalDialect.fold_ident(ident)
    }
    fn lexicon(&self) -> pbps_dialect::Lexicon {
        pbps_dialect::MinimalDialect.lexicon()
    }
    fn validate_table(&self, name: &TableName, table: &Table) -> Vec<pbps_dialect::DialectError> {
        pbps_dialect::MinimalDialect.validate_table(name, table)
    }
    fn transaction_framing(
        &self,
        zone: &pbps_model::SessionZone,
    ) -> pbps_dialect::TransactionFraming {
        pbps_dialect::MinimalDialect.transaction_framing(zone)
    }
    fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
        pbps_dialect::MinimalDialect.probe_framing()
    }
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
    crate::diff_partial(base, desired, &Postgres, &Default::default())
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
    // What each replacement column was approved as is kept for a later chain.
    assert_eq!(record.types.len(), record.replacement.columns.len());
    assert!(
        record
            .types
            .values()
            .all(|shape| shape.ty.base == "integer")
    );
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
    assert_eq!(new.types.len(), new.replacement.columns.len());
    assert_ne!(
        new.types, old.types,
        "the refreshed approval keeps what it was approved as"
    );
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
    assert_eq!(second.rebuilds.len(), 2, "one record per link");
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
    both.tables.insert(record.replacement_root.clone(), table());
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
    headless.tables.remove(&record.replacement_root);
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
fn an_engine_that_cannot_rebuild_refuses_every_lineage_record() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let target = side(&desired, &approval);
    // Whether its baseline holds the source or the replacement: a record in
    // another engine's files was edited in by hand.
    for environment in [side(&base, &ids), target] {
        let other = crate::diff_partial(
            environment,
            target,
            &pbps_dialect::MinimalDialect,
            &Default::default(),
        );
        assert!(first_rebuild_error(&other).contains("does not support table rebuilds"));
        assert!(other.changes.changes.is_empty());
    }
    // Without a record the engine's name is irrelevant.
    let plain = crate::diff_partial(
        side(&base, &ids),
        side(&base, &ids),
        &pbps_dialect::MinimalDialect,
        &Default::default(),
    );
    assert!(plain.errors.is_empty(), "{:?}", plain.errors);
}

/// Two approvals in a row, with ordinary identity edits between them: the
/// first source `B` (key `k`), the middle `N` (renamed `k`, added `extra`,
/// key `bucket`) and the last `M` (`bucket` widened). Returns the declarations
/// and identities of each.
struct Chain {
    base: (Schema, IdsFile),
    middle: (Schema, IdsFile),
    last: (Schema, IdsFile),
}

fn chained(second_cap: u64) -> Chain {
    let (base, ids, desired) = fixture();
    let first = recorded(&base, &ids, &desired);
    let mut mid = desired.clone();
    let parent = mid.tables.get_mut(&table()).unwrap();
    let k = parent.columns.shift_remove("k").unwrap();
    parent.columns.insert("old_k".into(), k);
    parent
        .columns
        .insert("extra".into(), Column::new("integer".parse().unwrap()));
    let mid_ids = crate::resolve(
        &mid,
        &first,
        &[pbps_model::Intent::RenameColumn {
            table: table(),
            from: "k".into(),
            to: "old_k".into(),
        }],
        &ctx(),
    )
    .unwrap()
    .ids;
    let mut last = mid.clone();
    last.tables
        .get_mut(&table())
        .unwrap()
        .columns
        .get_mut("bucket")
        .unwrap()
        .ty = "bigint".parse().unwrap();
    let last_ids = record(
        side(&mid, &mid_ids),
        &last,
        &mid_ids,
        &table(),
        second_cap,
        &reason(),
        &ctx(),
    )
    .unwrap();
    Chain {
        base: (base, ids),
        middle: (mid, mid_ids),
        last: (last, last_ids),
    }
}

#[test]
fn an_environment_behind_on_a_chain_is_qualified_for_one_composed_rebuild() {
    let chain = chained(30);
    let head = side(&chain.last.0, &chain.last.1);
    assert_eq!(chain.last.1.rebuilds.len(), 2);

    // Holding the first source: one rebuild to the last link, shown as composed.
    let behind = diff(side(&chain.base.0, &chain.base.1), head);
    let why = first_rebuild_error(&behind);
    assert!(
        why.contains("composed over 2 approved links") && why.contains("not available yet"),
        "{why}"
    );
    assert!(behind.changes.changes.is_empty());
    let root = qualify(side(&chain.base.0, &chain.base.1), head, &table()).unwrap();
    assert_eq!(root.earlier.len(), 1);
    assert_eq!(
        root.source(),
        &records_source(&chain),
        "the oldest source it holds"
    );
    assert_eq!(root.before, chain.base.0.tables);
    assert_eq!(root.after, chain.last.0.tables);
    // Only the last link's checksum describes today's declarations.
    assert_ne!(root.earlier[0].target_checksum, root.intent.target_checksum);

    // Holding the middle replacement: just the last link, as before.
    let middle = qualify(side(&chain.middle.0, &chain.middle.1), head, &table()).unwrap();
    assert!(middle.earlier.is_empty());
    assert_eq!(middle.source(), &middle.intent.source);
    assert!(
        first_rebuild_error(&diff(side(&chain.middle.0, &chain.middle.1), head))
            .ends_with("not available yet")
    );

    // Holding the last replacement, or nothing: no rebuild to plan.
    let done = diff(head, head);
    assert!(done.errors.is_empty() && done.changes.changes.is_empty());
    let fresh = diff(side(&Schema::default(), &IdsFile::default()), head);
    assert!(fresh.errors.is_empty(), "{:?}", fresh.errors);
}

fn records_source(chain: &Chain) -> pbps_model::RebuildIdentities {
    let first = chain
        .last
        .1
        .rebuilds
        .get(chain.base.1.table_uid(&table()).unwrap());
    first.unwrap().source.clone()
}

#[test]
fn a_composed_rebuild_obeys_every_links_cap() {
    // The earlier link allowed 20 rows, the later 30: the one copy may take 20.
    let root = {
        let chain = chained(30);
        qualify(
            side(&chain.base.0, &chain.base.1),
            side(&chain.last.0, &chain.last.1),
            &table(),
        )
        .unwrap()
    };
    assert_eq!((root.intent.max_rows, root.cap()), (30, 20));
    // And the other way round, a later link may be the strict one.
    let chain = chained(5);
    let root = qualify(
        side(&chain.base.0, &chain.base.1),
        side(&chain.last.0, &chain.last.1),
        &table(),
    )
    .unwrap();
    assert_eq!(root.cap(), 5);
    // A single link's cap is its own.
    let middle = qualify(
        side(&chain.middle.0, &chain.middle.1),
        side(&chain.last.0, &chain.last.1),
        &table(),
    )
    .unwrap();
    assert_eq!(middle.cap(), 5);
}

#[test]
fn a_composed_rebuilds_columns_follow_their_uids_with_the_types_of_each_approval() {
    let chain = chained(30);
    let root = qualify(
        side(&chain.base.0, &chain.base.1),
        side(&chain.last.0, &chain.last.1),
        &table(),
    )
    .unwrap();
    let columns = root.composed_columns();
    let column = |name: &str| table().column(name);
    let shape = |stage: &pbps_model::ColumnStage| stage.shape.as_ref().map(|s| s.ty.base.clone());
    // Renamed between the approvals: followed by its uid, not its name.
    let renamed = &columns[&column("old_k")];
    assert!(renamed.from_environment);
    let names: Vec<_> = renamed
        .stages
        .iter()
        .map(|s| s.column.raw().to_string())
        .collect();
    assert_eq!(names, ["app.p.k", "app.p.k", "app.p.old_k"]);
    assert_eq!(
        renamed.stages.iter().map(shape).collect::<Vec<_>>(),
        [None, Some("integer".into()), Some("integer".into())],
        "the environment's own column keeps the type its baseline holds"
    );
    // Converted by the later approval: each stop keeps the type it was approved with.
    let widened = &columns[&column("bucket")];
    assert_eq!(
        widened.stages.iter().map(shape).collect::<Vec<_>>(),
        [None, Some("integer".into()), Some("bigint".into())]
    );
    // Born after the first approval: nothing in the environment to read.
    let born = &columns[&column("extra")];
    assert!(!born.from_environment);
    assert_eq!(born.stages.len(), 1);
    // A single link has the environment's column and its replacement only.
    let single = qualify(
        side(&chain.middle.0, &chain.middle.1),
        side(&chain.last.0, &chain.last.1),
        &table(),
    )
    .unwrap();
    assert!(
        single
            .composed_columns()
            .values()
            .all(|c| c.stages.len() <= 2)
    );
}

#[test]
fn a_composed_rebuild_refuses_what_a_single_rebuild_would_and_what_a_chain_cannot_say() {
    let chain = chained(30);
    let head = side(&chain.last.0, &chain.last.1);
    let base = side(&chain.base.0, &chain.base.1);
    // The last link's declarations moved on: refused with the command, by the
    // last link's cap.
    let mut moved = chain.last.0.clone();
    moved.tables.get_mut(&table()).unwrap().description = Some("changed".into());
    let stale = diff(base, side(&moved, &chain.last.1));
    assert!(
        first_rebuild_error(&stale)
            .contains("pbps rebuild-table app.p --max-rows 30 --reason <why>")
    );
    // An environment holding part of the middle link, or of two links, is never
    // read as behind or as done.
    let mut partial = chain.middle.1.clone();
    let old_k = partial
        .column_uid(&table().column("old_k"))
        .unwrap()
        .clone();
    partial.columns.remove(&old_k);
    assert!(
        first_rebuild_error(&diff(side(&chain.middle.0, &partial), head)).contains("only part of")
    );
    let mut mixed = chain.base.1.clone();
    let (uid, column) = chain.middle.1.columns.iter().next().unwrap();
    mixed.columns.insert(uid.clone(), column.clone());
    assert!(first_rebuild_error(&diff(side(&chain.base.0, &mixed), head)).contains("only part of"));
    // A chain whose records no longer agree with the identities around them is
    // refused by name rather than followed.
    let mut broken = chain.last.1.clone();
    let first = broken
        .rebuilds
        .values_mut()
        .find(|r| r.max_rows == 20)
        .unwrap();
    first.replacement.tables.clear();
    let refused = diff(base, side(&chain.last.0, &broken));
    assert!(first_rebuild_error(&refused).contains("does not match the identities around it"));
    assert!(qualify(base, side(&chain.last.0, &broken), &table()).is_err());
    // The environment has to hold something of the chain to be rebuilt.
    assert!(
        qualify(
            side(&Schema::default(), &IdsFile::default()),
            head,
            &table()
        )
        .is_err()
    );
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
            &Postgres
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
        "types_orphan",
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
            // A type kept for a column the record does not replace.
            "types_orphan" => {
                let shape = bad.rebuilds[key].types.values().next().unwrap().clone();
                bad.rebuilds
                    .get_mut(key)
                    .unwrap()
                    .types
                    .insert(Uid::generate(UidKind::Column), shape);
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

fn renamed_parent() -> TableName {
    TableName::new("app", "q")
}

/// An ordinary rename of the live replacement after an approval, as the files
/// would hold it: the parent to `app.q`, and the key column `bucket` to
/// `slot`. The declarations and the identities that the rename intents give.
fn renamed_after(schema: &Schema, ids: &IdsFile) -> (Schema, IdsFile) {
    let mut schema = schema.clone();
    let parent = schema.tables.get_mut(&table()).unwrap();
    let column = parent.columns.shift_remove("bucket").unwrap();
    parent.columns.insert("slot".into(), column);
    for key in &mut parent.partition_by.as_mut().unwrap().columns {
        if *key == pbps_model::ColumnName::from("bucket") {
            *key = "slot".into();
        }
    }
    let ids = crate::resolve(
        &schema,
        ids,
        &[pbps_model::Intent::RenameColumn {
            table: table(),
            from: "bucket".into(),
            to: "slot".into(),
        }],
        &ctx(),
    )
    .unwrap()
    .ids;
    let parent = schema.tables.remove(&table()).unwrap();
    schema.tables.insert(renamed_parent(), parent);
    for leaf in schema.tables.values_mut() {
        if let Some(of) = &mut leaf.partition_of {
            of.parent = renamed_parent();
        }
    }
    let ids = crate::resolve(
        &schema,
        &ids,
        &[pbps_model::Intent::RenameTable {
            from: table(),
            to: renamed_parent(),
        }],
        &ctx(),
    )
    .unwrap()
    .ids;
    (schema, ids)
}

#[test]
fn a_rename_after_the_approval_is_followed_by_identity_when_the_approval_is_refreshed() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let key = ids.table_uid(&table()).unwrap();
    let old = approval.rebuilds[key].clone();
    let (renamed, renamed_ids) = renamed_after(&desired, &approval);
    let q = renamed_parent();

    // The approval is stale, and says how to refresh it under the new name.
    let stale = qualify(side(&base, &ids), side(&renamed, &renamed_ids), &q).unwrap_err();
    assert!(
        stale
            .why
            .contains("re-approve it with `pbps rebuild-table app.q --max-rows 20"),
        "{stale}"
    );

    // Whichever baseline the files were compared with: the one without the
    // approval, or the committed one that has it.
    for committed in [(&base, &ids), (&desired, &approval)] {
        let refreshed = record(
            side(committed.0, committed.1),
            &renamed,
            &renamed_ids,
            &q,
            20,
            &reason(),
            &ctx(),
        )
        .unwrap();
        refreshed.validate().unwrap();
        let new = &refreshed.rebuilds[key];
        assert_eq!(
            refreshed.rebuilds.len(),
            1,
            "the record is rewritten in place"
        );
        assert_eq!(
            new.source, old.source,
            "the source is what the environments hold"
        );
        assert_eq!(new.replacement_root, old.replacement_root);
        assert_eq!(
            new.continues, old.continues,
            "what continues what is by identity"
        );
        assert_eq!(new.replacement.tables[&new.replacement_root], q);
        assert_eq!(
            new.source_column(&q.column("slot")),
            Some(&table().column("bucket")),
            "the renamed column still continues the source's"
        );
        assert_eq!(
            refreshed
                .rebuild_into(&new.replacement_root)
                .map(|(k, _)| k),
            Some(key)
        );
    }
}

#[test]
fn an_environment_holding_the_source_is_told_of_a_rename_it_cannot_rebuild_across() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let (renamed, renamed_ids) = renamed_after(&desired, &approval);
    let q = renamed_parent();
    let refreshed = record(
        side(&desired, &approval),
        &renamed,
        &renamed_ids,
        &q,
        20,
        &reason(),
        &ctx(),
    )
    .unwrap();
    // Not the missing-table refusal, and not the re-approval command: the
    // approval is current, and the rename is what the plan cannot carry.
    let behind = diff(side(&base, &ids), side(&renamed, &refreshed));
    let why = first_rebuild_error(&behind);
    assert!(
        why.contains("renamed after the approval") && why.contains("app.p"),
        "{why}"
    );
    assert!(!why.contains("re-approve"), "{why}");
    // An environment that already applied the rewrite has nothing to rebuild.
    assert!(
        diff(side(&renamed, &refreshed), side(&renamed, &refreshed))
            .errors
            .is_empty()
    );
    // And the unrenamed approval still qualifies for the one that holds it.
    qualify(side(&base, &ids), side(&desired, &approval), &table()).unwrap();
}

#[test]
fn a_rename_after_the_head_of_a_chain_keeps_every_link_and_its_columns() {
    let built = chained(30);
    let key = built.base.1.table_uid(&table()).unwrap().clone();
    let (renamed, renamed_ids) = renamed_after(&built.last.0, &built.last.1);
    let q = renamed_parent();
    let refreshed = record(
        side(&built.last.0, &built.last.1),
        &renamed,
        &renamed_ids,
        &q,
        30,
        &reason(),
        &ctx(),
    )
    .unwrap();
    refreshed.validate().unwrap();
    assert_eq!(refreshed.rebuilds.len(), 2);
    let head_root = refreshed.table_uid(&q).unwrap();
    let (head_key, head) = refreshed.rebuild_into(head_root).unwrap();
    let links = chain(&refreshed, head_key, head);
    assert_eq!(links.len(), 2, "both links are still read through the head");
    assert_eq!(links[0].0, &key);
    assert_eq!(
        head.source, built.last.1.rebuilds[head_key].source,
        "the head keeps the previous link's replacement as its source"
    );
    // The environment that holds the first source is still behind on the chain.
    assert_eq!(stand(&built.base.1, &links), Stand::Source(0));
    assert_eq!(stand(&built.middle.1, &links), Stand::Source(1));
    // The columns still follow their lineage through the renamed one.
    let lineage = pbps_model::RebuildPartitionedTable {
        table: q.clone(),
        earlier: vec![links[0].1.clone()],
        intent: head.clone(),
        before: Default::default(),
        after: Default::default(),
    }
    .composed_columns();
    let slot = &lineage[&q.column("slot")];
    assert!(slot.from_environment, "{slot:?}");
    assert_eq!(slot.stages.len(), 3);
    assert_eq!(slot.stages[0].column, table().column("bucket"));
    // Withdrawn, the head gives the middle files back exactly.
    assert_eq!(withdraw(&refreshed, &q).unwrap(), built.middle.1);
}

#[test]
fn a_withdrawal_after_a_rename_restores_the_file_before_the_approval() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let (renamed, renamed_ids) = renamed_after(&desired, &approval);
    let q = renamed_parent();
    // The sources come back under their own names whatever the replacement
    // was renamed to meanwhile, before the approval is refreshed and after.
    assert_eq!(withdraw(&renamed_ids, &q).unwrap(), ids);
    let refreshed = record(
        side(&desired, &approval),
        &renamed,
        &renamed_ids,
        &q,
        20,
        &reason(),
        &ctx(),
    )
    .unwrap();
    assert_eq!(withdraw(&refreshed, &q).unwrap(), ids);
    // The name the rename left is not the table any more.
    assert_eq!(
        withdraw(&refreshed, &table()).unwrap_err().why,
        "the table has no identity"
    );
}

#[test]
fn a_record_whose_pairing_contradicts_its_identities_is_unreadable() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let key = ids.table_uid(&table()).unwrap().clone();
    let broken = |edit: &dyn Fn(&mut Rebuild)| {
        let mut file = approval.clone();
        edit(file.rebuilds.get_mut(&key).unwrap());
        file.validate().is_err()
    };
    assert!(!broken(&|_| {}));
    // The replacement parent must be one of the replacement tables.
    assert!(broken(&|r| r.replacement_root = key.clone()));
    // It must continue the record's own source parent.
    assert!(broken(&|r| {
        let column = r.source.columns.keys().next().unwrap().clone();
        r.continues.insert(r.replacement_root.clone(), column);
    }));
    // A column continues a column, and no source column is continued twice.
    assert!(broken(&|r| {
        let (to, from) = r
            .continues
            .iter()
            .find(|(to, _)| *to != &r.replacement_root)
            .unwrap();
        let (to, from) = (to.clone(), from.clone());
        let other = r
            .replacement
            .columns
            .keys()
            .find(|u| **u != to)
            .unwrap()
            .clone();
        r.continues.insert(other, from);
    }));
    assert!(broken(&|r| {
        let table = r
            .replacement
            .tables
            .keys()
            .find(|u| **u != r.replacement_root)
            .unwrap()
            .clone();
        let column = r.source.columns.keys().next().unwrap().clone();
        r.continues.insert(table, column);
    }));
    // And a pairing that names no member of the record at all.
    assert!(broken(&|r| {
        r.continues.insert(
            Uid::generate(UidKind::Column),
            Uid::generate(UidKind::Column),
        );
    }));
}

#[test]
fn a_column_renamed_after_the_approval_is_still_the_environments_own_column() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    // Only the column: the parent keeps its name, so the plan can qualify.
    let mut renamed = desired.clone();
    let parent = renamed.tables.get_mut(&table()).unwrap();
    let column = parent.columns.shift_remove("bucket").unwrap();
    parent.columns.insert("slot".into(), column);
    parent.partition_by.as_mut().unwrap().columns = vec!["slot".into()];
    let renamed_ids = crate::resolve(
        &renamed,
        &approval,
        &[pbps_model::Intent::RenameColumn {
            table: table(),
            from: "bucket".into(),
            to: "slot".into(),
        }],
        &ctx(),
    )
    .unwrap()
    .ids;
    let refreshed = record(
        side(&base, &ids),
        &renamed,
        &renamed_ids,
        &table(),
        20,
        &reason(),
        &ctx(),
    )
    .unwrap();
    let root = qualify(side(&base, &ids), side(&renamed, &refreshed), &table()).unwrap();
    let slot = &root.composed_columns()[&table().column("slot")];
    assert!(slot.from_environment, "{slot:?}");
    assert_eq!(slot.stages[0].column, table().column("bucket"));
    // A column the approval never paired is the one that is born.
    let mut grown = renamed.clone();
    grown
        .tables
        .get_mut(&table())
        .unwrap()
        .columns
        .insert("extra".into(), Column::new("integer".parse().unwrap()));
    let grown_ids = crate::resolve(&grown, &refreshed, &[], &ctx()).unwrap().ids;
    let regrown = record(
        side(&base, &ids),
        &grown,
        &grown_ids,
        &table(),
        20,
        &reason(),
        &ctx(),
    )
    .unwrap();
    let born = &qualify(side(&base, &ids), side(&grown, &regrown), &table())
        .unwrap()
        .composed_columns()[&table().column("extra")];
    assert!(!born.from_environment);
}

#[test]
fn a_column_dropped_after_the_approval_leaves_the_pairing_when_the_approval_is_refreshed() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    let key = ids.table_uid(&table()).unwrap();
    let mut dropped = desired.clone();
    dropped
        .tables
        .get_mut(&table())
        .unwrap()
        .columns
        .shift_remove("k");
    let dropped_ids = crate::resolve(
        &dropped,
        &approval,
        &[pbps_model::Intent::DropColumn {
            column: table().column("k"),
            reason: reason(),
        }],
        &ctx(),
    )
    .unwrap()
    .ids;
    let refreshed = record(
        side(&base, &ids),
        &dropped,
        &dropped_ids,
        &table(),
        20,
        &reason(),
        &ctx(),
    )
    .unwrap();
    refreshed.validate().unwrap();
    let (old, new) = (&approval.rebuilds[key], &refreshed.rebuilds[key]);
    assert_eq!(
        new.replacement.columns.len() + 1,
        old.replacement.columns.len()
    );
    assert_eq!(new.continues.len() + 1, old.continues.len());
    assert!(new.continues_are_whole());
    assert_eq!(new.source, old.source);
}

/// Two approvals that end where they began, as declarations: the key `k` to
/// `bucket` and back, or the key column `bucket` widened and narrowed again.
/// Returns the declarations and identities at the start and after each link.
fn there_and_back(widen: bool) -> (Schema, IdsFile, Schema, IdsFile, IdsFile) {
    let (base, _, away) = fixture();
    let (start, away) = if widen {
        let mut start = base;
        let parent = start.tables.get_mut(&table()).unwrap();
        parent.partition_by.as_mut().unwrap().columns = vec!["bucket".into()];
        let mut away = start.clone();
        away.tables
            .get_mut(&table())
            .unwrap()
            .columns
            .get_mut("bucket")
            .unwrap()
            .ty = "bigint".parse().unwrap();
        (start, away)
    } else {
        (base, away)
    };
    let ids = crate::resolve(&start, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let first = recorded(&start, &ids, &away);
    let back = record(
        side(&away, &first),
        &start,
        &first,
        &table(),
        30,
        &reason(),
        &ctx(),
    )
    .unwrap();
    (start, ids, away, first, back)
}

#[test]
fn a_chain_that_ends_where_it_began_is_qualified_as_one_composed_rebuild() {
    for widen in [false, true] {
        let (start, ids, away, first, back) = there_and_back(widen);
        assert_eq!(back.rebuilds.len(), 2, "each link met its own rule");
        // The environment at the first source: the endpoints are one shape, and
        // the rows still need the approved identities and conversions.
        let root = qualify(side(&start, &ids), side(&start, &back), &table()).unwrap();
        assert_eq!(root.earlier.len(), 1, "{widen}");
        assert_eq!(root.before, root.after, "no net change in the declarations");
        assert_eq!(root.cap(), 20, "the strictest link's cap");
        let columns = root.composed_columns();
        assert!(
            columns[&table().column("bucket")].from_environment,
            "{widen}: a column carried through both links reads the environment's"
        );
        let behind = diff(side(&start, &ids), side(&start, &back));
        let why = first_rebuild_error(&behind);
        assert!(
            why.contains("composed over 2 approved links"),
            "{widen}: {why}"
        );
        // Holding the middle link is one ordinary rebuild, which does change.
        qualify(side(&away, &first), side(&start, &back), &table()).unwrap();
        // The environment that has everything has nothing to rebuild.
        assert!(
            diff(side(&start, &back), side(&start, &back))
                .errors
                .is_empty()
        );
    }
}

#[test]
fn a_composed_rebuild_without_a_net_change_still_meets_every_other_rule() {
    let (start, ids, away, first, back) = there_and_back(false);
    // A single link keeps the rule that its key or a key column type changes.
    let error = qualify(side(&away, &first), side(&away, &back), &table()).unwrap_err();
    assert!(
        error
            .why
            .contains("neither the partition key nor a key column type changed"),
        "{error}"
    );
    // The endpoints are still held to the shape of one rewrite: an unrelated
    // edit, a list-partitioned parent, a computed column.
    let mut unrelated = start.clone();
    unrelated
        .tables
        .insert(TableName::new("app", "other"), Table::default());
    let error = qualify(side(&start, &ids), side(&unrelated, &back), &table()).unwrap_err();
    assert!(error.why.contains("unrelated schema edits"), "{error}");
    let mut listed = start.clone();
    listed
        .tables
        .get_mut(&table())
        .unwrap()
        .partition_by
        .as_mut()
        .unwrap()
        .strategy = PartitionStrategy::List;
    let error = qualify(side(&start, &ids), side(&listed, &back), &table()).unwrap_err();
    assert!(error.why.contains("only a managed RANGE parent"), "{error}");
    // And the last link's approval still has to match the declarations.
    let mut moved = start.clone();
    moved
        .tables
        .get_mut(&table())
        .unwrap()
        .columns
        .insert("extra".into(), Column::new("integer".parse().unwrap()));
    let error = qualify(side(&start, &ids), side(&moved, &back), &table()).unwrap_err();
    assert!(error.why.contains("re-approve it with"), "{error}");
}

fn conversion() -> pbps_model::Conversion {
    pbps_model::Conversion {
        from: "text".into(),
        to: "integer".into(),
        using: "x::integer".into(),
        allowed_at: "2026-10-24".into(),
        operator: "tester".into(),
    }
}

/// The fixture plus a table the rebuild does not touch, with a conversion
/// recorded for one of its columns.
fn with_unrelated_conversion() -> (Schema, IdsFile, Schema, Uid) {
    let (mut base, _, mut desired) = fixture();
    let other = TableName::new("app", "other");
    let spec = Table {
        columns: [("v".into(), Column::new("integer".parse().unwrap()))]
            .into_iter()
            .collect(),
        ..Table::default()
    };
    base.tables.insert(other.clone(), spec.clone());
    desired.tables.insert(other.clone(), spec);
    let mut ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let uid = ids.column_uid(&other.column("v")).unwrap().clone();
    ids.conversions.insert(uid.clone(), conversion());
    ids.validate().unwrap();
    (base, ids, desired, uid)
}

#[test]
fn an_approval_keeps_the_conversion_of_a_column_it_does_not_replace() {
    let (base, ids, desired, other) = with_unrelated_conversion();
    let approval = recorded(&base, &ids, &desired);
    assert_eq!(approval.conversions.keys().collect::<Vec<_>>(), [&other]);
    approval.validate().unwrap();
    // Re-approval and withdrawal leave it as they find it.
    let refreshed = record(
        side(&desired, &approval),
        &desired,
        &approval,
        &table(),
        30,
        &reason(),
        &ctx(),
    )
    .unwrap();
    assert_eq!(refreshed.conversions, approval.conversions);
    assert_eq!(withdraw(&refreshed, &table()).unwrap(), ids);
}

#[test]
fn an_approval_over_a_recorded_conversion_of_a_source_column_is_refused() {
    let (base, mut ids, desired) = fixture();
    let bucket = ids.column_uid(&table().column("bucket")).unwrap().clone();
    ids.conversions.insert(bucket.clone(), conversion());
    // The approval retires the column the conversion belongs to; it must not
    // write a file that validation refuses, nor drop the intent unnamed.
    let refused = record(
        side(&base, &ids),
        &desired,
        &ids,
        &table(),
        20,
        &reason(),
        &ctx(),
    );
    assert!(refused.is_err());
    // The same file without the conversion is approved.
    ids.conversions.remove(&bucket);
    assert!(
        record(
            side(&base, &ids),
            &desired,
            &ids,
            &table(),
            20,
            &reason(),
            &ctx()
        )
        .is_ok()
    );
}

#[test]
fn a_withdrawal_takes_the_conversion_of_a_replacement_column_with_it() {
    let (base, ids, desired, other) = with_unrelated_conversion();
    let approval = recorded(&base, &ids, &desired);
    let replacement = approval
        .column_uid(&table().column("bucket"))
        .unwrap()
        .clone();
    let mut later = approval.clone();
    later.conversions.insert(replacement.clone(), conversion());
    later.validate().unwrap();
    // The conversion described a column the withdrawal removes: the file comes
    // back as it was before the approval, and the other column's survives.
    let restored = withdraw(&later, &table()).unwrap();
    assert_eq!(restored, ids);
    assert!(restored.conversions.contains_key(&other));
    assert!(!restored.conversions.contains_key(&replacement));
    // A refreshed approval keeps the conversion of a column it still holds.
    let refreshed = record(
        side(&desired, &later),
        &desired,
        &later,
        &table(),
        30,
        &reason(),
        &ctx(),
    )
    .unwrap();
    assert!(refreshed.conversions.contains_key(&replacement));
    refreshed.validate().unwrap();
}

#[test]
fn a_chain_link_meets_the_conversion_rules_of_the_link_before_it() {
    let (start, _, away, first, back) = there_and_back(false);
    let middle = first.column_uid(&table().column("bucket")).unwrap().clone();
    // The second approval retires the first link's replacement columns, so a
    // conversion recorded for one is refused as for any source column.
    let mut converted = first.clone();
    converted.conversions.insert(middle.clone(), conversion());
    assert!(
        record(
            side(&away, &converted),
            &start,
            &converted,
            &table(),
            30,
            &reason(),
            &ctx(),
        )
        .is_err()
    );
    // Withdrawing the second link brings the middle columns back, and a
    // conversion recorded for the last link's columns goes with them.
    let last = back.column_uid(&table().column("bucket")).unwrap().clone();
    let mut later = back.clone();
    later.conversions.insert(last.clone(), conversion());
    let restored = withdraw(&later, &table()).unwrap();
    assert_eq!(restored, first);
    assert!(restored.conversions.is_empty());
}

#[test]
fn a_conversion_left_on_a_retired_column_makes_the_lineage_unreadable_by_name() {
    let (base, ids, desired) = fixture();
    let approval = recorded(&base, &ids, &desired);
    // A merge of a recorded conversion with an approval that retired its
    // column: the plan must refuse, not read the rebuild as done or absent.
    let retired = ids.column_uid(&table().column("bucket")).unwrap().clone();
    let mut merged = approval.clone();
    merged.conversions.insert(retired, conversion());
    let why = first_rebuild_error(&diff(side(&base, &ids), side(&desired, &merged)));
    assert!(why.contains("conversion"), "{why}");
    // The same file without the conversion is not unreadable.
    let clean = diff(side(&base, &ids), side(&desired, &approval));
    assert!(first_rebuild_error(&clean).contains("recorded intent is valid"));
}
