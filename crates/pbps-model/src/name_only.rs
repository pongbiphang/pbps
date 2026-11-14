//! Kinds a [`Schema`] holds by name only (#2855, DEC-2855.1).
//!
//! Some objects are read, held and declared but have no identity record and no
//! plan can create, alter or drop them yet (today: text search objects, DEC-2728.1).
//! Every consumer of a whole schema used to decide for itself what to do with
//! such a kind, and the default of "treat it like a table" was wrong at nine of
//! them. The rule now lives here, in a few functions that are the only place
//! that knows which fields of a [`Schema`] are which:
//!
//! - [`Schema::classify`] splits a schema into the kinds a plan can change and
//!   the kinds held by name only, by exhaustive destructuring. A new field of
//!   `Schema` fails to compile there until it is classified, and nowhere else
//!   has to enumerate kinds.
//! - the **compared form** ([`Schema::compared_with`]): type spellings normalized
//!   for every kind, and a name-only kind kept only where the other side names it;
//! - the **recorded form** ([`Schema::as_recorded`], [`Schema::as_recorded_after_apply`]):
//!   a state records for a name-only kind only what the declarations name, and an
//!   apply carries the previous record forward, since no plan can change one;
//! - the **buildable form** ([`Schema::buildable`]): without the kinds no plan can
//!   build, for the builds from empty;
//! - one **drift report** ([`Schema::name_only_drift`]) for recorded objects that are
//!   gone or changed, which no checksum sees (the checksums stay as they are, so no
//!   format version moves).
//!
//! Moving a kind from here to the planned kinds is one edit in [`Schema::classify`]
//! and its inverse, and every consumer follows.

use crate::{ColumnType, ManagedType, Schema, SearchObject, SearchRef};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

/// How a dialect spells a column type; `None` leaves it as written. A type the
/// dialect cannot normalize is already a finding of its own, and rewriting what a
/// message quotes would make it name something the file does not (DECISIONS 187).
pub type Spell<'a> = &'a dyn Fn(&ColumnType) -> Option<ColumnType>;

/// The kinds a plan cannot yet create, alter or drop, held by their name.
///
/// A new kind joins this struct and the destructures below, which the compiler
/// lists; a kind that gains plans leaves them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameOnly {
    /// Text search configurations and dictionaries (DEC-2728.1).
    pub search: BTreeMap<SearchRef, SearchObject>,
}

/// A [`Schema`] split by [`Schema::classify`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Classified {
    /// Every kind a plan can change; the name-only fields are empty.
    pub plannable: Schema,
    pub name_only: NameOnly,
}

impl Classified {
    /// The schema these halves were split from.
    #[must_use]
    pub fn into_schema(self) -> Schema {
        let NameOnly { search } = self.name_only;
        Schema {
            search,
            ..self.plannable
        }
    }
}

/// What differs between a recorded name-only object and the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameOnlyChange {
    Gone,
    Changed,
}

/// One recorded name-only object that is gone from the database or changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameOnlyDrift {
    /// The object as a person reads it, escaped once: `text search dictionary `app.x``.
    pub object: String,
    pub change: NameOnlyChange,
}

impl fmt::Display for NameOnlyDrift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.change {
            NameOnlyChange::Gone => {
                write!(
                    f,
                    "{} was recorded and is no longer in the database",
                    self.object
                )
            }
            NameOnlyChange::Changed => {
                write!(
                    f,
                    "{} differs from the definition recorded for it",
                    self.object
                )
            }
        }
    }
}

fn drift_of<K: Ord, V: PartialEq>(
    recorded: &BTreeMap<K, V>,
    live: &BTreeMap<K, V>,
    shown: impl Fn(&K) -> String,
    out: &mut Vec<NameOnlyDrift>,
) {
    for (name, was) in recorded {
        let change = match live.get(name) {
            None => NameOnlyChange::Gone,
            Some(now) if now != was => NameOnlyChange::Changed,
            Some(_) => continue,
        };
        out.push(NameOnlyDrift {
            object: shown(name),
            change,
        });
    }
}

fn restricted<K: Ord, V>(held: BTreeMap<K, V>, named: &BTreeMap<K, V>) -> BTreeMap<K, V> {
    held.into_iter()
        .filter(|(name, _)| named.contains_key(name))
        .collect()
}

impl NameOnly {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let Self { search } = self;
        search.is_empty()
    }

    /// Only the objects `named_by` names, by key and not by definition: an object
    /// of a name both hold is kept whatever its definition, so a difference stays
    /// visible to the comparison it is for.
    #[must_use]
    pub fn restricted_to(self, named_by: &Self) -> Self {
        let Self { search } = self;
        let Self { search: named } = named_by;
        Self {
            search: restricted(search, named),
        }
    }

    /// Recorded objects that are gone from `live` or hold another definition. An
    /// object only `live` holds is not here: it was never recorded, and so it is not
    /// this state's to call drift (DEC-2728.1).
    #[must_use]
    pub fn drift(&self, live: &Self) -> Vec<NameOnlyDrift> {
        let Self { search } = self;
        let Self { search: live } = live;
        let mut out = Vec::new();
        drift_of(search, live, |name| name.shown().to_string(), &mut out);
        out
    }

    /// Type spellings: no name-only kind names a column type today. A kind that
    /// does (a sequence's type) is spelled here, so every comparer spells it.
    #[must_use]
    pub fn spelled(self, _spell: Spell<'_>) -> Self {
        let Self { search } = self;
        Self { search }
    }
}

