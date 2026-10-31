//! Shared declaration-route fixtures; no YAML or unchecked emission.
#![allow(dead_code)]
use pbps_dialect::Dialect;
use pbps_diff::order::{
    CanonicalPlan,
    deployment::{
        Sources,
        schedule::{Declarations, Facts},
    },
};
use pbps_model::object_ref::ObjectDeps;
use pbps_model::*;

pub const FACETS: &[&str] = &[
    "column",
    "default",
    "not_null",
    "primary_key",
    "unique",
    "foreign_key",
    "check",
    "index",
    "generation",
];
#[derive(Clone)]
pub struct Fixture {
    pub schema: Schema,
    pub ids: IdsFile,
    pub deps: ObjectDeps,
    pub owner: String,
}

pub fn generated_facet(facet: &str, engine: &str) -> Fixture {
    let mut fixture = Fixture::new("generation", engine);
    fixture.owner = format!("{facet}::app.t.g");
    fixture.deps.0[0].0 = fixture.owner.parse().unwrap();
    let table = fixture
        .schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap();
    table.columns.shift_remove("x");
    fixture.ids.columns.remove(&"c_bbbbbb".parse().unwrap());
    if engine == "mssql" {
        fixture
            .schema
            .modules
            .get_mut(&"app.f".parse().unwrap())
            .unwrap()
            .definition = "() RETURNS integer WITH SCHEMABINDING AS BEGIN RETURN 7; END".into();
        let computed = table.computed.get_mut("g").unwrap();
        computed.persisted = true;
        computed.not_null = facet == "not_null";
    } else {
        table.columns.get_mut("g").unwrap().nullable = facet != "not_null";
    }
    fixture
}

