//! Apply's comparisons of a fresh capture with the sealed evidence (#616).
//!
//! Both sides are manifests the same adapter sealed under the same key, so
//! the comparison is of fingerprints and logical identities; nothing here
//! reads a catalog. Each difference is named, for the refusal to say what
//! moved.

use super::{
    InputManifest, ObjectIdentity, ObjectOwnership, ObjectTransition, Prerequisite, Surface,
};
use crate::TableName;
use std::collections::{BTreeMap, BTreeSet};

impl InputManifest {
    /// What differs between this sealed opening manifest and `fresh`, the
    /// same scope and lookups captured before apply's first statement. Any
    /// difference refuses: the plan was sealed against another target.
    pub fn opening_differences(&self, fresh: &Self) -> Vec<String> {
        let mut differences = self.header_differences(fresh);
        if self.scope() != fresh.scope() {
            differences.push("the capture's scope".into());
        }
        let sealed = by_object(self.prerequisites());
        let now = by_object(fresh.prerequisites());
        for (object, record) in &sealed {
            match now.get(object) {
                None => differences.push(format!("{} is gone", describe(object))),
                Some(current) if current != record => {
                    differences.push(format!("{} changed", describe(object)));
                }
                Some(_) => {}
            }
        }
        for object in now.keys().filter(|o| !sealed.contains_key(*o)) {
            differences.push(format!("{} arrived", describe(object)));
        }
        if self.membership() != fresh.membership() {
            for (sealed, current) in self.membership().iter().zip(fresh.membership()) {
                if sealed != current {
                    differences.push(format!("the candidates {}", predicate(sealed)));
                }
            }
            if self.membership().len() != fresh.membership().len() {
                differences.push("the candidate predicates".into());
            }
        }
        if self.runtime_bound() != fresh.runtime_bound() {
            differences.push("the runtime-bound routines".into());
        }
        differences.extend(lookup_differences(self, fresh, &BTreeSet::new()));
        differences
    }

    /// The sealed records the plan installs that a closing read may find
    /// under another name, or not at all: placeholders with no bindings of
    /// their own that no record binds to and no lookup names. They are kept
    /// for a candidate set or the runtime-bound set, where a name the engine
    /// chose (a key's, a sequence's) can differ between scratch and the
    /// target (#616).
    pub fn vanishable(&self) -> BTreeSet<ObjectIdentity> {
        let named: BTreeSet<&ObjectIdentity> = self
            .prerequisites()
            .iter()
            .flat_map(|p| p.bindings.iter().map(|b| &b.target))
            .chain(
                self.identifications()
                    .iter()
                    .filter_map(|l| l.resolved.as_ref()),
            )
            .collect();
        self.prerequisites()
            .iter()
            .filter(|p| p.is_managed_closing() && p.bindings.is_empty())
            .filter(|p| !named.contains(&p.object))
            .map(|p| p.object.clone())
            .collect()
    }

    /// What differs between this sealed closing manifest and `fresh`, its
    /// scope less [`Self::vanishable`] captured after apply's last statement
    /// (DEC-1274.1):
    ///
    /// - an untouched record has the sealed properties and bindings;
    /// - a record the plan installs (a placeholder) is present with the
    ///   sealed bindings, unless it is vanishable; its properties are the
    ///   managed revalidation's;
    /// - candidate membership, the runtime-bound set and the signature
    ///   lookups are the sealed ones, except for records the plan installs;
    /// - a record the seal does not hold is one the plan's changes own: its
    ///   proved owner is a transition's surface, or a part of the table a
    ///   transition's surface is in, since one statement on a table reaches
    ///   its whole tree and the table is the unit of the closing inventory
    ///   (DEC-614.1, DEC-614.2, DEC-1274.1). Such a record's declared
    ///   properties are the managed revalidation's, like a placeholder's.
    pub fn closing_differences(
        &self,
        fresh: &Self,
        transitions: &[ObjectTransition],
    ) -> Vec<String> {
        let vanishable = self.vanishable();
        let mut differences = self.header_differences(fresh);
        let sealed = by_object(self.prerequisites());
        let now = by_object(fresh.prerequisites());
        for (object, record) in &sealed {
            match now.get(object) {
                None if vanishable.contains(*object) => {}
                None => differences.push(format!("{} is gone", describe(object))),
                Some(current) if record.is_managed_closing() => {
                    if current.bindings != record.bindings {
                        differences.push(format!("{}'s bindings changed", describe(object)));
                    }
                }
                Some(current) if current != record => {
                    differences.push(format!("{} changed", describe(object)));
                }
                Some(_) => {}
            }
        }
        // A record the seal does not hold, which the plan's declarations
        // prove the plan's changes own.
        let installed: BTreeSet<&ObjectIdentity> = now
            .iter()
            .filter(|(object, _)| !sealed.contains_key(*object))
            .filter(|(_, record)| {
                transitions
                    .iter()
                    .any(|transition| owns(transition, &record.ownership))
            })
            .map(|(object, _)| *object)
            .collect();
        for object in now
            .keys()
            .filter(|o| !sealed.contains_key(*o) && !installed.contains(*o))
        {
            differences.push(format!(
                "{} arrived, and no change of the plan owns it",
                describe(object)
            ));
        }
        let moved = |sealed: &BTreeSet<ObjectIdentity>, now: &BTreeSet<ObjectIdentity>| {
            sealed
                .difference(now)
                .any(|object| !vanishable.contains(object))
                || now
                    .difference(sealed)
                    .any(|object| !installed.contains(object))
        };
        if self.membership().len() != fresh.membership().len() {
            differences.push("the candidate predicates".into());
        }
        for (sealed, current) in self.membership().iter().zip(fresh.membership()) {
            if sealed.predicate != current.predicate || moved(&sealed.members, &current.members) {
                differences.push(format!("the candidates {}", predicate(sealed)));
            }
        }
        if moved(self.runtime_bound(), fresh.runtime_bound()) {
            differences.push("the runtime-bound routines".into());
        }
        differences.extend(lookup_differences(self, fresh, &vanishable));
        differences
    }

