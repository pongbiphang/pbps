//! Lossless dependency evidence; closure validation never orders operations (DEC-2298.5).
use std::collections::{BTreeMap, BTreeSet};

use super::{Dependent, Holds};
use pbps_model::{ModuleId, Uid, UidKind, escape::Escaped};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Root {
    Module(ModuleId),
    Table(Uid),
}

/// A successful empty read is Present(empty), never Absent or Unreadable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Fact<T> {
    #[default]
    Absent,
    Present(T),
    Unreadable(String),
}

impl<T> Fact<T> {
    pub fn require(&self, name: &str) -> Result<&T, String> {
        match self {
            Self::Present(value) => Ok(value),
            Self::Absent => Err(format!("{}: catalog fact is absent", Escaped(name))),
            Self::Unreadable(why) => Err(format!(
                "{}: catalog fact is unreadable: {}",
                Escaped(name),
                Escaped(why)
            )),
        }
    }
}

/// Catalog-local identity, including attribute identity; descriptions are not keys.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Object {
    pub catalog: String,
    pub id: u64,
    pub attribute: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ownership {
    Composite,
    Array,
    ReturnRule,
    KeyIndex,
    PartitionClone,
    ConstraintTrigger,
    Attribute,
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Held {
    Root(Root),
    Dependent(Holds),
    /// The action owner, not a reason to discard the implementation's edges.
    Owned {
        by: Object,
        kind: Ownership,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Binding {
    Ownership(Ownership),
    Object,
    Column,
    Argument { position: u32, array: bool },
    Result { array: bool },
    ForeignKey { key: Object },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Link {
    pub dependent: Object,
    /// The engine's dependency category, including internal/partition edges.
    pub kind: String,
    pub binding: Binding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub described: String,
    pub held: Held,
    /// Present(empty) proves this node was visited, rather than merely discovered.
    pub dependents: Fact<BTreeSet<Link>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    pub roots: BTreeMap<Root, Fact<Object>>,
    pub nodes: BTreeMap<Object, Fact<Node>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Closure {
    /// Validated ultimate action owner, shared by restoration and name slots.
    pub owners: BTreeMap<Object, Object>,
    pub nodes: BTreeMap<Object, Node>,
    pub edges: BTreeSet<(Object, Link)>,
    pub surfaces: BTreeMap<Object, Dependent>,
    /// Referenced key evidence does not make the key a teardown obligation.
    pub bindings: BTreeMap<Object, Node>,
}

impl Graph {
    fn node(&self, id: &Object) -> Result<&Node, String> {
        self.nodes
            .get(id)
            .unwrap_or(&Fact::Absent)
            .require(&format!("{id:?}"))
    }

    pub fn closure(&self, root: &Root) -> Result<Closure, String> {
        if matches!(root, Root::Table(uid) if uid.kind() != UidKind::Table) {
            return Err("dependency root requires a table UID".into());
        }
        let start = self
            .roots
            .get(root)
            .unwrap_or(&Fact::Absent)
            .require(&format!("{root:?}"))?;
        if self.node(start)?.held != Held::Root(root.clone()) {
            return Err(format!(
                "{}: catalog root binding disagrees",
                Escaped(&self.node(start)?.described)
            ));
        }
        let mut out = Closure::default();
        let mut pending = vec![start.clone()];
        while let Some(id) = pending.pop() {
            if out.nodes.contains_key(&id) {
                continue;
            }
            let node = self.node(&id)?;
            for link in node.dependents.require(&node.described)? {
                // Keep the edge even when its endpoint was visited through another parent.
                out.edges.insert((id.clone(), link.clone()));
                pending.push(link.dependent.clone());
                if let Binding::ForeignKey { key } = &link.binding {
                    let mut key = key;
                    let mut seen = BTreeSet::new();
                    loop {
                        let bound = self.node(key)?;
                        if !seen.insert(key) {
                            return Err(format!(
                                "{}: binding ownership cycle",
                                Escaped(&bound.described)
                            ));
                        }
                        out.bindings.insert(key.clone(), bound.clone());
                        let Held::Owned { by, .. } = &bound.held else {
                            break;
                        };
                        key = by;
                    }
                }
            }
            if let Held::Owned { by, .. } = &node.held {
                pending.push(by.clone());
            }
            if let Held::Dependent(holds) = &node.held {
                out.surfaces.insert(
                    id.clone(),
                    Dependent {
                        described: node.described.clone(),
                        holds: holds.clone(),
                    },
                );
            }
            out.nodes.insert(id, node.clone());
        }
        let mut owners = BTreeMap::new();
        for id in out.nodes.keys() {
            let mut owner = id;
            let mut seen = BTreeSet::new();
            while let Held::Owned { by, .. } = &out.nodes[owner].held {
                if !seen.insert(owner) {
                    return Err(format!(
                        "{}: ownership cycle",
                        Escaped(&out.nodes[owner].described)
                    ));
                }
                owner = by;
            }
            owners.insert(id, owner);
        }
        // Validate the quotient for cycles, without returning an order or selecting a rank.
        let mut waits: BTreeMap<_, BTreeSet<_>> =
            owners.values().map(|id| (*id, BTreeSet::new())).collect();
        let mut users: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
        for (from, link) in &out.edges {
            let (a, b) = (owners[from], owners[&link.dependent]);
            if a != b || !matches!(link.binding, Binding::Ownership(_)) {
                waits.get_mut(b).unwrap().insert(a);
                users.entry(a).or_default().insert(b);
            }
        }
        let mut ready: Vec<_> = waits
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(id, _)| *id)
            .collect();
        while let Some(id) = ready.pop() {
            waits.remove(id);
            for next in users.get(id).into_iter().flatten() {
                let deps = waits.get_mut(next).unwrap();
                if deps.remove(id) && deps.is_empty() {
                    ready.push(*next);
                }
            }
        }
        if !waits.is_empty() {
            return Err(format!(
                "dependency cycle blocks {}",
                waits
                    .keys()
                    .map(|id| Escaped(&out.nodes[*id].described).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.owners = owners
            .into_iter()
            .map(|(id, owner)| (id.clone(), owner.clone()))
            .collect();
        Ok(out)
    }
}

/// Compatibility input preserves the old module list's order and multiplicity.
/// It cannot fabricate physical edges or prove a table-root closure complete.
pub struct ModuleAdapter<'a>(&'a BTreeMap<ModuleId, Vec<Dependent>>);

impl<'a> ModuleAdapter<'a> {
    pub fn new(found: &'a BTreeMap<ModuleId, Vec<Dependent>>) -> Self {
        Self(found)
    }
    pub fn get(&self, module: &ModuleId) -> Option<&'a Vec<Dependent>> {
        self.0.get(module)
    }
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;

/// Physical evidence for restoring keys and composite bindings, not plan authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub name: pbps_model::TableName,
    pub row_type: Object,
    pub array_type: Object,
    pub attributes: BTreeMap<i32, (pbps_model::ColumnName, Object)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub index: Object,
    pub columns: Vec<Object>,
    pub referenced_columns: Vec<Object>,
}

/// Physical spellings and parser-chosen index attribute names, never describe text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartSpelling {
    pub table: pbps_model::TableName,
    pub name: pbps_model::ObjectLocalName,
    pub kind: pbps_model::operation::NameKind,
    pub index_attributes: Option<Vec<pbps_model::ColumnName>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub spellings: Fact<BTreeMap<Object, PartSpelling>>,
    pub graph: Graph,
    pub relations: BTreeMap<Object, Relation>,
    pub keys: BTreeMap<Object, Key>,
    /// Catalog type OIDs in declaration order, including OUT arguments.
    pub routines: BTreeMap<Object, (Vec<Object>, Object)>,
    pub publications: Fact<BTreeSet<(String, pbps_model::TableName)>>,
}

impl Catalog {
    /// A complete read is still not permission to destroy an unmanaged object.
    pub fn checked(&self, root: &Root, declared: &pbps_model::Schema) -> Result<Closure, String> {
        if let Some((publication, table)) =
            self.publications.require("publication membership")?.first()
        {
            return Err(format!(
                "{} is in publication {}; rebuild is refused",
                table.shown(),
                Escaped(publication)
            ));
        }
        let closure = self.graph.closure(root)?;
        for dependent in closure.surfaces.values() {
            if !dependent.managed(declared) {
                return Err(format!(
                    "{}: unmanaged dependent; declare or remove it before rebuilding",
                    Escaped(&dependent.described)
                ));
            }
        }
        Ok(closure)
    }
}

/// Complete namespace reads, including objects outside the managed tree.
/// Constraint names are not unique within a schema: every holder must leave.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamespaceNames {
    pub exists: bool,
    pub relations: BTreeMap<pbps_model::ObjectLocalName, Object>,
    pub types: BTreeMap<pbps_model::ObjectLocalName, Object>,
    pub constraints: BTreeMap<pbps_model::ObjectLocalName, BTreeSet<Object>>,
}
pub type Naming = Fact<BTreeMap<String, NamespaceNames>>;
