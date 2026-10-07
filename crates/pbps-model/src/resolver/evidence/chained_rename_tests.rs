use super::transition_scope_tests::{object, seal};
use super::*;
use crate::resolver::{ObjectOwnership, Surface};
use crate::{PlannedChange, TableName};

fn chain(column: bool, intermediate: bool, rename_table: bool) -> (ChangeSet, ResolverEvidence) {
    let (fixture_changes, mut evidence, compiled) =
        super::transition_tests::rename_endpoints(column);
    // Until sealing, `evidence.after` holds the compiled manifest, as in
    // `transition_scope_tests::fixture`.
    evidence.after = compiled;
    let fixture_parent = column.then(|| {
        evidence
            .transitions
            .iter()
            .find(|t| matches!(t.surface, Surface::Table(_)))
            .unwrap()
            .before
            .iter()
            .next()
            .unwrap()
            .clone()
    });
    let source = match &fixture_changes.changes[0].change {
        Change::RenameColumn { table, from, .. } => Surface::Column(table.column(from)),
        Change::RenameTable { from, .. } => Surface::Table(from.clone()),
        Change::CreateTable { .. }
        | Change::DetachPartition { .. }
        | Change::DropTable { .. }
        | Change::AddColumn { .. }
        | Change::AddComputedColumn { .. }
        | Change::SetReplicaIdentity { .. }
        | Change::SetStorageParameters { .. }
        | Change::SetTablePersistence { .. }
        | Change::SetPartitionDefault { .. }
        | Change::SetPartitionNotNull { .. }
        | Change::SetIndexStorageParameters { .. }
        | Change::DropComputedColumn { .. }
        | Change::DropColumn { .. }
        | Change::AlterColumnType { .. }
        | Change::AlterColumnNullability { .. }
        | Change::AlterColumnDefault { .. }
        | Change::AlterColumnExpression { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::DropUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::DropForeignKey { .. }
        | Change::AddCheck { .. }
        | Change::DropCheck { .. }
        | Change::AddIndex { .. }
        | Change::DropIndex { .. }
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::CreateModule { .. }
        | Change::AlterModule { .. }
        | Change::DropModule { .. }
        | Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. }
        | Change::PublicExecution { .. } => panic!("rename fixture"),
    };
    let rename_transition = evidence
        .transitions
        .iter()
        .find(|transition| transition.surface == source)
        .unwrap();
    let template = evidence
        .before
        .prerequisites()
        .iter()
        .find(|p| rename_transition.before.contains(&p.object))
        .unwrap()
        .clone();
    let old_object = template.object.clone();
    let new_object = rename_transition.after.iter().next().unwrap().clone();
    let old_table: TableName = "app.old".parse().unwrap();
    let table: TableName = "app.t".parse().unwrap();
    let mut changes = ChangeSet::default();
    if rename_table {
        changes
            .changes
            .push(PlannedChange::new(Change::RenameTable {
                uid: "t_000000".parse().unwrap(),
                from: old_table.clone(),
                to: table.clone(),
                defaults: vec![],
            }));
    }
    let rename = |first: bool, from: &str, to: &str| {
        if column {
            Change::RenameColumn {
                uid: if first { "c_000000" } else { "c_111111" }.parse().unwrap(),
                table: table.clone(),
                from: from.into(),
                to: to.into(),
                table_was: rename_table.then(|| old_table.clone()),
            }
        } else {
            Change::RenameTable {
                uid: if first { "t_000000" } else { "t_111111" }.parse().unwrap(),
                from: format!("app.{from}").parse().unwrap(),
                to: format!("app.{to}").parse().unwrap(),
                defaults: vec![],
            }
        }
    };
    if intermediate {
        changes
            .changes
            .push(PlannedChange::new(rename(true, "a", "tmp")));
    }
    changes
        .changes
        .push(PlannedChange::new(rename(false, "b", "c")));
    changes.changes.push(PlannedChange::new(rename(
        true,
        if intermediate { "tmp" } else { "a" },
        "b",
    )));
    let surface = |name: &str, before: bool, child: bool| {
        let table = if column {
            if before && rename_table {
                old_table.clone()
            } else {
                table.clone()
            }
        } else {
            format!("app.{name}").parse().unwrap()
        };
        if child {
            Surface::Default(table.column(if column { name } else { "n" }))
        } else if column {
            Surface::Column(table.column(name))
        } else {
            Surface::Table(table)
        }
    };
    evidence.transitions.clear();
    let mut opening = vec![];
    let mut closing = vec![];
    for (from, to) in [("a", "b"), ("b", "c")] {
        let mut transition = ObjectTransition {
            references: BTreeSet::new(),
            surface: surface(from, false, false),
            before: BTreeSet::new(),
            after: BTreeSet::new(),
        };
        for child in [false, true] {
            for before in [true, false] {
                let name = if before { from } else { to };
                let mut record = template.clone();
                record.object = object(&format!(
                    "{}-{name}-{child}",
                    if before { "opening" } else { "closing" }
                ));
                record.ownership = ObjectOwnership::Surface(surface(name, before, child));
                record.bindings.clear();
                if before {
                    transition.before.insert(record.object.clone());
                    opening.push(record);
                } else {
                    transition.after.insert(record.object.clone());
                    closing.push(record);
                }
            }
        }
        evidence.transitions.push(transition);
    }
    if rename_table {
        let mut before = template.clone();
        before.object = object("opening-table");
        before.ownership = ObjectOwnership::Surface(Surface::Table(old_table.clone()));
        before.bindings.clear();
        let mut after = before.clone();
        after.object = object("closing-table");
        after.ownership = ObjectOwnership::Surface(Surface::Table(table.clone()));
        // A table aggregate contains both column renames and the table's own
        // records, while their endpoint names still belong to different UIDs.
        evidence.transitions = vec![ObjectTransition {
            references: BTreeSet::new(),
            // The final table label also covers the column changes
            // whose statement-time owner is the renamed table.
            surface: Surface::Table(table),
            before: opening
                .iter()
                .map(|p| p.object.clone())
                .chain([before.object.clone()])
                .collect(),
            after: closing
                .iter()
                .map(|p| p.object.clone())
                .chain([after.object.clone()])
                .collect(),
        }];
        opening.push(before);
        closing.push(after);
    } else if column {
        // The helper's app.v parent is not this chain's app.t parent. Column
        // renames preserve app.t and need its independent exact inventory.
        let mut parent = template.clone();
        parent.object = object("retained-table");
        parent.ownership = ObjectOwnership::Surface(Surface::Table(table.clone()));
        parent.bindings.clear();
        evidence.transitions.push(ObjectTransition {
            references: BTreeSet::new(),
            surface: Surface::Table(table),
            before: BTreeSet::from([parent.object.clone()]),
            after: BTreeSet::from([parent.object.clone()]),
        });
        opening.push(parent.clone());
        closing.push(parent);
    }
    for (manifest, removed, added) in [
        (&mut evidence.before, old_object, opening),
        (&mut evidence.after, new_object, closing),
    ] {
        let mut records: Vec<_> = manifest
            .prerequisites()
            .iter()
            .filter(|p| p.object != removed && fixture_parent.as_ref() != Some(&p.object))
            .cloned()
            .collect();
        records.extend(added);
        records.sort_by(|a, b| a.object.cmp(&b.object));
        let mut json = serde_json::to_value(&*manifest).unwrap();
        json["prerequisites"] = serde_json::to_value(records).unwrap();
        for predicate in json["membership"].as_array_mut().unwrap() {
            predicate["members"] = serde_json::json!([]);
        }
        *manifest = serde_json::from_value(json).unwrap();
    }
    evidence
        .transitions
        .sort_by(|a, b| a.surface.cmp(&b.surface));
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    (changes, evidence)
}

