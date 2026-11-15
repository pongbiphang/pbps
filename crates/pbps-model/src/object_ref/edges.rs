//! The saved form of the declared annotations: a flat list of edges (DEC-2469.7).

use super::{ObjectDeps, ObjectRef, Problem};
use crate::ModuleDeps;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeSet;

/// One `owner -> target` annotation as the saved plan and the state hold it.
///
/// The derived order is owner first, then target. That order is part of the
/// saved bytes (see [`ObjectEdges`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObjectEdge {
    pub owner: ObjectRef,
    pub target: ObjectRef,
}

/// The `object_deps` field of a saved plan and of a state snapshot.
///
/// # Why a flat sorted list and not the in-memory map
///
/// A pair has one spelling, so a duplicate owner has no wire form and a
/// reader cannot be asked to pick between two lists for the same owner. A
/// reference cannot be a JSON object key (it is itself an object), and a map
/// keyed by its text would need a second grammar. The cost is that an owner
/// with no targets is not written; the loader never declares one and the
/// ordering passes read an empty set the same as none.
///
/// # Why the order is checked on read
///
/// The list is hashed into the plan checksum, so two spellings of one set
/// would be two plans. The order is the derived `Ord` of [`ObjectRef`], which
/// follows the variant order of `Root`: reordering those variants is a format
/// change, and `the_wire_order_follows_the_root_variants` pins it. Only
/// structure is checked here. Whether an endpoint exists, or an edge closes a
/// cycle, needs a schema and is asked where one is in hand.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ObjectEdges(Vec<ObjectEdge>);

impl ObjectEdges {
    /// The edges in canonical order, repeats dropped. Infallible: a set has no
    /// wrong order to refuse.
    pub fn from_edges(edges: impl IntoIterator<Item = ObjectEdge>) -> Self {
        Self(
            edges
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        )
    }

    /// The edges of declared annotations. Two entries for one owner are
    /// refused: they would merge on the wire and hide which one was meant.
    pub fn from_deps(deps: &ObjectDeps) -> Result<Self, Problem> {
        let mut owners = BTreeSet::new();
        for (owner, _) in &deps.0 {
            if !owners.insert(owner) {
                return Err(Problem::DuplicateOwner(owner.clone()));
            }
        }
        Ok(Self::from_edges(deps.0.iter().flat_map(
            |(owner, targets)| {
                targets.iter().map(|target| ObjectEdge {
                    owner: owner.clone(),
                    target: target.clone(),
                })
            },
        )))
    }

    /// A state row written before `object_deps` held module edges only.
    pub fn from_legacy(deps: &ModuleDeps) -> Self {
        Self::from_edges(deps.iter().flat_map(|(owner, targets)| {
            let module = |id: &crate::ModuleId| ObjectRef::whole(super::Root::Module(id.clone()));
            targets.iter().map(move |target| ObjectEdge {
                owner: module(owner),
                target: module(target),
            })
        }))
    }

    /// The in-memory form, one entry per owner.
    pub fn to_deps(&self) -> ObjectDeps {
        let mut out: Vec<(ObjectRef, BTreeSet<ObjectRef>)> = Vec::new();
        // Sorted by owner, so one owner's edges are adjacent.
        for edge in &self.0 {
            match out.last_mut() {
                Some((owner, targets)) if *owner == edge.owner => {
                    targets.insert(edge.target.clone());
                }
                _ => out.push((edge.owner.clone(), [edge.target.clone()].into())),
            }
        }
        ObjectDeps(out)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ObjectEdge> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The list as read, checked for the one form it may take.
    fn from_wire(edges: Vec<ObjectEdge>) -> Result<Self, String> {
        let shown = |e: &ObjectEdge| format!("{} -> {}", e.owner.shown(), e.target.shown());
        for pair in edges.windows(2) {
            match pair[0].cmp(&pair[1]) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    return Err(format!("`object_deps` lists {} twice", shown(&pair[0])));
                }
                std::cmp::Ordering::Greater => {
                    return Err(format!(
                        "`object_deps` is not sorted: {} comes before {}",
                        shown(&pair[0]),
                        shown(&pair[1])
                    ));
                }
            }
        }
        Ok(Self(edges))
    }
}

impl<'de> Deserialize<'de> for ObjectEdges {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{Error, SeqAccess, Visitor};
        /// Reads the list one edge at a time, so that a bad one is named by
        /// its position; a bare serde message would say only that something in
        /// the list was wrong.
        ///
        /// Each edge is read straight from the input and never through a
        /// generic JSON value. A value keeps the last of two members with one
        /// name, so a repeated member would be gone before the typed readers
        /// that refuse it ran.
        struct Edges;
        impl<'de> Visitor<'de> for Edges {
            type Value = Vec<ObjectEdge>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a list of `object_deps` edges")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut edges = Vec::new();
                loop {
                    let at = edges.len();
                    match seq.next_element::<ObjectEdge>() {
                        Ok(Some(edge)) => edges.push(edge),
                        Ok(None) => return Ok(edges),
                        Err(e) => {
                            return Err(A::Error::custom(format!("`object_deps` edge {at}: {e}")));
                        }
                    }
                }
            }
        }
        Self::from_wire(d.deserialize_seq(Edges)?).map_err(D::Error::custom)
    }
}
