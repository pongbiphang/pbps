use super::*;

#[test]
fn computed_column_and_not_null_demands_select_the_computed_suffix() {
    for facet in ["column", "not_null", "generation"] {
        let mut f = generated_facet(facet, "mssql");
        let table = f.schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
        let constant = table.computed["g"].clone();
        table.computed.insert("a_prefix".into(), constant.clone());
        table.computed.insert("z_suffix".into(), constant);
        let plan = f.compile(&pbps_mssql::Mssql).unwrap();
        let changes = &plan.changes().changes;
        let bare = changes.iter().position(|p| matches!(&p.change, Change::CreateTable { name, table, .. }
            if name == &"app.t".parse().unwrap() && table.computed.keys().map(|c| c.raw()).eq(["a_prefix"]))).unwrap();
        let function = changes
            .iter()
            .position(|p| matches!(&p.change, Change::CreateModule { .. }))
            .unwrap();
        let column = changes
            .iter()
            .position(|p| {
                matches!(&p.change, Change::AddComputedColumn { name, computed, .. }
            if name.raw() == "g" && computed.not_null == (facet == "not_null"))
            })
            .unwrap();
        let suffix = changes.iter().position(|p| matches!(&p.change, Change::AddComputedColumn { name, .. } if name.raw() == "z_suffix")).unwrap();
        assert!(
            bare < function && function < column && column < suffix,
            "{facet}"
        );
        assert!(
            !changes
                .iter()
                .any(|p| matches!(p.change, Change::AlterColumnNullability { .. }))
        );
        assert!(
            plan.annotation_edges()
                .unwrap()
                .iter()
                .all(|edge| edge.before < edge.after)
        );
    }
}

#[test]
#[allow(clippy::wildcard_enum_match_arm)]
fn generated_column_and_not_null_demands_keep_the_postgres_recipe() {
    for version in [160015, 180006] {
        let engine = pbps_pg::Postgres::new().with_server_version(version);
        for facet in ["column", "not_null"] {
            let f = generated_facet(facet, "postgres");
            let plan = f.compile(&engine).unwrap();
            let changes = &plan.changes().changes;
            let function = changes
                .iter()
                .position(|p| matches!(p.change, Change::CreateModule { .. }))
                .unwrap();
            let part = changes
                .iter()
                .position(|p| match &p.change {
                    Change::AddColumn { name, .. } => facet == "column" && name.raw() == "g",
                    Change::AlterColumnNullability {
                        column,
                        to_nullable: false,
                        ..
                    } => facet == "not_null" && column.name.raw() == "g",
                    _ => false,
                })
                .unwrap();
            assert!(function < part, "{version} {facet}");
            assert!(
                !changes
                    .iter()
                    .any(|p| matches!(p.change, Change::AlterColumnExpression { .. }))
            );
        }
    }
}

#[test]
fn a_computed_only_sql_server_create_names_the_missing_physical_column() {
    for facet in ["column", "not_null", "generation"] {
        let mut f = generated_facet(facet, "mssql");
        f.schema
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .clear();
        f.ids.columns.remove(&"c_aaaaaa".parse().unwrap());
        let why = f.compile(&pbps_mssql::Mssql).unwrap_err();
        assert!(why.contains("ordinary physical column"), "{facet}: {why}");
    }
}

#[test]
fn computed_columns_still_wait_for_their_actual_function_inputs() {
    let engine = pbps_mssql::Mssql;
    for facet in ["column", "not_null"] {
        let mut f = generated_facet(facet, "mssql");
        f.schema
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .computed
            .get_mut("g")
            .unwrap()
            .expression = "app.f()".into();
        let plan = f.compile(&engine).unwrap();
        let changes = &plan.changes().changes;
        let function = changes
            .iter()
            .position(|p| matches!(p.change, Change::CreateModule { .. }))
            .unwrap();
        let column = changes
            .iter()
            .position(|p| matches!(p.change, Change::AddComputedColumn { .. }))
            .unwrap();
        assert!(function < column);
        // An acyclic annotation cannot override the expression's actual input.
        f.deps.0.remove(0);
        f.deps.0[0].1 = [f.owner.parse().unwrap()].into();
        assert!(
            f.compile(&engine).is_err(),
            "the call still needs its function: {facet}"
        );
    }
}

