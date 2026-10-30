use super::*;
#[path = "../../../pbps-diff/tests/fixtures/prefix2469.rs"]
mod prefix_fixture;

#[test]
fn prefix_facets_unlock_a_later_column_without_waiting_for_it() {
    fn check(engine: &impl Dialect) {
        for facet in prefix_fixture::FACETS {
            let f = prefix_fixture::scene(facet, engine.name(), false);
            let plan = f
                .compile(engine)
                .unwrap_or_else(|why| panic!("{} {facet}: {why}", engine.name()));
            let (part, function, column) = prefix_fixture::positions(plan.changes(), facet);
            assert!(
                part < function && function < column,
                "{facet}: {part} {function} {column}"
            );
            assert!(
                plan.annotation_edges()
                    .unwrap()
                    .iter()
                    .all(|edge| edge.before < edge.after)
            );
        }
    }
    check(&pbps_pg::Postgres::new().with_server_version(160015));
    check(&pbps_pg::Postgres::new().with_server_version(180006));
    check(&pbps_mssql::Mssql);
}

#[test]
fn suffix_readers_keep_their_actual_prerequisites_and_refuse_a_real_cycle() {
    fn check(engine: &impl Dialect) {
        for facet in ["check", "unique", "index", "foreign_key"] {
            let mut f = prefix_fixture::scene(facet, engine.name(), true);
            let why = f.compile(engine).unwrap_err();
            assert!(
                why.contains("cycle") || why.contains("wait on one another"),
                "{facet}: {why}"
            );
            f.deps.0[1].1 = ["table::app.t".parse().unwrap()].into();
            let plan = f.compile(engine).unwrap();
            let (part, function, column) = prefix_fixture::positions(plan.changes(), facet);
            assert!(
                function < column && column < part,
                "{facet}: {part} {function} {column}"
            );
        }
        let mut f = prefix_fixture::scene("default", engine.name(), false);
        f.deps.0[1].1 = ["default::app.t.x".parse().unwrap()].into();
        assert!(
            f.compile(engine).is_err(),
            "an absent facet is not available"
        );
    }
    check(&pbps_pg::Postgres::new().with_server_version(180006));
    check(&pbps_mssql::Mssql);
}

#[test]
fn final_generation_installers_follow_inputs_instead_of_declaration_slots() {
    let f = prefix_fixture::generations();
    let plan = f
        .compile(&pbps_pg::Postgres::new().with_server_version(180006))
        .unwrap();
    let steps = &plan.changes().changes;
    let a = steps.iter().position(|p| matches!(&p.change, Change::AlterColumnExpression { column, .. } if column.name.raw() == "a")).unwrap();
    let c = steps.iter().position(|p| matches!(&p.change, Change::AlterColumnExpression { column, .. } if column.name.raw() == "c")).unwrap();
    let function = steps
        .iter()
        .position(|p| matches!(&p.change, Change::CreateModule { id, .. } if id.name() == "f"))
        .unwrap();
    assert!(c < function && function < a);
    let why = f
        .compile(&pbps_pg::Postgres::new().with_server_version(160015))
        .unwrap_err();
    assert!(why.contains("SET EXPRESSION"), "{why}");
}

#[test]
fn empty_not_null_needs_the_placeholder_column_and_can_unlock_its_generation() {
    let f = prefix_fixture::generated_not_null();
    let plan = f
        .compile(&pbps_pg::Postgres::new().with_server_version(180006))
        .unwrap();
    let changes = &plan.changes().changes;
    let nn = changes.iter().position(|p| matches!(&p.change, Change::AlterColumnNullability { column, .. } if column.name.raw() == "g")).unwrap();
    let function = changes
        .iter()
        .position(|p| matches!(&p.change, Change::CreateModule { id, .. } if id.name() == "f"))
        .unwrap();
    let generation = changes.iter().position(|p| matches!(&p.change, Change::AlterColumnExpression { column, .. } if column.name.raw() == "g")).unwrap();
    assert!(nn < function && function < generation);
    assert!(
        f.compile(&pbps_pg::Postgres::new().with_server_version(160015))
            .unwrap_err()
            .contains("SET EXPRESSION")
    );
}
