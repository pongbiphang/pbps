use super::*;

fn auxiliary(after: Vec<Node>, before: Vec<Node>) -> Auxiliary<usize> {
    Auxiliary {
        name: "external ext.x".into(),
        after,
        before,
        metadata: 99,
    }
}

#[test]
fn auxiliary_is_a_hard_boundary_inside_the_compile_range() {
    let changes = vec![role("first"), role("last"), role("consumer")];
    let plan = settle(
        &pbps_dialect::MinimalDialect,
        Input::new(prepared(&changes, &[(0, 1)]), parts(&changes, &[0, 1, 2])).with_auxiliary(
            vec![auxiliary(
                vec![Node::Operation(0), Node::Operation(1)],
                vec![Node::Operation(2)],
            )],
        ),
    )
    .unwrap();
    assert!(plan.trial().is_none());
    // A compile range includes this node as well as the managed groups. A
    // managed trial may reorder neither side across it, or retry the setup
    // connection's work. No auxiliary borrows an operation ID or origin.
    assert_eq!(
        plan.steps(),
        [
            Step::Managed { part: 0 },
            Step::Managed { part: 1 },
            Step::Auxiliary { metadata: 99 },
            Step::Managed { part: 2 }
        ]
    );
    assert_eq!(
        plan.operations()
            .iter()
            .map(|o| o.id().to_string())
            .collect::<Vec<_>>(),
        ["0.0", "1.0", "2.0"]
    );
    assert_eq!(plan.changes().changes.len(), 3);
    let ordinary = settle(
        &pbps_dialect::MinimalDialect,
        Input::new(prepared(&changes, &[(0, 1)]), parts(&changes, &[0, 1, 2])),
    )
    .unwrap();
    assert_eq!(
        ordinary.steps(),
        [
            Step::Managed { part: 0 },
            Step::Managed { part: 1 },
            Step::Managed { part: 2 }
        ]
    );
}

#[test]
fn auxiliary_waits_through_skipped_operations_and_can_supply_another_auxiliary() {
    let changes = vec![role("consumer"), role("skipped"), role("producer")];
    let mut second = auxiliary(vec![Node::Auxiliary(0)], vec![Node::Operation(0)]);
    second.metadata = 100;
    let plan = settle(
        &pbps_dialect::MinimalDialect,
        Input::new(prepared(&changes, &[(2, 1)]), parts(&changes, &[0, 2]))
            .with_auxiliary(vec![auxiliary(vec![Node::Operation(1)], vec![]), second]),
    )
    .unwrap();
    assert_eq!(
        plan.steps(),
        [
            Step::Managed { part: 0 },
            Step::Auxiliary { metadata: 99 },
            Step::Auxiliary { metadata: 100 },
            Step::Managed { part: 1 }
        ]
    );
    assert_eq!(
        plan.operations()
            .iter()
            .map(|o| o.id().to_string())
            .collect::<Vec<_>>(),
        ["2.0", "0.0"]
    );
}

#[test]
fn auxiliary_missing_prerequisites_and_cycles_refuse_by_name() {
    let changes = vec![role("managed")];
    for node in [
        auxiliary(vec![Node::Operation(1)], vec![]),
        auxiliary(vec![Node::Operation(0)], vec![Node::Operation(0)]),
    ] {
        let why = settle(
            &pbps_dialect::MinimalDialect,
            Input::new(prepared(&changes, &[]), parts(&changes, &[0])).with_auxiliary(vec![node]),
        )
        .unwrap_err();
        assert!(why.contains("external ext.x"), "{why}");
        assert!(why.contains("missing") || why.contains("managed"), "{why}");
    }
}

#[test]
fn empty_auxiliary_route_retains_dropped_guesses_before_part_projection() {
    let changes = vec![role("first"), role("omitted"), role("last")];
    let mut facts = prepared(&changes, &[(0, 1), (1, 0), (1, 2)]);
    facts.edges[1].source = crate::order::Source::Guess;
    // Part projection skips operation 1 and reverses the input parts. Trace
    // still names the prepared operations, including the skipped cycle member.
    let at = [Some(1), None, Some(0)];
    let mut dropped = Vec::new();
    let (managed, _) =
        super::super::auxiliary::settle::<usize>(&facts, &at, &[], &mut dropped).unwrap();
    assert_eq!(managed, [0, 1, 2]);
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].edge, facts.edges[1]);
    assert_eq!(dropped[0].members, [0, 1]);
    assert!(dropped[0].backward_only);

    let mut forward = prepared(&changes, &[(0, 1), (1, 2)]);
    forward.edges[0].source = crate::order::Source::Guess;
    let mut kept = Vec::new();
    super::super::auxiliary::settle::<usize>(&forward, &at, &[], &mut kept).unwrap();
    assert!(kept.is_empty());
}
