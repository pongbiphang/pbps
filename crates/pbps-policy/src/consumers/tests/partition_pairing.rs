use super::*;

fn endpoints() -> (Schema, IdsFile, Schema, IdsFile, TableName) {
    let (mut attached, inherited, leaf) = partitioned();
    attached
        .tables
        .get_mut(&name())
        .unwrap()
        .columns
        .insert("other".into(), Column::new(ColumnType::simple("integer")));
    let mut inherited = inherited;
    inherited
        .columns
        .insert(uid("c_cccccc"), name().column("other"));
    let mut detached = attached.clone();
    detached
        .tables
        .insert(leaf.clone(), attached.tables[&name()].clone());
    detached.tables.get_mut(&leaf).unwrap().partition_by = None;
    let mut owned = inherited.clone();
    owned.columns.insert(uid("c_bbbbbb"), leaf.column("old"));
    owned.columns.insert(uid("c_dddddd"), leaf.column("other"));
    (attached, inherited, detached, owned, leaf)
}

fn joined(sides: &[Side<'_>; 2], leaf: &TableName) {
    let interfaces = effective::Interfaces::of(sides, &ChangeSet::default());
    let columns: Vec<_> = interfaces
        .columns()
        .filter(|p| {
            p.table == &uid("t_bbbbbb") && p.addresses().any(|at| at == &leaf.column("old"))
        })
        .collect();
    assert_eq!(columns.len(), 1, "one interface, not two one-sided columns");
    let pair = columns[0];
    assert!(pair.before.is_some() && pair.after.is_some());
    assert!(pair.has_uid(&uid("c_aaaaaa")) && pair.has_uid(&uid("c_bbbbbb")));
}

#[test]
fn attachment_pairs_retiring_column_uids_before_comparing_nullability() {
    for required_domain in [false, true] {
        let (mut attached, inherited, mut detached, owned, leaf) = endpoints();
        detached
            .tables
            .get_mut(&leaf)
            .unwrap()
            .columns
            .get_mut("old")
            .unwrap()
            .nullable = false;
        if required_domain {
            for schema in [&mut attached, &mut detached] {
                schema
                    .types
                    .insert("app.d".parse().unwrap(), domain(true, None, None));
                for table in schema.tables.values_mut() {
                    if let Some(column) = table.columns.get_mut("old") {
                        column.ty = ColumnType::reference("app.d".parse().unwrap());
                    }
                }
            }
        }
        let changes = vec![
            Change::AttachPartition {
                uid: uid("t_bbbbbb"),
                table: leaf.clone(),
                parent: name(),
                bound: pbps_model::PartitionBound::Default,
                key: vec!["old".into()],
                shape: Box::new(attached.tables[&leaf].clone()),
            },
            Change::SetPartitionNotNull {
                uid: uid("t_bbbbbb"),
                table: leaf.clone(),
                column: "old".into(),
                not_null: false,
            },
        ];
        let sides = [
            Side {
                schema: &detached,
                ids: &owned,
            },
            Side {
                schema: &attached,
                ids: &inherited,
            },
        ];
        for entry in ["app.leaf", "app.leaf.old", "app.leaf.other", "app.t"] {
            let found = findings(
                &detached,
                &attached,
                &owned,
                &inherited,
                &consumers(entry, false),
                changes.clone(),
            );
            let expected =
                usize::from(!required_domain && matches!(entry, "app.leaf" | "app.leaf.old"));
            assert_eq!(
                found.len(),
                expected,
                "{entry}, domain required={required_domain}: {found:?}"
            );
            for (at, finding) in found {
                assert_eq!(at, 1);
                assert_eq!(finding.id, "consumer.unproven");
                assert!(
                    finding
                        .message
                        .contains(&format!("consumers.billing-v41.uses: {entry}"))
                );
            }
        }
        joined(&sides, &leaf);
    }
}

#[test]
fn detachment_pairs_new_column_uids_before_comparing_domain_nullability() {
    for local_required in [false, true] {
        let (mut attached, mut inherited, mut detached, mut owned, leaf) = endpoints();
        let ty: pbps_model::TypeRef = "app.d".parse().unwrap();
        for (schema, not_null) in [(&mut attached, true), (&mut detached, false)] {
            schema
                .types
                .insert(ty.clone(), domain(not_null, None, None));
            for table in schema.tables.values_mut() {
                if let Some(column) = table.columns.get_mut("old") {
                    column.ty = ColumnType::reference(ty.clone());
                }
            }
        }
        for ids in [&mut inherited, &mut owned] {
            ids.types.insert(uid("y_aaaaaa"), ty.clone());
        }
        attached
            .tables
            .get_mut(&leaf)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns
            .entry("old".into())
            .or_default()
            .not_null = local_required;
        detached
            .tables
            .get_mut(&leaf)
            .unwrap()
            .columns
            .get_mut("old")
            .unwrap()
            .nullable = !local_required;
        let changes = vec![
            Change::DetachPartition {
                uid: uid("t_bbbbbb"),
                table: leaf.clone(),
                parent: name(),
                names: vec![],
                shape: Box::new(detached.tables[&leaf].clone()),
            },
            Change::AlterDomain {
                uid: uid("y_aaaaaa"),
                name: ty.clone(),
                from: Box::new(attached.types[&ty].clone()),
                to: Box::new(detached.types[&ty].clone()),
                half: pbps_model::DomainHalf::Loosen,
            },
        ];
        let sides = [
            Side {
                schema: &attached,
                ids: &inherited,
            },
            Side {
                schema: &detached,
                ids: &owned,
            },
        ];
        for entry in ["app.leaf", "app.leaf.old", "app.leaf.other"] {
            let found = findings(
                &attached,
                &detached,
                &inherited,
                &owned,
                &consumers(entry, false),
                changes.clone(),
            );
            let expected = usize::from(!local_required && entry != "app.leaf.other");
            assert_eq!(
                found.len(),
                expected,
                "{entry}, local required={local_required}: {found:?}"
            );
            for (at, finding) in found {
                assert_eq!(at, 1);
                assert_eq!(finding.id, "consumer.unproven");
                assert!(
                    finding
                        .message
                        .contains(&format!("consumers.billing-v41.uses: {entry}"))
                );
            }
        }
        joined(&sides, &leaf);
    }
}

#[test]
fn exact_source_uids_keep_their_pair_before_partition_name_bridges() {
    let (before, old, _, _, leaf) = endpoints();
    let mut after = before.clone();
    let parent = after.tables.get_mut(&name()).unwrap();
    parent.columns.shift_remove("old").unwrap();
    let surviving = parent.columns.shift_remove("other").unwrap();
    parent.columns.insert("old".into(), surviving);
    let mut new = old.clone();
    new.columns.remove(&uid("c_aaaaaa"));
    new.columns.insert(uid("c_cccccc"), name().column("old"));
    let sides = [
        Side {
            schema: &before,
            ids: &old,
        },
        Side {
            schema: &after,
            ids: &new,
        },
    ];
    let interfaces = effective::Interfaces::of(&sides, &ChangeSet::default());
    // The departed source sorts first and can name the survivor's new address.
    // Its bridge must not consume the survivor's exact (table, source UID) key.
    for owner in [name(), leaf] {
        let pairs: Vec<_> = interfaces
            .columns()
            .filter(|pair| Some(pair.table) == old.table_uid(&owner))
            .collect();
        let exact = pairs
            .iter()
            .find(|pair| {
                pair.before
                    .is_some_and(|column| column.at == owner.column("other"))
            })
            .unwrap();
        assert_eq!(
            exact.after.map(|column| &column.at),
            Some(&owner.column("old"))
        );
        let departed = pairs
            .iter()
            .find(|pair| {
                pair.before
                    .is_some_and(|column| column.at == owner.column("old"))
            })
            .unwrap();
        assert!(
            departed.after.is_none(),
            "a reused inherited name is not the exact survivor"
        );
    }
}

#[test]
fn inherited_replacements_keep_the_supplied_column_exception_for_sibling_writers() {
    let (before, old, _, _, leaf) = endpoints();
    for with_default in [true, false] {
        let mut after = before.clone();
        let mut replacement = Column::new(ColumnType::simple("integer")).not_null();
        if with_default {
            replacement.value_source = ValueSource::Default("0".into());
        }
        after
            .tables
            .get_mut(&name())
            .unwrap()
            .columns
            .insert("other".into(), replacement.clone());
        let mut new = old.clone();
        new.columns.remove(&uid("c_cccccc"));
        new.columns.insert(uid("c_eeeeee"), name().column("other"));
        let changes = vec![
            Change::DropColumn {
                uid: uid("c_cccccc"),
                column: name().column("other"),
            },
            Change::AddColumn {
                uid: uid("c_eeeeee"),
                table: name(),
                name: "other".into(),
                column: Box::new(replacement),
            },
        ];
        for entry in ["app.t.old", "app.leaf.old"] {
            let found = findings(
                &before,
                &after,
                &old,
                &new,
                &consumers(entry, true),
                changes.clone(),
            );
            assert_eq!(
                found.len(),
                usize::from(!with_default),
                "{entry}, default={with_default}: {found:?}"
            );
            if !with_default {
                assert_eq!(found[0].0, 1);
                assert_eq!(found[0].1.id, "consumer.unproven");
            }
        }
        let sides = [
            Side {
                schema: &before,
                ids: &old,
            },
            Side {
                schema: &after,
                ids: &new,
            },
        ];
        let interfaces = effective::Interfaces::of(
            &sides,
            &ChangeSet {
                changes: changes.into_iter().map(PlannedChange::new).collect(),
            },
        );
        let replacement = interfaces
            .columns()
            .find(|pair| pair.table == &uid("t_bbbbbb") && pair.has_uid(&uid("c_eeeeee")))
            .unwrap();
        assert!(
            replacement.before.is_none(),
            "a reused inherited spelling does not establish continuity"
        );
        assert_eq!(replacement.after.unwrap().at, leaf.column("other"));
    }
}
