//! Annotation decomposition keeps inherited identity separate from scratch parts.
use crate::certified_fixture::FixtureEmission as _;
use pbps_diff::order::{Source, propose, scratch};
use pbps_model::object_ref::{BoundRoot, Problem, Registry};
use pbps_model::operation::{ColumnKey, ModuleEdits, ObjectKey, Operations, PlanOwners, Spelling};
use pbps_model::plan_names::PlanNames;
use pbps_model::{Change, IdsFile};
use pbps_pg::Postgres;

#[path = "support/annotation_partitions.rs"]
mod fixture;
use fixture::Scene;

#[test]
fn a_partition_default_installs_after_its_annotated_module_at_every_depth() {
    for depth in [1, 3] {
        for version in [160_015, 180_006] {
            let pg = Postgres::new().with_server_version(version);
            let scene = Scene::new(depth);
            let existing = Scene::new(depth)
                .with_existing_ancestors()
                .plan(&pg)
                .unwrap();
            assert!(existing.annotation_edges().is_ok());
            let plan = scene.plan(&pg).unwrap();
            let at = |predicate: fn(&Change) -> bool| {
                plan.changes
                    .iter()
                    .position(|p| predicate(&p.change))
                    .unwrap()
            };
            let module = at(|c| matches!(c, Change::CreateModule { .. }));
            let default = at(|c| matches!(c, Change::SetPartitionDefault { .. }));
            let relation = plan
                .changes
                .iter()
                .position(|p| {
                    matches!(&p.change,
                Change::CreateTable { name, .. } if name == &scene.child)
                })
                .unwrap();
            assert!(relation < module && module < default);
            assert!(
                plan.annotation_edges()
                    .unwrap()
                    .iter()
                    .any(|e| e.source == Source::Annotation
                        && e.before == module
                        && e.after == default)
            );
            let empty = IdsFile::default();
            let owners = PlanOwners::new(
                PlanNames::of(&plan.changes, &scene.ids).unwrap(),
                &empty,
                &scene.ids,
            );
            let bound = Registry::new(&scene.schema)
                .bind(
                    &"default::app.child.x".parse().unwrap(),
                    &owners,
                    Spelling::Final,
                )
                .unwrap();
            assert!(
                matches!(bound.root, BoundRoot::InheritedColumn { table, column }
                if &table == scene.ids.table_uid(&scene.child).unwrap()
                && &column == scene.ids.column_uid(&"app.root.x".parse().unwrap()).unwrap())
            );
            let ops = Operations::of_plan(&plan.changes, ModuleEdits::Rebuilt);
            assert!(matches!(ops.as_slice()[default].key(&owners).unwrap(),
                ObjectKey::Default(ColumnKey::Partition { table, column })
                if table.uid() == scene.ids.table_uid(&scene.child).unwrap() && column.raw() == "x"));
        }
    }
}

#[test]
fn an_annotated_partition_bootstrap_reaches_the_complete_reconstruction() {
    for depth in [1, 3] {
        for inherited in [false, true] {
            for version in [160_015, 180_006] {
                let scene = Scene::new(depth);
                let scene = if inherited {
                    scene.with_root_default().with_existing_ancestors()
                } else {
                    scene
                };
                let pg = Postgres::new().with_server_version(version);
                let bootstrap = scene.input().unwrap();
                let prepared =
                    scratch::prepare(&bootstrap, &scene.ids, &Default::default(), &pg).unwrap();
                let reconstruction = pbps_pg::resolver::reconstruct::Reconstruction::new(
                    &pg, &bootstrap, &scene.ids,
                )
                .unwrap();
                let default = prepared
                    .changes()
                    .changes
                    .iter()
                    .position(|p| {
                        matches!(&p.change,
                    Change::AlterColumnDefault { column, .. } if column.table == scene.child)
                    })
                    .unwrap();
                let module = prepared
                    .changes()
                    .changes
                    .iter()
                    .position(|p| matches!(p.change, Change::CreateModule { .. }))
                    .unwrap();
                assert!(prepared.edges().iter().any(|e| e.before == module
                    && e.after == default
                    && e.source == Source::Annotation));
                assert_eq!(reconstruction.len(), prepared.changes().changes.len());
                assert!(reconstruction.next_fill().is_none());
                assert!(reconstruction.observations().is_none());
            }
        }
    }
}

#[test]
fn an_inherited_fallback_does_not_install_the_partitions_deferred_own_default() {
    let scene = Scene::new(3).with_root_default().with_existing_ancestors();
    let plan = scene.plan(&Postgres::new()).unwrap();
    let prepared = scratch::prepare(
        &scene.input().unwrap(),
        &scene.ids,
        &Default::default(),
        &Postgres::new(),
    )
    .unwrap();
    assert!(prepared.changes().annotation_edges().is_ok());
    let module = plan
        .changes
        .iter()
        .position(|p| matches!(p.change, Change::CreateModule { .. }))
        .unwrap();
    let default = plan
        .changes
        .iter()
        .position(|p| matches!(p.change, Change::SetPartitionDefault { .. }))
        .unwrap();
    assert!(module < default);
}

