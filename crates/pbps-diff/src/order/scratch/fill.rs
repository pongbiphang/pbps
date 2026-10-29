//! Scratch-only work around immutable managed groups (DEC-2134.1).
use super::{Engine, ScratchPlan};
use std::{
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
};

/// A prerequisite is a compiler source identity, never an executable change.
#[derive(Debug, Clone)]
pub enum FillDependency {
    Managed(usize),
    Fill(usize),
    /// Proven absence; an unreadable catalog must fail before construction.
    Unavailable(String),
}
#[derive(Debug, Clone)]
pub struct FillNode {
    pub name: String,
    pub dependencies: Vec<FillDependency>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillWork {
    Declared(usize),
    Fill(usize),
}
/// Placement cannot reorder or manufacture a managed operation or its SQL.
#[derive(Debug, Clone)]
pub struct FillProgram<E: Engine> {
    work: Vec<FillWork>,
    omitted: Vec<usize>,
    engine: PhantomData<fn() -> E>,
}
impl<E: Engine> FillProgram<E> {
    pub fn work(&self) -> &[FillWork] {
        &self.work
    }
    #[cfg(feature = "test-fixtures")]
    pub fn fixture_insert_fill(mut self, position: usize, fill: usize) -> Self {
        self.work.insert(position, FillWork::Fill(fill));
        self
    }
    pub fn omitted(&self) -> &[usize] {
        &self.omitted
    }
}
impl<E: Engine> ScratchPlan<E> {
    /// Complete external placement before lowering, retaining the settled
    /// managed order. A generated suffix supplies its own source operation.
    pub fn fill_program(&self, nodes: &[FillNode]) -> Result<FillProgram<E>, String> {
        place(
            &self.parts.iter().map(|p| p.operation).collect::<Vec<_>>(),
            nodes,
        )
    }
}
fn place<E: Engine>(managed: &[usize], nodes: &[FillNode]) -> Result<FillProgram<E>, String> {
    let positions: BTreeMap<_, _> = managed
        .iter()
        .enumerate()
        .map(|(i, op)| (*op, i + 1))
        .collect();
    let mut needed = vec![BTreeSet::new(); nodes.len()];
    let mut users = vec![Vec::new(); nodes.len()];
    for (i, node) in nodes.iter().enumerate() {
        for dependency in &node.dependencies {
            if let FillDependency::Fill(before) = dependency {
                if *before >= nodes.len() {
                    return Err(format!("{} names an unknown fill prerequisite", node.name));
                }
                needed[i].insert(*before);
            }
        }
        for &before in &needed[i] {
            users[before].push(i);
        }
    }
    let mut ready: BTreeSet<_> = nodes
        .iter()
        .enumerate()
        .filter(|(i, _)| needed[*i].is_empty())
        .map(|(i, node)| (node.name.clone(), i))
        .collect();
    let mut anchors = vec![None; nodes.len()];
    let mut buckets = vec![Vec::new(); managed.len() + 1];
    let mut omitted = Vec::new();
    let mut visited = 0;
    while let Some((_, at)) = ready.pop_first() {
        let mut anchor = Some(0);
        for dependency in &nodes[at].dependencies {
            let before = match dependency {
                FillDependency::Managed(op) => positions.get(op).copied(),
                FillDependency::Fill(i) => anchors[*i],
                FillDependency::Unavailable(_) => None,
            };
            anchor = anchor.zip(before).map(|(a, b)| a.max(b));
        }
        anchors[at] = anchor;
        match anchor {
            Some(at_position) => buckets[at_position].push(at),
            None => omitted.push(at),
        }
        visited += 1;
        for &user in &users[at] {
            needed[user].remove(&at);
            if needed[user].is_empty() {
                ready.insert((nodes[user].name.clone(), user));
            }
        }
    }
    if visited != nodes.len() {
        let names = nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| !needed[*i].is_empty())
            .map(|(_, n)| pbps_model::escape::Escaped(&n.name).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!("fill dependency cycle: {names}"));
    }
    let mut work = Vec::with_capacity(managed.len() + nodes.len() - omitted.len());
    for (position, fills) in buckets.into_iter().enumerate() {
        work.extend(fills.into_iter().map(FillWork::Fill));
        if position < managed.len() {
            work.push(FillWork::Declared(position));
        }
    }
    Ok(FillProgram {
        work,
        omitted,
        engine: PhantomData,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use FillDependency::{Fill, Managed, Unavailable};
    use FillWork::{Declared as D, Fill as F};
    use pbps_dialect::MinimalDialect;
    fn node(name: &str, dependencies: Vec<FillDependency>) -> FillNode {
        FillNode {
            name: name.into(),
            dependencies,
        }
    }
    fn program(managed: &[usize], nodes: &[FillNode]) -> FillProgram<MinimalDialect> {
        place(managed, nodes).unwrap()
    }
    #[test]
    fn each_fill_follows_its_own_last_prerequisite_without_holding_an_independent_fill() {
        let nodes = [
            node("early", vec![Managed(10)]),
            node("late", vec![Managed(10), Managed(30)]),
            node("dependent", vec![Fill(1)]),
        ];
        assert_eq!(
            program(&[10, 20, 30], &nodes).work(),
            [D(0), F(0), D(1), D(2), F(1), F(2)]
        );
        // Removing the late relation changes only that fill's readiness.
        let nodes = [
            nodes[0].clone(),
            node("late", vec![Managed(10)]),
            nodes[2].clone(),
        ];
        assert_eq!(
            program(&[10, 20, 30], &nodes).work(),
            [D(0), F(0), F(1), F(2), D(1), D(2)]
        );
    }
    #[test]
    fn proven_absence_prunes_only_its_transitive_dependants() {
        let nodes = [
            node("absent", vec![Unavailable("app.renamed".into())]),
            node("dependent", vec![Fill(0)]),
            node("independent", vec![Managed(10)]),
            node("empty", vec![]),
        ];
        let p = program(&[10], &nodes);
        assert_eq!(p.omitted(), [0, 1]);
        assert_eq!(p.work(), [F(3), D(0), F(2)]);
        assert_eq!(
            program(&[10], &[node("missing operation", vec![Managed(20)])]).omitted(),
            [0]
        );
    }
    #[test]
    fn independent_ties_are_named_and_a_cycle_is_never_a_partial_program() {
        let nodes = [node("z", vec![Managed(10)]), node("a", vec![Managed(10)])];
        assert_eq!(program(&[10], &nodes).work(), [D(0), F(1), F(0)]);
        let cycle = [node("ext.a", vec![Fill(1)]), node("ext.b", vec![Fill(0)])];
        let error = place::<MinimalDialect>(&[10], &cycle).unwrap_err();
        assert!(error.contains("ext.a") && error.contains("ext.b"));
        assert!(place::<MinimalDialect>(&[], &[node("unknown", vec![Fill(2)])]).is_err());
    }
}
