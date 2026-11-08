//! What every object a package provides needs before a plan relies on it
//! (DEC-2447.13): the rules of E's type route (DEC-2447.12) that do not depend
//! on what the object is. A type, and later an operator class or an operator,
//! is a provider-owned object identified by a schema and a name, with a digest
//! of what it does and the package that owns it.
//!
//! These are plain functions over a [`Standing`], the few facts the rules read,
//! and a [`Noun`] that words the refusals. They are not a trait with associated
//! types: what differs per kind is how one object is read and digested, and
//! that stays with the kind (`user_types` for a type). The kernel decides which
//! package an object belongs to, whether the project manages it at the
//! installed version, what a plan that updates or moves the package may do, and
//! whether a placeholder is proved by the sealed manifest.

use std::collections::BTreeMap;

use pbps_model::escape::Escaped;
use pbps_model::{Change, ChangeSet, ProviderMember, Schema, TypePin, TypeProvider};

/// The facts the rules read about one pinned object.
#[derive(Debug, Clone, Copy)]
pub struct Standing<'a> {
    pub schema: &'a str,
    pub name: &'a str,
    /// A placeholder for an object the plan itself brings (nothing to read).
    pub planned: bool,
    pub provider: Option<&'a TypeProvider>,
}

impl<'a> From<&'a TypePin> for Standing<'a> {
    fn from(pin: &'a TypePin) -> Self {
        Standing {
            schema: &pin.schema,
            name: &pin.name,
            planned: crate::type_refs::is_planned(pin),
            provider: pin.provider.as_ref(),
        }
    }
}

/// How a refusal names the kind of object, so the words of the type route stay
/// exactly as they were.
#[derive(Debug, Clone, Copy)]
pub struct Noun {
    /// "type"
    pub object: &'static str,
    /// What relies on the object: "a column".
    pub consumer: &'static str,
}

/// The nouns of E's route.
pub const TYPE: Noun = Noun {
    object: "type",
    consumer: "a column",
};

/// The packages the project declares, by the schema each is declared in. A
/// package the target lacks is created by this plan or the plan is refused
/// elsewhere, and the sealed manifest then decides whether an object is its own
/// ([`proof`]). The last schema is the desired one.
pub fn packages_by_schema<'a>(
    schemas: &[&'a Schema],
) -> BTreeMap<&'a str, Vec<(&'a str, &'a str)>> {
    let mut by_name: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    for schema in schemas {
        for (name, definition) in &schema.extensions {
            by_name.insert(
                name.raw(),
                (definition.schema.as_str(), definition.version.as_str()),
            );
        }
    }
    let mut by_schema: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (name, (schema, version)) in by_name {
        by_schema.entry(schema).or_default().push((name, version));
    }
    by_schema
}

