//! A prefix facet unlocks the function needed to append the ordinary suffix.
#![allow(dead_code)]
pub use super::fixture::Fixture;
use pbps_model::{object_ref::ObjectDeps, *};
pub const FACETS: &[&str] = &[
    "default",
    "not_null",
    "primary_key",
    "unique",
    "check",
    "index",
    "foreign_key",
];
pub fn scene(facet: &str, engine: &str, suffix_input: bool) -> Fixture {
    let mut f = Fixture::new("column", engine);
    let table = f.schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
    let input = if suffix_input { "x" } else { "anchor" };
    let owner = match facet {
        "default" => {
            table.columns["anchor"].value_source = ValueSource::Default("7".into());
            "default::app.t.anchor"
        }
        "not_null" => "not_null::app.t.anchor",
        "primary_key" => {
            table.primary_key = Some(
                serde_json::from_value(serde_json::json!({"name":"pk_t", "columns":["anchor"]}))
                    .unwrap(),
            );
            "primary_key::app.t"
        }
        "unique" => {
            table.unique.insert(
                "uq_t".into(),
                serde_json::from_value(serde_json::json!({"columns":["anchor",input]})).unwrap(),
            );
            "unique::app.t.uq_t"
        }
        "check" => {
            table.checks.insert(
                "ck_t".into(),
                CheckConstraint {
                    expression: format!("{input} > 0"),
                },
            );
            "check::app.t.ck_t"
        }
        "index" => {
            table.indexes.insert("ix_t".into(), serde_json::from_value(serde_json::json!({"columns":[{"name":"anchor","descending":false}],"include":[input],"filter":format!("{input} > 0")})).unwrap());
            "index::app.t.ix_t"
        }
        "foreign_key" => {
            table.foreign_keys.insert("fk_t".into(), serde_json::from_value(serde_json::json!({"columns":[input],"references_table":"app.p","references_columns":["anchor"]})).unwrap());
            "foreign_key::app.t.fk_t"
        }
        _ => panic!("unknown facet"),
    };
    // Avoid duplicate key/include columns in the prefix-only controls.
    if !suffix_input {
        if let Some(unique) = table.unique.get_mut("uq_t") {
            unique.columns.pop();
        }
        if let Some(index) = table.indexes.get_mut("ix_t") {
            index.include.clear();
        }
    }
    f.deps = ObjectDeps(vec![
        (
            "column::app.t.x".parse().unwrap(),
            ["app.f".parse().unwrap()].into(),
        ),
        ("app.f".parse().unwrap(), [owner.parse().unwrap()].into()),
    ]);
    f.owner = owner.into();
    f
}
#[allow(clippy::wildcard_enum_match_arm)]
pub fn positions(changes: &ChangeSet, facet: &str) -> (usize, usize, usize) {
    let part = changes
        .changes
        .iter()
        .position(|p| match (&p.change, facet) {
            (Change::AlterColumnDefault { column, .. }, "default") => column.name.raw() == "anchor",
            (Change::AlterColumnNullability { column, .. }, "not_null") => {
                column.name.raw() == "anchor"
            }
            (Change::SetPrimaryKey { table, .. }, "primary_key") => {
                table == &"app.t".parse().unwrap()
            }
            (Change::AddUnique { .. }, "unique")
            | (Change::AddCheck { .. }, "check")
            | (Change::AddIndex { .. }, "index")
            | (Change::AddForeignKey { .. }, "foreign_key") => true,
            _ => false,
        })
        .unwrap();
    let function = changes
        .changes
        .iter()
        .position(|p| matches!(&p.change, Change::CreateModule { id, .. } if id.name() == "f"))
        .unwrap();
    let column = changes
        .changes
        .iter()
        .position(|p| matches!(&p.change, Change::AddColumn { name, .. } if name.raw() == "x"))
        .unwrap();
    (part, function, column)
}

pub fn generations() -> Fixture {
    let mut f = Fixture::new("column", "postgres");
    let name: TableName = "app.t".parse().unwrap();
    let table = f.schema.tables.get_mut(&name).unwrap();
    table.columns.clear();
    for (column, value) in [("a", "7"), ("c", "11")] {
        table.columns.insert(
            column.into(),
            Column {
                value_source: ValueSource::Generated(Generated {
                    expression: value.into(),
                    stored: true,
                }),
                ..Column::new("integer".parse().unwrap())
            },
        );
    }
    f.ids
        .columns
        .insert("c_aaaaaa".parse().unwrap(), name.column("a"));
    f.ids
        .columns
        .insert("c_bbbbbb".parse().unwrap(), name.column("c"));
    f.schema.modules.insert(
        "app.g".parse().unwrap(),
        Module {
            definition: "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN 11".into(),
            ..f.schema.modules[&"app.f".parse().unwrap()].clone()
        },
    );
    f.deps = ObjectDeps(vec![
        (
            "generation::app.t.c".parse().unwrap(),
            ["app.g".parse().unwrap()].into(),
        ),
        (
            "generation::app.t.a".parse().unwrap(),
            ["app.f".parse().unwrap()].into(),
        ),
        (
            "app.f".parse().unwrap(),
            ["generation::app.t.c".parse().unwrap()].into(),
        ),
    ]);
    f
}

pub fn generated_not_null() -> Fixture {
    let mut f = Fixture::new("generation", "postgres");
    f.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns["g"]
        .nullable = false;
    f.deps = ObjectDeps(vec![
        (
            "generation::app.t.g".parse().unwrap(),
            ["app.f".parse().unwrap()].into(),
        ),
        (
            "app.f".parse().unwrap(),
            ["not_null::app.t.g".parse().unwrap()].into(),
        ),
    ]);
    f
}
