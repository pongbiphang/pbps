#[path = "fixtures/domain2469.rs"]
mod fixture;
use fixture::*;
use pbps_dialect::{Dialect, lower::lower_all};
use pbps_model::{object_ref::ObjectDeps, *};
use pbps_pg::Postgres;

#[test]
fn demanded_domain_facets_split_without_a_lexical_function_call() {
    let engine = Postgres::new().with_server_version(160015);
    for facet in ["default", "not_null", "check"] {
        for call in [false, true] {
            let f = Fixture::new(facet, call);
            let plan = f
                .compile(&engine)
                .unwrap_or_else(|why| panic!("{facet}: {why}"));
            let changes = &plan.changes().changes;
            let created = changes
                .iter()
                .position(|p| matches!(p.change, Change::CreateType { .. }))
                .unwrap();
            let function = changes
                .iter()
                .position(|p| matches!(p.change, Change::CreateModule { .. }))
                .unwrap();
            let installed = changes
                .iter()
                .position(|p| matches!(p.change, Change::AlterDomain { .. }))
                .unwrap();
            assert!(created < function && function < installed);
            assert!(
                plan.annotation_edges()
                    .unwrap()
                    .iter()
                    .all(|edge| edge.before < edge.after)
            );
            let Change::CreateType {
                uid, definition, ..
            } = &changes[created].change
            else {
                panic!()
            };
            assert_eq!(uid, &"y_aaaaaa".parse().unwrap());
            assert_eq!(definition.as_ref(), &domain(None, false, &[]));
            let expected: std::collections::BTreeSet<_> = match facet {
                "check" => [RiskClass::Constraint].into(),
                "not_null" => [RiskClass::NotNull].into(),
                _ => Default::default(),
            };
            assert_eq!(changes[installed].risks, expected);
            for mode in [PlanMode::Transactional, PlanMode::Staged] {
                let ordered = plan.clone().ordered(mode, &engine);
                let lowered = lower_all(
                    &ordered as &dyn pbps_diff::order::PlanView<Postgres>,
                    &engine,
                )
                .unwrap();
                assert_eq!(
                    lowered.groups()[installed].origin().source(),
                    lowered.groups()[created].origin().source()
                );
            }
        }
    }
}

#[test]
fn reordered_domain_facets_preserve_complete_authored_and_emitted_definitions() {
    let engine = Postgres::new().with_server_version(180006);
    let f = Fixture::multiple();
    let plan = f.compile(&engine).unwrap();
    let mut declared = Declared::default();
    declared.advance(plan.changes());
    assert_eq!(declared.types, f.schema.types);
    let ordered = plan.ordered(PlanMode::Transactional, &engine);
    let lowered = lower_all(
        &ordered as &dyn pbps_diff::order::PlanView<Postgres>,
        &engine,
    )
    .unwrap();
    let definitions: Vec<_> = lowered
        .iter()
        .filter_map(|s| s.declared_type.as_ref().and_then(|t| t.definition.as_ref()))
        .collect();
    assert_eq!(
        *definitions.last().unwrap(),
        &f.schema.types[&"c2469.d".parse().unwrap()]
    );
    let mut checks = Vec::new();
    for step in &definitions {
        if let ManagedType::Domain {
            checks: current, ..
        } = step
        {
            let names: Vec<_> = current.keys().map(|n| n.raw()).collect();
            for previous in &checks {
                assert!(names.contains(previous));
            }
            checks = names;
        }
    }
}

