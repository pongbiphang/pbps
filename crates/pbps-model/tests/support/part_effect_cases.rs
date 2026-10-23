//! One corpus for each dependency consumer's equivalence check. Kept outside
//! product APIs: test fixtures must not become another source of model facts.

use pbps_model::Change;
#[path = "rebuild.rs"]
mod rebuild;

pub fn cases() -> Vec<(String, Change)> {
    let values: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("part-effects.json")).unwrap();
    values
        .into_iter()
        .map(|value| {
            let name = value["name"].as_str().unwrap().to_owned();
            let change = serde_json::from_value(value["change"].clone())
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            (name, change)
        })
        .chain(
            [
                "RebuildPartitionedTable",
                "CopyTableRows",
                "AnalyzeTableTree",
            ]
            .into_iter()
            .map(str::to_owned)
            .zip(rebuild::changes()),
        )
        .chain(std::iter::once((
            "BuildPartitionedIndex".into(),
            Change::BuildPartitionedIndex(Box::new(rebuild::index_recipe())),
        )))
        .collect()
}
