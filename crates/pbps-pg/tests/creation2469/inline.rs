#[path = "../../../pbps-diff/tests/fixtures/creation2469_inline.rs"]
mod required;
use super::*;
use pbps_dialect::lower::lower_all;
use pbps_model::{Change, PlanMode};

fn root(plan: &pbps_model::ChangeSet) -> &pbps_model::Table {
    plan.changes
        .iter()
        .find_map(|p| {
            if let Change::CreateTable { name, table, .. } = &p.change
                && name == &"app.t".parse().unwrap()
            {
                Some(table.as_ref())
            } else {
                None
            }
        })
        .unwrap()
}
#[test]
fn versioning_retains_its_independent_primary_key_and_period_in_create() {
    let engine = pbps_mssql::Mssql;
    for versioned in [false, true] {
        let f = required::temporal(versioned);
        let plan = f.compile(&engine).unwrap();
        let prefix = root(plan.changes());
        assert_eq!(prefix.primary_key.is_some(), versioned);
        assert!(prefix.columns.contains_key("vf") && prefix.columns.contains_key("vt"));
        assert!(!prefix.columns.contains_key("x"));
        assert!(!plan.changes().changes.iter().any(|p|matches!(&p.change,Change::SetPrimaryKey{table,..} if table==&"app.t".parse().unwrap())));
        let ordered = plan.ordered(PlanMode::Transactional, &engine);
        lower_all(
            &ordered as &dyn pbps_diff::order::PlanView<pbps_mssql::Mssql>,
            &engine,
        )
        .unwrap();
    }
}
#[test]
fn required_groups_preserve_provider_only_and_before_create_dependencies() {
    let engine = pbps_mssql::Mssql;
    let mut f = required::temporal(true);
    f.deps = ObjectDeps(vec![(
        "app.f".parse().unwrap(),
        ["primary_key::app.t".parse().unwrap()].into(),
    )]);
    assert!(
        root(f.compile(&engine).unwrap().changes())
            .primary_key
            .is_some()
    );
    f.deps = ObjectDeps(vec![(
        "primary_key::app.t".parse().unwrap(),
        ["app.f".parse().unwrap()].into(),
    )]);
    let plan = f.compile(&engine).unwrap();
    let function = plan
        .changes()
        .changes
        .iter()
        .position(|p| matches!(p.change, Change::CreateModule { .. }))
        .unwrap();
    let table = plan
        .changes()
        .changes
        .iter()
        .position(
            |p| matches!(&p.change,Change::CreateTable{name,..} if name==&"app.t".parse().unwrap()),
        )
        .unwrap();
    assert!(function < table);
    let shape = f.schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
    shape.columns.get_mut("anchor").unwrap().value_source =
        pbps_model::ValueSource::Default("app.f()".into());
    f.deps = ObjectDeps(vec![(
        "primary_key::app.t".parse().unwrap(),
        ["default::app.t.anchor".parse().unwrap()].into(),
    )]);
    let plan = f.compile(&engine).unwrap();
    assert!(matches!(
        root(plan.changes()).columns["anchor"].value_source,
        pbps_model::ValueSource::Default(_)
    ));
    assert!(!plan.changes().changes.iter().any(|p|matches!(&p.change,Change::AlterColumnDefault{column,..} if column==&"app.t.anchor".parse().unwrap())));
}
#[test]
fn an_inline_requirement_names_the_real_conflict_instead_of_emitting_a_broken_create() {
    for (surface, why) in [
        ("primary_key::app.t", "primary key"),
        ("column::app.t.vf", "PERIOD FOR SYSTEM_TIME"),
        ("not_null::app.t.vf", "PERIOD FOR SYSTEM_TIME"),
        ("column::app.t.anchor", "inline"),
    ] {
        let f = required::require(required::temporal(true), surface);
        let error = f.compile(&pbps_mssql::Mssql).unwrap_err();
        assert!(
            error.contains("inline") && error.contains(why),
            "{surface}: {error}"
        );
    }
    let mut f = required::temporal(true);
    f.deps = ObjectDeps(vec![(
        "not_null::app.t.vf".parse().unwrap(),
        ["column::app.t.vf".parse().unwrap()].into(),
    )]);
    assert!(f.compile(&pbps_mssql::Mssql).is_ok());
    f.deps = ObjectDeps(vec![(
        "column::app.t.vf".parse().unwrap(),
        ["not_null::app.t.vf".parse().unwrap()].into(),
    )]);
    let error = f.compile(&pbps_mssql::Mssql).unwrap_err();
    assert!(
        error.contains("inline") && error.contains("PERIOD FOR SYSTEM_TIME"),
        "{error}"
    );
    let f = required::require(required::partitioned(), "column::app.t.anchor");
    for version in [160015, 180006] {
        let error = f
            .compile(&pbps_pg::Postgres::new().with_server_version(version))
            .unwrap_err();
        assert!(
            error.contains("inline") && error.contains("PARTITION BY"),
            "{error}"
        );
    }
}
#[test]
fn a_partition_key_stays_inline_while_a_later_column_waits() {
    for version in [160015, 180006] {
        let f = required::partitioned();
        let plan = f
            .compile(&pbps_pg::Postgres::new().with_server_version(version))
            .unwrap();
        let prefix = root(plan.changes());
        assert!(prefix.columns.contains_key("anchor"));
        assert!(!prefix.columns.contains_key("x"));
        assert!(prefix.partition_by.is_some());
    }
}
#[test]
fn selecting_identity_not_null_moves_its_whole_column_installation() {
    fn check<E: Dialect>(engine: E) {
        let f = required::identity(engine.name());
        let plan = f.compile(&engine).unwrap();
        assert!(plan.changes().changes.iter().any(|p|matches!(&p.change,Change::AddColumn{column,..} if matches!(column.value_source,pbps_model::ValueSource::Identity(_)) && !column.nullable)));
        assert!(
            !plan
                .changes()
                .changes
                .iter()
                .any(|p| matches!(p.change, Change::AlterColumnNullability { .. }))
        );
    }
    check(pbps_mssql::Mssql);
    check(pbps_pg::Postgres::new());
}