#[test]
fn empty_domain_demand_keeps_changes_proof_and_sql_bytes() {
    let engine = Postgres::new();
    let mut f = Fixture::multiple();
    f.deps = ObjectDeps::default();
    let mut plan = pbps_diff::order::propose(f.bootstrap(&engine).unwrap().changes().clone());
    let before = serde_json::to_vec(&(plan.changes(), plan.proof())).unwrap();
    pbps_diff::order::generated::expand(&mut plan, &f.ids, &Default::default(), &engine).unwrap();
    assert_eq!(
        before,
        serde_json::to_vec(&(plan.changes(), plan.proof())).unwrap()
    );
    let Change::CreateType { .. } = &plan.changes().changes[0].change else {
        panic!()
    };
    assert!(
        !plan
            .changes()
            .changes
            .iter()
            .any(|p| matches!(p.change, Change::AlterDomain { .. }))
    );
    let empty = Schema::default();
    let ids = IdsFile::default();
    let before = pbps_diff::Side {
        schema: &empty,
        ids: &ids,
    };
    let input = pbps_diff::diff(
        before,
        pbps_diff::Side {
            schema: &f.schema,
            ids: &f.ids,
        },
        &engine,
        &Default::default(),
    )
    .unwrap();
    let declared = pbps_diff::order::deployment::schedule::Declarations {
        schema: &f.schema,
        sources: pbps_diff::order::deployment::Sources {
            before,
            after: &f.ids,
        },
        module_deps: &Default::default(),
    };
    let plain = pbps_diff::order::deployment::schedule::schedule(
        input,
        &declared,
        &Default::default(),
        &engine,
    )
    .unwrap()
    .changes;
    let annotated = f.compile(&engine).unwrap();
    assert_eq!(
        serde_json::to_vec(&(plain.changes(), plain.proof())).unwrap(),
        serde_json::to_vec(&(annotated.changes(), annotated.proof())).unwrap()
    );
    let plain = plain.ordered(PlanMode::Transactional, &engine);
    let annotated = annotated.ordered(PlanMode::Transactional, &engine);
    let a = lower_all(&plain as &dyn pbps_diff::order::PlanView<Postgres>, &engine).unwrap();
    let b = lower_all(
        &annotated as &dyn pbps_diff::order::PlanView<Postgres>,
        &engine,
    )
    .unwrap();
    assert_eq!(
        a.groups()
            .iter()
            .map(|g| g.statements())
            .collect::<Vec<_>>(),
        b.groups()
            .iter()
            .map(|g| g.statements())
            .collect::<Vec<_>>()
    );
}

#[test]
fn missing_domain_facets_and_external_aliases_are_not_available() {
    let engine = Postgres::new();
    for target in [
        "default::type::c2469.d",
        "check::type::c2469.d.missing",
        "default::type::c2469.alias",
    ] {
        let mut f = Fixture::new("check", false);
        f.deps = deps(&[("module::c2469.accept(integer)", &[target])]);
        assert!(f.compile(&engine).is_err(), "{target}");
    }
    let f = Fixture::new("check", false);
    let create = Change::CreateType {
        uid: "y_aaaaaa".parse().unwrap(),
        name: "c2469.d".parse().unwrap(),
        definition: Box::new(f.schema.types[&"c2469.d".parse().unwrap()].clone()),
    };
    let engine = pbps_mssql::Mssql;
    let parts = engine
        .creation_parts(
            &create,
            &f.ids,
            &[],
            false,
            &["check::type::c2469.d.positive".parse().unwrap()].into(),
            &Default::default(),
        )
        .unwrap();
    assert_eq!(
        parts.unwrap_or_else(|| vec![create.clone()]),
        vec![create.clone()]
    );
    let original =
        pbps_diff::order::singleton(PlannedChange::new(create), PlanMode::Transactional, &engine);
    assert!(
        lower_all(
            &original as &dyn pbps_diff::order::PlanView<pbps_mssql::Mssql>,
            &engine
        )
        .is_err()
    );
}

#[test]
fn baseline_domain_facet_removal_waits_for_its_annotated_consumer() {
    let engine = Postgres::new();
    let base = Fixture::new("check", false);
    let mut goal = base.clone();
    goal.schema.modules.clear();
    goal.schema
        .types
        .insert("c2469.d".parse().unwrap(), domain(None, false, &[]));
    let baseline = deps(&[(
        "module::c2469.accept(integer)",
        &["check::type::c2469.d.positive"],
    )]);
    let plan = goal
        .compile_from(
            &base.schema,
            &base.ids,
            &baseline,
            &ObjectDeps::default(),
            &engine,
        )
        .unwrap();
    let changes = &plan.changes().changes;
    let removed = changes
        .iter()
        .position(|p| matches!(p.change, Change::DropModule { .. }))
        .unwrap();
    let facet = changes
        .iter()
        .position(|p| {
            matches!(
                p.change,
                Change::AlterDomain {
                    half: DomainHalf::Loosen,
                    ..
                }
            )
        })
        .unwrap();
    assert!(removed < facet);
    let mut remaining = base.clone();
    remaining
        .schema
        .types
        .insert("c2469.d".parse().unwrap(), domain(None, false, &[]));
    assert!(
        remaining
            .compile_from(&base.schema, &base.ids, &baseline, &baseline, &engine)
            .is_err()
    );
}

