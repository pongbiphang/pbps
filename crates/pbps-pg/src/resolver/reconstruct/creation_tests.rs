use super::*;
#[path = "../../../../pbps-diff/tests/fixtures/creation2469.rs"]
mod fixture;

#[test]
fn scratch_facets_and_external_setup_boundaries_are_not_conflated() {
    let engine = crate::Postgres::new();
    let nn = fixture::Fixture::new("not_null", "postgres");
    assert!(Reconstruction::new(&engine, &nn.compile(&engine).unwrap(), &nn.ids).is_ok());
    let mut f = fixture::Fixture::new("default", "postgres");
    f.schema
        .roles
        .insert("worker2469".into(), pbps_model::Role::default());
    f.ids
        .roles
        .insert("r_aaaaaa".parse().unwrap(), "worker2469".into());
    f.schema.modules.insert(
        "app.after_role".parse().unwrap(),
        pbps_model::Module {
            kind: ModuleKind::View,
            definition: "SELECT current_user::text AS role_name".into(),
            description: None,
            options: Default::default(),
        },
    );
    f.deps.0.extend([
        (
            "role::worker2469".parse().unwrap(),
            ["default::app.t.x".parse().unwrap()].into(),
        ),
        (
            "app.after_role".parse().unwrap(),
            ["role::worker2469".parse().unwrap()].into(),
        ),
    ]);
    let error = f.compile(&engine).unwrap_err();
    assert!(error.contains("fixed external setup boundary"), "{error}");
    // A role supplied before the baseline is available, but contributes no executable SQL.
    f.deps
        .0
        .retain(|(owner, _)| owner != &"role::worker2469".parse().unwrap());
    f.deps.0[0].1.insert("role::worker2469".parse().unwrap());
    let base = pbps_model::Schema {
        roles: f.schema.roles.clone(),
        ..Default::default()
    };
    let ids = pbps_model::IdsFile {
        roles: f.ids.roles.clone(),
        ..Default::default()
    };
    let plan = f.compile_from(&base, &ids, &f.deps, &engine).unwrap();
    let built = Reconstruction::new(&engine, &plan, &f.ids).unwrap();
    assert!(
        !built
            .steps
            .iter()
            .any(|s| matches!(s.subject, Change::CreateRole { .. }))
    );
}

#[test]
fn demanded_default_paths_survive_a_real_skipped_policy_vertex() {
    let engine = crate::Postgres::new();
    let f = fixture::Fixture::new("default", "postgres").with_skipped_policy();
    let plan = f.bootstrap(&engine).unwrap();
    let built = Reconstruction::new(&engine, &plan, &f.ids).unwrap();
    let default = built
        .steps
        .iter()
        .position(|s| matches!(s.subject, Change::AlterColumnDefault { .. }))
        .unwrap();
    let after = built
        .steps
        .iter()
        .position(|s| s.module.as_ref() == Some(&"app.after_policy".parse().unwrap()))
        .unwrap();
    assert!(
        !built
            .steps
            .iter()
            .any(|s| matches!(s.subject, Change::CreatePolicy { .. }))
    );
    assert!(!built.trial.can_precede(after, default));
    assert!(built.trial.can_precede(default, after));
    let mut cycle = f.deps.clone();
    cycle.0.push((
        "table::app.t".parse().unwrap(),
        ["app.after_policy".parse().unwrap()].into(),
    ));
    assert!(
        f.compile_from(&Default::default(), &Default::default(), &cycle, &engine)
            .is_err()
    );
}
