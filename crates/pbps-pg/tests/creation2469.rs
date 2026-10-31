#[path = "../../pbps-diff/tests/fixtures/creation2469.rs"]
mod fixture;
use fixture::*;
use pbps_dialect::Dialect;
use pbps_model::{Change, object_ref::ObjectDeps};

fn pin<E: Dialect>(engine: E) {
    for facet in FACETS {
        let f = Fixture::new(facet, engine.name());
        let plan = f
            .compile(&engine)
            .unwrap_or_else(|e| panic!("{} {facet}: {e}", engine.name()));
        let provider = plan
            .changes()
            .changes
            .iter()
            .position(|p| matches!(&p.change, Change::CreateModule { .. }))
            .unwrap();
        let edges = plan.annotation_edges().unwrap();
        assert!(
            edges
                .iter()
                .any(|e| e.before == provider && e.after > provider),
            "{facet}: {edges:?}"
        );
        assert!(
            edges.iter().all(|e| e.before < e.after),
            "{facet}: {edges:?}"
        );
        let mut invalid = f.deps.clone();
        invalid.0.push((
            "table::app.t".parse().unwrap(),
            [f.owner.parse().unwrap()].into(),
        ));
        assert!(
            f.compile_from(&Default::default(), &Default::default(), &invalid, &engine)
                .is_err(),
            "{facet}"
        );
        let empty = pbps_model::Schema::default();
        let empty_ids = pbps_model::IdsFile::default();
        let input = pbps_diff::diff(
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
        let mut a = pbps_diff::order::propose(input.changes().clone());
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
                if engine.rebuilds_modules() {
                    pbps_model::operation::ModuleEdits::Rebuilt
                } else {
                    pbps_model::operation::ModuleEdits::InPlace
                },
            )
            .unwrap();
        pbps_diff::order::generated::expand(&mut a, &f.ids, &Default::default(), &engine).unwrap();
        pbps_diff::order::generated::expand(&mut b, &f.ids, &Default::default(), &engine).unwrap();
        assert_eq!(
            serde_json::to_vec(&(a.changes(), a.proof())).unwrap(),
            serde_json::to_vec(&(b.changes(), b.proof())).unwrap(),
            "empty demand must not rewrite {facet}"
        );
    }
}
#[test]
fn portable_facets_use_the_canonical_route_on_both_engines() {
    pin(pbps_pg::Postgres::new());
    pin(pbps_mssql::Mssql);
}
#[test]
fn pg16_names_the_placeholder_limit_and_pg18_exposes_only_the_final_generation() {
    let mut f = Fixture::new("generation", "postgres");
    f.schema
        .modules
        .get_mut(&"app.f".parse().unwrap())
        .unwrap()
        .definition =
        "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN (SELECT g FROM app.t LIMIT 1)".into();
    f.deps.0[1].1 = ["column::app.t.g".parse().unwrap()].into();
    let old = pbps_pg::Postgres::new().with_server_version(160015);
    let new = pbps_pg::Postgres::new().with_server_version(180006);
    let error = f.compile(&old).unwrap_err();
    assert!(error.contains("SET EXPRESSION"), "{error}");
    let plan = f.compile(&new).unwrap();
    let changes = &plan.changes().changes;
    let column=changes.iter().position(|p|matches!(p.change,Change::CreateTable { ref name, .. } if *name=="app.t".parse().unwrap())).unwrap();
    let provider = changes
        .iter()
        .position(|p| matches!(p.change, Change::CreateModule { .. }))
        .unwrap();
    let generation = changes
        .iter()
        .position(|p| matches!(p.change, Change::AlterColumnExpression { .. }))
        .unwrap();
    assert!(column < provider && provider < generation);
}

#[test]
fn standing_add_column_defaults_and_the_first_mssql_column_keep_legal_groups() {
    let f = Fixture::new("default", "postgres");
    let mut base = f.schema.clone();
    base.tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns
        .shift_remove("x");
    let mut base_ids = f.ids.clone();
    base_ids.columns.remove(&"c_bbbbbb".parse().unwrap());
    for pg in [true, false] {
        let changes = if pg {
            f.compile_from(&base, &base_ids, &f.deps, &pbps_pg::Postgres::new())
                .unwrap()
                .changes()
                .clone()
        } else {
            f.compile_from(&base, &base_ids, &f.deps, &pbps_mssql::Mssql)
                .unwrap()
                .changes()
                .clone()
        };
        assert!(changes.changes.iter().any(|p|matches!(&p.change, Change::AddColumn { column, .. } if matches!(column.value_source, pbps_model::ValueSource::Default(_)))));
        assert!(
            !changes
                .changes
                .iter()
                .any(|p| matches!(p.change, Change::AlterColumnDefault { .. }))
        );
    }
    let mut f = Fixture::new("column", "mssql");
    f.deps = ObjectDeps(vec![(
        "table::app.t".parse().unwrap(),
        ["column::app.t.anchor".parse().unwrap()].into(),
    )]);
    let error = f.compile(&pbps_mssql::Mssql).unwrap_err();
    assert!(error.contains("internal creation cycle"), "{error}");
}

