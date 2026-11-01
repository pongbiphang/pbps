use super::*;
#[path = "../../../pbps-diff/tests/fixtures/forward2469.rs"]
mod forward_fixture;

#[test]
fn demanded_suffix_and_forward_generations_share_one_legal_recipe() {
    for pairs in [1, 2] {
        for validators in [false, true] {
            let f = forward_fixture::Scene::new(pairs, "postgres", validators);
            for version in [170000, 180006] {
                let engine = pbps_pg::Postgres::new().with_server_version(version);
                let plan = f.compile(&engine).unwrap();
                let table = plan
                    .changes()
                    .changes
                    .iter()
                    .find_map(|p| {
                        if let Change::CreateTable { name, table, .. } = &p.change
                            && name == &"app.t".parse().unwrap()
                        {
                            Some(table)
                        } else {
                            None
                        }
                    })
                    .unwrap();
                assert!(!table.columns.contains_key("b"));
                let pbps_model::ValueSource::Generated(g) = &table.columns["a"].value_source else {
                    panic!()
                };
                assert!(!g.expression.contains('b'), "{}", g.expression);
                assert_eq!(
                    plan.changes()
                        .changes
                        .iter()
                        .filter(|p| matches!(p.change, Change::AlterColumnExpression { .. }))
                        .count(),
                    pairs
                );
                let physical: Vec<_> = table
                    .columns
                    .keys()
                    .map(|c| c.raw().to_owned())
                    .chain(plan.changes().changes.iter().filter_map(|p| {
                        if let Change::AddColumn { table, name, .. } = &p.change {
                            (table == &"app.t".parse().unwrap()).then(|| name.raw().to_owned())
                        } else {
                            None
                        }
                    }))
                    .collect();
                assert_eq!(physical, f.columns);
                let steps = &plan.changes().changes;
                for (generated, input) in [("a", "b"), ("c", "d")].into_iter().take(pairs) {
                    let read = steps.iter().position(|p| matches!(&p.change, Change::AddColumn { name, .. } if name.raw() == input)).unwrap();
                    let install = steps.iter().position(|p| matches!(&p.change, Change::AlterColumnExpression { column, .. } if column.name.raw() == generated)).unwrap();
                    assert!(read < install, "{generated} must wait for {input}");
                    for (at, p) in steps.iter().enumerate() {
                        if p.change.table() == Some(&"app.t".parse().unwrap())
                            && matches!(
                                p.change,
                                Change::SetPrimaryKey { .. }
                                    | Change::AddUnique { .. }
                                    | Change::AddForeignKey { .. }
                                    | Change::AddCheck { .. }
                                    | Change::AddIndex { .. }
                            )
                        {
                            let input = steps.iter().position(|p| matches!(&p.change, Change::AddColumn { name, .. } if name.raw() == "b")).unwrap();
                            assert!(input < at, "validators in this fixture read b");
                        }
                    }
                }
            }
            let error = f
                .compile(&pbps_pg::Postgres::new().with_server_version(160015))
                .unwrap_err();
            assert!(error.contains("SET EXPRESSION"), "{error}");
            let m = forward_fixture::Scene::new(pairs, "mssql", validators);
            let plan = m.compile(&pbps_mssql::Mssql).unwrap();
            assert!(
                !plan
                    .changes()
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, Change::AlterColumnExpression { .. }))
            );
        }
    }
}

#[test]
fn actual_whole_shape_requirements_remain_a_correct_refusal() {
    let mut f = forward_fixture::Scene::new(1, "postgres", false);
    f.schema
        .modules
        .get_mut(&"app.f".parse().unwrap())
        .unwrap()
        .definition =
        "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN (SELECT b FROM app.t LIMIT 1)".into();
    let error = f
        .compile(&pbps_pg::Postgres::new().with_server_version(180006))
        .unwrap_err();
    assert!(error.contains("wait on one another"), "{error}");
    let f = forward_fixture::Scene::new(1, "postgres", false);
    assert!(
        f.compile(&pbps_pg::Postgres::new())
            .unwrap_err()
            .contains("SET EXPRESSION")
    );
}

#[test]
fn a_final_prefix_generation_can_unlock_a_later_demanded_column() {
    let mut f = forward_fixture::Scene::new(2, "postgres", true);
    f.deps.0[3].1 = ["generation::app.t.a".parse().unwrap()].into();
    let plan = f
        .compile(&pbps_pg::Postgres::new().with_server_version(180006))
        .unwrap();
    let steps = &plan.changes().changes;
    let generation = steps.iter().position(|p| matches!(&p.change, Change::AlterColumnExpression { column, .. } if column.name.raw() == "a")).unwrap();
    let later = steps
        .iter()
        .position(|p| matches!(&p.change, Change::AddColumn { name, .. } if name.raw() == "d"))
        .unwrap();
    let input = steps
        .iter()
        .position(|p| matches!(&p.change, Change::AddColumn { name, .. } if name.raw() == "b"))
        .unwrap();
    assert!(input < generation && generation < later);
    let mut m = forward_fixture::Scene::new(2, "mssql", true);
    m.deps.0[3].1 = ["generation::app.t.a".parse().unwrap()].into();
    assert!(m.compile(&pbps_mssql::Mssql).is_err());
}

#[test]
fn a_literal_matching_a_late_column_does_not_require_set_expression() {
    let mut f = forward_fixture::Scene::new(1, "postgres", false);
    let pbps_model::ValueSource::Generated(g) = &mut f
        .schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns["a"]
        .value_source
    else {
        panic!()
    };
    g.expression = "length('b')".into();
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
}