#[test]
fn empty_demand_retains_engine_recipe_bytes() {
    fn check<E: Dialect>(engine: E, f: Fixture) {
        let change = Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: "app.t".parse().unwrap(),
            table: Box::new(f.schema.tables[&"app.t".parse().unwrap()].clone()),
            beside_default: None,
            moves_at_most: None,
        };
        let refs: std::collections::BTreeSet<pbps_model::object_ref::ObjectRef> =
            Default::default();
        let old = engine
            .generated_table_parts(&change, &f.ids, &[], false)
            .unwrap()
            .unwrap_or_else(|| vec![change.clone()]);
        let new = engine
            .creation_parts(
                &change,
                &f.ids,
                &[],
                false,
                (&refs).into(),
                &Default::default(),
            )
            .unwrap()
            .unwrap_or_else(|| vec![change]);
        assert_eq!(
            serde_json::to_vec(&old).unwrap(),
            serde_json::to_vec(&new).unwrap()
        );
    }
    check(pbps_mssql::Mssql, required::temporal(true));
    check(pbps_mssql::Mssql, required::temporal(false));
    check(pbps_pg::Postgres::new(), required::partitioned());
    check(pbps_pg::Postgres::new(), required::identity("postgres"));
}

#[test]
fn a_moved_column_defers_only_facets_that_actually_need_it() {
    fn check<E: Dialect>(engine: E) {
        let mut f = Fixture::new("column", engine.name());
        let table = f.schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
        table.primary_key =
            Some(serde_json::from_value(serde_json::json!({"columns":["anchor"]})).unwrap());
        for (suffix, column) in [("prefix", "anchor"), ("suffix", "x")] {
            table.unique.insert(
                format!("uq_{suffix}").into(),
                serde_json::from_value(serde_json::json!({"columns":[column]})).unwrap(),
            );
            table.checks.insert(
                format!("ck_{suffix}").into(),
                pbps_model::CheckConstraint {
                    expression: format!("{column}>=0"),
                },
            );
            table.indexes.insert(
                format!("ix_{suffix}").into(),
                serde_json::from_value(
                    serde_json::json!({"columns":[{"name":column,"descending":false}]}),
                )
                .unwrap(),
            );
        }
        let plan = f.compile(&engine).unwrap();
        let prefix = root(plan.changes());
        assert!(prefix.primary_key.is_some());
        assert!(
            prefix.unique.contains_key("uq_prefix") && !prefix.unique.contains_key("uq_suffix")
        );
        assert!(
            prefix.checks.contains_key("ck_prefix") && !prefix.checks.contains_key("ck_suffix"),
            "{}: {:?}",
            engine.name(),
            prefix.checks.keys().collect::<Vec<_>>()
        );
        assert!(
            prefix.indexes.contains_key("ix_prefix") && !prefix.indexes.contains_key("ix_suffix")
        );
    }
    check(pbps_mssql::Mssql);
    check(pbps_pg::Postgres::new());
}

#[test]
fn an_earlier_inline_demand_does_not_swallow_a_later_legal_suffix() {
    fn check<E: Dialect>(engine: E) {
        let f = required::separated_providers(engine.name());
        let plan = f.compile(&engine).unwrap();
        let changes = &plan.changes().changes;
        let module = |n: &str| {
            changes
                .iter()
                .position(|p| matches!(&p.change,Change::CreateModule{id,..} if id.name()==n))
                .unwrap()
        };
        let table=changes.iter().position(|p|matches!(&p.change,Change::CreateTable{name,..} if name==&"app.t".parse().unwrap())).unwrap();
        let column=changes.iter().position(|p|matches!(&p.change,Change::AddColumn{table,..} if table==&"app.t".parse().unwrap())).unwrap();
        assert!(module("f") < table && table < module("after") && module("after") < column);
        assert!(root(plan.changes()).columns.contains_key("anchor"));
    }
    check(pbps_pg::Postgres::new().with_server_version(160015));
    check(pbps_pg::Postgres::new().with_server_version(180006));
    check(pbps_mssql::Mssql);
    let f = required::separated_generation();
    let plan = f
        .compile(&pbps_pg::Postgres::new().with_server_version(180006))
        .unwrap();
    assert!(!root(plan.changes()).columns.contains_key("y"));
    assert!(root(plan.changes()).checks.contains_key("ck_key"));
    assert!(
        plan.changes()
            .changes
            .iter()
            .any(|p| matches!(p.change, Change::AlterColumnExpression { .. }))
    );
    assert!(
        f.compile(&pbps_pg::Postgres::new().with_server_version(160015))
            .unwrap_err()
            .contains("SET EXPRESSION")
    );
}
