//! Shared library input for the partition-default annotation regression.
use pbps_diff::order::{PlanningPlan, generated, propose};
use pbps_model::object_ref::ObjectDeps;
use pbps_model::operation::Snapshot;
use pbps_model::{
    Change, ChangeSet, Column, IdsFile, Module, ModuleKind, PartitionBy, PartitionColumn,
    PartitionOf, PartitionStrategy, PlannedChange, Schema, Table, TableName,
};
use pbps_pg::Postgres;

pub struct Scene {
    pub schema: Schema,
    before: Schema,
    base: IdsFile,
    pub ids: IdsFile,
    pub changes: ChangeSet,
    pub annotations: ObjectDeps,
    pub child: TableName,
}
impl Scene {
    pub fn new(depth: usize) -> Self {
        let by = || PartitionBy {
            strategy: PartitionStrategy::Range,
            columns: vec!["x".into()],
            external: false,
        };
        let root: TableName = "app.root".parse().unwrap();
        let child: TableName = "app.child".parse().unwrap();
        let mut schema = Schema::default();
        schema.tables.insert(
            root.clone(),
            Table {
                columns: [("x".into(), Column::new("integer".parse().unwrap()))]
                    .into_iter()
                    .collect(),
                partition_by: Some(by()),
                ..Default::default()
            },
        );
        let mut parent = root;
        for at in 0..depth {
            let name = if at + 1 == depth {
                child.clone()
            } else {
                TableName::new("app", format!("middle_{at}"))
            };
            schema.tables.insert(
                name.clone(),
                Table {
                    partition_of: Some(PartitionOf {
                        parent,
                        bound: pbps_model::PartitionBound::Default,
                        columns: if name == child {
                            [(
                                "x".into(),
                                PartitionColumn {
                                    default: Some("app.f()".into()),
                                    not_null: true,
                                },
                            )]
                            .into()
                        } else {
                            Default::default()
                        },
                    }),
                    partition_by: (at + 1 != depth).then(by),
                    ..Default::default()
                },
            );
            parent = name;
        }
        let module = "app.f()".parse().unwrap();
        schema.modules.insert(
            module,
            Module {
                kind: ModuleKind::Function,
                definition: "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN 7".into(),
                description: None,
                options: Default::default(),
            },
        );
        let ids = IdsFile {
            tables: schema
                .tables
                .keys()
                .map(|name| {
                    (
                        pbps_model::Uid::derived(
                            pbps_model::UidKind::Table,
                            &name.raw().to_string(),
                            0,
                        ),
                        name.clone(),
                    )
                })
                .collect(),
            columns: schema
                .tables
                .iter()
                .flat_map(|(name, table)| {
                    table.columns.keys().map(move |column| {
                        let column = name.column(column);
                        (
                            pbps_model::Uid::derived(
                                pbps_model::UidKind::Column,
                                &column.raw().to_string(),
                                0,
                            ),
                            column,
                        )
                    })
                })
                .collect(),
            ..Default::default()
        };
        // Deliberately put the provider last. The default needs a separate
        // installation boundary even though the relation must already exist.
        let mut tables = vec!["app.root".parse().unwrap()];
        tables.extend(
            (0..depth.saturating_sub(1)).map(|at| TableName::new("app", format!("middle_{at}"))),
        );
        tables.push(child.clone());
        let mut changes: Vec<_> = tables
            .into_iter()
            .map(|name| {
                PlannedChange::new(Change::CreateTable {
                    uid: ids.table_uid(&name).unwrap().clone(),
                    table: Box::new(schema.tables[&name].clone()),
                    name,
                    beside_default: None,
                    moves_at_most: None,
                })
            })
            .collect();
        changes.extend(schema.modules.iter().map(|(id, module)| {
            PlannedChange::new(Change::CreateModule {
                id: id.clone(),
                module: Box::new(module.clone()),
            })
        }));
        let annotations = ObjectDeps(
            [
                (
                    "default::app.child.x".parse().unwrap(),
                    ["module::app.f()".parse().unwrap()].into(),
                ),
                (
                    "module::app.f()".parse().unwrap(),
                    ["table::app.child".parse().unwrap()].into(),
                ),
            ]
            .into(),
        );
        Self {
            schema,
            before: Schema::default(),
            base: IdsFile::default(),
            ids,
            changes: ChangeSet { changes },
            annotations,
            child,
        }
    }
    pub fn with_root_default(mut self) -> Self {
        self.schema
            .tables
            .get_mut(&"app.root".parse().unwrap())
            .unwrap()
            .columns["x"]
            .value_source = pbps_model::ValueSource::Default("1".into());
        for planned in &mut self.changes.changes {
            if let Change::CreateTable { name, table, .. } = &mut planned.change {
                **table = self.schema.tables[name].clone();
            }
        }
        self
    }
    pub fn with_existing_ancestors(mut self) -> Self {
        self.before = self.schema.clone();
        self.before.tables.remove(&self.child);
        self.before.modules.clear();
        self.base = self.ids.clone();
        self.base.tables.retain(|_, name| name != &self.child);
        self.changes.changes.retain(|p| {
            !matches!(&p.change,
            Change::CreateTable { name, .. } if name != &self.child)
        });
        self
    }
    pub fn input(&self) -> Result<PlanningPlan, String> {
        propose(self.changes.clone()).with_annotations(
            Snapshot {
                schema: &self.before,
                ids: &self.base,
            },
            Snapshot {
                schema: &self.schema,
                ids: &self.ids,
            },
            &ObjectDeps::default(),
            &self.annotations,
        )
    }
    pub fn plan(&self, engine: &Postgres) -> Result<PlanningPlan, String> {
        let mut plan = self.input()?;
        generated::expand(&mut plan, &self.ids, &Default::default(), engine)?;
        generated::settle(&mut plan, &self.schema, &Default::default(), engine)?;
        Ok(plan)
    }
}
