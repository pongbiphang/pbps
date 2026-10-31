//! Typed domain facets share the ordinary diff/canonical/scratch route.
#![allow(dead_code)]
use pbps_dialect::Dialect;
use pbps_diff::{
    Side,
    order::{
        CanonicalPlan, DiffPlan,
        deployment::{
            Sources,
            schedule::{Declarations, Facts, diff_annotated},
        },
    },
};
use pbps_model::{object_ref::ObjectDeps, *};

#[derive(Clone)]
pub struct Fixture {
    pub schema: Schema,
    pub ids: IdsFile,
    pub deps: ObjectDeps,
}
pub fn domain(default: Option<&str>, not_null: bool, checks: &[(&str, &str)]) -> ManagedType {
    ManagedType::Domain {
        base: "integer".parse().unwrap(),
        default: default.map(str::to_owned),
        not_null,
        checks: checks
            .iter()
            .map(|(n, t)| ((*n).into(), (*t).into()))
            .collect(),
        collation: None,
    }
}
pub fn deps(rows: &[(&str, &[&str])]) -> ObjectDeps {
    ObjectDeps(
        rows.iter()
            .map(|(owner, providers)| {
                (
                    owner.parse().unwrap(),
                    providers.iter().map(|p| p.parse().unwrap()).collect(),
                )
            })
            .collect(),
    )
}
pub fn function(definition: &str) -> Module {
    Module {
        kind: ModuleKind::Function,
        definition: definition.into(),
        description: None,
        options: Default::default(),
    }
}
impl Fixture {
    pub fn new(facet: &str, call: bool) -> Self {
        let name: TypeRef = "c2469.d".parse().unwrap();
        let definition = match facet {
            "default" => domain(Some("7"), false, &[]),
            "not_null" => domain(None, true, &[]),
            "check" => domain(
                None,
                false,
                &[(
                    "positive",
                    if call {
                        "c2469.accept(VALUE)"
                    } else {
                        "VALUE > 0"
                    },
                )],
            ),
            _ => panic!("unknown facet"),
        };
        let owner = match facet {
            "check" => "check::type::c2469.d.positive",
            "default" => "default::type::c2469.d",
            _ => "not_null::type::c2469.d",
        };
        Self {
            schema: Schema { types: [(name.clone(), definition)].into(), modules: [("c2469.accept(integer)".parse().unwrap(), function("(v integer) RETURNS boolean LANGUAGE sql IMMUTABLE RETURN 'c2469.d'::regtype IS NOT NULL AND v > 0"))].into(), ..Default::default() },
            ids: IdsFile { types: [("y_aaaaaa".parse().unwrap(), name)].into(), ..Default::default() },
            deps: deps(&[(owner, &["module::c2469.accept(integer)"]), ("module::c2469.accept(integer)", &["type::c2469.d"])]),
        }
    }
    pub fn multiple() -> Self {
        let mut f = Self::new("check", false);
        f.schema.types.insert(
            "c2469.d".parse().unwrap(),
            domain(
                Some(" 7 "),
                true,
                &[("a", " VALUE > 0 "), ("z", " VALUE < 100 ")],
            ),
        );
        for id in ["c2469.ready", "c2469.nn_ready"] {
            f.schema.modules.insert(
                id.parse().unwrap(),
                function("() RETURNS boolean LANGUAGE sql IMMUTABLE RETURN TRUE"),
            );
        }
        f.deps = deps(&[
            ("default::type::c2469.d", &["module::c2469.ready"]),
            ("check::type::c2469.d.a", &["module::c2469.accept(integer)"]),
            ("module::c2469.accept(integer)", &["check::type::c2469.d.z"]),
            ("check::type::c2469.d.z", &["module::c2469.ready"]),
            ("module::c2469.ready", &["type::c2469.d"]),
            ("not_null::type::c2469.d", &["module::c2469.nn_ready"]),
            ("module::c2469.nn_ready", &["default::type::c2469.d"]),
        ]);
        f
    }
    pub fn bootstrap<E: Dialect>(&self, engine: &E) -> Result<DiffPlan, String> {
        let empty = Schema::default();
        let ids = IdsFile::default();
        let before = Side {
            schema: &empty,
            ids: &ids,
        };
        pbps_diff::diff(
            before,
            Side {
                schema: &self.schema,
                ids: &self.ids,
            },
            engine,
            &Default::default(),
        )
        .map_err(|e| format!("{e:?}"))?
        .with_annotations(
            operation::Snapshot {
                schema: &empty,
                ids: &ids,
            },
            operation::Snapshot {
                schema: &self.schema,
                ids: &self.ids,
            },
            &ObjectDeps::default(),
            &self.deps,
            if engine.rebuilds_modules() {
                operation::ModuleEdits::Rebuilt
            } else {
                operation::ModuleEdits::InPlace
            },
        )
    }
    pub fn compile<E: Dialect>(&self, engine: &E) -> Result<CanonicalPlan<E>, String> {
        self.compile_from(
            &Schema::default(),
            &IdsFile::default(),
            &ObjectDeps::default(),
            &self.deps,
            engine,
        )
    }
    pub fn compile_from<E: Dialect>(
        &self,
        base: &Schema,
        ids: &IdsFile,
        baseline: &ObjectDeps,
        goal: &ObjectDeps,
        engine: &E,
    ) -> Result<CanonicalPlan<E>, String> {
        let before = Side { schema: base, ids };
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
            baseline,
            goal,
            &Facts::default(),
            engine,
        )
        .map(|s| s.changes)
    }
}