fn accepts_chain(column: bool, rename_table: bool) {
    for intermediate in [false, true] {
        let (changes, evidence) = chain(column, intermediate, rename_table);
        let projected = seal(&changes, &evidence).expect("valid UID rename chain was refused");
        super::tests::assert_closing(&projected, &evidence.after);
        let decoded: ResolverEvidence =
            serde_json::from_value(serde_json::to_value(&projected).unwrap()).unwrap();
        decoded.validate(&changes).unwrap();
        // Every owner and child remains mandatory at each snapshot. A name
        // another UID reused must never excuse a missing endpoint record.
        for index in 0..evidence.transitions.len() {
            for before in [true, false] {
                let inventory = if before {
                    &evidence.transitions[index].before
                } else {
                    &evidence.transitions[index].after
                };
                for object in inventory {
                    let mut wrong = evidence.clone();
                    if before {
                        wrong.transitions[index].before.remove(object);
                    } else {
                        wrong.transitions[index].after.remove(object);
                    }
                    assert!(
                        seal(&changes, &wrong).is_err(),
                        "missing chained rename record accepted"
                    );
                    // The reader holds no compiled records, so only an
                    // omitted opening record is its to refuse.
                    if before {
                        let mut saved = projected.clone();
                        saved.transitions = wrong.transitions;
                        assert!(
                            saved.validate(&changes).is_err(),
                            "saved chained rename omission accepted"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn chained_table_renames_keep_distinct_recorded_identities() {
    accepts_chain(false, false);
}

#[test]
fn chained_column_renames_keep_distinct_recorded_identities() {
    accepts_chain(true, false);
}

#[test]
fn table_and_column_rename_chains_keep_both_snapshot_owners() {
    accepts_chain(true, true);
}