#[test]
#[allow(clippy::wildcard_enum_match_arm)]
fn a_demanded_column_moves_its_entire_suffix_in_declaration_order() {
    fn check<E: Dialect>(engine: E) {
        let mut f = Fixture::new("column", engine.name());
        let t = "app.t".parse().unwrap();
        f.schema.tables.get_mut(&t).unwrap().columns.insert(
            "y".into(),
            pbps_model::Column::new("integer".parse().unwrap()),
        );
        f.schema.tables.get_mut(&t).unwrap().computed.insert(
            "z".into(),
            pbps_model::ComputedColumn {
                expression: "7".into(),
                persisted: false,
                not_null: false,
            },
        );
        // PostgreSQL models generation as a typed column, not SQL Server's computed map.
        if engine.name() == "postgres" {
            f.schema.tables.get_mut(&t).unwrap().computed.clear();
        }
        f.ids.columns.insert(
            "c_dddddd".parse().unwrap(),
            pbps_model::ColumnRef {
                table: t,
                name: "y".into(),
            },
        );
        let plan = f.compile(&engine).unwrap();
        let suffix: Vec<_> = plan
            .changes()
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::AddColumn { table, name, .. }
                    if table.schema == "app" && table.name == "t" =>
                {
                    Some(name.raw())
                }
                Change::AddComputedColumn { table, name, .. }
                    if table.schema == "app" && table.name == "t" =>
                {
                    Some(name.raw())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            suffix,
            if engine.name() == "postgres" {
                vec!["x", "y"]
            } else {
                vec!["x", "y", "z"]
            }
        );
    }
    check(pbps_pg::Postgres::new());
    check(pbps_mssql::Mssql);
}

#[test]
fn a_primary_key_waits_for_its_demanded_not_null_facet() {
    fn check<E: Dialect>(engine: E) {
        let mut f = Fixture::new("not_null", engine.name());
        f.schema
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .primary_key = Some(
            serde_json::from_value(serde_json::json!({"name":"pk_nn","columns":["x"]})).unwrap(),
        );
        let plan = f.compile(&engine).unwrap();
        let nn = plan
            .changes()
            .changes
            .iter()
            .position(|p| {
                matches!(
                    p.change,
                    Change::AlterColumnNullability {
                        to_nullable: false,
                        ..
                    }
                )
            })
            .unwrap();
        let key=plan.changes().changes.iter().position(|p|matches!(p.change,Change::SetPrimaryKey{ref table,..} if table.schema=="app" && table.name=="t")).unwrap();
        assert!(nn < key);
        f.deps.0[1].1 = ["primary_key::app.t".parse().unwrap()].into();
        assert!(f.compile(&engine).is_err());
    }
    check(pbps_pg::Postgres::new());
    check(pbps_mssql::Mssql);
}

#[test]
fn demanded_not_null_keeps_creation_risk_only_before_the_first_write() {
    fn check<E: Dialect>(engine: E) {
        let f = Fixture::new("not_null", engine.name());
        let plan = f.compile(&engine).unwrap();
        let mut changes = plan.changes().clone();
        let nn = changes
            .changes
            .iter()
            .position(|p| {
                matches!(
                    p.change,
                    Change::AlterColumnNullability {
                        to_nullable: false,
                        ..
                    }
                )
            })
            .unwrap();
        let risks = pbps_diff::order::generated::risks(&changes, &f.ids, &engine);
        assert!(!risks[nn].contains(&pbps_model::RiskClass::NotNull));
        assert_eq!(changes.changes[nn].risks, risks[nn]);
        let write = pbps_model::PlannedChange::new(Change::InsertRow {
            table: "app.t".parse().unwrap(),
            key_column: "anchor".into(),
            identity_key: false,
            key: "1".into(),
            row: Default::default(),
            defaults: Default::default(),
            types: Default::default(),
        });
        changes.changes.insert(nn, write);
        assert!(
            pbps_diff::order::generated::risks(&changes, &f.ids, &engine)[nn + 1]
                .contains(&pbps_model::RiskClass::NotNull)
        );
        let mut standing = plan.changes().clone();
        standing.changes.retain(|p|!matches!(&p.change,Change::CreateTable{name,..} if name.schema=="app" && name.name=="t"));
        let nn = standing
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::AlterColumnNullability { .. }))
            .unwrap();
        assert!(
            pbps_diff::order::generated::risks(&standing, &f.ids, &engine)[nn]
                .contains(&pbps_model::RiskClass::NotNull)
        );
    }
    check(pbps_pg::Postgres::new());
    check(pbps_mssql::Mssql);
}

