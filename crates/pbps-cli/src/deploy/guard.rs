//! Expected observations migrate here one family at a time (DEC-1901.1).

mod check;
mod created;
mod expect;
mod parts;
mod read;
mod tables;

pub(super) use check::{
    Fields, columns, columns_after, created_place, declared_column, implied, own, system_time,
    table,
};
pub(super) use created::CreatedChanges;
pub(super) use expect::{ColumnChanges, OwnNames, TableChanges};
pub(super) use parts::PartChanges;
pub(super) use read::{Read, Staged};
pub(super) use tables::Tables;

/// Kept readings compare exactly; only a field the plan changes can be
/// unpromised between statements (SPEC 7.6). The two planned forms preserve
/// each family's existing checkpoint policy rather than inferring one.
#[derive(Clone)]
enum Req<T> {
    Keep(T),
    Planned(T),
    PlannedOrKeep(T, T),
    Unpromised,
}

/// Two catalog readings keep their text; only a changed expression loses
/// its declared spelling when the engine stores it (DECISIONS 166).
#[derive(Clone)]
enum Expr<'a> {
    Keep(Option<&'a str>),
    Planned(bool),
    Unpromised,
}

/// Read-only field views of the exclusive value source. A planned expression
/// may move without excusing a change to its source kind or owned metadata.
struct Source<'a> {
    default: Option<&'a str>,
    identity: Option<&'a pbps_model::Identity>,
    sequence: Option<&'a pbps_model::OwnedSequence>,
    generated: Option<&'a pbps_model::Generated>,
}

impl<'a> Source<'a> {
    fn of(source: &'a pbps_model::ValueSource) -> Self {
        use pbps_model::ValueSource;
        let (default, identity, sequence, generated) = match source {
            ValueSource::None => (None, None, None, None),
            ValueSource::Default(text) => (Some(text.as_str()), None, None, None),
            ValueSource::Identity(identity) => (None, Some(identity), None, None),
            ValueSource::Sequence(sequence) => (None, None, Some(sequence), None),
            ValueSource::Generated(generated) => (None, None, None, Some(generated)),
        };
        Self {
            default,
            identity,
            sequence,
            generated,
        }
    }
}

impl Expr<'_> {
    fn accepts(&self, observed: Option<&str>) -> bool {
        match self {
            Self::Keep(kept) => *kept == observed,
            Self::Planned(present) => *present == observed.is_some(),
            Self::Unpromised => true,
        }
    }
}

impl<T: PartialEq> Req<T> {
    fn at_close(kept: T, planned: Option<T>, whole: bool) -> Self {
        match (planned, whole) {
            (None, _) => Self::Keep(kept),
            (Some(value), true) => Self::Planned(value),
            (Some(_), false) => Self::Unpromised,
        }
    }

    fn accepts(&self, observed: &T) -> bool {
        match self {
            Self::Keep(kept) | Self::Planned(kept) => observed == kept,
            Self::PlannedOrKeep(planned, kept) => observed == planned || observed == kept,
            Self::Unpromised => true,
        }
    }
}

#[cfg(test)]
mod tests;

/// Both inputs are pinned by the saved plan; observations supply no identity intent.
#[derive(Clone, Copy)]
pub(super) struct Plan<'a> {
    pub(super) changes: &'a pbps_model::ChangeSet,
    pub(super) ids: &'a pbps_model::IdsFile,
}

#[cfg(test)]
mod fixture;
#[cfg(test)]
pub(super) use fixture::ids as fixture_ids;