/// The package a missing object belongs to, when the project declares exactly
/// one in the schema it names. `None` when it declares none there (the caller
/// refuses the object as absent); an error naming the candidates when it
/// declares several, because the kernel will not guess which provides it.
pub fn defer_to_package(
    declared: &BTreeMap<&str, Vec<(&str, &str)>>,
    schema: &str,
    shown: &str,
    noun: Noun,
) -> Option<Result<TypeProvider, String>> {
    let packages = declared.get(schema)?;
    Some(match packages.as_slice() {
        [(extension, version)] => Ok(TypeProvider {
            extension: (*extension).to_owned(),
            version: (*version).to_owned(),
            moved_to: None,
        }),
        several => Err(format!(
            "the {} `{shown}` is not in the target database and the project declares {} \
             packages in schema `{}` ({}); say which provides it by declaring them in \
             separate schemas",
            noun.object,
            several.len(),
            Escaped(schema),
            several
                .iter()
                .map(|(n, _)| format!("`{}`", Escaped(n)))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    })
}

/// The proof after the rehearsal for one placeholder: the package the plan
/// creates, or moves and updates, must list that exact member among its sealed
/// members (as the engine names it), so an object of the same spelling from
/// anywhere else does not stand in for the one the order assumed (DEC-2447.12).
/// A created package is held to its manifest, a changed one to the manifest it
/// has after the change.
pub fn proof(
    standing: Standing<'_>,
    member: &ProviderMember,
    noun: Noun,
    created: &[pbps_model::ExtensionPin],
    changed: &[pbps_model::ExtensionChangePin],
) -> Result<(), String> {
    let Some(provider) = standing.provider else {
        return Err(format!(
            "the {} pin for `{}.{}` names no package",
            noun.object,
            Escaped(standing.schema),
            Escaped(standing.name)
        ));
    };
    let manifest = created
        .iter()
        .find(|e| e.name.raw() == provider.extension)
        .map(|e| &e.manifest)
        .or_else(|| {
            changed
                .iter()
                .find(|e| e.name.raw() == provider.extension)
                .map(|e| &e.after)
        })
        .ok_or_else(|| {
            format!(
                "the {} `{}.{}` is taken as a member of the extension `{}`, which this \
                 plan neither creates nor changes",
                noun.object,
                Escaped(standing.schema),
                Escaped(standing.name),
                Escaped(&provider.extension)
            )
        })?;
    if !manifest.members.contains(member) {
        return Err(format!(
            "the {} `{}.{}` is not a member of the extension `{}` after the plan's change, \
             so {} cannot rely on this plan to bring it",
            noun.object,
            Escaped(standing.schema),
            Escaped(standing.name),
            Escaped(&provider.extension),
            noun.consumer
        ));
    }
    Ok(())
}

/// A provided object is usable only when the project manages its package at
/// the version the target holds: a package nobody opted in has no proof route
/// (DEC-2447.3, DEC-2447.12). `shown` is the object as a message names it.
pub fn check_package(
    schemas: &[&Schema],
    extension: &str,
    version: &str,
    shown: &str,
    noun: Noun,
) -> Result<(), String> {
    let declared = schemas.iter().find_map(|s| {
        s.extensions
            .iter()
            .find(|(name, _)| name.raw() == extension)
            .map(|(_, definition)| definition)
    });
    match declared {
        None => Err(format!(
            "the {} `{shown}` belongs to the extension `{}`, which this project does not \
             manage; opt it in with `pbps pull --extension {}` and record it with \
             `pbps baseline`, then plan again",
            noun.object,
            Escaped(extension),
            Escaped(extension),
        )),
        Some(definition) if definition.version != version => Err(format!(
            "the {} `{shown}` belongs to the extension `{}` at version `{}`, and the \
             project records version `{}`; bring them together before planning",
            noun.object,
            Escaped(extension),
            Escaped(version),
            Escaped(&definition.version),
        )),
        Some(_) => Ok(()),
    }
}

/// A package is not updated in the plan that uses one of its objects
/// (DEC-2447.12): an update can change what the object does (measured:
/// `hstore` 1.4 to 1.8 adds its subscript handler), the object's pin covers
/// that, and a pin that moves inside the plan that approved it can never be
/// checked. The update goes in its own plan, which re-reads and re-pins it.
/// `used` answers, for a package name, the first object of it the plan uses.
pub fn refuse_update_beside_provided(
    used: impl Fn(&str) -> Option<String>,
    changes: &ChangeSet,
    noun: Noun,
) -> Result<(), String> {
    for planned in &changes.changes {
        let Change::UpdateExtension { name, .. } = &planned.change else {
            continue;
        };
        if let Some(shown) = used(name.raw()) {
            return Err(format!(
                "this plan updates extension `{}` and also uses its {} `{shown}`; an update \
                 can change what the {} does, so update the package in a plan of its own first",
                name.shown(),
                noun.object,
                noun.object
            ));
        }
    }
    Ok(())
}

/// Where an object of `extension` standing in `schema` is once this plan's move
/// of the package has run, so that every later read of it can find it
/// (DEC-2447.12). `None` when the plan does not move that package from there.
pub fn moved_to(extension: &str, schema: &str, changes: &ChangeSet) -> Option<String> {
    changes.changes.iter().find_map(|planned| {
        let Change::SetExtensionSchema { name, from, to, .. } = &planned.change else {
            return None;
        };
        (name.raw() == extension && from.schema == schema).then(|| to.schema.clone())
    })
}

/// A pin read again is the pin that was sealed, except that an object its
/// package's move carried is in the schema the plan said it would be
/// (DEC-2447.12). `unmoved` is the pin without its note of where it will be;
/// `relocated` is the pin set in a schema.
pub fn same_pin<P: PartialEq>(
    sealed: &P,
    now: &P,
    standing: impl Fn(&P) -> (String, Option<String>),
    unmoved: impl Fn(&P) -> P,
    relocated: impl Fn(P, &str) -> P,
) -> bool {
    // Before the move, the object is where it was and the pin is what was
    // sealed, less the note of where it will be.
    let before = unmoved(sealed);
    if before == *now {
        return true;
    }
    // After it, the object is in the schema the plan said and nothing else moved.
    let (now_schema, _) = standing(now);
    let moved = standing(sealed)
        .1
        .is_some_and(|schema| schema == now_schema);
    moved && relocated(before, &now_schema) == *now
}

/// The rules are proved on a kind that is not a type: a fake object with its own
/// pin, so a rule that quietly depends on `TypePin` or `UserType` fails here
/// and not in the next kind (DEC-2447.13). Each rule sits beside its negative.
#[cfg(test)]
mod tests {
    use super::*;

    /// A pin of the fake kind: a schema, a name, a digest and the provider.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Fake {
        schema: String,
        name: String,
        digest: String,
        provider: Option<TypeProvider>,
    }

    fn fake(schema: &str, name: &str, provider: Option<(&str, Option<&str>)>) -> Fake {
        Fake {
            schema: schema.into(),
            name: name.into(),
            digest: "d1".into(),
            provider: provider.map(|(extension, moved)| TypeProvider {
                extension: extension.into(),
                version: "1.0".into(),
                moved_to: moved.map(str::to_owned),
            }),
        }
    }

    const KIND: Noun = Noun {
        object: "widget",
        consumer: "an index",
    };

    fn standing(pin: &Fake) -> Standing<'_> {
        Standing {
            schema: &pin.schema,
            name: &pin.name,
            planned: false,
            provider: pin.provider.as_ref(),
        }
    }

    fn schema_with(packages: &[(&str, &str, &str)]) -> Schema {
        let mut schema = Schema::default();
        for (name, in_schema, version) in packages {
            schema.extensions.insert(
                (*name).into(),
                pbps_model::Extension {
                    schema: (*in_schema).into(),
                    version: (*version).into(),
                },
            );
        }
        schema
    }

    fn manifest(members: &[(&str, &str)]) -> pbps_model::ExtensionProvider {
        pbps_model::ExtensionProvider {
            definition: pbps_model::Extension {
                schema: "ext".into(),
                version: "1.0".into(),
            },
            requires: Default::default(),
            members: members
                .iter()
                .map(|(class, name)| ProviderMember {
                    class: (*class).to_owned(),
                    names: vec![(*name).to_owned()],
                    args: Vec::new(),
                })
                .collect(),
        }
    }

    /// Exactly one package in the schema takes a missing object; two are
    /// refused by name (never guessed), and no package leaves it to the caller.
    #[test]
    fn a_missing_object_belongs_to_exactly_one_declared_package() {
        let one = schema_with(&[("alpha", "ext", "1.0")]);
        let two = schema_with(&[("alpha", "ext", "1.0"), ("beta", "ext", "2.0")]);
        let elsewhere = schema_with(&[("alpha", "other", "1.0")]);
        let provider = defer_to_package(&packages_by_schema(&[&one]), "ext", "ext.w", KIND)
            .expect("a package stands there")
            .unwrap();
        assert_eq!(provider.extension, "alpha");
        assert_eq!(provider.version, "1.0");
        assert_eq!(provider.moved_to, None);
        let refused = defer_to_package(&packages_by_schema(&[&two]), "ext", "ext.w", KIND)
            .expect("packages stand there")
            .unwrap_err();
        assert!(
            refused.contains("2 packages") && refused.contains("widget"),
            "{refused}"
        );
        assert!(
            refused.contains("alpha") && refused.contains("beta"),
            "{refused}"
        );
        assert!(
            defer_to_package(&packages_by_schema(&[&elsewhere]), "ext", "ext.w", KIND).is_none()
        );
    }

    /// A package the project does not manage, or manages at another version, is
    /// no provider; the matching one is.
    #[test]
    fn a_package_must_be_managed_at_the_installed_version() {
        let managed = schema_with(&[("alpha", "ext", "1.0")]);
        check_package(&[&managed], "alpha", "1.0", "ext.w", KIND).unwrap();
        let unmanaged = check_package(&[&managed], "beta", "1.0", "ext.w", KIND).unwrap_err();
        assert!(unmanaged.contains("does not manage"), "{unmanaged}");
        let other = check_package(&[&managed], "alpha", "2.0", "ext.w", KIND).unwrap_err();
        assert!(
            other.contains("at version `2.0`") && other.contains("`1.0`"),
            "{other}"
        );
    }

    fn extension(schema: &str, version: &str) -> pbps_model::Extension {
        pbps_model::Extension {
            schema: schema.into(),
            version: version.into(),
        }
    }

    fn update(name: &str) -> ChangeSet {
        ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(Change::UpdateExtension {
                uid: "e_aaaaaa".parse().unwrap(),
                name: name.into(),
                from: extension("ext", "1.0"),
                to: extension("ext", "1.1"),
                requires: Default::default(),
                releases: Default::default(),
            })],
        }
    }

    /// An update of a package whose object the plan uses is refused; an update
    /// of another package, no update, and no use are not.
    #[test]
    fn a_package_is_not_updated_beside_a_use_of_its_object() {
        let uses = |extension: &str| (extension == "alpha").then(|| "ext.w".to_owned());
        let refused = refuse_update_beside_provided(uses, &update("alpha"), KIND).unwrap_err();
        assert!(
            refused.contains("also uses its widget `ext.w`"),
            "{refused}"
        );
        refuse_update_beside_provided(uses, &update("beta"), KIND).unwrap();
        refuse_update_beside_provided(uses, &ChangeSet::default(), KIND).unwrap();
        refuse_update_beside_provided(|_| None, &update("alpha"), KIND).unwrap();
    }

    /// The placeholder is proved by the sealed manifest naming the exact member
    /// of the package the pin names: a lookalike of another class or in another
    /// package, a package the plan does not bring and a pin with no package
    /// are all refused.
    #[test]
    fn a_placeholder_is_proved_by_the_exact_member_of_its_package() {
        let planned = fake("ext", "w", Some(("alpha", None)));
        let member = ProviderMember {
            class: "widget".to_owned(),
            names: vec!["ext.w".to_owned()],
            args: Vec::new(),
        };
        let create = |name: &str, members: &[(&str, &str)]| pbps_model::ExtensionPin {
            uid: "e_aaaaaa".parse().unwrap(),
            name: name.into(),
            definition: pbps_model::Extension {
                schema: "ext".into(),
                version: "1.0".into(),
            },
            manifest: manifest(members),
        };
        let proved = |created: &[pbps_model::ExtensionPin]| {
            proof(standing(&planned), &member, KIND, created, &[])
        };
        proved(&[create("alpha", &[("widget", "ext.w")])]).unwrap();
        // Negatives: the member is of another class, in another package, absent.
        let lookalike = proved(&[create("alpha", &[("function", "ext.w")])]).unwrap_err();
        assert!(lookalike.contains("not a member"), "{lookalike}");
        let other = proved(&[create("beta", &[("widget", "ext.w")])]).unwrap_err();
        assert!(other.contains("neither creates nor changes"), "{other}");
        assert!(
            proved(&[])
                .unwrap_err()
                .contains("neither creates nor changes")
        );
        let none = fake("ext", "w", None);
        let unpackaged = proof(standing(&none), &member, KIND, &[], &[]).unwrap_err();
        assert!(unpackaged.contains("names no package"), "{unpackaged}");
    }

    fn same(sealed: &Fake, now: &Fake) -> bool {
        same_pin(
            sealed,
            now,
            |pin| {
                (
                    pin.schema.clone(),
                    pin.provider.as_ref().and_then(|p| p.moved_to.clone()),
                )
            },
            |pin| {
                let mut pin = pin.clone();
                if let Some(provider) = &mut pin.provider {
                    provider.moved_to = None;
                }
                pin
            },
            |pin, schema| Fake {
                schema: schema.to_owned(),
                ..pin
            },
        )
    }

    /// A pin read again is the pin sealed, except that an object its package's
    /// move carried is in the schema the plan said; any other schema, a changed
    /// digest, or a pin that says nothing of a move is not.
    #[test]
    fn a_pin_may_differ_only_by_the_schema_its_move_gave_it() {
        let sealed = fake("ext", "w", Some(("alpha", Some("moved"))));
        assert!(same(&sealed, &fake("ext", "w", Some(("alpha", None)))));
        assert!(same(&sealed, &fake("moved", "w", Some(("alpha", None)))));
        assert!(!same(
            &sealed,
            &fake("elsewhere", "w", Some(("alpha", None)))
        ));
        let mut changed = fake("moved", "w", Some(("alpha", None)));
        changed.digest = "d2".into();
        assert!(!same(&sealed, &changed));
        let stays = fake("ext", "w", Some(("alpha", None)));
        assert!(!same(&stays, &fake("moved", "w", Some(("alpha", None)))));
        assert!(!same(&fake("ext", "w", None), &fake("moved", "w", None)));
    }

    /// The note of where a moved package's object will be: the plan's move of
    /// that package from that schema, and no other.
    #[test]
    fn a_move_names_only_its_own_package_and_schema() {
        let set = ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(Change::SetExtensionSchema {
                uid: "e_aaaaaa".parse().unwrap(),
                name: "alpha".into(),
                from: extension("ext", "1.0"),
                to: extension("moved", "1.0"),
                releases: Default::default(),
            })],
        };
        assert_eq!(moved_to("alpha", "ext", &set).as_deref(), Some("moved"));
        assert_eq!(moved_to("beta", "ext", &set), None);
        assert_eq!(moved_to("alpha", "elsewhere", &set), None);
        assert_eq!(moved_to("alpha", "ext", &ChangeSet::default()), None);
    }
}