    /// The rule, server, key and session. Not the baseline, the ledger's
    /// latest entry as the capture read it: every attempt that fails under
    /// the deployment lock adds an entry and records nothing about the
    /// target, so comparing it would refuse every retry of a valid plan.
    /// Apply's ordinary drift check holds the recorded state the ledger
    /// carries to the plan's baseline instead (SPEC 7.3).
    fn header_differences(&self, fresh: &Self) -> Vec<String> {
        let mut differences = Vec::new();
        if (self.adapter(), self.engine_major(), self.key_id())
            != (fresh.adapter(), fresh.engine_major(), fresh.key_id())
        {
            differences.push("the capture rule, server version or key".into());
        }
        if self.session() != fresh.session() {
            differences.push("the session's roles and settings".into());
        }
        differences
    }
}

/// Whether `ownership`, a record's proved owner, is `transition`'s to
/// change: its surface, or a part of the same table.
fn owns(transition: &ObjectTransition, ownership: &ObjectOwnership) -> bool {
    let ObjectOwnership::Surface(owner) = ownership else {
        return false;
    };
    ownership.permits(&transition.surface)
        || table(owner).is_some_and(|table_name| table(&transition.surface) == Some(table_name))
}

/// The table a table-family surface is part of.
fn table(surface: &Surface) -> Option<&TableName> {
    match surface {
        Surface::Table(table) => Some(table),
        Surface::Column(column) | Surface::Default(column) => Some(&column.table),
        Surface::Check { table, .. } | Surface::Index { table, .. } => Some(table),
        Surface::Namespace(_) | Surface::Module(_) => None,
    }
}

fn lookup_differences(
    sealed: &InputManifest,
    fresh: &InputManifest,
    vanishable: &BTreeSet<ObjectIdentity>,
) -> Vec<String> {
    let mut differences = Vec::new();
    if sealed.identifications().len() != fresh.identifications().len() {
        differences.push("the signature lookups".into());
    }
    for (sealed, current) in sealed.identifications().iter().zip(fresh.identifications()) {
        let same_lookup = (&sealed.signature, &sealed.search_path, &sealed.kind)
            == (&current.signature, &current.search_path, &current.kind);
        // A lookup never resolves to a vanishable record, which no lookup
        // names; the parameter keeps the rule in one place.
        let resolved = sealed.resolved == current.resolved
            || sealed
                .resolved
                .as_ref()
                .is_some_and(|object| vanishable.contains(object));
        if !same_lookup || !resolved {
            differences.push(format!(
                "`{}` under `{}` resolves to another routine",
                sealed.signature, sealed.search_path
            ));
        }
    }
    differences
}

fn by_object(records: &[Prerequisite]) -> BTreeMap<&ObjectIdentity, &Prerequisite> {
    records.iter().map(|p| (&p.object, p)).collect()
}

/// A record as a refusal names it: its class, then its name with what
/// tells it apart, such as an overload's argument types or a rule's
/// relation.
fn describe(object: &ObjectIdentity) -> String {
    format!("{} {}", object.class, spelled(object))
}

fn spelled(object: &ObjectIdentity) -> String {
    let name = object.name.join(".");
    if object.signature.is_empty() {
        return name;
    }
    let parts: Vec<String> = object.signature.iter().map(spelled).collect();
    format!("{name}({})", parts.join(", "))
}

fn predicate(membership: &super::Membership) -> String {
    let set = &membership.predicate;
    format!(
        "{} {}.{}",
        set.class,
        set.namespace.as_deref().unwrap_or("*"),
        set.name.as_deref().unwrap_or("*")
    )
}

#[cfg(test)]
mod tests;
