use super::*;
use pbps_diff::order::deployment::{
    Sources,
    schedule::{Declarations, Facts, diff_annotated},
};
use pbps_model::{Schema, ValueSource};

fn compile<E: Dialect>(
    engine: &E,
    f: &Fixture,
    base: &Schema,
    desired: &Schema,
    before: &ObjectDeps,
    goal: &ObjectDeps,
) -> pbps_diff::order::CanonicalPlan<E> {
    let before_side = pbps_diff::Side {
        schema: base,
        ids: &f.ids,
    };
    diff_annotated(
        before_side,
        &Declarations {
            schema: desired,
            sources: Sources {
                before: before_side,
                after: &f.ids,
            },
            module_deps: &Default::default(),
        },
        &Default::default(),
        before,
        goal,
        &Facts::default(),
        engine,
    )
    .unwrap()
    .changes
}

fn check<E: Dialect>(engine: E) {
    let f = Fixture::new("default", engine.name());
    let base = f.schema.clone();
    let mut desired = base.clone();
    let table = "app.t".parse().unwrap();
    desired.tables.get_mut(&table).unwrap().columns["x"].value_source = ValueSource::None;
    desired
        .modules
        .get_mut(&"app.f".parse().unwrap())
        .unwrap()
        .definition = if engine.name() == "postgres" {
        "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN 8"
    } else {
        "() RETURNS integer AS BEGIN RETURN 8; END"
    }
    .into();
    let deps = ObjectDeps(vec![(
        f.owner.parse().unwrap(),
        ["app.f".parse().unwrap()].into(),
    )]);
    let removed = compile(&engine, &f, &base, &desired, &deps, &ObjectDeps::default());
    let edges = removed.annotation_edges().unwrap();
    if engine.rebuilds_modules() {
        let removal = removed
            .changes()
            .changes
            .iter()
            .position(|p| matches!(&p.change, Change::AlterColumnDefault { to: None, .. }))
            .unwrap();
        let provider = removed
            .changes()
            .changes
            .iter()
            .position(|p| {
                matches!(
                    &p.change,
                    Change::AlterModule { .. } | Change::DropModule { .. }
                )
            })
            .unwrap();
        assert!(
            edges
                .iter()
                .any(|e| e.before == removal && e.after == provider)
        );
        assert!(removal < provider);
    } else {
        // A CREATE OR ALTER provider stays available, even though its
        // declaration has both an installation and a removal boundary.
        assert!(edges.is_empty());
    }
    // New facet installation still waits for the provider's new definition,
    // including on the engine that keeps the provider's object alive.
    let mut before_install = base.clone();
    before_install.tables.get_mut(&table).unwrap().columns["x"].value_source = ValueSource::None;
    let mut after_install = desired.clone();
    after_install.tables.get_mut(&table).unwrap().columns["x"].value_source =
        ValueSource::Default("7".into());
    let installed = compile(
        &engine,
        &f,
        &before_install,
        &after_install,
        &ObjectDeps::default(),
        &deps,
    );
    let provider = installed
        .changes()
        .changes
        .iter()
        .position(|p| {
            matches!(
                &p.change,
                Change::AlterModule { .. } | Change::CreateModule { .. }
            )
        })
        .unwrap();
    let facet = installed
        .changes()
        .changes
        .iter()
        .position(|p| matches!(&p.change, Change::AlterColumnDefault { to: Some(_), .. }))
        .unwrap();
    assert!(
        installed
            .annotation_edges()
            .unwrap()
            .iter()
            .any(|e| e.before == provider && e.after == facet)
    );
    assert!(provider < facet);
}

#[test]
fn facet_installation_and_removal_use_the_same_engine_continuity_evidence() {
    check(pbps_pg::Postgres::new());
    check(pbps_mssql::Mssql);
}

#[test]
fn mssql_scratch_accepts_each_portable_facet_with_auxiliary_boundaries() {
    let engine = pbps_mssql::Mssql;
    for facet in FACETS {
        let f = Fixture::new(facet, "mssql");
        let input = f.bootstrap(&engine).unwrap();
        // The deferred external boundary exercises master's interleaved
        // auxiliary route even when no external declaration is supplied.
        let result = pbps_mssql::resolver::reconstruct::Reconstruction::with_chained(
            &engine,
            &input,
            &f.ids,
            &Default::default(),
            None,
            true,
        );
        assert!(result.is_ok(), "{facet}: {result:?}");
    }
}