#[test]
fn hard_column_prerequisites_do_not_depend_on_module_text() {
    let mut f = Fixture::new("generation", "postgres");
    f.deps.0[1].1 = ["column::app.t.g".parse().unwrap()].into();
    let pg = pbps_pg::Postgres::new().with_server_version(180006);
    let plan = f.compile(&pg).unwrap();
    assert!(
        plan.changes()
            .changes
            .iter()
            .any(|p| matches!(p.change, Change::AlterColumnExpression { .. }))
    );
    let old = f
        .compile(&pbps_pg::Postgres::new().with_server_version(160015))
        .unwrap_err();
    assert!(old.contains("SET EXPRESSION"), "{old}");
    let mut transitive = f.clone();
    let module = transitive.schema.modules[&"app.f".parse().unwrap()].clone();
    transitive
        .schema
        .modules
        .insert("app.h".parse().unwrap(), module);
    transitive.deps.0[1].1 = ["app.h".parse().unwrap()].into();
    transitive.deps.0.push((
        "app.h".parse().unwrap(),
        ["column::app.t.g".parse().unwrap()].into(),
    ));
    assert!(transitive.compile(&pg).is_ok());
    // A lexical generation call also consumes the module's typed prerequisites.
    f.deps.0.remove(0);
    f.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns
        .get_mut("g")
        .unwrap()
        .value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
        expression: "app.f()".into(),
        stored: true,
    });
    assert!(f.compile(&pg).is_ok());
}

#[test]
fn an_inline_generation_provider_in_the_prefix_needs_no_set_expression() {
    let mut f = Fixture::new("generation", "postgres");
    let table = f.schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
    table.columns.shift_insert(
        2,
        "h".into(),
        pbps_model::Column {
            value_source: pbps_model::ValueSource::Generated(pbps_model::Generated {
                expression: "7".into(),
                stored: true,
            }),
            ..pbps_model::Column::new("integer".parse().unwrap())
        },
    );
    f.ids
        .columns
        .insert("c_eeeeee".parse().unwrap(), "app.t.h".parse().unwrap());
    f.deps.0[1].1 = ["generation::app.t.h".parse().unwrap()].into();
    let plan = f
        .compile(&pbps_pg::Postgres::new().with_server_version(160015))
        .unwrap();
    assert!(
        !plan
            .changes()
            .changes
            .iter()
            .any(|p| matches!(p.change, Change::AlterColumnExpression { .. }))
    );
    assert!(plan.changes().changes.iter().any(
        |p| matches!(&p.change,Change::CreateTable{table,..} if table.columns.contains_key("h"))
    ));
    let table = f.schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
    let h = table.columns.shift_remove("h").unwrap();
    table.columns.insert("h".into(), h);
    let error = f
        .compile(&pbps_pg::Postgres::new().with_server_version(160015))
        .unwrap_err();
    assert!(error.contains("SET EXPRESSION"), "{error}");
    assert!(
        f.compile(&pbps_pg::Postgres::new().with_server_version(180006))
            .is_ok()
    );
}

#[test]
fn empty_demand_keeps_each_module_transition_in_complete_shape_analysis() {
    let mut f = Fixture::new("generation", "postgres");
    f.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns
        .get_mut("g")
        .unwrap()
        .value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
        expression: "app.f()".into(),
        stored: true,
    });
    let final_module = f.schema.modules[&"app.f".parse().unwrap()].clone();
    f.schema
        .modules
        .get_mut(&"app.f".parse().unwrap())
        .unwrap()
        .definition =
        "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN (SELECT g FROM app.t LIMIT 1)".into();
    let pg = pbps_pg::Postgres::new().with_server_version(160015);
    let empty = pbps_model::Schema::default();
    let ids = pbps_model::IdsFile::default();
    let input = pbps_diff::diff(
        pbps_diff::Side {
            schema: &empty,
            ids: &ids,
        },
        pbps_diff::Side {
            schema: &f.schema,
            ids: &f.ids,
        },
        &pg,
        &Default::default(),
    )
    .unwrap();
    let mut changes = input.changes().clone();
    changes
        .changes
        .push(pbps_model::PlannedChange::new(Change::AlterModule {
            id: "app.f".parse().unwrap(),
            module: Box::new(final_module),
        }));
    let create = changes
        .changes
        .iter()
        .find(|p| matches!(&p.change,Change::CreateTable{name,..} if name.name=="t"))
        .unwrap();
    assert!(
        pbps_diff::order::generated::parts(
            create,
            &f.ids,
            &changes.changes,
            &Default::default(),
            &pg
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn generation_can_depend_directly_on_its_column_existence() {
    let mut f = Fixture::new("generation", "postgres");
    f.deps.0[0].1 = ["column::app.t.g".parse().unwrap()].into();
    let plan = f
        .compile(&pbps_pg::Postgres::new().with_server_version(180006))
        .unwrap();
    assert!(
        plan.changes()
            .changes
            .iter()
            .any(|p| matches!(p.change, Change::AlterColumnExpression { .. }))
    );
    let error = f
        .compile(&pbps_pg::Postgres::new().with_server_version(160015))
        .unwrap_err();
    assert!(error.contains("SET EXPRESSION"), "{error}");
}

#[path = "creation2469/continuity.rs"]
mod continuity;

#[path = "creation2469/forward.rs"]
mod forward;

#[path = "creation2469/prefix.rs"]
mod prefix;

#[path = "creation2469/computed.rs"]
mod computed;

#[path = "creation2469/inline.rs"]
mod inline;
