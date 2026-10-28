use super::*;
use pbps_dialect::MinimalDialect;
use pbps_model::{Uid, UidKind, resolver::OrderReason};
fn program(
    n: usize,
    edges: &[(usize, usize)],
    parts: &[usize],
) -> Result<TrialProgram<MinimalDialect>, String> {
    let names = ChangeSet {
        changes: (0..n)
            .map(|at| {
                PlannedChange::new(Change::CreateRole {
                    uid: Uid::derived(UidKind::Role, &at.to_string(), 0),
                    name: at.to_string().into(),
                })
            })
            .collect(),
    };
    let edges: Vec<_> = edges
        .iter()
        .map(|&(before, after)| Constraint {
            before,
            after,
            source: super::super::Source::Lexical,
            reason: OrderReason::Structural,
        })
        .collect();
    TrialProgram::project(&edges, parts, &names)
}
#[test]
fn hard_readiness_survives_no_sql_projection_and_permutation() {
    // Logical 0 -> omitted 1 -> 2. Settled ranks deliberately differ.
    let p = program(4, &[(0, 1), (1, 2)], &[2, 0, 3]).unwrap();
    let mut c = p.start(0..3, &p.begin()).unwrap();
    let a = c.next_ready().unwrap();
    assert_eq!(a.position(), 1);
    let a = c.next_ready().unwrap();
    assert_eq!(a.position(), 2);
    a.succeeded();
    let a = c.next_ready().unwrap();
    assert_eq!(a.position(), 1);
    a.succeeded();
    let a = c.next_ready().unwrap();
    assert_eq!(a.position(), 0);
    a.succeeded();
    assert!(c.next_ready().is_none());
    assert!(c.finish().is_some());
}
#[test]
fn each_generation_is_bounded_and_the_same_verdicts_give_the_same_order() {
    let p = program(6, &[], &(0..6).collect::<Vec<_>>()).unwrap();
    let mut recorded = Vec::new();
    for _ in 0..2 {
        let mut c = p.start(0..6, &p.begin()).unwrap();
        let mut expected = 5;
        let mut attempts = Vec::new();
        while let Some(a) = c.next_ready() {
            let at = a.position();
            attempts.push(at);
            if at == expected {
                a.succeeded();
                expected = expected.saturating_sub(1);
            }
        }
        assert_eq!(attempts.len(), 6 * 7 / 2);
        assert!(c.finish().is_some());
        recorded.push(attempts);
    }
    assert_eq!(recorded[0], recorded[1]);
    let mut c = p.start(0..6, &p.begin()).unwrap();
    for _ in 0..6 {
        let _attempt = c.next_ready().unwrap();
    }
    assert!(c.next_ready().is_none());
    assert!(c.finish().is_none());
}
#[test]
fn syntax_waits_for_ordinary_quiescence_and_intervening_progress() {
    let p = program(3, &[], &[0, 1, 2]).unwrap();
    let mut c = p.start(0..3, &p.begin()).unwrap();
    c.next_ready().unwrap().park_syntax();
    for at in [1, 2] {
        let a = c.next_ready().unwrap();
        assert_eq!(a.position(), at);
        a.succeeded();
    }
    let a = c.next_ready().unwrap();
    assert_eq!(a.position(), 0);
    assert!(a.is_recovery());
    let p = program(1, &[], &[0]).unwrap();
    let mut c = p.start(0..1, &p.begin()).unwrap();
    c.next_ready().unwrap().park_syntax();
    assert!(c.next_ready().is_none());
}
#[test]
fn ranges_need_an_opaque_complete_prefix_and_never_pull_later_providers() {
    let p = program(2, &[(1, 0)], &[0, 1]).unwrap();
    let mut c = p.start(0..1, &p.begin()).unwrap();
    assert!(c.next_ready().is_none());
    assert_eq!(c.blocked_by(0).collect::<Vec<_>>(), [1]);
    assert!(p.start(1..2, &p.begin()).is_err());
    assert!(p.start(0..3, &p.begin()).is_err());
    let p = program(2, &[], &[0, 1]).unwrap();
    let mut c = p.start(0..1, &p.begin()).unwrap();
    c.next_ready().unwrap().succeeded();
    let prefix = c.finish().unwrap();
    let mut suffix = p.start(1..2, &prefix).unwrap();
    assert_eq!(suffix.next_ready().unwrap().position(), 1);
}
#[test]
fn hard_cycles_and_absent_endpoints_are_refused_before_trials() {
    assert!(program(2, &[(0, 1), (1, 0)], &[0, 1]).is_err());
    assert!(program(1, &[(0, 1)], &[0]).is_err());
}
