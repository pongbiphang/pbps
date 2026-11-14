use super::*;
use pbps_model::{Change, ChangeSet, Column, DataMode, DataScope, ManagedType, PlannedChange};

fn mood(uid: &str) -> (IdsFile, Schema) {
    let ids = IdsFile {
        types: [(uid.parse().unwrap(), "app.mood".parse().unwrap())].into(),
        ..Default::default()
    };
    let mut table = pbps_model::Table::default();
    table
        .columns
        .insert("m".into(), Column::new("app.mood".parse().unwrap()));
    table.primary_key = Some(pbps_model::PrimaryKey {
        name: None,
        columns: vec!["m".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    });
    let mut schema = Schema::default();
    schema.types.insert(
        "app.mood".parse().unwrap(),
        ManagedType::Enum {
            values: vec!["sad".into(), "ok".into()],
        },
    );
    schema.tables.insert("app.moods".parse().unwrap(), table);
    (ids, schema)
}

fn plan(ids: &IdsFile, renames: &[(&str, &str, &str)]) -> pbps_model::SavedPlan {
    pbps_model::SavedPlan::new(
        pbps_model::PlanOrigin::Database,
        "postgres",
        "2026-10-26T00:00:00Z",
        pbps_model::PlanBaseline {
            description: "test".into(),
            checksum: String::new(),
            database_collation: None,
            time_zone: None,
        },
        ChangeSet {
            changes: renames
                .iter()
                .enumerate()
                .map(|(ordinal, (uid, from, to))| {
                    PlannedChange::new(Change::RenameEnumValue {
                        uid: uid.parse().unwrap(),
                        name: "app.mood".parse().unwrap(),
                        from: (*from).into(),
                        to: (*to).into(),
                        ordinal,
                        retained: Vec::new(),
                        definition: Box::new(ManagedType::Enum {
                            values: vec![(*to).into()],
                        }),
                    })
                })
                .collect(),
        },
        ids.clone(),
    )
}

fn keys(scopes: &DataScopes) -> Vec<String> {
    scopes[&"app.moods".parse::<TableName>().unwrap()]
        .keys
        .iter()
        .map(|k| k.as_str().to_owned())
        .collect()
}

/// DEC-2346.2: a staged baseline, like the transactional one, asks the
/// rows for the label they still hold when the plan renames it, and for
/// nothing else.
#[test]
fn the_baseline_asks_for_the_label_a_key_holds_before_the_plans_rename() {
    let (ids, schema) = mood("y_000001");
    let scopes: DataScopes = [(
        "app.moods".parse().unwrap(),
        DataScope {
            mode: DataMode::Ensure,
            keys: ["blue".into(), "ok".into()].into(),
        },
    )]
    .into();
    let held = |renames: &[(&str, &str, &str)]| {
        keys(&held_before_the_plan(
            &plan(&ids, renames),
            &schema,
            &ids,
            &scopes,
        ))
    };
    assert_eq!(held(&[("y_000001", "sad", "blue")]), ["blue", "ok", "sad"]);
    // No rename: unchanged. The rename of another enum: unchanged.
    assert_eq!(held(&[]), ["blue", "ok"]);
    assert_eq!(held(&[("y_000009", "sad", "blue")]), ["blue", "ok"]);
    // A rename of a label no key holds adds the label it came from, which
    // no row holds either: a read that finds nothing.
    assert_eq!(held(&[("y_000001", "gone", "ok")]), ["blue", "gone", "ok"]);
}
/// The guard's side of the chain: the baseline trimmed to the rows the plan
/// still manages is carried through the renames and meets the closing
/// read (DEC-2346.2).
#[test]
fn a_chain_of_renames_over_a_narrow_ensure_table_moves_no_row() {
    let (ids, _) = mood("y_000001");
    let table = |labels: &[&str], keys: &[&str]| {
        let mut schema = Schema::default();
        schema.types.insert(
            "app.mood".parse().unwrap(),
            ManagedType::Enum {
                values: labels.iter().map(|l| (*l).to_owned()).collect(),
            },
        );
        let mut t = pbps_model::Table::default();
        t.columns
            .insert("m".into(), Column::new("app.mood".parse().unwrap()));
        t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["m".into()],
            storage_parameters: Default::default(),
            filegroup: None,
        });
        t.data = Some(pbps_model::TableData {
            mode: DataMode::Ensure,
            rows: keys
                .iter()
                .map(|k| ((*k).into(), pbps_model::Row::default()))
                .collect(),
        });
        schema.tables.insert("app.moods".parse().unwrap(), t);
        schema
    };
    let chain = |ids: &IdsFile| {
        let mut changes = plan(ids, &[("y_000001", "b", "c"), ("y_000001", "a", "b")]).changes;
        for (step, labels) in changes.changes.iter_mut().zip([["a", "c"], ["b", "c"]]) {
            if let Change::RenameEnumValue { definition, .. } = &mut step.change {
                **definition = ManagedType::Enum {
                    values: labels.iter().map(|l| (*l).to_owned()).collect(),
                };
            }
        }
        changes
    };
    let check = |changes: &ChangeSet, before: &Schema, after: &Schema| {
        let mut ids = guard::fixture_ids(changes, before);
        ids.types = mood("y_000001").0.types;
        let plan = guard::Plan { changes, ids: &ids };
        refuse_pinned_movement(
            &pbps_pg::Postgres::new(),
            guard::Read::transactional(plan),
            before,
            after,
            "prod",
        )
    };
    let changes = chain(&ids);
    // The baseline kept to the managed row `a`, which the plan leaves as
    // `b`; the unmanaged `c` is not in the closing read.
    let before = table(&["a", "b"], &["a"]);
    check(&changes, &before, &table(&["b", "c"], &["b"])).unwrap();
    // A real deletion of the declared row is still refused.
    let error = check(&changes, &before, &table(&["b", "c"], &[]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("app.moods"), "{error}");
    // The unmanaged row `c` read into the baseline (not trimmed) is the
    // finding's refusal: it is not in the closing read.
    let untrimmed = table(&["a", "b"], &["a", "b"]);
    let error = check(&changes, &untrimmed, &table(&["b", "c"], &["b"]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("app.moods"), "{error}");
    // No rename: a deletion is refused as ever.
    let none = ChangeSet::default();
    let same = table(&["a", "b"], &["b"]);
    check(&none, &same, &table(&["a", "b"], &["b"])).unwrap();
    assert!(check(&none, &same, &table(&["a", "b"], &[])).is_err());
}
/// DEC-2346.2: the scopes the checksum is taken over hold the key a row has
/// before the plan's rename, on the side that pins and the side that
/// verifies; a plan that renames no label a key holds pins what it always
/// pinned.
#[test]
fn the_checksum_is_taken_over_the_keys_rows_hold_before_the_plans_rename() {
    let (ids, schema) = mood("y_000001");
    let scope = |keys: &[&str]| DataScope {
        mode: DataMode::Ensure,
        keys: keys.iter().map(|k| (*k).into()).collect(),
    };
    let name: TableName = "app.moods".parse().unwrap();
    // A takeover: the rows were never managed, so the recorded scopes
    // hold nothing, and the declaration spells the label it renames to.
    let recorded = DataScopes::new();
    let planned: DataScopes = [(name.clone(), scope(&["blue", "ok"]))].into();
    let pinned = |renames: &[(&str, &str, &str)]| {
        let plan = plan(&ids, renames);
        pinned_baseline_scopes(
            &recorded,
            &planned,
            &plan.changes,
            &plan.ids,
            (&schema, &ids),
        )
    };
    let keys = |scopes: &DataScopes| -> Vec<String> {
        scopes[&name]
            .keys
            .iter()
            .map(|k| k.as_str().to_owned())
            .collect()
    };
    assert_eq!(
        keys(&pinned(&[("y_000001", "sad", "blue")])),
        ["blue", "ok", "sad"]
    );
    // Negatives: no rename, and the rename of a label no declared key
    // holds, pin exactly what `pinned_scopes` did.
    assert_eq!(pinned(&[]), pinned_scopes(&recorded, &planned));
    assert_eq!(
        pinned(&[("y_000001", "gone", "other")]),
        pinned_scopes(&recorded, &planned)
    );
    // The rename of another enum's label does not widen it either.
    assert_eq!(
        pinned(&[("y_000009", "sad", "blue")]),
        pinned_scopes(&recorded, &planned)
    );
    // A recorded scope stays in the union, as it was.
    let recorded: DataScopes = [(name.clone(), scope(&["ok", "zed"]))].into();
    let widened = pinned_baseline_scopes(
        &recorded,
        &planned,
        &plan(&ids, &[("y_000001", "sad", "blue")]).changes,
        &ids,
        (&schema, &ids),
    );
    assert_eq!(keys(&widened), ["blue", "ok", "sad", "zed"]);
}