impl Fixture {
    pub fn new(facet: &str, engine: &str) -> Self {
        let t: TableName = "app.t".parse().unwrap();
        let mut shape: Table = serde_json::from_value(serde_json::json!({"columns": {"anchor": {"type": "integer", "nullable": false}, "x": {"type": "integer", "nullable": true}}})).unwrap();
        let mut ids = IdsFile {
            tables: [
                ("t_aaaaaa".parse().unwrap(), t.clone()),
                ("t_bbbbbb".parse().unwrap(), "app.p".parse().unwrap()),
            ]
            .into(),
            columns: [
                ("c_aaaaaa".parse().unwrap(), t.column("anchor")),
                ("c_bbbbbb".parse().unwrap(), t.column("x")),
                ("c_cccccc".parse().unwrap(), "app.p.anchor".parse().unwrap()),
            ]
            .into(),
            ..Default::default()
        };
        let owner = match facet {
            "column" => "column::app.t.x",
            "default" => { shape.columns["x"].value_source = ValueSource::Default("7".into()); "default::app.t.x" }
            "not_null" => { shape.columns["x"].nullable = false; "not_null::app.t.x" }
            "generation" => {
                if engine == "postgres" {
                    shape.columns.insert("g".into(), Column { value_source: ValueSource::Generated(Generated { expression: "7".into(), stored: true }), ..Column::new("integer".parse().unwrap()) });
                    ids.columns.insert("c_dddddd".parse().unwrap(), t.column("g"));
                } else { shape.computed.insert("g".into(), ComputedColumn { expression: "7".into(), persisted: false, not_null: false }); }
                "generation::app.t.g"
            }
            "primary_key" => { shape.primary_key = Some(serde_json::from_value(serde_json::json!({"name":"pk_t","columns":["anchor"]})).unwrap()); "primary_key::app.t" }
            "unique" => { shape.unique.insert("uq_t".into(), serde_json::from_value(serde_json::json!({"columns":["x"]})).unwrap()); "unique::app.t.uq_t" }
            "foreign_key" => { shape.foreign_keys.insert("fk_t".into(), serde_json::from_value(serde_json::json!({"columns":["x"],"references_table":"app.p","references_columns":["anchor"]})).unwrap()); "foreign_key::app.t.fk_t" }
            "check" => { shape.checks.insert("ck_t".into(), CheckConstraint { expression: "x >= 0".into() }); "check::app.t.ck_t" }
            "index" => { shape.indexes.insert("ix_t".into(), serde_json::from_value(serde_json::json!({"columns":[{"name":"x","descending":false}]})).unwrap()); "index::app.t.ix_t" }
            _ => panic!("unknown fixture facet"),
        }.to_owned();
        let parent: Table = serde_json::from_value(serde_json::json!({"columns":{"anchor":{"type":"integer","nullable":false}},"primary_key":{"name":"pk_p","columns":["anchor"]}})).unwrap();
        let schema = Schema {
            tables: [(t, shape), ("app.p".parse().unwrap(), parent)].into(),
            modules: [(
                "app.f".parse().unwrap(),
                Module {
                    kind: ModuleKind::Function,
                    definition: if engine == "postgres" {
                        "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN 7"
                    } else {
                        "() RETURNS integer AS BEGIN RETURN 7; END"
                    }
                    .into(),
                    description: None,
                    options: Default::default(),
                },
            )]
            .into(),
            ..Default::default()
        };
        let deps = ObjectDeps(vec![
            (owner.parse().unwrap(), ["app.f".parse().unwrap()].into()),
            (
                "app.f".parse().unwrap(),
                ["table::app.t".parse().unwrap()].into(),
            ),
        ]);
        Self {
            schema,
            ids,
            deps,
            owner,
        }
    }
    pub fn with_skipped_policy(mut self) -> Self {
        self.schema
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .row_policies
            .insert(
                "later".into(),
                RowPolicy {
                    using: Some("x >= 0".into()),
                    ..Default::default()
                },
            );
        self.schema.modules.insert(
            "app.after_policy".parse().unwrap(),
            Module {
                kind: ModuleKind::View,
                definition: "SELECT 7 AS x".into(),
                description: None,
                options: Default::default(),
            },
        );
        self.deps.0.extend([
            (
                "policy::app.t.later".parse().unwrap(),
                ["default::app.t.x".parse().unwrap()].into(),
            ),
            (
                "app.after_policy".parse().unwrap(),
                ["policy::app.t.later".parse().unwrap()].into(),
            ),
        ]);
        self
    }
    pub fn bootstrap<E: Dialect>(&self, engine: &E) -> Result<pbps_diff::order::DiffPlan, String> {
        let empty = Schema::default();
        let ids = IdsFile::default();
        pbps_diff::diff(
            pbps_diff::Side {
                schema: &empty,
                ids: &ids,
            },
            pbps_diff::Side {
                schema: &self.schema,
                ids: &self.ids,
            },
            engine,
            &Hints::default(),
        )
        .map_err(|e| format!("{e:?}"))?
        .with_annotations(
            pbps_model::operation::Snapshot {
                schema: &empty,
                ids: &ids,
            },
            pbps_model::operation::Snapshot {
                schema: &self.schema,
                ids: &self.ids,
            },
            &ObjectDeps::default(),
            &self.deps,
            if engine.rebuilds_modules() {
                pbps_model::operation::ModuleEdits::Rebuilt
            } else {
                pbps_model::operation::ModuleEdits::InPlace
            },
        )
    }
    pub fn compile<E: Dialect>(&self, engine: &E) -> Result<CanonicalPlan<E>, String> {
        self.compile_from(&Schema::default(), &IdsFile::default(), &self.deps, engine)
    }
    pub fn compile_from<E: Dialect>(
        &self,
        base: &Schema,
        base_ids: &IdsFile,
        deps: &ObjectDeps,
        engine: &E,
    ) -> Result<CanonicalPlan<E>, String> {
        let before = pbps_diff::Side {
            schema: base,
            ids: base_ids,
        };
        pbps_diff::order::deployment::schedule::diff_annotated(
            before,
            &Declarations {
                schema: &self.schema,
                sources: Sources {
                    before,
                    after: &self.ids,
                },
                module_deps: &Default::default(),
            },
            &Hints::default(),
            &ObjectDeps::default(),
            deps,
            &Facts::default(),
            engine,
        )
        .map(|s| s.changes)
    }
}
