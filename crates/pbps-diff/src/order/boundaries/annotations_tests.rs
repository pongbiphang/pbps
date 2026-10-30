use super::*;
use pbps_model::object_ref::ObjectDeps;
use pbps_model::{IdsFile, Schema};

fn annotated(
    reverse: bool,
) -> (
    PlanningPlan,
    Operations,
    Vec<Effects>,
    State,
    Schema,
    IdsFile,
) {
    let (p, _, mut fx, base) = fixture();
    let mut before = Schema::default();
    before
        .tables
        .insert("app.p".parse().unwrap(), Default::default());
    before
        .tables
        .insert("app.a".parse().unwrap(), Default::default());
    let mut after = before.clone();
    for name in ["unused", "another"] {
        after.roles.insert(name.into(), Default::default());
    }
    let mut ids = IdsFile::default();
    ids.tables
        .insert("t_aaaaaa".parse().unwrap(), "app.p".parse().unwrap());
    ids.tables
        .insert("t_bbbbbb".parse().unwrap(), "app.a".parse().unwrap());
    ids.roles
        .insert("r_aaaaaa".parse().unwrap(), "unused".into());
    ids.roles
        .insert("r_bbbbbb".parse().unwrap(), "another".into());
    let (owner, provider) = if reverse {
        ("role::unused", "role::another")
    } else {
        ("role::another", "role::unused")
    };
    let goal = ObjectDeps([(owner.parse().unwrap(), [provider.parse().unwrap()].into())].into());
    let p = p
        .with_annotations(
            Snapshot {
                schema: &before,
                ids: &ids,
            },
            Snapshot {
                schema: &after,
                ids: &ids,
            },
            &ObjectDeps::default(),
            &goal,
        )
        .unwrap();
    let contract = p.annotations.clone().unwrap();
    let p = crate::order::writebacks::PlanningInput::new(vec![PlannedChange::new(
        Change::CreateRole {
            uid: "r_bbbbbb".parse().unwrap(),
            name: "another".into(),
        },
    )])
    .append_to(p);
    assert!(std::sync::Arc::ptr_eq(
        &contract,
        p.annotations.as_ref().unwrap()
    ));
    assert_eq!(p.boundaries.names(&p).changes.len(), p.logical);
    fx.push(Effects::default());
    let ops = p.identities.clone().unwrap();
    (p, ops, fx, base, before, ids)
}

#[test]
fn receipts_bind_annotation_boundaries_and_reject_unsatisfied_order() {
    let (mut p, ops, fx, base, _, _) = annotated(false);
    let annotations = p.annotation_edges().unwrap();
    assert_eq!(annotations.len(), 1);
    p.boundaries
        .discharge(ops.as_slice(), &fx, &base, &annotations)
        .unwrap();
    assert!(p.boundaries.ensure(&p).is_ok());
    let original = p.annotations.clone();
    p.annotations = None;
    assert!(p.boundaries.ensure(&p).is_err());
    p.annotations = original;
    assert!(p.boundaries.ensure(&p).is_ok());
    p.annotations = annotated(true).0.annotations;
    assert!(p.boundaries.ensure(&p).is_err());
    let annotations = p.annotation_edges().unwrap();
    let error = p
        .boundaries
        .discharge(ops.as_slice(), &fx, &base, &annotations)
        .unwrap_err();
    assert!(error.contains("annotation boundary"), "{error}");
}

#[test]
fn replay_passes_annotations_to_the_same_membership_receipt() {
    for reverse in [false, true] {
        let (mut p, _, _, _, schema, ids) = annotated(reverse);
        let side = crate::Side {
            schema: &schema,
            ids: &ids,
        };
        let error = replay(&mut p, side, side, &pbps_dialect::MinimalDialect, None).unwrap_err();
        assert!(
            error.contains(if reverse {
                "annotation boundary"
            } else {
                "no symbolic transfer"
            }),
            "{error}"
        );
        assert!(p.boundaries.ensure(&p).is_err());
    }
}

#[test]
fn annotation_paths_qualify_writer_versions_without_choosing_a_fork_or_cycle() {
    use pbps_model::operation::symbolic::*;
    let (_, original, fx, mut state, _, _) = annotated(false);
    let Clause::Move(m) = &fx[0].symbolic[0] else {
        panic!()
    };
    let key = Key {
        parent: m.parent.clone(),
        property: Property::Address,
    };
    let value = |text: &str| {
        Fact::Known(Value {
            datum: Datum::Definition(text.into()),
            bindings: Default::default(),
            ownership: Ownership::Own,
        })
    };
    state.cells.insert(
        (key.parent.clone(), key.property.clone()),
        Cell {
            value: value("base"),
            version: Version::Base,
            origin: None,
        },
    );
    let baseline = crate::order::Baseline::empty().with_symbolic(state);
    let initial: Vec<_> = (0..3)
        .map(|at| {
            if at == 2 {
                Effects::default()
            } else {
                Effects {
                    symbolic: vec![Clause::Recurse(RecursiveWrite {
                        key: key.clone(),
                        input: Version::Base,
                        from: value("base"),
                        to: value(if at == 0 { "first" } else { "second" }),
                        scope: Scope::ParentOnly,
                        policy: WritePolicy::PreserveOwnership,
                        lifetime: Lifetime::Retain,
                    })],
                    ..Default::default()
                }
            }
        })
        .collect();
    for (pairs, predecessor, successor) in [
        (vec![], None, None),
        (vec![(0, 1)], Some(0), Some(1)),
        (vec![(0, 2), (2, 1)], Some(0), Some(1)),
        (vec![(1, 0)], Some(1), Some(0)),
        (vec![(0, 1), (1, 0)], None, None),
    ] {
        let annotations: Vec<_> = pairs
            .into_iter()
            .map(|(before, after)| crate::order::Constraint {
                before,
                after,
                source: crate::order::Source::Annotation,
                reason: pbps_model::resolver::OrderReason::Structural,
            })
            .collect();
        let mut effects = initial.clone();
        bind(&original, &mut effects, &baseline, &annotations);
        if let (Some(before), Some(after)) = (predecessor, successor) {
            assert!(
                matches!(&effects[after].symbolic[..], [Clause::Recurse(w)] if w.input==Version::After(original.as_slice()[before].id()))
            );
        } else {
            assert_eq!(effects, initial, "a fork or cycle must stay unqualified");
        }
    }
}
