use crate::engine_effects::{facts, symbolic};
use pbps_model::operation::symbolic::*;
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
use pbps_model::plan_names::PlanNames;
use pbps_model::{Change, ValueSource};
use pbps_model::{Column, IdsFile, PlannedChange, Schema, Table, TableName};
use serde_json::{Value as Json, json};

pub(in crate::engine_effects) fn change(kind: &str, fields: Json) -> Change {
    let mut row = json!({"op":kind, "uid":"c_aaaaaa", "column":"app.p.d"});
    if matches!(kind, "add_column" | "rename_column" | "rename_table") {
        row.as_object_mut().unwrap().remove("column");
    }
    for (k, v) in fields.as_object().unwrap() {
        row[k] = v.clone();
    }
    serde_json::from_value(row).unwrap()
}
pub(in crate::engine_effects) fn fixture(
    change: &Change,
    version: Option<i64>,
    edit: impl FnOnce(&mut Facts),
) -> Vec<Clause> {
    let p: TableName = "app.p".parse().unwrap();
    let mut schema = Schema::default();
    let mut table = Table::default();
    let mut d = Column::new("integer".parse().unwrap());
    d.value_source = ValueSource::Default("app.f(1)".into());
    table.columns.insert("d".into(), d);
    schema.tables.insert(p.clone(), table);
    let mut ids = IdsFile::default();
    ids.tables.insert("t_aaaaaa".parse().unwrap(), p.clone());
    ids.columns
        .insert("c_aaaaaa".parse().unwrap(), p.column("d"));
    let side = Snapshot {
        schema: &schema,
        ids: &ids,
    };
    let parent = side.table_identity(&p).unwrap();
    let ck = side.column_identity(&p.column("d")).unwrap();
    let mut facts = Facts::default();
    facts.parents.insert(parent.clone());
    for (a, value) in facts::column(
        &crate::Postgres::new(),
        &p,
        &"d".into(),
        &schema.tables[&p].columns["d"],
    ) {
        facts.base.cells.insert(
            (parent.clone(), Property::Column(ck.clone(), a)),
            Cell {
                value,
                version: Version::Base,
                origin: None,
            },
        );
    }
    facts.base.cells.insert(
        (parent, Property::Address),
        Cell {
            value: facts::definition("app.p"),
            version: Version::Base,
            origin: None,
        },
    );
    facts.goal = facts.base.clone();
    edit(&mut facts);
    let owners = PlanOwners::new(PlanNames::of(&[], &ids).unwrap(), &ids, &ids);
    let ops = Operations::of_plan(&[PlannedChange::new(change.clone())], ModuleEdits::Rebuilt);
    let mut pg = crate::Postgres::new();
    if let Some(v) = version {
        pg = pg.with_server_version(v);
    }
    symbolic(&pg, &ops.as_slice()[0], &owners, side, side, &facts)
        .unwrap()
        .symbolic
}
pub(in crate::engine_effects) fn rows(clauses: &[Clause]) -> String {
    clauses
        .iter()
        .map(|c| match c {
            Clause::Recurse(w) => {
                let a = match &w.key.property {
                    Property::Column(_, a) => format!("{a:?}"),
                    Property::Address => "Address".into(),
                    Property::Part(_) | Property::Rows => panic!(),
                };
                format!("{a}/{:?}/{:?}/{:?}", w.lifetime, w.scope, w.policy)
            }
            other @ (Clause::Move(_) | Clause::Restore(_) | Clause::Require(_)) => {
                panic!("{other:?}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
