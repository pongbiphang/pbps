//! The former per-kind production assertions, sharing class.rs's independent replay.
use super::*;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(untagged)]
enum Step {
    Change(Box<Change>),
    Attach { attach: bool },
}
#[derive(Deserialize)]
struct Added {
    column: Column,
    candidate: bool,
}
#[derive(Deserialize)]
struct Case {
    label: String,
    #[serde(default)]
    rename: bool,
    defaults: Option<[Option<String>; 2]>,
    not_null: Option<[bool; 4]>,
    added: Option<Added>,
    address: Option<TableName>,
    #[serde(default)]
    clear_own: bool,
    steps: Vec<Step>,
    inputs: Vec<Vec<usize>>,
    #[serde(default)]
    input_operations: bool,
    sequence: Option<Vec<usize>>,
    #[serde(default)]
    boundary: bool,
    #[serde(default)]
    independent: Vec<[usize; 2]>,
    #[serde(default)]
    no_boundary: Vec<[usize; 2]>,
    #[serde(default)]
    edges: Vec<[usize; 2]>,
    #[serde(default)]
    unmet_empty: bool,
}
impl Case {
    fn build(&self) -> (Scene, Vec<Change>) {
        let mut s = scene(self.rename);
        let p = name("app.p");
        let a = name("app.a");
        if let Some(values) = &self.defaults {
            for (schema, value) in [(&mut s.before, &values[0]), (&mut s.after, &values[1])] {
                schema
                    .tables
                    .get_mut(&p)
                    .unwrap()
                    .columns
                    .get_mut("d")
                    .unwrap()
                    .value_source = value
                    .clone()
                    .map_or(ValueSource::None, ValueSource::Default);
            }
        }
        if let Some([old, new, candidate, own]) = self.not_null {
            s.before
                .tables
                .get_mut(&p)
                .unwrap()
                .columns
                .get_mut("d")
                .unwrap()
                .nullable = !old;
            s.after
                .tables
                .get_mut(&p)
                .unwrap()
                .columns
                .get_mut("d")
                .unwrap()
                .nullable = !new;
            s.before
                .tables
                .get_mut(&a)
                .unwrap()
                .columns
                .get_mut("d")
                .unwrap()
                .nullable = !candidate;
            s.after
                .tables
                .get_mut(&a)
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap()
                .columns
                .get_mut("d")
                .unwrap()
                .not_null = own;
        }
        if let Some(added) = &self.added {
            s.after
                .tables
                .get_mut(&p)
                .unwrap()
                .columns
                .insert("x".into(), added.column.clone());
            s.ids.columns.insert(uid("c_eeeeee"), p.column("x"));
            if added.candidate {
                s.before
                    .tables
                    .get_mut(&a)
                    .unwrap()
                    .columns
                    .insert("x".into(), added.column.clone());
                s.base_ids.columns.insert(uid("c_ffffff"), a.column("x"));
            }
        }
        if self.clear_own {
            s.after
                .tables
                .get_mut(&a)
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap()
                .columns
                .clear();
        }
        if let Some(new) = &self.address {
            let parent = s.after.tables.remove(&p).unwrap();
            s.after.tables.insert(new.clone(), parent);
            s.after
                .tables
                .get_mut(&a)
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap()
                .parent = new.clone();
            s.ids.tables.insert(uid("t_aaaaaa"), new.clone());
            for at in s.ids.columns.values_mut().filter(|at| at.table == p) {
                at.table = new.clone();
            }
        }
        let steps = self
            .steps
            .iter()
            .map(|step| match step {
                Step::Change(c) => c.as_ref().clone(),
                Step::Attach { attach: true } => attach(&s),
                Step::Attach { attach: false } => panic!("{}: missing attach", self.label),
            })
            .collect();
        (s, steps)
    }
}
#[test]
fn production_cut_data_keeps_every_order_edge_and_replay_assertion() {
    let cases: Vec<Case> = serde_json::from_str(include_str!("cuts.json")).unwrap();
    assert_cases(&cases);
}
fn assert_cases(cases: &[Case]) {
    let mut count = 0;
    for case in cases {
        let (s, steps) = case.build();
        for input in &case.inputs {
            let logical: Vec<_> = input.iter().map(|&i| steps[i].clone()).collect();
            let (order, graph) = if case.input_operations {
                read_input(&s, steps.clone(), Some(input))
            } else {
                read(&s, logical)
            };
            if let Some(sequence) = &case.sequence {
                assert_eq!(
                    &order.iter().map(|&i| input[i]).collect::<Vec<_>>(),
                    sequence,
                    "{} / {input:?}",
                    case.label
                );
            }
            let index = |logical| input.iter().position(|&i| i == logical).unwrap();
            let between = |e: &pbps_diff::order::Edge, a, b| {
                (e.before == index(a) && e.after == index(b))
                    || (e.before == index(b) && e.after == index(a))
            };
            if case.boundary {
                assert!(
                    graph
                        .edges
                        .iter()
                        .any(|e| matches!(e.because, pbps_diff::order::Because::Boundary(_))),
                    "{}",
                    case.label
                );
            }
            for &[a, b] in &case.independent {
                assert!(
                    !graph.edges.iter().any(|e| between(e, a, b)),
                    "{}: {:?}",
                    case.label,
                    graph.edges
                );
            }
            for &[a, b] in &case.no_boundary {
                assert!(
                    !graph.edges.iter().any(|e| between(e, a, b)
                        && matches!(e.because, pbps_diff::order::Because::Boundary(_))),
                    "{}: {:?}",
                    case.label,
                    graph.edges
                );
            }
            for &[a, b] in &case.edges {
                assert!(
                    graph
                        .edges
                        .iter()
                        .any(|e| e.before == index(a) && e.after == index(b)),
                    "{}: {:?}",
                    case.label,
                    graph.edges
                );
            }
            if case.unmet_empty {
                assert!(graph.symbolic_unmet.is_empty(), "{}", case.label);
            }
            count += 1;
        }
    }
    assert_eq!(cases.len(), 13);
    assert_eq!(count, 22);
}

#[test]
fn a_corrupted_cut_order_is_rejected_by_the_same_oracle() {
    let mut cases: Vec<Case> = serde_json::from_str(include_str!("cuts.json")).unwrap();
    cases[0].sequence = Some(vec![1, 0]);
    assert!(std::panic::catch_unwind(|| assert_cases(&cases)).is_err());
}
