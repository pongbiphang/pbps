use pbps_model::*;

pub fn changes() -> Vec<Change> {
    serde_json::from_str(include_str!("../fixtures/rebuild_operations.json")).unwrap()
}

#[allow(dead_code)] // Shared by model, order and emitter tests with different seams.
pub fn approval() -> (
    RebuildPartitionedTable,
    IdsFile,
    IdsFile,
    Vec<IntermediateAddress>,
) {
    let Change::RebuildPartitionedTable(root) = changes().remove(0) else {
        panic!("root fixture")
    };
    let mut base = root.intent.source.as_ids();
    let parent = base.index().table_uid(&root.table).unwrap().clone();
    base.rebuilds.insert(parent, root.intent.clone());
    base.validate().unwrap();
    let desired = root.intent.replacement.as_ids();
    let addresses = root
        .intent
        .replacement
        .tables
        .iter()
        .map(|(uid, name)| IntermediateAddress {
            uid: uid.clone(),
            address: TableName::new("app", format!("tmp_{}", name.name)),
        })
        .chain(
            root.intent
                .source
                .tables
                .iter()
                .map(|(uid, name)| IntermediateAddress {
                    uid: uid.clone(),
                    address: TableName::new("app", format!("old_{}", name.name)),
                }),
        )
        .collect();
    (*root, base, desired, addresses)
}

#[allow(dead_code)]
pub fn index_recipe() -> BuildPartitionedIndex {
    let Change::CopyTableRows(copy) = changes().remove(1) else {
        panic!()
    };
    BuildPartitionedIndex {
        parent: copy.target,
        name: "p_idx".into(),
        definition: PartitionIndexDefinition::Index(
            serde_json::from_str(r#"{"columns":[{"name":"k"}]}"#).unwrap(),
        ),
        clones: copy
            .target_partitions
            .into_iter()
            .map(|leaf| PartitionIndexClone {
                leaf,
                name: "p1_k_idx".into(),
            })
            .collect(),
    }
}
