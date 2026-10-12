//! Expected observations migrate here one family at a time (DEC-1901.1).

mod check;
mod expect;

pub(super) use check::{system_time, table};
pub(super) use expect::TableChanges;

/// Kept readings compare exactly; only a field the plan changes can be
/// unpromised between statements (SPEC 7.6). The two planned forms preserve
/// each family's existing checkpoint policy rather than inferring one.
enum Req<T> {
    Keep(T),
    Planned(T),
    PlannedOrKeep(T, T),
    Unpromised,
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
