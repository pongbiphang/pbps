use super::*;
use pbps_model::operation::PartKey;

const NAME: &str = "part\n\u{1b}\u{202e}\\n\\u{1b}\\u{202e}\\雪é";
const SHOWN: &str = r"part\n\u{1b}\u{202e}\\n\\u{1b}\\u{202e}\\雪é";

#[test]
fn missing_symbolic_properties_show_names_once_without_changing_lookup_keys() {
    let mut f = Fixture::new(1);
    let property = Property::Part(PartKey::Check(NAME.into()));
    let lookalike = Property::Part(PartKey::Check(NAME.replace('\n', r"\n").into()));
    f.parent = property.clone();
    f.base.cells.insert(
        (f.p.clone(), lookalike.clone()),
        cell_value(&f.p, &lookalike, value("1")),
    );
    f.write(0, Version::Base, value("1"), value("7"));
    let before = (f.base.clone(), f.fx.clone());
    let detail = format!("missing property Part(Check(\"{SHOWN}\")) of t_aaaaaa");
    let expected = format!("{}: {detail}; {REMEDY}", f.ops[0].id());
    let graph = f.graph();
    assert_eq!(graph.symbolic_unmet.len(), 1);
    assert_eq!(graph.symbolic_unmet[0].to_string(), expected);
    assert_eq!(
        project(&f.ops, &f.fx, &f.base).unwrap_err().to_string(),
        expected
    );
    assert!(f.refusal().contains(&expected));
    assert!(matches!(
        Pending::new(&[], &[], &f.base).lookup(&f.p, &property),
        Lookup::Unavailable(why) if why == detail
    ));
    assert_eq!((f.base.clone(), f.fx.clone()), before);
    assert!(matches!(&property, Property::Part(PartKey::Check(n)) if n.raw() == NAME));
    let keys = BTreeSet::from([lookalike.clone(), property.clone()]);
    assert_eq!(keys.len(), 2);
    assert_eq!(keys.first(), Some(&property));

    f.base.cells.insert(
        (f.p.clone(), property.clone()),
        cell_value(&f.p, &property, value("1")),
    );
    assert!(f.admits());
    assert!(project(&f.ops, &f.fx, &f.base).is_ok());
    assert!(f.base.cells.contains_key(&(f.p.clone(), lookalike)));
}

#[test]
fn symbolic_boundaries_show_keys_once_and_keep_the_same_admissible_orders() {
    let mut f = Fixture::new(2);
    f.parent = Property::Part(PartKey::Check(NAME.into()));
    f.base.cells.insert(
        (f.p.clone(), f.parent.clone()),
        cell_value(&f.p, &f.parent, value("1")),
    );
    f.base
        .cells
        .get_mut(&(f.a.clone(), f.local.clone()))
        .unwrap()
        .value = Fact::Known(value("7"));
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::SameDefinition,
        TransferPolicy::Inherit,
        inherited("impossible"),
    );
    let before = (f.base.clone(), f.fx.clone());
    let key = format!(
        "Key {{ parent: {:?}, property: Part(Check(\"{SHOWN}\")) }}",
        f.p
    );
    let expected = format!(
        "{}: no boundary satisfies {key} for child t_bbbbbb; {REMEDY}",
        f.ops[1].id()
    );
    let graph = f.graph();
    assert_eq!(graph.symbolic_unmet.len(), 1);
    assert_eq!(graph.symbolic_unmet[0].to_string(), expected);
    assert!(graph.edges.is_empty());
    assert!(!f.admits());
    assert_eq!((f.base.clone(), f.fx.clone()), before);

    let Clause::Move(m) = &mut f.fx[1].symbolic[0] else {
        unreachable!()
    };
    let Boundary::All(transfers) = &mut m.boundary else {
        unreachable!()
    };
    transfers[0].goal = Fact::Known(inherited("7"));
    assert!(f.admits());
    assert_eq!(f.edges(), BTreeSet::from([(f.ops[0].id(), f.ops[1].id())]));
    assert!(project(&f.ops, &f.fx, &f.base).is_ok());
    let backwards = f.reordered(&[1, 0]);
    assert_eq!(
        project(&backwards.ops, &backwards.fx, &backwards.base)
            .unwrap_err()
            .to_string(),
        format!(
            "{}: attach shape differs at Part(Check(\"{SHOWN}\")); {REMEDY}",
            f.ops[1].id()
        )
    );
}