#[test]
fn demanded_decorators_over_generation_keep_their_independent_boundaries() {
    fn check(engine: &impl Dialect) {
        for facet in ["default", "check", "unique", "primary_key", "index"] {
            let mut f = generated_facet("not_null", engine.name());
            let table = f.schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
            f.owner = match facet {
                "default" => {
                    table.columns.get_mut("anchor").unwrap().value_source = pbps_model::ValueSource::Default("7".into());
                    "default::app.t.anchor"
                }
                "check" => {
                    table.checks.insert("ck_g".into(), pbps_model::CheckConstraint { expression: "g >= 0".into() });
                    "check::app.t.ck_g"
                }
                "unique" => {
                    table.unique.insert("uq_g".into(), serde_json::from_value(serde_json::json!({"columns":["g"]})).unwrap());
                    "unique::app.t.uq_g"
                }
                "primary_key" => {
                    table.primary_key = Some(serde_json::from_value(serde_json::json!({"name":"pk_g","columns":["g"]})).unwrap());
                    "primary_key::app.t"
                }
                "index" => {
                    table.indexes.insert("ix_g".into(), serde_json::from_value(serde_json::json!({"columns":[{"name":"g","descending":false}],"include":["anchor"],"filter":"anchor >= 0"})).unwrap());
                    "index::app.t.ix_g"
                }
                _ => unreachable!(),
            }.into();
            f.deps.0[0].0 = f.owner.parse().unwrap();
            let errors = engine.validate_table(&"app.t".parse().unwrap(), table);
            if engine.name() == "mssql" && facet == "primary_key" {
                assert!(
                    errors
                        .iter()
                        .any(|error| error.to_string().contains("key over a computed column"))
                );
                continue;
            }
            assert!(errors.is_empty(), "{} {facet}: {errors:?}", engine.name());
            let plan = f
                .compile(engine)
                .unwrap_or_else(|why| panic!("{} {facet}: {why}", engine.name()));
            assert!(
                plan.annotation_edges()
                    .unwrap()
                    .iter()
                    .all(|edge| edge.before < edge.after)
            );
            // A decorator can wait for a function without moving the generation it reads.
            assert!(!plan.changes().changes.iter().any(|p| matches!(
                p.change,
                Change::AddComputedColumn { .. } | Change::AlterColumnExpression { .. }
            )));
        }
    }
    check(&pbps_mssql::Mssql);
    check(&pbps_pg::Postgres::new().with_server_version(160015));
    check(&pbps_pg::Postgres::new().with_server_version(180006));
}

#[test]
fn empty_computed_demand_preserves_changes_and_proof_bytes() {
    let engine = pbps_mssql::Mssql;
    for facet in ["column", "not_null"] {
        let mut f = generated_facet(facet, "mssql");
        f.deps = ObjectDeps::default();
        let empty = pbps_model::Schema::default();
        let empty_ids = pbps_model::IdsFile::default();
        let plain = pbps_diff::diff(
            pbps_diff::Side {
                schema: &empty,
                ids: &empty_ids,
            },
            pbps_diff::Side {
                schema: &f.schema,
                ids: &f.ids,
            },
            &engine,
            &Default::default(),
        )
        .unwrap();
        let mut a = pbps_diff::order::propose(plain.changes().clone());
        let mut b = a
            .clone()
            .with_annotations(
                pbps_model::operation::Snapshot {
                    schema: &empty,
                    ids: &empty_ids,
                },
                pbps_model::operation::Snapshot {
                    schema: &f.schema,
                    ids: &f.ids,
                },
                &ObjectDeps::default(),
                &ObjectDeps::default(),
                pbps_model::operation::ModuleEdits::InPlace,
            )
            .unwrap();
        pbps_diff::order::generated::expand(&mut a, &f.ids, &Default::default(), &engine).unwrap();
        pbps_diff::order::generated::expand(&mut b, &f.ids, &Default::default(), &engine).unwrap();
        assert_eq!(
            serde_json::to_vec(&(a.changes(), a.proof())).unwrap(),
            serde_json::to_vec(&(b.changes(), b.proof())).unwrap()
        );
    }
}
