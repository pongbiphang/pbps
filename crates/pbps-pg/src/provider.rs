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

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError};
use pbps_model::escape::Escaped;
use pbps_model::{
    Change, ChangeSet, ProviderKind, ProviderMember, ProviderPin, Schema, TypePin, TypeProvider,
};

use crate::pins::Within;

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
    proof_where(standing, |m| m == member, noun, created, changed)
}

/// [`proof`] for an object whose member entry carries more than the pin knows: an
/// operator is listed with its operand types, which the pin (schema and symbol) does not
/// hold, so `is_member` says which entry is it.
pub fn proof_where(
    standing: Standing<'_>,
    is_member: impl Fn(&ProviderMember) -> bool,
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
    if !manifest.members.iter().any(is_member) {
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

impl<'a> From<&'a ProviderPin> for Standing<'a> {
    fn from(pin: &'a ProviderPin) -> Self {
        Standing {
            schema: &pin.schema,
            name: &pin.name,
            planned: pin.planned,
            provider: pin.provider.as_ref(),
        }
    }
}

fn failed(why: impl Into<String>) -> DbError {
    DbError::BadRow(why.into())
}

/// A kind of provider-owned object that a sealed plan pins by `ProviderPin`: an operator class
/// or an operator. What differs per kind is here (how one is read and named, what a key may do
/// with it, how a refusal names what holds it); the seal, the check and the proof are
/// [`seal_kind`], [`check_kind`] and [`proof_kind`], written once (DEC-2447.14, #2448 C1b).
pub(crate) trait PinKind {
    /// What a plan names: the spelling of one object.
    type Wanted: Clone + Ord;
    /// An object that is there, as it was read.
    type Item;
    /// The answer of one read: present, absent or unreadable.
    type Read;
    /// One place the desired schema names an object.
    type Use;
    const NOUN: Noun;
    const KIND: ProviderKind;

    fn uses(schemas: &[&Schema]) -> Vec<Self::Use>;
    fn used(used: &Self::Use) -> &Self::Wanted;
    /// The objects the changes of a plan name, which a sealed plan must carry a pin for.
    fn named_by(changes: &ChangeSet) -> BTreeSet<Self::Wanted>;
    async fn read(conn: &mut Conn, within: Within, wanted: &[Self::Wanted]) -> Vec<Self::Read>;
    fn present(read: &Self::Read) -> Option<&Self::Item>;
    fn is_absent(read: &Self::Read) -> bool;
    /// Why this answer is not an object a plan can rely on.
    fn refusal(read: &Self::Read, wanted: &Self::Wanted) -> Option<String>;
    fn item_wanted(item: &Self::Item) -> &Self::Wanted;
    fn extension(item: &Self::Item) -> Option<&(String, String)>;
    fn pin(item: &Self::Item) -> ProviderPin;
    fn placeholder(wanted: &Self::Wanted, provider: TypeProvider) -> ProviderPin;
    fn wanted_of(pin: &ProviderPin) -> Self::Wanted;
    fn in_schema(wanted: &Self::Wanted, schema: &str) -> Self::Wanted;
    /// The schema and the name a refusal shows.
    fn qualified(wanted: &Self::Wanted) -> (&str, &str);
    /// A refusal about an object, with what holds it.
    fn held_by(uses: &[Self::Use], wanted: &Self::Wanted, why: String) -> String;
    /// What a key may not do with an object that stands; a kind with no such rule has none.
    fn refuse_uses(_items: &[&Self::Item], _uses: &[Self::Use]) -> Result<(), String> {
        Ok(())
    }
    fn is_member(pin: &ProviderPin, member: &ProviderMember) -> bool;
}

fn shown<K: PinKind>(wanted: &K::Wanted) -> String {
    let (schema, name) = K::qualified(wanted);
    format!("{}.{}", Escaped(schema), Escaped(name))
}

/// Reads and checks every object of the kind the desired schema names; one missing from the
/// target in the schema of exactly one declared package is that package's placeholder
/// (DEC-2447.12).
pub(crate) async fn seal_kind<K: PinKind>(
    conn: &mut Conn,
    schemas: &[&Schema],
    changes: &ChangeSet,
) -> Result<Vec<ProviderPin>, DbError> {
    let uses = K::uses(schemas);
    let wanted: Vec<K::Wanted> = uses
        .iter()
        .map(|u| K::used(u).clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let declared = packages_by_schema(schemas);
    let reads = K::read(conn, Within::OwnTransaction, &wanted).await;
    let mut pins = Vec::new();
    let mut items = Vec::new();
    for (one, read) in wanted.iter().zip(&reads) {
        let name = shown::<K>(one);
        let (schema, _) = K::qualified(one);
        if K::is_absent(read) && declared.contains_key(schema) {
            let package = defer_to_package(&declared, schema, &name, K::NOUN)
                .expect("the schema holds a declared package")
                .map_err(|why| failed(K::held_by(&uses, one, why)))?;
            pins.push(K::placeholder(one, package));
        } else if let Some(item) = K::present(read) {
            if let Some(why) = K::refusal(read, one) {
                return Err(failed(K::held_by(&uses, one, why)));
            }
            let (extension, version) =
                K::extension(item).expect("a refusal-free object has a package");
            check_package(schemas, extension, version, &name, K::NOUN)
                .map_err(|why| failed(K::held_by(&uses, one, why)))?;
            items.push(item);
            pins.push(K::pin(item));
        } else {
            let why = K::refusal(read, one).expect("a non-present object refuses");
            return Err(failed(K::held_by(&uses, one, why)));
        }
    }
    K::refuse_uses(&items, &uses).map_err(failed)?;
    refuse_update_beside_provided(
        |extension| {
            items
                .iter()
                .find(|i| K::extension(i).is_some_and(|(e, _)| e == extension))
                .map(|i| shown::<K>(K::item_wanted(i)))
        },
        changes,
        K::NOUN,
    )
    .map_err(failed)?;
    for pin in pins.iter_mut().filter(|p| !p.planned) {
        if let Some(package) = &mut pin.provider {
            package.moved_to = moved_to(&package.extension, &pin.schema, changes);
        }
    }
    Ok(pins)
}

/// A pin read again is the pin that was sealed, except that an object its package's move
/// carried is in the schema the plan said it would be.
pub(crate) fn same_moved(sealed: &ProviderPin, now: &ProviderPin) -> bool {
    same_pin(
        sealed,
        now,
        |p| {
            (
                p.schema.clone(),
                p.provider.as_ref().and_then(|x| x.moved_to.clone()),
            )
        },
        |p| {
            let mut p = p.clone();
            if let Some(x) = &mut p.provider {
                x.moved_to = None;
            }
            p
        },
        |p, schema| ProviderPin {
            schema: schema.to_owned(),
            ..p
        },
    )
}

/// Refuses a sealed plan whose objects of the kind are not what it was sealed over: one the plan
/// names with no pin, an object that is gone, or one that changed.
pub(crate) async fn check_kind<K: PinKind>(
    conn: &mut Conn,
    within: Within,
    expected: &[ProviderPin],
    changes: &ChangeSet,
) -> Result<(), DbError> {
    let ours: Vec<&ProviderPin> = expected.iter().filter(|p| p.kind == K::KIND).collect();
    let pinned: BTreeSet<K::Wanted> = ours.iter().map(|p| K::wanted_of(p)).collect();
    for wanted in K::named_by(changes) {
        if !pinned.contains(&wanted) {
            return Err(failed(format!(
                "plan has no pin for the {} `{}`; plan again against the database",
                K::NOUN.object,
                shown::<K>(&wanted)
            )));
        }
    }
    let live: Vec<&ProviderPin> = ours.into_iter().filter(|p| !p.planned).collect();
    // Where each object is now: under its own name or the name its package's move gave it; it
    // keeps its OID, so exactly one is there.
    let mut candidates = Vec::new();
    for pin in &live {
        candidates.push(K::wanted_of(pin));
        if let Some(schema) = pin.provider.as_ref().and_then(|p| p.moved_to.as_deref()) {
            candidates.push(K::in_schema(&K::wanted_of(pin), schema));
        }
    }
    let reads: BTreeMap<K::Wanted, K::Read> = candidates
        .iter()
        .cloned()
        .zip(K::read(conn, within, &candidates).await)
        .collect();
    judge::<K>(&live, &reads)
}

/// Each live pin against what was read now: the object must be there, under its own name or the
/// one its package's move gave it, and be the object that was sealed.
fn judge<K: PinKind>(
    live: &[&ProviderPin],
    reads: &BTreeMap<K::Wanted, K::Read>,
) -> Result<(), DbError> {
    for pin in live {
        let own = K::wanted_of(pin);
        let moved = pin
            .provider
            .as_ref()
            .and_then(|p| p.moved_to.as_deref())
            .map(|schema| K::in_schema(&own, schema));
        let now = moved
            .as_ref()
            .and_then(|m| reads.get(m).filter(|r| K::present(r).is_some()))
            .or_else(|| reads.get(&own));
        match now.and_then(|r| K::present(r)) {
            Some(item) if same_moved(pin, &K::pin(item)) => {}
            _ => {
                return Err(failed(format!(
                    "the {} `{}.{}` changed since planning; plan again",
                    K::NOUN.object,
                    Escaped(&pin.schema),
                    Escaped(&pin.name)
                )));
            }
        }
    }
    Ok(())
}

/// The proof after the rehearsal for each placeholder of the kind (DEC-2447.12).
pub(crate) fn proof_kind<K: PinKind>(
    pins: &[ProviderPin],
    created: &[pbps_model::ExtensionPin],
    changed: &[pbps_model::ExtensionChangePin],
) -> Result<(), String> {
    for pin in pins.iter().filter(|p| p.kind == K::KIND && p.planned) {
        proof_where(
            Standing::from(pin),
            |m| K::is_member(pin, m),
            K::NOUN,
            created,
            changed,
        )?;
    }
    Ok(())
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
    /// The comparison after the read, for both kinds that use it: the object must be there and
    /// be what was sealed, wherever its package's move put it, and anything else is "changed"
    /// (DEC-2447.12). Each rule sits beside its negative.
    fn judging<K: PinKind>(
        sealed: K::Item,
        digest_changed: K::Item,
        carried_to_other: K::Item,
        absent: K::Read,
        present: impl Fn(K::Item) -> K::Read,
    ) {
        let mut pin = K::pin(&sealed);
        let own = K::wanted_of(&pin);
        let read_of = |entries: Vec<(K::Wanted, K::Read)>| entries.into_iter().collect();
        // Unchanged is accepted; a changed digest, an absent object and an unreadable one are
        // "changed since planning".
        judge::<K>(&[&pin], &read_of(vec![(own.clone(), present(sealed))])).unwrap();
        let why = judge::<K>(
            &[&pin],
            &read_of(vec![(own.clone(), present(digest_changed))]),
        )
        .unwrap_err()
        .to_string();
        assert!(why.contains("changed since planning"), "{why}");
        assert!(judge::<K>(&[&pin], &read_of(vec![(own.clone(), absent)])).is_err());
        assert!(judge::<K>(&[&pin], &read_of(vec![])).is_err());
        // An object its package's move carried is found where the plan said, and only there.
        pin.provider.as_mut().unwrap().moved_to = Some("elsewhere".into());
        let there = K::in_schema(&own, "elsewhere");
        let reads = read_of(vec![(there, present(carried_to_other))]);
        judge::<K>(&[&pin], &reads).unwrap();
    }

    #[test]
    fn a_class_read_again_is_the_class_that_was_sealed() {
        use crate::class_pins::ClassPins;
        use crate::class_read::{Class, ClassRead, Wanted};
        let class = |schema: &str, digest: &str| Class {
            wanted: Wanted {
                method: "gin".into(),
                schema: schema.into(),
                name: "gin_trgm_ops".into(),
            },
            oid: 1,
            is_default: false,
            input_type: "text".into(),
            extension: Some(("pg_trgm".into(), "1.6".into())),
            family_extension: Some("pg_trgm".into()),
            digest: digest.into(),
        };
        judging::<ClassPins>(
            class("ext", "d"),
            class("ext", "other"),
            class("elsewhere", "d"),
            ClassRead::Absent,
            |c| ClassRead::Present(Box::new(c)),
        );
    }

    #[test]
    fn an_operator_read_again_is_the_operator_that_was_sealed() {
        use crate::operator_pins::OperatorPins;
        use crate::operator_read::{Operator, OperatorRead, Wanted};
        let operator = |schema: &str, digest: &str| Operator {
            wanted: Wanted {
                schema: schema.into(),
                symbol: "%".into(),
            },
            oid: 1,
            extension: Some(("pg_trgm".into(), "1.6".into())),
            mixed: false,
            digest: digest.into(),
        };
        judging::<OperatorPins>(
            operator("ext", "d"),
            operator("ext", "other"),
            operator("elsewhere", "d"),
            OperatorRead::Absent,
            |o| OperatorRead::Present(Box::new(o)),
        );
    }
}
