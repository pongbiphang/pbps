use super::*;
use pbps_dialect::{Dialect, DialectError};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Default)]
struct Probe {
    requests: AtomicUsize,
}
impl Dialect for Probe {
    fn name(&self) -> &'static str {
        "annotation-probe"
    }
    fn normalize_type(
        &self,
        ty: &pbps_model::ColumnType,
    ) -> Result<pbps_model::ColumnType, DialectError> {
        pbps_dialect::MinimalDialect.normalize_type(ty)
    }
    fn type_change_risk(
        &self,
        from: &pbps_model::ColumnType,
        to: &pbps_model::ColumnType,
    ) -> pbps_dialect::TypeChangeRisk {
        pbps_dialect::MinimalDialect.type_change_risk(from, to)
    }
    fn fold_ident<'a>(&self, name: &'a str) -> std::borrow::Cow<'a, str> {
        pbps_dialect::MinimalDialect.fold_ident(name)
    }
    fn quote_ident(&self, name: &str) -> Result<String, DialectError> {
        pbps_dialect::MinimalDialect.quote_ident(name)
    }
    fn lexicon(&self) -> pbps_dialect::Lexicon {
        pbps_dialect::MinimalDialect.lexicon()
    }
    fn validate_table(&self, name: &pbps_model::TableName, table: &Table) -> Vec<DialectError> {
        pbps_dialect::MinimalDialect.validate_table(name, table)
    }
    fn intermediate_tables(
        &self,
        change: &Change,
        strategy: pbps_model::Strategy,
    ) -> Result<Vec<pbps_model::TableName>, DialectError> {
        pbps_dialect::MinimalDialect.intermediate_tables(change, strategy)
    }
    fn transaction_framing(
        &self,
        zone: &pbps_model::SessionZone,
    ) -> pbps_dialect::TransactionFraming {
        pbps_dialect::MinimalDialect.transaction_framing(zone)
    }
    fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
        pbps_dialect::MinimalDialect.probe_framing()
    }
    fn annotation_parts(
        &self,
        change: &Change,
        ids: &IdsFile,
        _: &[pbps_model::ModuleId],
        _: bool,
        demanded: &BTreeSet<ObjectRef>,
        _: &BTreeSet<pbps_model::ColumnName>,
    ) -> Result<Option<Vec<Change>>, DialectError> {
        assert!(demanded.contains(&"default::app.t.x".parse().unwrap()));
        self.requests.fetch_add(1, Ordering::SeqCst);
        let Change::CreateTable { name, table, .. } = change else {
            return Ok(None);
        };
        let mut bare = change.clone();
        let Change::CreateTable { table: shape, .. } = &mut bare else {
            panic!()
        };
        shape.columns["x"].value_source = pbps_model::ValueSource::None;
        let pbps_model::ValueSource::Default(expression) = &table.columns["x"].value_source else {
            panic!()
        };
        Ok(Some(vec![
            bare,
            Change::AlterColumnDefault {
                uid: ids.column_uid(&name.column("x")).unwrap().clone(),
                column: name.column("x"),
                from: None,
                to: Some(expression.clone()),
            },
        ]))
    }
}
impl crate::order::scratch::Engine for Probe {
    type Input = crate::order::scratch::Prepared;
    type Metadata = ();
    fn blueprint(&self, input: Self::Input) -> crate::order::scratch::Input<()> {
        let parts = input
            .changes()
            .changes()
            .changes
            .iter()
            .enumerate()
            .filter(|(_, change)| {
                !matches!(&change.change, Change::CreateModule { id, .. } if *id == "app.middle".parse().unwrap())
            })
            .map(|(operation, change)| crate::order::scratch::Part {
                operation,
                change: change.clone(),
                metadata: (),
            })
            .collect();
        crate::order::scratch::Input::new(input, parts)
    }
    fn trial_order(&self) -> bool {
        true
    }
}

