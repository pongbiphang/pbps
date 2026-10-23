//! Shared rebuild member lifecycle cases, absent from production builds.

use pbps_model::{
    BoundDatum, Column, PartitionBound, PartitionBy, PartitionOf, PartitionStrategy, Schema, Table,
    TableName,
};

#[derive(Clone, Copy, Debug)]
enum Members {
    Keep,
    Add,
    Remove,
    Replace,
}

#[derive(Clone, Copy, Debug)]
enum Key {
    Switch,
    Widen,
    Add,
    Remove,
    Replace,
    Unchanged,
}

pub struct Case {
    pub name: String,
    pub desired: Schema,
    pub refusal: Option<&'static str>,
}

pub fn root() -> TableName {
    TableName::new("app", "p")
}

fn column() -> Column {
    Column::new("integer".parse().unwrap())
}

fn partition(from: &str, to: &str) -> Table {
    Table {
        partition_of: Some(PartitionOf {
            parent: root(),
            bound: PartitionBound::Range {
                from: vec![BoundDatum::Value(from.into())],
                to: vec![BoundDatum::Value(to.into())],
            },
            columns: Default::default(),
        }),
        ..Table::default()
    }
}

pub fn source() -> Schema {
    let mut schema = Schema::default();
    schema.tables.insert(
        root(),
        Table {
            columns: ["k", "bucket", "payload"]
                .into_iter()
                .map(|name| (name.into(), column()))
                .collect(),
            partition_by: Some(PartitionBy {
                columns: vec!["k".into()],
                strategy: PartitionStrategy::Range,
                external: false,
            }),
            ..Table::default()
        },
    );
    schema
        .tables
        .insert(TableName::new("app", "p1"), partition("0", "10"));
    schema
}

pub fn cases() -> Vec<Case> {
    let members = [
        Members::Keep,
        Members::Add,
        Members::Remove,
        Members::Replace,
    ];
    let keys = [
        Key::Switch,
        Key::Widen,
        Key::Add,
        Key::Remove,
        Key::Replace,
        Key::Unchanged,
    ];
    let mut cases = Vec::new();
    // The full product includes all single changes and all pairs, including
    // add + remove of the same kind (replacement). Triples catch interactions
    // that a list of independent examples would leave to another review.
    for non_key in members {
        for key in keys {
            for leaf in members {
                let mut desired = source();
                let parent = desired.tables.get_mut(&root()).unwrap();
                if matches!(non_key, Members::Remove | Members::Replace) {
                    parent.columns.shift_remove("payload");
                }
                if matches!(non_key, Members::Add | Members::Replace) {
                    parent.columns.insert("extra".into(), column());
                }
                match key {
                    Key::Switch => {
                        parent.partition_by.as_mut().unwrap().columns = vec!["bucket".into()];
                    }
                    Key::Widen => {
                        parent.columns.get_mut("k").unwrap().ty = "bigint".parse().unwrap();
                    }
                    Key::Add | Key::Replace => {
                        parent.columns.insert("next_key".into(), column());
                        parent.partition_by.as_mut().unwrap().columns = vec!["next_key".into()];
                        if matches!(key, Key::Replace) {
                            parent.columns.shift_remove("k");
                        }
                    }
                    Key::Remove => {
                        parent.columns.shift_remove("k");
                        parent.partition_by.as_mut().unwrap().columns = vec!["bucket".into()];
                    }
                    Key::Unchanged => {}
                }
                if matches!(leaf, Members::Remove | Members::Replace) {
                    desired.tables.remove(&TableName::new("app", "p1"));
                }
                if matches!(leaf, Members::Add | Members::Replace) {
                    desired
                        .tables
                        .insert(TableName::new("app", "p2"), partition("10", "20"));
                }
                cases.push(Case {
                    name: format!("column={non_key:?}, key={key:?}, partition={leaf:?}"),
                    desired,
                    refusal: matches!(key, Key::Unchanged)
                        .then_some("neither the partition key nor a key column type changed"),
                });
            }
        }
    }
    cases
}