impl Schema {
    /// The one place that says which kinds a plan can change and which are held by
    /// name only. No `..`: a new field must be classified here.
    #[must_use]
    pub fn classify(self) -> Classified {
        let Schema {
            tables,
            types,
            search,
            extensions,
            modules,
            roles,
        } = self;
        Classified {
            plannable: Schema {
                tables,
                types,
                search: BTreeMap::new(),
                extensions,
                modules,
                roles,
            },
            name_only: NameOnly { search },
        }
    }

    /// The name-only part of this schema, cloned.
    #[must_use]
    pub fn name_only(&self) -> NameOnly {
        let Schema {
            tables: _,
            types: _,
            search,
            extensions: _,
            modules: _,
            roles: _,
        } = self;
        NameOnly {
            search: search.clone(),
        }
    }

    /// This schema with each name-only object kept only where `named_by` names it.
    #[must_use]
    pub fn with_name_only_restricted_to(self, named_by: &Schema) -> Schema {
        let Classified {
            plannable,
            name_only,
        } = self.classify();
        Classified {
            plannable,
            name_only: name_only.restricted_to(&named_by.name_only()),
        }
        .into_schema()
    }

    /// This schema with every type in the dialect's spelling, for every kind.
    #[must_use]
    pub fn with_types_spelled(self, spell: Spell<'_>) -> Schema {
        let Classified {
            mut plannable,
            name_only,
        } = self.classify();
        for definition in plannable.types.values_mut() {
            if let ManagedType::Domain { base, .. } = definition
                && let Some(ty) = spell(base)
            {
                *base = ty;
            }
        }
        for table in plannable.tables.values_mut() {
            for column in table.columns.values_mut() {
                if let Some(ty) = spell(&column.ty) {
                    column.ty = ty;
                }
            }
        }
        Classified {
            plannable,
            name_only: name_only.spelled(spell),
        }
        .into_schema()
    }

    /// The compared form: type spellings normalized, and each name-only object kept
    /// only where `named_by` names it. A drift comparison projects one side only (the
    /// live one, onto what the record names); a comparison of two peers projects each
    /// onto the other.
    #[must_use]
    pub fn compared_with(&self, named_by: &Schema, spell: Spell<'_>) -> Schema {
        self.clone()
            .with_name_only_restricted_to(named_by)
            .with_types_spelled(spell)
    }

    /// Whether the two are equal as compared: each side kept only where the other
    /// names its name-only objects. Type spellings are the caller's, where it has a
    /// dialect (`pbps_dialect::compared`).
    #[must_use]
    pub fn equal_as_compared(&self, other: &Schema) -> bool {
        if self.name_only().is_empty() && other.name_only().is_empty() {
            return self == other;
        }
        self.clone().with_name_only_restricted_to(other)
            == other.clone().with_name_only_restricted_to(self)
    }

    /// Whether `live` is this recorded schema. One-sided: `live` is kept only where
    /// the record names its name-only objects, so one the record never held is not a
    /// difference, and a recorded one that is gone or changed still is. (The
    /// symmetric [`Schema::equal_as_compared`] would drop a recorded object that is
    /// gone along with the live ones, and hide exactly the drift this asks about.)
    #[must_use]
    pub fn recorded_matches(&self, live: &Schema) -> bool {
        if live.name_only().is_empty() {
            return self == live;
        }
        self == &live.clone().with_name_only_restricted_to(self)
    }

    /// The recorded form of a read: a state records for a name-only kind only what
    /// the declarations name. One the database holds that no declaration names has
    /// no identity, so recording it would make dropping it drift.
    #[must_use]
    pub fn as_recorded(self, declared: &Schema) -> Schema {
        self.with_name_only_restricted_to(declared)
    }

    /// The recorded form after an apply: the previous record's name-only objects,
    /// carried forward as they were. A plan can not create, alter or drop one, so
    /// what the read-back holds of them is not the plan's result, and a record that
    /// took it would bless a change made outside the plan.
    #[must_use]
    pub fn as_recorded_after_apply(self, previous: &Schema) -> Schema {
        let Classified { plannable, .. } = self.classify();
        Classified {
            plannable,
            name_only: previous.name_only(),
        }
        .into_schema()
    }

    /// The buildable form: without the kinds no plan can build, for a build from
    /// nothing. Borrowed when nothing is held by name only.
    #[must_use]
    pub fn buildable(&self) -> Cow<'_, Schema> {
        if self.name_only().is_empty() {
            Cow::Borrowed(self)
        } else {
            Cow::Owned(self.clone().classify().plannable)
        }
    }

    /// The one report of recorded name-only objects that are gone from the database
    /// or changed. `verify`, `status` and the `plan --db` drift gate call it; the
    /// checksums leave these kinds out and still do (#2846).
    #[must_use]
    pub fn name_only_drift(recorded: &Schema, live: &Schema) -> Vec<NameOnlyDrift> {
        recorded.name_only().drift(&live.name_only())
    }
}

#[cfg(test)]
mod tests;