#[test]
fn an_annotation_requests_existing_decomposition_before_projecting_group_edges() {
    let mut f = Fixture::new();
    f.after
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns["x"]
        .value_source = pbps_model::ValueSource::Default("1".into());
    let goal = dependencies(&[
        ("default::app.t.x", &["app.f"]),
        ("app.f", &["table::app.t"]),
    ]);
    let mut plan = f.plan(f.creates(), &ObjectDeps::default(), &goal).unwrap();
    let engine = Probe::default();
    assert_eq!(
        crate::order::generated::expand(&mut plan, &f.ids, &Default::default(), &engine).unwrap(),
        1
    );
    assert_eq!(engine.requests.load(Ordering::SeqCst), 1);
    assert_eq!(Schedule::new(3).settle(plan).unwrap().sequence, [1, 0, 2]);
    let mut plain = propose(ChangeSet {
        changes: f.creates().into_iter().map(PlannedChange::new).collect(),
    });
    assert_eq!(
        crate::order::generated::expand(&mut plain, &f.ids, &Default::default(), &engine).unwrap(),
        0
    );
    assert_eq!(engine.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn scratch_hard_readiness_projects_through_a_skipped_logical_vertex() {
    let mut f = Fixture::new();
    f.after
        .modules
        .insert("app.middle".parse().unwrap(), view());
    let goal = dependencies(&[
        ("app.f", &["app.middle"]),
        ("app.middle", &["table::app.t"]),
    ]);
    let mut changes = f.creates();
    changes.insert(
        1,
        Change::CreateModule {
            id: "app.middle".parse().unwrap(),
            module: Box::new(view()),
        },
    );
    let plan = f.plan(changes, &ObjectDeps::default(), &goal).unwrap();
    let engine = Probe::default();
    let prepared =
        crate::order::scratch::prepare(&plan, &f.ids, &Default::default(), &engine).unwrap();
    assert_eq!(
        prepared
            .edges()
            .iter()
            .filter(|e| e.source == Source::Annotation)
            .count(),
        2
    );
    let settled = crate::order::scratch::settle(&engine, prepared).unwrap();
    assert_eq!(settled.parts().len(), 2);
    let trial = settled.trial().unwrap();
    assert!(!trial.can_precede(1, 0));
    assert!(trial.can_precede(0, 1));
}

#[test]
fn the_generic_verifier_checks_bound_non_module_edges() {
    let f = Fixture::new();
    let goal = dependencies(&[("app.f", &["table::app.t"])]);
    let plan = f.plan(f.creates(), &ObjectDeps::default(), &goal).unwrap();
    assert!(matches!(
        crate::order::verify::annotations(&plan).unwrap().as_slice(),
        [crate::order::Violation::BrokenDeclared { .. }]
    ));
    let result = Schedule::new(2).settle(plan).unwrap();
    assert!(
        crate::order::verify::annotations(&result.changes)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_settlement_reuses_the_logical_name_timeline_after_a_rename() {
    let mut f = Fixture::new();
    let table = f.after.tables.remove(&"app.t".parse().unwrap()).unwrap();
    f.after
        .tables
        .insert("app.renamed".parse().unwrap(), table.clone());
    f.ids
        .tables
        .insert("t_aaaaaa".parse().unwrap(), "app.renamed".parse().unwrap());
    f.ids.columns.insert(
        "c_aaaaaa".parse().unwrap(),
        "app.renamed.x".parse().unwrap(),
    );
    let changes = vec![
        Change::CreateModule {
            id: "app.f".parse().unwrap(),
            module: Box::new(view()),
        },
        Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: "app.t".parse().unwrap(),
            table: Box::new(table),
            beside_default: None,
            moves_at_most: None,
        },
        Change::RenameTable {
            uid: "t_aaaaaa".parse().unwrap(),
            from: "app.t".parse().unwrap(),
            to: "app.renamed".parse().unwrap(),
            defaults: Default::default(),
        },
    ];
    let plan = f
        .plan(
            changes,
            &ObjectDeps::default(),
            &dependencies(&[("app.f", &["table::app.renamed"])]),
        )
        .unwrap();
    let mut schedule = Schedule::new(3);
    schedule.edge(2, 0, Source::Effect, OrderReason::Structural);
    let result = schedule.settle(plan).unwrap();
    assert_eq!(result.sequence, [1, 2, 0]);
    assert!(
        result
            .changes
            .annotation_edges()
            .unwrap()
            .iter()
            .all(|e| e.before < e.after)
    );
}

#[test]
fn a_contained_surface_cannot_install_its_own_indivisible_owner_first() {
    let mut f = Fixture::new();
    f.after
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns["x"]
        .value_source = pbps_model::ValueSource::Default("1".into());
    let goal = dependencies(&[("table::app.t", &["default::app.t.x"])]);
    let plan = f.plan(f.creates(), &ObjectDeps::default(), &goal).unwrap();
    let text = plan.annotation_edges().unwrap_err();
    assert!(
        text.contains("internal creation cycle")
            && text.contains("table::app.t")
            && text.contains("default::app.t.x"),
        "{text}"
    );
}

#[test]
fn scope_tails_retain_annotations_and_reserved_event_ids() {
    use pbps_model::operation::{ModuleEdits, Operations, Origin};
    for (tracked, annotated) in [(false, true), (true, false), (true, true)] {
        let f = Fixture::new();
        let goal = dependencies(&[("app.f", &["table::app.t"])]);
        let mut plan = if annotated {
            f.plan(f.creates(), &ObjectDeps::default(), &goal).unwrap()
        } else {
            propose(ChangeSet {
                changes: f.creates().into_iter().map(PlannedChange::new).collect(),
            })
        };
        let contract = plan.annotations.clone();
        if tracked {
            let operations = Operations::of_plan(&plan.changes, ModuleEdits::Rebuilt);
            plan.allocator.reserve(&operations);
            plan.identities = Some(operations);
        }
        // A temporarily held companion must stay reserved across an append.
        let held = plan.allocator.allocate(
            [(Origin::Whole(0), plan.changes[0].clone())],
            ModuleEdits::Rebuilt,
        );
        let scope = PlannedChange::new(Change::SetDataMode {
            table: "app.t".parse().unwrap(),
            from: None,
            to: Some(pbps_model::data::DataMode::Exact),
        });
        let mut appended =
            crate::order::writebacks::PlanningInput::new(vec![scope]).append_to(plan);
        if let Some(contract) = contract {
            assert!(std::sync::Arc::ptr_eq(
                appended.annotations.as_ref().unwrap(),
                &contract
            ));
        } else {
            assert!(appended.annotations.is_none());
        }
        assert_eq!(
            appended.annotation_edges().unwrap().len(),
            usize::from(annotated)
        );
        assert_eq!(appended.identities.is_some(), tracked);
        let fresh = appended.allocator.allocate(
            [(Origin::Whole(0), appended.changes[0].clone())],
            ModuleEdits::Rebuilt,
        );
        assert_ne!(fresh.as_slice()[0].id(), held.as_slice()[0].id());
    }
}

#[test]
fn fill_placement_consumes_the_annotation_settled_managed_order() {
    use crate::order::scratch::{FillDependency, FillNode, FillWork};
    let f = Fixture::new();
    let engine = Probe::default();
    let build = |goal| {
        let plan = f.plan(f.creates(), &ObjectDeps::default(), &goal)?;
        let prepared = crate::order::scratch::prepare(&plan, &f.ids, &Default::default(), &engine)?;
        crate::order::scratch::settle(&engine, prepared)
    };
    let settled = build(dependencies(&[("app.f", &["table::app.t"])])).unwrap();
    assert!(matches!(
        settled.parts()[0].change.change,
        Change::CreateTable { .. }
    ));
    let node = |dependency| FillNode {
        name: "external dependent".into(),
        dependencies: vec![dependency],
    };
    let table = settled.parts()[0].operation;
    let program = settled
        .fill_program(&[node(FillDependency::Managed(table))])
        .unwrap();
    assert_eq!(
        program.work(),
        &[
            FillWork::Declared(0),
            FillWork::Fill(0),
            FillWork::Declared(1)
        ]
    );
    let omitted = settled
        .fill_program(&[node(FillDependency::Unavailable("app.absent".into()))])
        .unwrap();
    assert_eq!(omitted.omitted(), &[0]);
    assert_eq!(
        omitted.work(),
        &[FillWork::Declared(0), FillWork::Declared(1)]
    );
    assert!(
        build(dependencies(&[
            ("app.f", &["table::app.t"]),
            ("table::app.t", &["app.f"])
        ]))
        .is_err()
    );
}

#[test]
fn annotated_teardowns_consume_retained_input_addresses_after_a_rename() {
    use pbps_model::operation::{
        AddressChain, ModuleEdits, Operations, Origin, Source as LogicalSource,
    };
    use pbps_model::plan_names::PlanNames;
    for intermediate in [false, true] {
        let address = if intermediate {
            "app.middle"
        } else {
            "app.renamed"
        };
        let mut f = Fixture::new();
        f.after
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .checks
            .insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: "x > 0".into(),
                },
            );
        f.before = f.after.clone();
        f.base = f.ids.clone();
        let mut table = f.after.tables.remove(&"app.t".parse().unwrap()).unwrap();
        table.checks.clear();
        f.after.tables.insert("app.renamed".parse().unwrap(), table);
        f.after.modules.clear();
        f.ids
            .tables
            .insert("t_aaaaaa".parse().unwrap(), "app.renamed".parse().unwrap());
        f.ids.columns.insert(
            "c_aaaaaa".parse().unwrap(),
            "app.renamed.x".parse().unwrap(),
        );
        let mut changes = vec![
            Change::RenameTable {
                uid: "t_aaaaaa".parse().unwrap(),
                from: "app.t".parse().unwrap(),
                to: address.parse().unwrap(),
                defaults: vec![],
            },
            Change::DropModule {
                id: "app.f".parse().unwrap(),
                kind: ModuleKind::View,
            },
            Change::DropCheck {
                table: address.parse().unwrap(),
                name: "ck".into(),
            },
        ];
        if intermediate {
            changes.push(Change::RenameTable {
                uid: "t_aaaaaa".parse().unwrap(),
                from: address.parse().unwrap(),
                to: "app.renamed".parse().unwrap(),
                defaults: vec![],
            });
        }
        let mut plan = f
            .plan(
                changes,
                &dependencies(&[("check::app.t.ck", &["app.f"])]),
                &ObjectDeps::default(),
            )
            .unwrap();
        plan.origins[2] = Origin::Teardown(LogicalSource::Change(2));
        let raw = Operations::expanded(
            plan.logical,
            plan.origins
                .iter()
                .cloned()
                .zip(plan.changes.iter().cloned()),
            ModuleEdits::Rebuilt,
        );
        let names = PlanNames::of(&plan.changes, &f.ids).unwrap();
        let chain = AddressChain::bind(
            &names,
            &raw,
            &f.base,
            &f.ids,
            &Default::default(),
            &[raw.as_slice()[2].id()].into(),
        );
        let operations = raw.with_address_chain(chain);
        assert!(if intermediate {
            matches!(
                operations.as_slice()[2].origin().spelling(),
                pbps_model::operation::Spelling::Input(_)
            )
        } else {
            matches!(
                operations.as_slice()[2].origin().spelling(),
                pbps_model::operation::Spelling::Final
            )
        });
        plan.allocator.reserve(&operations);
        plan.identities = Some(operations);
        assert!(
            plan.annotation_edges()
                .unwrap()
                .iter()
                .any(|e| e.before == 2 && e.after == 1)
        );
        let settled = Schedule::new(plan.changes.len()).settle(plan).unwrap();
        assert_eq!(
            settled.sequence,
            if intermediate {
                vec![0, 2, 1, 3]
            } else {
                vec![0, 2, 1]
            }
        );
        assert!(
            settled
                .changes
                .annotation_edges()
                .unwrap()
                .iter()
                .all(|e| e.before < e.after)
        );
        let mut unbound = settled.changes;
        unbound.identities = None;
        assert!(unbound.annotation_edges().is_err());
    }
}

#[test]
fn tracked_in_place_module_edits_keep_annotation_removal_boundaries() {
    use pbps_model::operation::{ModuleEdits, Operations};
    let mut f = Fixture::new();
    f.after.modules.insert("app.g".parse().unwrap(), view());
    f.before = f.after.clone();
    f.base = f.ids.clone();
    f.after.modules.remove(&"app.g".parse().unwrap());
    f.after
        .modules
        .get_mut(&"app.f".parse().unwrap())
        .unwrap()
        .definition = "SELECT 2 AS x".into();
    let mut plan = f
        .plan(
            vec![
                Change::AlterModule {
                    id: "app.f".parse().unwrap(),
                    module: Box::new(f.after.modules[&"app.f".parse().unwrap()].clone()),
                },
                Change::DropModule {
                    id: "app.g".parse().unwrap(),
                    kind: ModuleKind::View,
                },
            ],
            &dependencies(&[("app.f", &["app.g"])]),
            &ObjectDeps::default(),
        )
        .unwrap();
    plan.identities = Some(Operations::of_plan(&plan.changes, ModuleEdits::InPlace));
    assert!(
        plan.annotation_edges()
            .unwrap()
            .iter()
            .any(|e| e.before == 0 && e.after == 1)
    );
}

#[test]
fn planning_shadow_readers_propagate_annotation_projection_errors() {
    let mut f = Fixture::new();
    f.after
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns["x"]
        .value_source = pbps_model::ValueSource::Default("1".into());
    let goal = dependencies(&[("table::app.t", &["default::app.t.x"])]);
    let mut changes = f.creates();
    changes.push(Change::InsertRow {
        table: "app.t".parse().unwrap(),
        key_column: "x".into(),
        identity_key: false,
        key: pbps_model::RowKey::from("1"),
        row: Default::default(),
        defaults: Default::default(),
        types: Default::default(),
    });
    let plan = f.plan(changes, &ObjectDeps::default(), &goal).unwrap();
    let expected = plan.annotation_edges().unwrap_err();
    assert!(expected.contains("internal creation cycle"), "{expected}");
    let before = crate::Side {
        schema: &f.before,
        ids: &f.base,
    };
    let after = crate::Side {
        schema: &f.after,
        ids: &f.ids,
    };
    let pg = pbps_dialect::MinimalDialect;
    let rewrite =
        crate::order::before_rewrites(plan.clone(), before, after, &pg, &Default::default(), None)
            .unwrap_err();
    assert_eq!(rewrite, expected);
    let release = crate::order::release_inputs::carry_inserts(
        &plan,
        Schedule::new(plan.changes.len()),
        &std::collections::BTreeSet::from([2]),
        before,
        after,
        &pg,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(release, expected);
}
