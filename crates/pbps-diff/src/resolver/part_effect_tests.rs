use super::*;
use pbps_model::resolver::{BoundSurface, ObjectIdentity};
use pbps_model::{Change, Column, Hints, Module, ModuleId, ModuleKind, Schema, Table, TableName};
use std::cell::Cell;

thread_local! {
    static PROJECTIONS: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn record_projection() {
    PROJECTIONS.set(PROJECTIONS.get() + 1);
}

fn measured<T>(run: impl FnOnce() -> T) -> (T, usize) {
    PROJECTIONS.set(0);
    let result = run();
    (result, PROJECTIONS.get())
}

fn identifiers(schema: &Schema, before: &IdsFile) -> IdsFile {
    crate::resolve(
        schema,
        before,
        &[],
        &crate::Context {
            operator: "1981".into(),
            today: "2026-10-14".into(),
        },
    )
    .unwrap()
    .ids
}

#[test]
fn projection_work_depends_on_steps_not_evidence_width() {
    let mut counts = Vec::new();
    for (columns, views) in [(16, 4), (64, 32)] {
        let table: TableName = "app.input".parse().unwrap();
        let mut definition = Table::default();
        for i in 0..columns {
            definition
                .columns
                .insert(format!("c{i}").into(), Column::new("int".parse().unwrap()));
        }
        let inputs: BTreeSet<_> = definition
            .columns
            .keys()
            .map(|name| Surface::Column(table.column(name)))
            .collect();
        let mut before = Schema::default();
        before.tables.insert(table.clone(), definition);
        let mut observations = Vec::new();
        for i in 0..views {
            let name = format!("app.v{i}");
            let id: ModuleId = name.parse().unwrap();
            before.modules.insert(
                id.clone(),
                Module {
                    kind: ModuleKind::View,
                    description: None,
                    definition: format!("CREATE VIEW {name} AS SELECT * FROM app.input"),
                    options: Default::default(),
                },
            );
            let bound = BoundSurface {
                object: ObjectIdentity {
                    class: "view".into(),
                    name: vec!["app".into(), format!("v{i}")],
                    signature: Vec::new(),
                },
                bindings: Vec::new(),
                managed_inputs: inputs.clone(),
            };
            observations.push(SurfaceResolution {
                surface: Surface::Module(id),
                current: Some(bound.clone()),
                desired: Some(bound),
            });
        }
        let ids = identifiers(&before, &IdsFile::default());
        let mut after = before.clone();
        for column in after.tables.get_mut(&table).unwrap().columns.values_mut() {
            column.deprecated = Some("retire later".into());
        }
        let (planned, projections) = measured(|| {
            plan(
                crate::Side {
                    schema: &before,
                    ids: &ids,
                },
                crate::Side {
                    schema: &after,
                    ids: &ids,
                },
                &Hints::default(),
                &observations,
                &pbps_dialect::MinimalDialect,
            )
            .unwrap()
        });
        assert_eq!(planned.changes.changes.len(), columns);
        assert!(
            planned
                .changes
                .changes
                .iter()
                .all(|p| { matches!(p.change, Change::SetColumnDeprecated { .. }) }),
            "declaration-only metadata must not rebuild a dependent"
        );
        println!("columns={columns} views={views} projections={projections}");
        let (assessment, assessment_projections) = measured(|| {
            assess(
                crate::Side {
                    schema: &before,
                    ids: &ids,
                },
                crate::Side {
                    schema: &after,
                    ids: &ids,
                },
                &planned.changes,
                &pbps_dialect::MinimalDialect,
            )
        });
        assert_eq!(assessment.count(Answer::Unaffected), views);
        assert!(!assessment.requires_resolution());
        println!("columns={columns} views={views} assessment_projections={assessment_projections}");
        counts.push((columns, projections, assessment_projections));
    }
    // Both ordinary rebuild selection and the prepared graph see every step.
    // Evidence width must not multiply classification work in either pass.
    for (columns, projections, assessment_projections) in counts {
        assert_eq!(projections, 2 * columns);
        assert_eq!(assessment_projections, columns);
    }
}

#[test]
fn the_prepared_graph_projects_the_split_steps_not_the_ordinary_positions() {
    use pbps_model::ValueSource;
    use pbps_model::resolver::OrderReason;

    let table: TableName = "app.t".parse().unwrap();
    let column = table.column("n");
    let mut definition = Table::default();
    let mut spec = Column::new("int".parse().unwrap());
    spec.value_source = ValueSource::Default("1".into());
    definition.columns.insert("n".into(), spec);
    let mut before = Schema::default();
    before.tables.insert(table.clone(), definition);
    let ids = identifiers(&before, &IdsFile::default());
    let mut after = before.clone();
    let spec = after
        .tables
        .get_mut(&table)
        .unwrap()
        .columns
        .get_mut("n")
        .unwrap();
    spec.value_source = ValueSource::Default("app.f()".into());
    spec.deprecated = Some("retire later".into());
    let function: ModuleId = "app.f()".parse().unwrap();
    after.modules.insert(
        function.clone(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "CREATE FUNCTION app.f() RETURNS int LANGUAGE SQL AS 'SELECT 2'".into(),
            options: Default::default(),
        },
    );
    let after_ids = identifiers(&after, &ids);
    let old = crate::Side {
        schema: &before,
        ids: &ids,
    };
    let wanted = crate::Side {
        schema: &after,
        ids: &after_ids,
    };
    let hints = Hints::default();
    let dialect = pbps_dialect::MinimalDialect;
    let ordinary = ordinary(old, wanted, &dialect, &hints).unwrap();
    assert_eq!(ordinary.changes.len(), 3);
    let prepared = prepare::changes(
        &ordinary,
        &before,
        &after,
        &after_ids,
        &ids,
        &BTreeSet::new(),
        &hints,
        &dialect,
    )
    .unwrap();
    assert_eq!(prepared.changes.changes.len(), 4);
    let bound = BoundSurface {
        object: ObjectIdentity {
            class: "default".into(),
            name: vec!["app".into(), "t".into(), "n".into()],
            signature: Vec::new(),
        },
        bindings: Vec::new(),
        managed_inputs: BTreeSet::new(),
    };
    let mut rebound = bound.clone();
    rebound
        .managed_inputs
        .insert(Surface::Module(function.clone()));
    let observations = [
        SurfaceResolution {
            surface: Surface::Default(column),
            current: Some(bound),
            desired: Some(rebound),
        },
        SurfaceResolution {
            surface: Surface::Module(function.clone()),
            current: None,
            desired: Some(BoundSurface {
                object: ObjectIdentity {
                    class: "function".into(),
                    name: vec!["app".into(), "f".into()],
                    signature: Vec::new(),
                },
                bindings: Vec::new(),
                managed_inputs: BTreeSet::new(),
            }),
        },
    ];
    let owners = pbps_model::operation::PlanOwners::new(
        pbps_model::plan_names::PlanNames::of(&ordinary.changes, &after_ids).unwrap(),
        &ids,
        &after_ids,
    );
    let (edges, projections) = measured(|| {
        graph::constraints(
            &prepared.changes,
            &prepared.origins,
            &owners,
            &observations,
            old,
            wanted,
        )
    });
    assert_eq!(projections, prepared.changes.changes.len());
    let creation = prepared
        .changes
        .changes
        .iter()
        .position(|p| {
            matches!(
                p.change,
                Change::AlterColumnDefault {
                    from: None,
                    to: Some(_),
                    ..
                }
            )
        })
        .unwrap();
    let provider = prepared
        .changes
        .changes
        .iter()
        .position(|p| matches!(&p.change, Change::CreateModule { id, .. } if id == &function))
        .unwrap();
    // Only this binding edge proves the new default is provided; the order
    // of the split pair is the effects' (DEC-1968.2).
    assert!(edges.iter().any(|e| e.before == provider
        && e.after == creation
        && e.reason == OrderReason::Binding
        && e.source == crate::order::Source::Binding));
    assert!(edges.iter().all(|e| e.before != e.after));
    let (planned, projections) =
        measured(|| plan(old, wanted, &hints, &observations, &dialect).unwrap());
    assert_eq!(
        projections,
        ordinary.changes.len() + prepared.changes.changes.len()
    );
    assert_eq!(planned.changes.changes.len(), 4);
    let steps = &planned.changes.changes;
    let at = |f: &dyn Fn(&Change) -> bool| steps.iter().position(|p| f(&p.change)).unwrap();
    // The old default is released before the new one is set, and the
    // function exists first: edges the effects and the evidence state.
    let removal = at(&|c| matches!(c, Change::AlterColumnDefault { to: None, .. }));
    let creation = at(&|c| {
        matches!(
            c,
            Change::AlterColumnDefault {
                from: None,
                to: Some(_),
                ..
            }
        )
    });
    let provider = at(&|c| matches!(c, Change::CreateModule { id, .. } if id == &function));
    assert!(removal < creation && provider < creation, "{steps:?}");
    // Negative: a deprecation emits no SQL, so nothing orders it against the
    // split pair; the old same-table rule put it between them (#1954).
    let deprecation = at(&|c| matches!(c, Change::SetColumnDeprecated { .. }));
    assert!(
        !planned.proof.edges().iter().any(|e| {
            (e.before == deprecation || e.after == deprecation)
                && [removal, creation].contains(&if e.before == deprecation {
                    e.after
                } else {
                    e.before
                })
        }),
        "{:?}",
        planned.proof.edges()
    );
    planned.proof.validate(&planned.changes).unwrap();
}
