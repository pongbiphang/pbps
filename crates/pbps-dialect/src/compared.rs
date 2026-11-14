//! A whole schema in the dialect's spelling (#2855, DEC-2855.1).
//!
//! The projection itself (which kinds are held by name only, and kept only where the
//! other side names them) is `pbps-model`'s, because it needs no dialect. The
//! normalization of type spellings needs one, and `pbps-model` cannot see
//! `pbps-dialect` (the crate boundary runs the other way), so the wrapper that joins
//! the two lives here: the lowest crate that sees both. Every comparer that used to
//! carry its own copy of the normalization calls these.

use crate::Dialect;
use pbps_model::{ColumnType, Schema};

fn spell(dialect: &(impl Dialect + ?Sized)) -> impl Fn(&ColumnType) -> Option<ColumnType> + '_ {
    // A type the dialect cannot normalize is left exactly as written: it is already
    // an error from `refuse_invalid_declarations`, and rewriting what a finding quotes
    // would make the message name something the file does not (DECISIONS 187).
    move |ty| dialect.normalize_type(ty).ok()
}

/// `schema` with every type, of every kind, in the dialect's canonical spelling and
/// nothing else changed.
#[must_use]
pub fn spelled(schema: &Schema, dialect: &(impl Dialect + ?Sized)) -> Schema {
    schema.clone().with_types_spelled(&spell(dialect))
}

/// The compared form of `schema`: types in the dialect's spelling, and a kind held by
/// name only kept only where `named_by` names it.
#[must_use]
pub fn compared(schema: &Schema, named_by: &Schema, dialect: &(impl Dialect + ?Sized)) -> Schema {
    schema.compared_with(named_by, &spell(dialect))
}
