//! Connected catalog dependency facts for the lightweight binding assessment.
//!
//! Pure data: no I/O and no engine-specific catalog class names. Absent
//! offline (`Option<DependencyFacts>::None`). Absent, empty, and unreadable
//! states are distinct across surfaces, ancestor ties, and catalog-held names.

use super::Surface;
use crate::{ColumnRef, ObjectName};
use std::collections::{BTreeMap, BTreeSet};

/// Outcome of a whole-plan catalog read: `Known(empty)` is a truthful empty
/// result; `Unreadable` means the read failed or was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadFact<T> {
    Known(T),
    Unreadable,
}

/// Creation-time dependency record for one kept surface.
///
/// An unreadable surface cannot hold edges. Absence from [`DependencyFacts::surfaces`] means
/// "not covered" and is never equivalent to `Parsed(empty)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceDependencies {
    /// The catalog read failed or could not establish completeness.
    Unreadable,
    /// The body was not bound at creation, so it has no creation-time
    /// binding to move; what it finds when it runs is reported as a known
    /// gap. What the engine did bind is here: the edges of the header (the
    /// parameter and return types, the parameter defaults), complete as
    /// [`Self::Parsed`]'s are.
    Unparsed(BTreeSet<DependencyEdge>),
    /// Parsed and bound at creation time with this complete forward edge set
    /// (gathered across all dependent objects). Empty is a truthful fact.
    Parsed(BTreeSet<DependencyEdge>),
}

/// One object bound by a parsed surface at creation time, at relation
/// granularity. Dialect-agnostic: logical names only, no OIDs or class names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DependencyEdge {
    /// A relation (table, view, sequence, index, materialized view).
    Relation(ObjectName),
    /// A column of a relation. Touched-object checks use relation granularity;
    /// the column name is kept so an arriving routine of that name is caught.
    Column(ColumnRef),
    /// A routine, operator, or aggregate by qualified name (signature-free so
    /// any arriving overload of that name matches).
    Routine(ObjectName),
    /// A type by qualified name (array dependencies use their element type).
    Type(ObjectName),
}

impl DependencyEdge {
    /// Whether an arriving unqualified name matches any name carried by this
    /// edge (relation, column or its owning relation, routine, or type).
    #[must_use]
    pub fn matches_arriving_name(&self, name: &str) -> bool {
        match self {
            Self::Relation(obj) | Self::Routine(obj) | Self::Type(obj) => obj.name == name,
            Self::Column(col) => col.table.name == name || col.name.raw() == name,
        }
    }
}

/// A direct ancestor of a relation whose change can affect readers of the
/// descendant.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ancestor {
    /// An inheritance parent or partition parent relation.
    Relation(ObjectName),
    /// A composite type that a typed table is declared `OF`.
    Type(ObjectName),
}

/// Whether the engine's system catalog holds an arriving unqualified name as a
/// function, operator, aggregate, or type. Absence from
/// [`DependencyFacts::catalog_names`] means unchecked, never [`Self::Clear`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CatalogName {
    Clear,
    Held,
    Unreadable,
}

/// Connected catalog dependency facts for the lightweight binding assessment.
///
/// Deliberately omits `Default` so callers must explicitly record whether
/// ancestor ties were read (`ReadFact::Known`) or unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyFacts {
    /// Per-surface facts keyed by each surface's target (base) spelling.
    pub surfaces: BTreeMap<Surface, SurfaceDependencies>,
    /// Direct ancestor ties: descendant relation -> direct ancestors.
    pub relation_ancestors: ReadFact<BTreeMap<ObjectName, BTreeSet<Ancestor>>>,
    /// System-catalog presence for each unqualified name arriving in the plan.
    pub catalog_names: BTreeMap<String, CatalogName>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TableName;

    #[test]
    fn absent_empty_unparsed_and_unreadable_surface_states_are_distinct() {
        let mut surfaces = BTreeMap::new();
        let covered_empty = Surface::Module("app.v_empty".parse().unwrap());
        let unparsed = Surface::Module("app.f_plpgsql()".parse().unwrap());
        let unreadable = Surface::Module("app.v_broken".parse().unwrap());
        let absent = Surface::Module("app.v_absent".parse().unwrap());

        surfaces.insert(
            covered_empty.clone(),
            SurfaceDependencies::Parsed(BTreeSet::new()),
        );
        surfaces.insert(
            unparsed.clone(),
            SurfaceDependencies::Unparsed(BTreeSet::new()),
        );
        surfaces.insert(unreadable.clone(), SurfaceDependencies::Unreadable);

        let facts = DependencyFacts {
            surfaces,
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::new(),
        };

        assert!(!facts.surfaces.contains_key(&absent));
        assert_eq!(
            facts.surfaces.get(&covered_empty),
            Some(&SurfaceDependencies::Parsed(BTreeSet::new()))
        );
        assert_eq!(
            facts.surfaces.get(&unparsed),
            Some(&SurfaceDependencies::Unparsed(BTreeSet::new()))
        );
        assert_eq!(
            facts.surfaces.get(&unreadable),
            Some(&SurfaceDependencies::Unreadable)
        );
        assert_ne!(
            ReadFact::<BTreeMap<ObjectName, BTreeSet<Ancestor>>>::Known(BTreeMap::new()),
            ReadFact::Unreadable
        );
        assert_ne!(CatalogName::Clear, CatalogName::Held);
        assert_ne!(CatalogName::Clear, CatalogName::Unreadable);
        assert_ne!(CatalogName::Held, CatalogName::Unreadable);
    }

    #[test]
    fn column_edge_matches_both_its_relation_name_and_its_column_name() {
        let table: TableName = "app.t".parse().unwrap();
        let edge = DependencyEdge::Column(table.column("note"));
        assert!(edge.matches_arriving_name("note"));
        assert!(edge.matches_arriving_name("t"));
        assert!(!edge.matches_arriving_name("other"));
    }
}
