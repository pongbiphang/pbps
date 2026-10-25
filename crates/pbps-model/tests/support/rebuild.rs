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
    // The environment holds the sources; the files, as approval left them,
    // hold the replacements, the retired sources and the lineage record.
    let base = root.intent.source.as_ids();
    let parent = base.index().table_uid(&root.table).unwrap().clone();
    let mut desired = root.intent.replacement.as_ids();
    for (uid, was) in root
        .intent
        .source
        .tables
        .iter()
        .map(|(u, n)| (u, n.raw().to_string()))
        .chain(
            root.intent
                .source
                .columns
                .iter()
                .map(|(u, c)| (u, c.raw().to_string())),
        )
    {
        desired.tombstones.insert(
            uid.clone(),
            Tombstone {
                was,
                dropped_at: root.intent.allowed_at.clone(),
                reason: root.intent.reason.as_ref().into(),
                operator: root.intent.operator.clone(),
            },
        );
    }
    desired.rebuilds.insert(parent, root.intent.clone());
    desired.validate().unwrap();
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
