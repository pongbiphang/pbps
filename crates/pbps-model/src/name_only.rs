//! Kinds a [`Schema`] holds by name only (#2855, DEC-2855.1).
//!
//! Some objects are read, held and declared but no plan can alter them yet: a plan
//! creates or drops a text search object by its identity and nothing more, and none
//! touches a standalone sequence (today: text search objects, DEC-2728.1, DEC-2728.2,
//! and standalone sequences, DEC-2721.1).
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
//!   apply carries the previous record forward, less what the plan drops and with
//!   what it creates, since no plan alters one;
//! - the **buildable form** ([`Schema::buildable`]): without the objects no identity
//!   names, for the builds from empty;
//! - one **drift report** ([`Schema::name_only_drift`]) for recorded objects that are
//!   gone or changed, which no checksum sees (the checksums stay as they are, so no
//!   format version moves).
//!
//! Moving a kind from here to the planned kinds is one edit in [`Schema::classify`]
//! and its inverse, and every consumer follows.

use crate::SearchChange;
use crate::{
    Change, ChangeSet, ColumnType, IdsFile, ManagedType, ObjectName, Schema, SearchObject,
    SearchRef, Sequence,
};
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
    /// Standalone sequences, by their name in the relation namespace (DEC-2721.1).
    pub sequences: BTreeMap<ObjectName, Sequence>,
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
        let NameOnly { search, sequences } = self.name_only;
        Schema {
            search,
            sequences,
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
        let Self { search, sequences } = self;
        search.is_empty() && sequences.is_empty()
    }

    /// Only the objects `named_by` names, by key and not by definition: an object
    /// of a name both hold is kept whatever its definition, so a difference stays
    /// visible to the comparison it is for.
    #[must_use]
    pub fn restricted_to(self, named_by: &Self) -> Self {
        let Self { search, sequences } = self;
        let Self {
            search: named_search,
            sequences: named_sequences,
        } = named_by;
        Self {
            search: restricted(search, named_search),
            sequences: restricted(sequences, named_sequences),
        }
    }

    /// These and the objects `other` names that these do not: the names of both.
    /// For a name both hold, this side's definition is kept.
    #[must_use]
    pub fn or_named_by(self, other: &Self) -> Self {
        let Self {
            mut search,
            mut sequences,
        } = self;
        let Self {
            search: named_search,
            sequences: named_sequences,
        } = other;
        for (name, object) in named_search {
            search.entry(name.clone()).or_insert_with(|| object.clone());
        }
        for (name, object) in named_sequences {
            sequences
                .entry(name.clone())
                .or_insert_with(|| object.clone());
        }
        Self { search, sequences }
    }

    /// Recorded objects that are gone from `live` or hold another definition. An
    /// object only `live` holds is not here: it was never recorded, and so it is not
    /// this state's to call drift (DEC-2728.1).
    #[must_use]
    pub fn drift(&self, live: &Self) -> Vec<NameOnlyDrift> {
        let Self { search, sequences } = self;
        let Self {
            search: live_search,
            sequences: live_sequences,
        } = live;
        let mut out = Vec::new();
        drift_of(
            search,
            live_search,
            |name| name.shown().to_string(),
            &mut out,
        );
        drift_of(
            sequences,
            live_sequences,
            |name| format!("sequence `{}`", name.shown()),
            &mut out,
        );
        out
    }

    /// What an apply leaves of these, by one rule: an object is recorded exactly when its
    /// identity is held (`live`) and the database holds it. The identities are as the
    /// database holds them now: a staged run keeps them current statement by statement,
    /// and a finished apply holds them as the plan leaves them. So an object the plan
    /// created is recorded once its create ran, one it dropped is gone once its drop ran,
    /// one a declaration adopted (it stood on the target and matched, so the plan made no
    /// change for it) is recorded with its new identity, and an object no identity names
    /// is not the project's and is never recorded.
    ///
    /// Which definition: an object the previous record holds keeps the definition it was
    /// recorded with, even where the read-back says otherwise (what it holds of the object
    /// is not the plan's result, and a record that took it would bless a change made
    /// outside the plan), and even where the database no longer holds it (`verify` names it
    /// as gone). The exception is an object `applied` creates or alters in place
    /// (DEC-2728.3): it is the plan's own result, and a staged create that was stopped and resumed has a
    /// previous record (the checkpoint) holding it half made, so the read-back's definition
    /// is the one recorded. Any other is as the read-back holds it, and is recorded only if
    /// it does.
    #[must_use]
    pub fn after_applying(self, read: &Self, applied: &ChangeSet, live: &IdsFile) -> Self {
        let Self {
            search: previous,
            sequences,
        } = self;
        let Self { search: read, .. } = read;
        let made: std::collections::BTreeSet<&SearchRef> = applied
            .changes
            .iter()
            .filter_map(|planned| {
                if let Change::Search(
                    SearchChange::CreateSearch { name, .. }
                    | SearchChange::AlterSearchMapping { name, .. }
                    | SearchChange::AlterSearchDictionary { name, .. },
                ) = &planned.change
                {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        // A rename moves an object's name and nothing else, so the record it had under the
        // old name is the one it keeps under the new one (DEC-2728.5). Without this the
        // new name has no record and the read-back would be recorded in its place. At a
        // staged checkpoint the identities may still hold the old name, which `previous`
        // answers directly, so the carry is only for the name the rename has reached.
        let renamed: BTreeMap<&SearchRef, &SearchRef> = applied
            .changes
            .iter()
            .filter_map(|planned| {
                if let Change::Search(SearchChange::RenameSearch { from, to, .. }) = &planned.change
                {
                    Some((to, from))
                } else {
                    None
                }
            })
            .collect();
        // And a configuration that maps a dictionary the plan renamed keeps the record it had,
        // under the dictionary's new spelling: the read-back spells it that way, and a record
        // left at the old name would read as drift.
        let moved: BTreeMap<SearchRef, SearchRef> = renamed
            .iter()
            .map(|(to, from)| ((*from).clone(), (*to).clone()))
            .collect();
        let search = live
            .search
            .values()
            .filter_map(|name| {
                let object = if made.contains(name) {
                    read.get(name).or_else(|| previous.get(name)).cloned()
                } else {
                    previous
                        .get(name)
                        .or_else(|| renamed.get(name).and_then(|from| previous.get(*from)))
                        .map(|held| held.respelled(&moved))
                        .or_else(|| read.get(name).cloned())
                };
                object.map(|object| (name.clone(), object))
            })
            .collect();
        // No plan touches a sequence, so the previous record's are carried as they were.
        Self { search, sequences }
    }

    /// The objects an identity names, by name alone: what an apply's read-back is kept to.
    /// Only the key decides what is kept (`restricted_to`), so the definition each takes
    /// here is a placeholder no one reads.
    #[must_use]
    pub fn named_by_identities(live: &IdsFile) -> Self {
        let Ok(template) = "pg_catalog.simple".parse::<crate::TypeRef>() else {
            return Self::default();
        };
        let search = live
            .search
            .values()
            .map(|name| {
                (
                    name.clone(),
                    SearchObject::Dictionary {
                        template: template.clone(),
                        options: BTreeMap::new(),
                    },
                )
            })
            .collect();
        Self {
            search,
            sequences: BTreeMap::new(),
        }
    }

    /// Only the objects an identity names. The others are not this project's, and a
    /// build from nothing may make only what is. A sequence has no identity yet
    /// (DEC-2721.1), so none is built.
    #[must_use]
    pub fn identified_by(self, ids: &IdsFile) -> Self {
        let Self { mut search, .. } = self;
        search.retain(|name, _| ids.search_uid(name).is_some());
        Self {
            search,
            sequences: BTreeMap::new(),
        }
    }

    /// Type spellings: a sequence names its integer type, which a read spells as the
    /// catalog does and a declaration as its author did. It is spelled here, so every
    /// comparer spells it.
    #[must_use]
    pub fn spelled(self, spell: Spell<'_>) -> Self {
        let Self {
            search,
            mut sequences,
        } = self;
        for sequence in sequences.values_mut() {
            if let Some(ty) = spell(&sequence.ty) {
                sequence.ty = ty;
            }
        }
        Self { search, sequences }
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
            sequences,
            extensions,
            modules,
            roles,
        } = self;
        Classified {
            plannable: Schema {
                tables,
                types,
                search: BTreeMap::new(),
                sequences: BTreeMap::new(),
                extensions,
                modules,
                roles,
            },
            name_only: NameOnly { search, sequences },
        }
    }

    /// The name-only part of this schema, cloned.
    #[must_use]
    pub fn name_only(&self) -> NameOnly {
        let Schema {
            tables: _,
            types: _,
            search,
            sequences,
            extensions: _,
            modules: _,
            roles: _,
        } = self;
        NameOnly {
            search: search.clone(),
            sequences: sequences.clone(),
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

    /// What a re-recording reads for: every name-only object the declarations name
    /// or the previous record holds. The read is compared with that record before it
    /// replaces it, and a read kept only to the declarations would show a recorded
    /// object whose declaration was since removed as gone from a database that still
    /// holds it. The comparison done, [`Schema::as_recorded`] gives what is recorded.
    #[must_use]
    pub fn naming_also_what_is_recorded(&self, recorded: &Schema) -> Schema {
        let Classified {
            plannable,
            name_only,
        } = self.clone().classify();
        Classified {
            plannable,
            name_only: name_only.or_named_by(&recorded.name_only()),
        }
        .into_schema()
    }

    /// The recorded form of a read: a state records for a name-only kind only what
    /// the declarations name. One the database holds that no declaration names has
    /// no identity, so recording it would make dropping it drift.
    #[must_use]
    pub fn as_recorded(self, declared: &Schema) -> Schema {
        self.with_name_only_restricted_to(declared)
    }

    /// What an apply's read-back is kept to, in place of the declarations (they name
    /// what a recording from them keeps): the name-only objects an identity names, as
    /// `live` holds them. A read kept to nothing would lose the objects the plan made or
    /// adopted.
    #[must_use]
    pub fn naming_what_identities_name(live: &IdsFile) -> Schema {
        Classified {
            plannable: Schema::default(),
            name_only: NameOnly::named_by_identities(live),
        }
        .into_schema()
    }

    /// The recorded form after an apply: a name-only object is recorded exactly when its
    /// identity is held in `live` and the database holds it, and one the previous record
    /// holds keeps its recorded definition unless `applied` creates it (see
    /// [`NameOnly::after_applying`]). No plan alters one, so what the read-back holds of
    /// any other is not the plan's result, and a record that took it would bless a change
    /// made outside the plan (DEC-2728.2).
    #[must_use]
    pub fn as_recorded_after_apply(
        self,
        previous: &Schema,
        applied: &ChangeSet,
        live: &IdsFile,
    ) -> Schema {
        let Classified {
            plannable,
            name_only: read,
        } = self.classify();
        Classified {
            plannable,
            name_only: previous.name_only().after_applying(&read, applied, live),
        }
        .into_schema()
    }

    /// The buildable form, for a build from nothing: a name-only object is built
    /// only where an identity names it, as a type or a table is. One no identity
    /// names is the database's, not the project's, and the differ would refuse it
    /// as having none. Borrowed when there is nothing to leave out.
    #[must_use]
    pub fn buildable(&self, ids: &IdsFile) -> Cow<'_, Schema> {
        if self.name_only().identified_by(ids) == self.name_only() {
            Cow::Borrowed(self)
        } else {
            let Classified {
                plannable,
                name_only,
            } = self.clone().classify();
            Cow::Owned(
                Classified {
                    plannable,
                    name_only: name_only.identified_by(ids),
                }
                .into_schema(),
            )
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