#[test]
fn empty_scalar_columns_need_the_type_before_its_facets_and_arrays_require_final_facets() {
    let engine = Postgres::new();
    for facet in ["default", "not_null", "check"] {
        for suffix in [false, true] {
            let f = Fixture::column_consumer(facet, suffix, false);
            let plan = f
                .compile(&engine)
                .unwrap_or_else(|why| panic!("{facet} {suffix}: {why}"));
            let parts = &plan.changes().changes;
            let column = parts
                .iter()
                .position(|p| {
                    if suffix {
                        matches!(&p.change,Change::AddColumn{name,..} if name.raw()=="x")
                    } else {
                        matches!(p.change, Change::CreateTable { .. })
                    }
                })
                .unwrap();
            let installed = parts
                .iter()
                .position(|p| matches!(p.change, Change::AlterDomain { .. }))
                .unwrap();
            assert!(column < installed);
            let mut rows = plan.changes().clone();
            rows.changes.push(PlannedChange::new(Change::InsertRow {
                table: "c2469.t".parse().unwrap(),
                key_column: "anchor".into(),
                identity_key: false,
                key: "1".into(),
                row: Row::default(),
                defaults: Default::default(),
                types: Default::default(),
            }));
            let edges = pbps_diff::order::generated::constraints(
                &rows,
                &f.schema,
                &Default::default(),
                &engine,
            );
            assert!(
                edges
                    .iter()
                    .any(|edge| edge.before == installed && edge.after == rows.changes.len() - 1)
            );
        }
        let array = Fixture::column_consumer(facet, false, true).compile(&engine);
        if facet == "default" {
            array.unwrap();
        } else {
            let why = array.unwrap_err();
            assert!(
                why.contains("cycle") || why.contains("wait on one another"),
                "{facet}: {why}"
            );
        }
    }
}

#[test]
fn a_domain_copy_waits_for_the_default_it_copies_and_rejects_a_copy_cycle() {
    let engine = Postgres::new();
    let mut f = Fixture::new("default", false);
    let child: TypeRef = "c2469.child".parse().unwrap();
    let mut definition = domain(None, false, &[]);
    if let ManagedType::Domain { base, .. } = &mut definition {
        *base = ColumnType::reference("c2469.d".parse().unwrap());
    }
    f.schema.types.insert(child.clone(), definition);
    f.ids
        .types
        .insert("y_bbbbbb".parse().unwrap(), child.clone());
    let plan = f.compile(&engine).unwrap();
    let parts = &plan.changes().changes;
    let default = parts
        .iter()
        .position(|p| matches!(p.change, Change::AlterDomain { .. }))
        .unwrap();
    let copy = parts
        .iter()
        .position(|p| matches!(&p.change,Change::CreateType{name,..} if name==&child))
        .unwrap();
    assert!(default < copy);
    f.deps.0[1].1 = ["type::c2469.child".parse().unwrap()].into();
    let why = f.compile(&engine).unwrap_err();
    assert!(
        why.contains("cycle") || why.contains("wait on one another"),
        "{why}"
    );
}

