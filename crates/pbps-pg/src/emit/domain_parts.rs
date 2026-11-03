//! Reordered creation facets retain complete per-statement definition evidence.
//! SQL still derives solely from the existing DomainDelta and DomainHalf.
//! An earlier typed CREATE and its UID establish context even after wire
//! round-trip; diagnostic provenance is deliberately irrelevant.
use pbps_model::{Change, ChangeSet, DomainDelta, IdsFile, ManagedType, Schema, TypeRef, Uid};
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct DomainParts(BTreeMap<usize, Change>);
impl DomainParts {
    #[allow(clippy::wildcard_enum_match_arm)]
    pub fn new(changes: &ChangeSet) -> Self {
        let mut current = BTreeMap::new();
        let mut out = Self::default();
        for (at, planned) in changes.changes.iter().enumerate() {
            match &planned.change {
                Change::CreateType {
                    uid, definition, ..
                } => {
                    current.insert(uid.clone(), definition.as_ref().clone());
                }
                Change::AlterDomain {
                    uid,
                    name,
                    from,
                    to,
                    half,
                } => {
                    let Some(now) = current.get_mut(uid) else {
                        continue;
                    };
                    let before = now.clone();
                    let Some(delta) = DomainDelta::of(from, to) else {
                        continue;
                    };
                    let Some(next) = delta.apply_to(now, *half) else {
                        continue;
                    };
                    *now = next;
                    out.0.insert(
                        at,
                        Change::AlterDomain {
                            uid: uid.clone(),
                            name: name.clone(),
                            from: Box::new(before),
                            to: Box::new(now.clone()),
                            half: *half,
                        },
                    );
                }
                Change::DropType { uid, .. } => {
                    current.remove(uid);
                }
                _ => {}
            }
        }
        out
    }
    pub fn at<'a>(&'a self, at: usize, original: &'a Change) -> &'a Change {
        self.0.get(&at).unwrap_or(original)
    }
}

