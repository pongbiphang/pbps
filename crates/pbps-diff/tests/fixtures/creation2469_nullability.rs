//! Nullability blockers share the ordinary canonical declaration route.
#![allow(dead_code)]
use super::fixture::Fixture;
use pbps_model::ColumnChange;
use pbps_model::ConstraintChange;
use pbps_model::IndexChange;
use pbps_model::ModuleChange;
use pbps_model::TableChange;
use pbps_model::{Change, ChangeSet, Table};

pub const BLOCKERS: &[&str] = &[
    "unique",
    "index",
    "unique_index",
    "include",
    "filter",
    "primary_key",
];

pub fn fixture(kind: &str, engine: &str) -> Fixture {
    let mut f = Fixture::new("not_null", engine);
    let table = f.schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
    match kind {
        "unique" | "unrelated" => {
            table.unique.insert(
                "uq_nn".into(),
                serde_json::from_value(serde_json::json!({
                    "columns": [if kind == "unique" { "x" } else { "anchor" }]
                }))
                .unwrap(),
            );
        }
        "primary_key" => {
            table.primary_key = Some(
                serde_json::from_value(serde_json::json!({"name":"pk_nn", "columns":["x"]}))
                    .unwrap(),
            )
        }
        "check" => {
            table.checks.insert(
                "ck_nn".into(),
                pbps_model::CheckConstraint {
                    expression: "x >= 0".into(),
                },
            );
        }
        "default" => {
            table.columns.get_mut("x").unwrap().value_source =
                pbps_model::ValueSource::Default("7".into())
        }
        "index" | "unique_index" | "include" | "filter" => {
            let mut index = serde_json::json!({"columns":[{"name":if matches!(kind, "include" | "filter") {"anchor"} else {"x"},"descending":false}],"unique":kind=="unique_index"});
            if kind == "include" {
                index["include"] = serde_json::json!(["x"]);
            }
            if kind == "filter" {
                index["filter"] = serde_json::json!("x IS NOT NULL");
            }
            table
                .indexes
                .insert("ix_nn".into(), serde_json::from_value(index).unwrap());
        }
        _ => panic!("unknown nullability fixture: {kind}"),
    }
    f
}

pub fn prefix(plan: &ChangeSet) -> &Table {
    plan.changes
        .iter()
        .find_map(|p| {
            if let Change::Table(TableChange::CreateTable { name, table, .. }) = &p.change
                && name == &"app.t".parse().unwrap()
            {
                Some(table.as_ref())
            } else {
                None
            }
        })
        .unwrap()
}

#[allow(clippy::wildcard_enum_match_arm)]
pub fn assert_order(plan: &ChangeSet, kind: &str, engine: &str) {
    let changes = &plan.changes;
    let create = changes.iter().position(|p| matches!(&p.change, Change::Table(TableChange::CreateTable {name,..}) if name == &"app.t".parse().unwrap())).unwrap();
    let function = changes
        .iter()
        .position(|p| matches!(p.change, Change::Module(ModuleChange::CreateModule { .. })))
        .unwrap();
    let nn = changes
        .iter()
        .position(|p| {
            matches!(
                p.change,
                Change::Column(ColumnChange::AlterColumnNullability {
                    to_nullable: false,
                    ..
                })
            )
        })
        .unwrap();
    assert!(create < function && function < nn, "{engine} {kind}");
    if engine == "mssql" || kind == "primary_key" {
        let install = changes
            .iter()
            .position(|p| match &p.change {
                Change::Constraint(ConstraintChange::AddUnique { name, .. }) => {
                    name.raw() == "uq_nn"
                }
                Change::Index(IndexChange::AddIndex { name, .. }) => name.raw() == "ix_nn",
                Change::Constraint(ConstraintChange::SetPrimaryKey { table, .. }) => {
                    table == &"app.t".parse().unwrap()
                }
                _ => false,
            })
            .unwrap_or_else(|| panic!("{engine} {kind}: blocker must be extracted"));
        assert!(nn < install, "{engine} {kind}: {nn} !< {install}");
    } else {
        let prefix = prefix(plan);
        assert!(
            prefix.unique.contains_key("uq_nn") || prefix.indexes.contains_key("ix_nn"),
            "PostgreSQL keeps its existing shape: {kind}"
        );
        assert!(!changes.iter().any(|p| matches!(
            p.change,
            Change::Constraint(ConstraintChange::AddUnique { .. })
                | Change::Index(IndexChange::AddIndex { .. })
        )));
    }
}
