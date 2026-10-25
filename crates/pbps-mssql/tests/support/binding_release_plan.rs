//! A binding release beside a computed-column rebuild owned by a later stage.
use pbps_model::{
    Change, ChangeSet, Column, ComputedColumn, IdsFile, Module, ModuleId, ModuleKind,
    PlannedChange, Schema, Table, TableName, Uid, UidKind,
};

pub fn combined(callee: &str) -> (Schema, Schema, IdsFile, ChangeSet) {
    let id = |name: &str| ModuleId::Named(TableName::new("app", name));
    let function = |bound: bool, body: &str| Module {
        kind: ModuleKind::Function,
        definition: format!(
            "(@x int) RETURNS int {} AS BEGIN RETURN {body} END",
            if bound { "WITH SCHEMABINDING" } else { "" }
        ),
        description: None,
        options: Default::default(),
    };
    let mut before = Schema::default();
    for (name, body) in [("f", "@x"), ("g", "app.f(@x)"), ("z", "@x")] {
        before.modules.insert(id(name), function(true, body));
    }
    let table = TableName::new("app", "t");
    let old = ComputedColumn {
        expression: format!("app.{callee}(id)"),
        persisted: false,
        not_null: false,
    };
    before.tables.insert(
        table.clone(),
        Table {
            columns: [("id".into(), Column::new("int".parse().unwrap()))].into(),
            computed: [("c".into(), old.clone())].into(),
            ..Default::default()
        },
    );
    let mut after = before.clone();
    after.modules.insert(id("f"), function(true, "@x + 1"));
    after.modules.insert(id("g"), function(false, "app.f(@x)"));
    after.modules.insert(id("z"), function(true, "@x + 2"));
    let new = ComputedColumn {
        persisted: true,
        ..old.clone()
    };
    after
        .tables
        .get_mut(&table)
        .unwrap()
        .computed
        .insert("c".into(), new.clone());
    let mut ids = IdsFile::default();
    ids.tables
        .insert(Uid::derived(UidKind::Table, "t", 0), table.clone());
    ids.columns
        .insert(Uid::derived(UidKind::Column, "t.id", 0), table.column("id"));
    let mut changes: Vec<_> = ["f", "g", "z"]
        .into_iter()
        .map(|name| {
            PlannedChange::new(Change::AlterModule {
                id: id(name),
                module: Box::new(after.modules[&id(name)].clone()),
            })
        })
        .collect();
    changes.extend([
        PlannedChange::new(Change::DropComputedColumn {
            table: table.clone(),
            name: "c".into(),
            computed: old,
        }),
        PlannedChange::new(Change::AddComputedColumn {
            table,
            name: "c".into(),
            computed: new,
        }),
    ]);
    (before, after, ids, ChangeSet { changes })
}