#[test]
fn scratch_annotations_share_the_inherited_installation_identity() {
    for depth in [1, 3] {
        let scene = Scene::new(depth);
        let prepared = scratch::prepare(
            &scene.input().unwrap(),
            &scene.ids,
            &Default::default(),
            &Postgres::new(),
        )
        .unwrap();
        let default = prepared.changes().changes.iter().position(|p| matches!(&p.change,
            Change::AlterColumnDefault { uid, column, .. }
                if Some(uid) == scene.ids.table_uid(&scene.child) && column == &scene.child.column("x"))).unwrap();
        let module = prepared
            .changes()
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::CreateModule { .. }))
            .unwrap();
        assert!(
            prepared
                .edges()
                .iter()
                .any(|e| e.source == Source::Annotation
                    && e.before == module
                    && e.after == default)
        );
    }
}

#[test]
fn inherited_generation_not_null_and_extracted_checks_keep_their_boundaries() {
    let mut scene = Scene::new(3);
    let mut generated = pbps_model::Column::new("integer".parse().unwrap());
    generated.value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
        expression: "x + 1".into(),
        stored: true,
    });
    scene
        .schema
        .tables
        .get_mut(&"app.root".parse().unwrap())
        .unwrap()
        .columns
        .insert("g".into(), generated);
    let column = "app.root.g".parse::<pbps_model::ColumnRef>().unwrap();
    scene.ids.columns.insert(
        pbps_model::Uid::derived(pbps_model::UidKind::Column, &column.raw().to_string(), 0),
        column,
    );
    scene
        .schema
        .tables
        .get_mut(&scene.child)
        .unwrap()
        .checks
        .insert(
            "ck".into(),
            serde_json::from_value(serde_json::json!({"expression": "x > 0"})).unwrap(),
        );
    for planned in &mut scene.changes.changes {
        if let Change::CreateTable { name, table, .. } = &mut planned.change {
            **table = scene.schema.tables[name].clone();
        }
    }
    scene.annotations.0 = [
        "generation::app.child.g",
        "not_null::app.child.x",
        "check::app.child.ck",
    ]
    .into_iter()
    .map(|owner| {
        (
            owner.parse().unwrap(),
            ["module::app.f()".parse().unwrap()].into(),
        )
    })
    .collect();
    let plan = scene.plan(&Postgres::new()).unwrap();
    let module = plan
        .changes
        .iter()
        .position(|p| matches!(p.change, Change::CreateModule { .. }))
        .unwrap();
    let child = plan
        .changes
        .iter()
        .position(|p| matches!(&p.change, Change::CreateTable { name, .. } if name == &scene.child))
        .unwrap();
    let check = plan
        .changes
        .iter()
        .position(|p| matches!(p.change, Change::AddCheck { .. }))
        .unwrap();
    assert!(module < child && module < check);
    assert!(
        plan.annotation_edges()
            .unwrap()
            .iter()
            .all(|edge| edge.source == Source::Annotation && edge.before == module)
    );
}

#[test]
fn a_missing_root_column_identity_remains_unrecorded() {
    let mut scene = Scene::new(3);
    scene.ids.columns.clear();
    let empty = IdsFile::default();
    let owners = PlanOwners::new(
        PlanNames::of(&scene.changes.changes, &scene.ids).unwrap(),
        &empty,
        &scene.ids,
    );
    assert!(matches!(
        Registry::new(&scene.schema).bind(
            &"default::app.child.x".parse().unwrap(),
            &owners,
            Spelling::Final
        ),
        Err(Problem::Unrecorded { .. })
    ));
    let error = scene.plan(&Postgres::new()).unwrap_err();
    assert!(
        error.contains("cannot bind default::app.child.x"),
        "{error}"
    );
}

#[test]
fn a_partition_override_cannot_create_an_undeclared_column() {
    let mut scene = Scene::new(3);
    let of = scene
        .schema
        .tables
        .get_mut(&scene.child)
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap();
    let own = of.columns.remove("x").unwrap();
    of.columns.insert("missing".into(), own);
    scene.annotations.0[0].0 = "default::app.child.missing".parse().unwrap();
    let error = scene.plan(&Postgres::new()).unwrap_err();
    assert!(error.contains("default::app.child.missing"), "{error}");
    assert!(error.contains("unknown dependency target"), "{error}");
}

#[test]
fn scratch_creation_bytes_do_not_change_without_annotations() {
    let mut actual = String::new();
    for depth in [1, 3] {
        for version in [160_015, 180_006] {
            let scene = Scene::new(depth);
            let pg = Postgres::new().with_server_version(version);
            let prepared = scratch::prepare(
                &propose(scene.changes.clone()),
                &scene.ids,
                &Default::default(),
                &pg,
            )
            .unwrap();
            let reconstruction = pbps_pg::resolver::reconstruct::Reconstruction::new(
                &pg,
                &propose(scene.changes.clone()),
                &scene.ids,
            )
            .unwrap();
            assert_eq!(reconstruction.len(), prepared.changes().changes.len());
            let sql: Vec<_> = prepared
                .changes()
                .changes
                .iter()
                .flat_map(|p| {
                    pg.fixture_emit(&p.change, p.strategy)
                        .unwrap()
                        .into_iter()
                        .map(|s| s.sql)
                })
                .collect();
            let bytes = serde_json::json!({ "changes": prepared.changes().changes(), "sql": sql,
                "origins": format!("{:?}", prepared.changes().origins()), "edges": format!("{:?}", prepared.edges()) });
            actual.push_str(&format!(
                "{depth}@{version}:{}\n",
                serde_json::to_string(&bytes).unwrap()
            ));
        }
    }
    assert_eq!(
        actual,
        include_str!("fixtures/annotation_partition_scratch.golden")
    );
}

#[cfg(test)]
mod certified_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pbps-diff/tests/support/emission.rs"
    ));
}