// SQL uses the authored halves above. Physical evidence separately retains a
// DEFAULT copied at CREATE, even after the base changes (DEC-2537.1).
struct Copies {
    types: BTreeMap<TypeRef, ManagedType>,
    names: BTreeMap<Uid, TypeRef>,
    desired: IdsFile,
    authored: BTreeMap<Uid, Option<String>>,
    creates: std::collections::BTreeSet<(Uid, TypeRef)>,
}
impl Copies {
    #[allow(clippy::wildcard_enum_match_arm)]
    fn new(before: &Schema, ids: &IdsFile, changes: &ChangeSet) -> Self {
        let mut out = Self {
            types: before.types.clone(),
            names: ids.types.clone(),
            desired: pbps_diff::identity::type_refs::after_changes(ids, changes),
            authored: BTreeMap::new(),
            creates: Default::default(),
        };
        for planned in &changes.changes {
            match &planned.change {
                Change::CreateType { uid, name, .. } => {
                    out.creates.insert((uid.clone(), name.clone()));
                }
                Change::EnumRebuild {
                    uid,
                    tmp,
                    step: pbps_model::EnumRebuildStep::Create { .. },
                    ..
                } => {
                    out.creates.insert((uid.clone(), tmp.clone()));
                }
                Change::AlterDomain {
                    uid,
                    from: definition,
                    ..
                }
                | Change::RenameType {
                    uid, definition, ..
                } => {
                    if ids.types.contains_key(uid)
                        && let ManagedType::Domain { default, .. } = definition.as_ref()
                    {
                        out.authored
                            .entry(uid.clone())
                            .or_insert_with(|| default.clone());
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn step(&mut self, uid: &Uid, name: &TypeRef, definition: Option<&ManagedType>, altered: bool) {
        let old = self.names.get(uid).and_then(|name| self.types.get(name));
        let Some(definition) = definition else {
            if let Some(name) = self.names.remove(uid) {
                self.types.remove(&name);
            }
            self.authored.remove(uid);
            return;
        };
        let mut physical = definition.clone();
        let created = self.creates.remove(&(uid.clone(), name.clone()));
        if let ManagedType::Domain { base, default, .. } = &mut physical {
            let authored = default.clone();
            if created {
                let positioned = IdsFile {
                    types: self.names.clone(),
                    ..Default::default()
                };
                let resolver =
                    pbps_diff::identity::type_refs::TypeResolver::new(&self.desired, &positioned);
                let mut at = base.clone();
                resolver.remap(
                    &mut at,
                    pbps_diff::identity::type_refs::TypeSide::Base,
                    pbps_diff::identity::type_refs::TypeSide::Desired,
                );
                if default.is_none()
                    && !base.is_array()
                    && let Some(ManagedType::Domain {
                        default: inherited, ..
                    }) = at.type_ref().and_then(|name| self.types.get(name))
                {
                    *default = inherited.clone();
                }
            } else if let Some(ManagedType::Domain {
                default: previous, ..
            }) = old
                && (!altered || self.authored.get(uid) == Some(&authored))
            {
                *default = previous.clone();
            }
            self.authored.insert(uid.clone(), authored);
        }
        if !created
            && let Some(old) = self.names.get(uid)
            && old != name
        {
            self.types.remove(old);
        }
        self.names.insert(uid.clone(), name.clone());
        self.types.insert(name.clone(), physical);
    }

    fn finish(self) -> BTreeMap<Uid, ManagedType> {
        self.names
            .into_iter()
            .filter_map(|(uid, name)| {
                self.types
                    .get(&name)
                    .cloned()
                    .map(|definition| (uid, definition))
            })
            .collect()
    }
}

impl DomainParts {
    pub(crate) fn after(
        before: &Schema,
        ids: &IdsFile,
        changes: &ChangeSet,
        executed: &[pbps_dialect::Statement],
    ) -> BTreeMap<Uid, ManagedType> {
        let mut copies = Copies::new(before, ids, changes);
        for step in executed.iter().filter_map(|s| s.declared_type.as_ref()) {
            copies.step(
                &step.uid,
                &step.name,
                step.definition.as_ref(),
                step.altered,
            );
        }
        copies.finish()
    }

    #[allow(clippy::wildcard_enum_match_arm)]
    pub(crate) fn completed(
        before: &Schema,
        ids: &IdsFile,
        changes: &ChangeSet,
    ) -> BTreeMap<Uid, ManagedType> {
        let parts = Self::new(changes);
        let mut copies = Copies::new(before, ids, changes);
        for (at, planned) in changes.changes.iter().enumerate() {
            match parts.at(at, &planned.change) {
                Change::CreateType {
                    uid,
                    name,
                    definition,
                }
                | Change::RenameType {
                    uid,
                    to: name,
                    definition,
                    ..
                }
                | Change::AddEnumValue {
                    uid,
                    name,
                    definition,
                    ..
                }
                | Change::RenameEnumValue {
                    uid,
                    name,
                    definition,
                    ..
                } => {
                    copies.step(uid, name, Some(definition), false);
                }
                Change::AlterDomain { uid, name, to, .. } => copies.step(uid, name, Some(to), true),
                Change::DropType { uid, name, .. } => copies.step(uid, name, None, false),
                Change::EnumRebuild {
                    uid,
                    name,
                    tmp,
                    step,
                    ..
                } => match step {
                    pbps_model::EnumRebuildStep::Create {
                        definition, over, ..
                    } => {
                        let effective = super::over_the_replacement(definition, over.as_ref());
                        copies.step(uid, tmp, Some(&effective), true);
                    }
                    pbps_model::EnumRebuildStep::Swap { definition, .. } => {
                        copies.step(uid, name, Some(definition), true)
                    }
                    pbps_model::EnumRebuildStep::Retype { .. } => {}
                },
                _ => {}
            }
        }
        copies.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{DomainHalf, PlannedChange, UidKind};
    fn uid(name: &str) -> Uid {
        Uid::derived(UidKind::Type, name, 0)
    }
    fn domain(base: &str, default: Option<&str>) -> ManagedType {
        ManagedType::Domain {
            base: base.parse().unwrap(),
            default: default.map(str::to_owned),
            not_null: false,
            checks: Default::default(),
            collation: None,
        }
    }
    fn create(name: &str, base: &str, default: Option<&str>) -> Change {
        Change::CreateType {
            uid: uid(name),
            name: name.parse().unwrap(),
            definition: Box::new(domain(base, default)),
        }
    }
    fn alter(name: &str, from: Option<&str>, to: Option<&str>) -> Change {
        Change::AlterDomain {
            uid: uid(name),
            name: name.parse().unwrap(),
            from: Box::new(domain("integer", from)),
            to: Box::new(domain("integer", to)),
            half: DomainHalf::Loosen,
        }
    }
    fn changes(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        }
    }
    fn default(ty: &ManagedType) -> Option<&str> {
        let ManagedType::Domain { default, .. } = ty else {
            panic!("domain")
        };
        default.as_deref()
    }
    #[test]
    fn only_scalar_creation_copies_the_default_present_at_that_position() {
        let changes = changes(vec![
            create("app.base", "integer", Some("7")),
            create("app.copy", "app.base", None),
            create("app.deep", "app.copy", None),
            create("app.explicit", "app.base", Some("9")),
            create("app.array", "app.base[]", None),
            alter("app.base", Some("7"), None),
            create("app.no_default", "app.base", None),
            alter("app.base", None, Some("11")),
            create("app.fresh", "app.base", None),
        ]);
        let out = DomainParts::completed(&Schema::default(), &IdsFile::default(), &changes);
        for (name, want) in [
            ("app.copy", Some("7")),
            ("app.deep", Some("7")),
            ("app.explicit", Some("9")),
            ("app.array", None),
            ("app.no_default", None),
            ("app.fresh", Some("11")),
        ] {
            assert_eq!(default(&out[&uid(name)]), want, "{name}");
        }
        // Physical equality cannot erase an authored SET or recopy after DROP.
        let mut copy = changes.clone();
        for (from, to) in [(None, Some("7")), (Some("7"), None)] {
            copy.changes.push(PlannedChange::new(Change::AlterDomain {
                uid: uid("app.copy"),
                name: "app.copy".parse().unwrap(),
                from: Box::new(domain("app.base", from)),
                to: Box::new(domain("app.base", to)),
                half: DomainHalf::Loosen,
            }));
            let out = DomainParts::completed(&Schema::default(), &IdsFile::default(), &copy);
            assert_eq!(default(&out[&uid("app.copy")]), to);
            let at = copy.changes.len() - 1;
            let parts = DomainParts::new(&copy);
            let Change::AlterDomain {
                from: actual_from,
                to: actual_to,
                ..
            } = parts.at(at, &copy.changes[at].change)
            else {
                panic!("ALTER")
            };
            assert_eq!(
                DomainDelta::of(actual_from, actual_to).unwrap().default,
                Some(to)
            );
        }
    }
    #[test]
    fn copied_defaults_follow_base_uids_through_a_rename_and_reused_name() {
        let before = Schema {
            types: [("app.base".parse().unwrap(), domain("integer", Some("7")))].into(),
            ..Default::default()
        };
        let ids = IdsFile {
            types: [(uid("app.base"), "app.base".parse().unwrap())].into(),
            ..Default::default()
        };
        let changes = changes(vec![
            Change::RenameType {
                uid: uid("app.base"),
                from: "app.base".parse().unwrap(),
                to: "app.moved".parse().unwrap(),
                definition: Box::new(domain("integer", Some("7"))),
            },
            Change::CreateType {
                uid: uid("replacement"),
                name: "app.base".parse().unwrap(),
                definition: Box::new(domain("integer", None)),
            },
            create("app.old", "app.moved", None),
            create("app.new", "app.base", None),
        ]);
        let out = DomainParts::completed(&before, &ids, &changes);
        assert_eq!(default(&out[&uid("app.old")]), Some("7"));
        assert_eq!(default(&out[&uid("app.new")]), None);
    }
    #[test]
    fn a_copy_resolves_the_desired_base_uid_before_its_name_is_released() {
        let before = Schema {
            types: [
                ("app.a".parse().unwrap(), domain("integer", Some("7"))),
                ("app.b".parse().unwrap(), domain("integer", None)),
            ]
            .into(),
            ..Default::default()
        };
        let ids = IdsFile {
            types: [
                (uid("a"), "app.a".parse().unwrap()),
                (uid("b"), "app.b".parse().unwrap()),
            ]
            .into(),
            ..Default::default()
        };
        let changes = changes(vec![
            create("app.copy", "app.b", None),
            Change::RenameType {
                uid: uid("b"),
                from: "app.b".parse().unwrap(),
                to: "app.c".parse().unwrap(),
                definition: Box::new(domain("integer", None)),
            },
            Change::RenameType {
                uid: uid("a"),
                from: "app.a".parse().unwrap(),
                to: "app.b".parse().unwrap(),
                definition: Box::new(domain("integer", Some("7"))),
            },
        ]);
        assert_eq!(
            default(&DomainParts::completed(&before, &ids, &changes)[&uid("app.copy")]),
            Some("7")
        );
    }

    #[test]
    fn unrelated_facets_preserve_a_baseline_copy() {
        let before = Schema {
            types: [("app.outer".parse().unwrap(), domain("app.base", Some("7")))].into(),
            ..Default::default()
        };
        let ids = IdsFile {
            types: [(uid("app.outer"), "app.outer".parse().unwrap())].into(),
            ..Default::default()
        };
        let mut to = domain("app.base", None);
        let ManagedType::Domain { not_null, .. } = &mut to else {
            unreachable!()
        };
        *not_null = true;
        let changes = changes(vec![Change::AlterDomain {
            uid: uid("app.outer"),
            name: "app.outer".parse().unwrap(),
            from: Box::new(domain("app.base", None)),
            to: Box::new(to),
            half: DomainHalf::Tighten,
        }]);
        assert_eq!(
            default(&DomainParts::completed(&before, &ids, &changes)[&uid("app.outer")]),
            Some("7")
        );
    }
}