#[test]
fn intra_operation_boundary_reasons_show_names_once() {
    let mut f = Fixture::new(1);
    f.parent = Property::Part(PartKey::Check(NAME.into()));
    f.base.cells.insert(
        (f.p.clone(), f.parent.clone()),
        cell_value(&f.p, &f.parent, value("1")),
    );
    f.base
        .cells
        .get_mut(&(f.a.clone(), f.local.clone()))
        .unwrap()
        .value = Fact::Known(value("7"));
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        0,
        MoveKind::Attach,
        ShapeCheck::SameDefinition,
        TransferPolicy::Inherit,
        inherited("7"),
    );
    f.fx[0].symbolic.reverse();
    let key = format!(
        "Key {{ parent: {:?}, property: Part(Check(\"{SHOWN}\")) }}",
        f.p
    );
    let graph = f.graph();
    assert_eq!(graph.symbolic_unmet.len(), 1);
    assert_eq!(
        graph.symbolic_unmet[0].to_string(),
        format!(
            "{}: symbolic clause 1 must precede clause 0 for Boundary({key}); {REMEDY}",
            f.ops[0].id()
        )
    );
    assert!(!f.admits());
    assert!(project(&f.ops, &f.fx, &f.base).is_err());
    f.fx[0].symbolic.reverse();
    assert!(f.admits());
    assert!(project(&f.ops, &f.fx, &f.base).is_ok());
}

#[test]
fn unsupported_symbolic_facts_show_reasons_once_through_requirements_merges_and_pending() {
    let mut f = Fixture::new(1);
    let detail = format!("unsupported capability (Some(\"{SHOWN}\")): {SHOWN}");
    let expected = format!("{}: {detail}; {REMEDY}", f.ops[0].id());
    f.fx[0].symbolic.push(Clause::Require(Fact::Unsupported {
        sqlstate: Some(NAME.into()),
        reason: NAME.into(),
    }));
    let before = f.fx.clone();
    assert_eq!(f.graph().symbolic_unmet[0].to_string(), expected);
    assert_eq!(
        project(&f.ops, &f.fx, &f.base).unwrap_err().to_string(),
        expected
    );
    assert_eq!(f.fx, before);
    assert!(f.graph().edges.is_empty());
    f.fx[0].symbolic = vec![Clause::Require(Fact::Known(()))];
    assert!(f.admits());
    assert!(project(&f.ops, &f.fx, &f.base).is_ok());

    f.base
        .cells
        .get_mut(&(f.p.clone(), f.parent.clone()))
        .unwrap()
        .value = Fact::Unsupported {
        sqlstate: Some(NAME.into()),
        reason: NAME.into(),
    };
    let before = f.base.clone();
    assert!(matches!(
        Pending::new(&[], &[], &f.base).lookup(&f.p, &f.parent),
        Lookup::Unavailable(why) if why == detail
    ));
    assert_eq!(f.base, before);

    let mut merge = Fixture::new(1);
    merge.movement(
        0,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Merge(Merge {
            cases: vec![MergeCase {
                parent: value("1"),
                candidate: value("9"),
                result: Fact::Unsupported {
                    sqlstate: Some(NAME.into()),
                    reason: NAME.into(),
                },
            }],
        }),
        value("9"),
    );
    let before = (merge.base.clone(), merge.fx.clone());
    assert_eq!(merge.graph().symbolic_unmet[0].to_string(), expected);
    assert_eq!(
        project(&merge.ops, &merge.fx, &merge.base)
            .unwrap_err()
            .to_string(),
        expected
    );
    assert_eq!((merge.base.clone(), merge.fx.clone()), before);
    assert!(merge.graph().edges.is_empty());
}

#[test]
fn completed_symbolic_prose_and_static_refusals_are_printed_verbatim() {
    let mut f = Fixture::new(1);
    f.fx[0].symbolic.push(Clause::Require(Fact::Unread));
    let expected = format!(
        "{}: the required catalog fact was not read; {REMEDY}",
        f.ops[0].id()
    );
    assert_eq!(f.graph().symbolic_unmet[0].to_string(), expected);
    assert_eq!(
        project(&f.ops, &f.fx, &f.base).unwrap_err().to_string(),
        expected
    );
    let tool_sentence = r"run `pbps plan --output 'C:\plans\next'`";
    assert_eq!(
        problem(f.ops[0].id(), tool_sentence).to_string(),
        format!(
            "{}: run `pbps plan --output 'C:\\plans\\next'`; {REMEDY}",
            f.ops[0].id()
        )
    );
}