impl Fixture {
    pub fn column_consumer(facet: &str, suffix: bool, array: bool) -> Self {
        let mut f = Self::new(facet, false);
        let name: TableName = "c2469.t".parse().unwrap();
        let mut table = Table::default();
        table
            .columns
            .insert("anchor".into(), Column::new("integer".parse().unwrap()));
        let ty = ColumnType::reference("c2469.d".parse().unwrap());
        table.columns.insert(
            "x".into(),
            Column::new(if array { ty.into_array() } else { ty }),
        );
        f.schema.tables.insert(name.clone(), table);
        f.ids
            .tables
            .insert("t_aaaaaa".parse().unwrap(), name.clone());
        f.ids
            .columns
            .insert("c_aaaaaa".parse().unwrap(), name.column("anchor"));
        f.ids
            .columns
            .insert("c_bbbbbb".parse().unwrap(), name.column("x"));
        f.deps.0[1].1 = ["column::c2469.t.x".parse().unwrap()].into();
        if suffix {
            f.schema.modules.insert(
                "c2469.column_ready".parse().unwrap(),
                function("() RETURNS boolean LANGUAGE sql IMMUTABLE RETURN TRUE"),
            );
            f.deps.0.push((
                "column::c2469.t.x".parse().unwrap(),
                ["module::c2469.column_ready".parse().unwrap()].into(),
            ));
        }
        f
    }
}

impl Fixture {
    pub fn indirect_check() -> Self {
        let mut f = Self::new("not_null", false);
        if let ManagedType::Domain { checks, .. } =
            f.schema.types.get_mut(&"c2469.d".parse().unwrap()).unwrap()
        {
            checks.insert("positive".into(), "c2469.accept(VALUE)".into());
        }
        f
    }
}

impl Fixture {
    pub fn split_defaults(explicit: bool) -> Self {
        let mut f = Self::new("default", false);
        let child: TypeRef = "c2469.child".parse().unwrap();
        let mut definition = domain(explicit.then_some("11"), false, &[]);
        if let ManagedType::Domain { base, .. } = &mut definition {
            *base = ColumnType::reference("c2469.d".parse().unwrap());
        }
        f.schema.types.insert(child.clone(), definition);
        f.ids.types.insert("y_bbbbbb".parse().unwrap(), child);
        f.schema.modules.get_mut(&"c2469.accept(integer)".parse().unwrap()).unwrap().definition = "(v integer) RETURNS boolean LANGUAGE sql IMMUTABLE RETURN 'c2469.child'::regtype IS NOT NULL AND v > 0".into();
        f.deps.0[1].1 = ["type::c2469.child".parse().unwrap()].into();
        if explicit {
            f.deps.0.push((
                "default::type::c2469.child".parse().unwrap(),
                ["module::c2469.accept(integer)".parse().unwrap()].into(),
            ));
        }
        f
    }
    pub fn split_shadowing_default() -> Self {
        let mut f = Self::split_defaults(true);
        let leaf: TypeRef = "c2469.leaf".parse().unwrap();
        let mut definition = domain(None, false, &[]);
        if let ManagedType::Domain { base, .. } = &mut definition {
            *base = ColumnType::reference("c2469.child".parse().unwrap());
        }
        f.schema.types.insert(leaf.clone(), definition);
        f.ids.types.insert("y_cccccc".parse().unwrap(), leaf);
        f.deps.0[1].1 = ["type::c2469.leaf".parse().unwrap()].into();
        f.deps.0[2].1 = ["type::c2469.child".parse().unwrap()].into();
        f
    }
}
