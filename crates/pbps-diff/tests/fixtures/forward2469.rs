//! Engine-valid forward references with typed ordinary-column demands.
#![allow(dead_code)]
use pbps_dialect::Dialect;
use pbps_diff::{
    Side,
    order::{
        CanonicalPlan,
        deployment::{
            Sources,
            schedule::{Declarations, Facts, diff_annotated},
        },
    },
};
use pbps_model::{object_ref::ObjectDeps, *};

pub struct Scene {
    pub schema: Schema,
    pub ids: IdsFile,
    pub deps: ObjectDeps,
    pub columns: Vec<&'static str>,
}
impl Scene {
    pub fn new(pairs: usize, engine: &str, validators: bool) -> Self {
        let name: TableName = "app.t".parse().unwrap();
        let mut table = Table::default();
        let mut ids = IdsFile {
            tables: [("t_aaaaaa".parse().unwrap(), name.clone())].into(),
            ..Default::default()
        };
        let mut columns = Vec::new();
        if engine == "mssql" {
            table
                .columns
                .insert("anchor".into(), Column::new("integer".parse().unwrap()));
            ids.columns
                .insert("c_eeeeee".parse().unwrap(), name.column("anchor"));
            columns.push("anchor");
        }
        for (at, (generated, input, value)) in [("a", "b", 7), ("c", "d", 11)]
            .into_iter()
            .take(pairs)
            .enumerate()
        {
            let mut plain = Column::new("integer".parse().unwrap());
            plain.nullable = false;
            plain.value_source = ValueSource::Default(value.to_string());
            if engine == "postgres" {
                let mut spec = Column::new("integer".parse().unwrap());
                spec.value_source = ValueSource::Generated(Generated {
                    expression: format!("{input} + 1"),
                    stored: true,
                });
                table.columns.insert(generated.into(), spec);
                ids.columns.insert(
                    if at == 0 { "c_aaaaaa" } else { "c_cccccc" }
                        .parse()
                        .unwrap(),
                    name.column(generated),
                );
                columns.push(generated);
            } else {
                table.computed.insert(
                    generated.into(),
                    ComputedColumn {
                        expression: format!("{input} + 1"),
                        persisted: false,
                        not_null: false,
                    },
                );
            }
            table.columns.insert(input.into(), plain);
            ids.columns.insert(
                if at == 0 { "c_bbbbbb" } else { "c_dddddd" }
                    .parse()
                    .unwrap(),
                name.column(input),
            );
            columns.push(input);
        }
        if engine == "mssql" {
            columns.push("a");
            if pairs == 2 {
                columns.push("c");
            }
        }
        let mut schema = Schema::default();
        if validators {
            table.primary_key = Some(
                serde_json::from_value(serde_json::json!({"name":"pk_t","columns":["b"]})).unwrap(),
            );
            table.unique.insert(
                "uq_t".into(),
                serde_json::from_value(serde_json::json!({"columns":["b"]})).unwrap(),
            );
            table.checks.insert(
                "ck_t".into(),
                CheckConstraint {
                    expression: "b > 0".into(),
                },
            );
            table.indexes.insert("ix_t".into(), serde_json::from_value(serde_json::json!({"columns":[{"name":"b","descending":false}],"filter":"b > 0"})).unwrap());
            table.foreign_keys.insert("fk_t".into(), serde_json::from_value(serde_json::json!({"columns":["b"],"references_table":"app.p","references_columns":["k"]})).unwrap());
            schema.tables.insert("app.p".parse().unwrap(), serde_json::from_value(serde_json::json!({"columns":{"k":{"type":"integer","nullable":false}},"primary_key":{"name":"pk_p","columns":["k"]}})).unwrap());
            ids.tables
                .insert("t_bbbbbb".parse().unwrap(), "app.p".parse().unwrap());
            ids.columns
                .insert("c_ffffff".parse().unwrap(), "app.p.k".parse().unwrap());
        }
        schema.tables.insert(name, table);
        for (id, value) in [("app.f", 7), ("app.g", 11)].into_iter().take(pairs) {
            schema.modules.insert(
                id.parse().unwrap(),
                Module {
                    kind: ModuleKind::Function,
                    definition: if engine == "postgres" {
                        format!("() RETURNS integer LANGUAGE sql IMMUTABLE RETURN {value}")
                    } else {
                        format!("() RETURNS integer AS BEGIN RETURN {value}; END")
                    },
                    description: None,
                    options: Default::default(),
                },
            );
        }
        let mut deps = ObjectDeps(vec![
            (
                "column::app.t.b".parse().unwrap(),
                ["app.f".parse().unwrap()].into(),
            ),
            (
                "app.f".parse().unwrap(),
                ["table::app.t".parse().unwrap()].into(),
            ),
        ]);
        if pairs == 2 {
            deps.0.extend([
                (
                    "column::app.t.d".parse().unwrap(),
                    ["app.g".parse().unwrap()].into(),
                ),
                (
                    "app.g".parse().unwrap(),
                    ["column::app.t.b".parse().unwrap()].into(),
                ),
            ]);
        }
        Self {
            schema,
            ids,
            deps,
            columns,
        }
    }
    pub fn compile<E: Dialect>(&self, engine: &E) -> Result<CanonicalPlan<E>, String> {
        let empty = Schema::default();
        let ids = IdsFile::default();
        let before = Side {
            schema: &empty,
            ids: &ids,
        };
        diff_annotated(
            before,
            &Declarations {
                schema: &self.schema,
                sources: Sources {
                    before,
                    after: &self.ids,
                },
                module_deps: &Default::default(),
            },
            &Default::default(),
            &ObjectDeps::default(),
            &self.deps,
            &Facts::default(),
            engine,
        )
        .map(|p| p.changes)
    }
}