#[test]
fn a_selected_domain_prefix_also_defers_its_existing_planned_callees() {
    let engine = Postgres::new();
    let f = Fixture::indirect_check();
    let plan = f.compile(&engine).unwrap();
    let steps = &plan.changes().changes;
    let function = steps
        .iter()
        .position(|p| matches!(p.change, Change::CreateModule { .. }))
        .unwrap();
    let check=steps.iter().position(|p|matches!(&p.change,Change::AlterDomain{from,to,half:DomainHalf::Tighten,..} if !DomainDelta::of(from,to).unwrap().add_checks.is_empty())).unwrap();
    assert!(function < check);
    let mut ordinary = Fixture::new("not_null", false);
    ordinary.schema.types.insert(
        "c2469.d".parse().unwrap(),
        domain(Some("7"), true, &[("positive", "VALUE > 0")]),
    );
    let plan = ordinary.compile(&engine).unwrap();
    let definition = plan
        .changes()
        .changes
        .iter()
        .find_map(|p| {
            if let Change::CreateType { definition, .. } = &p.change {
                Some(definition.as_ref())
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(
        definition,
        &domain(Some("7"), false, &[("positive", "VALUE > 0")])
    );
}

#[test]
fn an_actual_domain_callee_remains_a_hard_constraint() {
    let engine = Postgres::new();
    let mut f = Fixture::indirect_check();
    f.schema.modules.insert(
        "c2469.ready".parse().unwrap(),
        function("() RETURNS boolean LANGUAGE sql IMMUTABLE RETURN TRUE"),
    );
    f.deps = deps(&[
        ("not_null::type::c2469.d", &["module::c2469.ready"]),
        ("module::c2469.ready", &["type::c2469.d"]),
        (
            "module::c2469.accept(integer)",
            &["check::type::c2469.d.positive"],
        ),
    ]);
    let why = f.compile(&engine).unwrap_err();
    assert!(
        why.contains("cycle") || why.contains("wait on one another"),
        "{why}"
    );
}

#[test]
fn wire_round_trip_keeps_domain_statements_and_complete_definition_evidence() {
    let engine = Postgres::new();
    let f = Fixture::multiple();
    let canonical = f.compile(&engine).unwrap();
    for mode in [PlanMode::Transactional, PlanMode::Staged] {
        let ordered = canonical.clone().ordered(mode, &engine);
        let original = lower_all(
            &ordered as &dyn pbps_diff::order::PlanView<Postgres>,
            &engine,
        )
        .unwrap();
        let mut wire = SavedPlan::new(
            PlanOrigin::Database,
            "postgres",
            "2026-10-31T00:00:00Z",
            PlanBaseline {
                description: "domain fixture".into(),
                checksum: "baseline".into(),
                database_collation: None,
                time_zone: None,
            },
            ordered.changes().clone(),
            f.ids.clone(),
        );
        wire.mode = mode;
        wire.declared_type_pins = pbps_pg::type_refs::declared::seal(
            &Schema::default(),
            &IdsFile::default(),
            &f.schema,
            &f.ids,
        );
        let wire: SavedPlan = serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
        assert!(wire.changes.changes.iter().all(|p| p.provenance.is_none()));
        if mode == PlanMode::Staged {
            let checksum = wire.wire_fingerprint();
            let why = pbps_diff::order::admit(
                wire,
                &engine,
                std::path::Path::new("domain.plan.json"),
                &checksum,
            )
            .unwrap_err();
            assert!(
                why.to_string().contains("exactly one logical change"),
                "{why}"
            );
            continue;
        }
        let mut unexpected = wire.clone();
        for step in &mut unexpected.changes.changes {
            if let Change::CreateType { definition, .. } = &mut step.change
                && let ManagedType::Domain { checks, .. } = definition.as_mut()
            {
                checks.insert("unapproved_extra".into(), "VALUE <> 0".into());
            }
        }
        let changed = unexpected.wire_fingerprint();
        let why = pbps_diff::order::admit(
            unexpected,
            &engine,
            std::path::Path::new("domain.plan.json"),
            &changed,
        )
        .unwrap_err();
        assert!(why.to_string().contains("pinned intermediate"), "{why}");
        let mut missing = wire.clone();
        missing.changes.changes.retain(|p| {
            !matches!(&p.change, Change::AlterDomain{from,to,half:DomainHalf::Tighten,..} if DomainDelta::of(from,to).unwrap().set_not_null)
        });
        let changed = missing.wire_fingerprint();
        let why = pbps_diff::order::admit(
            missing,
            &engine,
            std::path::Path::new("domain.plan.json"),
            &changed,
        )
        .unwrap_err();
        assert!(why.to_string().contains("pinned intermediate"), "{why}");
        let checksum = wire.wire_fingerprint();
        let admitted = pbps_diff::order::admit(
            wire,
            &engine,
            std::path::Path::new("domain.plan.json"),
            &checksum,
        )
        .unwrap();
        let restored = lower_all(
            &admitted as &dyn pbps_diff::order::PlanView<Postgres>,
            &engine,
        )
        .unwrap();
        assert_eq!(
            original
                .groups()
                .iter()
                .map(|g| g.statements())
                .collect::<Vec<_>>(),
            restored
                .groups()
                .iter()
                .map(|g| g.statements())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn an_explicit_intermediate_domain_default_shadows_ancestor_prerequisites() {
    let engine = Postgres::new();
    let mut f = Fixture::new("default", false);
    for (uid, name, base, value) in [
        ("y_bbbbbb", "c2469.middle", "c2469.d", Some("11")),
        ("y_cccccc", "c2469.child", "c2469.middle", None),
    ] {
        let name: TypeRef = name.parse().unwrap();
        let mut definition = domain(value, false, &[]);
        if let ManagedType::Domain { base: ty, .. } = &mut definition {
            *ty = ColumnType::reference(base.parse().unwrap());
        }
        f.schema.types.insert(name.clone(), definition);
        f.ids.types.insert(uid.parse().unwrap(), name);
    }
    f.deps.0[1].1 = ["type::c2469.child".parse().unwrap()].into();
    let plan = f.compile(&engine).unwrap();
    let changes = &plan.changes().changes;
    let child = changes.iter().position(|p| matches!(&p.change,Change::CreateType{name,..} if name==&"c2469.child".parse().unwrap())).unwrap();
    let default = changes
        .iter()
        .position(|p| matches!(p.change, Change::AlterDomain { .. }))
        .unwrap();
    assert!(child < default);
}
