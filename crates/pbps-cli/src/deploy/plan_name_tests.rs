use super::*;

use pbps_model::{Change, PlannedChange, plan_names::PlanNames};

#[test]
fn catalog_parent_reads_never_include_a_rename_chains_temporary_name() {
    let (changes, ids) = crate::plan_fixture::chain();
    let index = PlanNames::of(&changes.changes, &ids).unwrap();
    let (_, parents) = object_reads(&changes, &index);
    assert_eq!(parents, [TableName::new("app", "original_table")]);
    assert!(!parents.contains(&TableName::new("app", "temporary_table")));
}

#[test]
fn a_referencing_key_is_removed_from_its_original_table_after_multiple_renames() {
    let (mut changes, mut ids) = crate::plan_fixture::chain();
    let parent = TableName::new("app", "parent");
    let leaf = TableName::new("app", "leaf");
    let leaf_uid: pbps_model::Uid = "t_bbbbbb".parse().unwrap();
    ids.tables.insert(leaf_uid.clone(), leaf.clone());
    changes
        .changes
        .push(PlannedChange::new(Change::DropForeignKey {
            table: TableName::new("app", "final_table"),
            name: "fk".into(),
        }));
    changes
        .changes
        .push(PlannedChange::new(Change::CreateTable {
            uid: leaf_uid,
            name: leaf,
            table: Box::new(pbps_model::Table {
                unlogged: true,
                partition_of: Some(pbps_model::PartitionOf {
                    parent: parent.clone(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..pbps_model::Table::default()
            }),
            beside_default: None,
            moves_at_most: None,
        }));
    let stored = TableName::new("app", "original_table");
    let found = pbps_pg::catalog::PermanentReferencer {
        parent,
        table: stored.clone(),
        key: "fk".into(),
        root_table: stored,
        root_key: "fk".into(),
    };
    let names = PlanNames::of(&changes.changes, &ids).unwrap();
    assert!(
        refuse_permanent_referencers(&changes, &names, std::slice::from_ref(&found), true).is_ok()
    );
    let another = pbps_pg::catalog::PermanentReferencer {
        key: "other".into(),
        root_key: "other".into(),
        ..found
    };
    assert!(refuse_permanent_referencers(&changes, &names, &[another], true).is_err());
}

#[test]
fn unlogged_parent_reads_use_the_holder_at_creation_not_the_final_spelling() {
    let (chain, mut ids) = crate::plan_fixture::chain();
    let leaf = TableName::new("app", "leaf");
    let leaf_uid: pbps_model::Uid = "t_bbbbbb".parse().unwrap();
    ids.tables.insert(leaf_uid.clone(), leaf.clone());
    let create_leaf = |parent: &str| {
        PlannedChange::new(Change::CreateTable {
            uid: leaf_uid.clone(),
            name: leaf.clone(),
            table: Box::new(pbps_model::Table {
                unlogged: true,
                partition_of: Some(pbps_model::PartitionOf {
                    parent: TableName::new("app", parent),
                    bound: pbps_model::PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..pbps_model::Table::default()
            }),
            beside_default: None,
            moves_at_most: None,
        })
    };
    let mut after_renames = chain.changes.clone();
    after_renames.push(create_leaf("final_table"));
    let mut before_renames = vec![create_leaf("original_table")];
    before_renames.extend(chain.changes);
    for changes in [&after_renames, &before_renames] {
        let names = PlanNames::of(changes, &ids).unwrap();
        assert_eq!(
            stored_unlogged_parents(&names),
            [TableName::new("app", "original_table")]
        );
    }
    let mut shape = pbps_model::Table::default();
    shape.columns.insert(
        "original_column".into(),
        pbps_model::Column::new("int".parse().unwrap()),
    );
    after_renames.insert(
        0,
        PlannedChange::new(Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: TableName::new("app", "original_table"),
            table: Box::new(shape),
            beside_default: None,
            moves_at_most: None,
        }),
    );
    let names = PlanNames::of(&after_renames, &ids).unwrap();
    assert!(
        stored_unlogged_parents(&names).is_empty(),
        "a new parent has no stored referencers"
    );
}
