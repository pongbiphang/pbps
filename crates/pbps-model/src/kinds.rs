//! The one list of the kinds a [`Schema`] holds (#2878).
//!
//! `Schema` is a struct of maps, and a consumer of a whole schema used to
//! write its own walk over the maps it knew. A kind added to `Schema`
//! compiled in every one of them, and each consumer that was not told about
//! it left the kind out without a word: a name that is never checked, a file
//! that is never listed, an object a drift check does not see. Those were
//! found one consumer at a time, in review.
//!
//! Here a consumer states its question as a trait, answers it once per kind,
//! and walks the schema with [`each_kind!`](crate::each_kind). The walk names
//! every kind in [`schema_kinds!`](crate::schema_kinds), so a kind with no
//! answer is a missing trait implementation: the consumer does not compile
//! until it says what it does with the kind, even when that is nothing.
//!
//! What this is not: a plugin mechanism. The list is static and compiled, and
//! nothing registers itself at run time (SPEC 14.3). The `Change` enum, the
//! operation effects and the SQL lowering stay exhaustive matches; this gives
//! the walks over `Schema` the same property those already have.

use std::collections::BTreeMap;

use crate::Schema;

/// One kind of object a [`Schema`] holds: a map of the schema, by its key.
///
/// Implemented by the marker types of this module and by nothing else. A
/// consumer bounds its own trait on this one and reads the kind's map through
/// [`SchemaKind::of`], so the consumer never names a field of `Schema`.
pub trait SchemaKind: 'static {
    /// What identifies one object of the kind within a schema.
    type Key: Ord;
    /// What the schema holds for it.
    type Value;

    /// The kind, as a value.
    const TAG: KindTag;

    /// The objects of this kind in `schema`.
    fn of(schema: &Schema) -> &BTreeMap<Self::Key, Self::Value>;

    /// The objects of this kind in `schema`, to change.
    fn of_mut(schema: &mut Schema) -> &mut BTreeMap<Self::Key, Self::Value>;
}

/// The kinds of a [`Schema`], once, for every macro that needs the list.
///
/// The order is the order a schema is written out and built: what the later
/// kinds may refer to comes first. It is not the field order of `Schema`,
/// which is the serialized order and must not move.
///
/// Invoked with the name of a callback macro of this crate and, optionally, tokens to pass
/// through to it first. Adding a kind is one line here; everything that walks
/// the kinds then fails to compile until the kind is answered.
#[doc(hidden)]
#[macro_export]
macro_rules! schema_kinds {
    ($callback:ident $(, $($passed:tt)*)?) => {
        $crate::$callback! {
            $(($($passed)*))?
            Extensions => extensions: $crate::name::ObjectLocalName => $crate::Extension, "extension";
            Types => types: $crate::TypeRef => $crate::ManagedType, "type";
            Search => search: $crate::SearchRef => $crate::SearchObject, "text search object";
            Tables => tables: $crate::name::TableName => $crate::Table, "table";
            Modules => modules: $crate::ModuleId => $crate::Module, "module";
            Roles => roles: $crate::RoleName => $crate::Role, "role";
        }
    };
}

// Exported only so that `schema_kinds!` can name it by path; it expands to
// items of this module and is of no use anywhere else.
#[doc(hidden)]
#[macro_export]
macro_rules! __define_kinds {
    ($($marker:ident => $field:ident: $key:ty => $value:ty, $label:literal;)+) => {
        /// A kind of object a [`Schema`] holds, as a value.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum KindTag {
            $(
                #[doc = concat!("The schema's `", stringify!($field), "`.")]
                $marker,
            )+
        }

        impl KindTag {
            /// Every kind, in the order [`each_kind!`](crate::each_kind) walks them.
            pub const ALL: &'static [KindTag] = &[$(KindTag::$marker),+];

            /// What a message calls one object of the kind.
            pub fn label(self) -> &'static str {
                match self {
                    $(KindTag::$marker => $label,)+
                }
            }
        }

        $(
            #[doc = concat!("The schema's `", stringify!($field), "`, as a [`SchemaKind`].")]
            #[derive(Debug, Clone, Copy)]
            pub struct $marker;

            impl SchemaKind for $marker {
                type Key = $key;
                type Value = $value;
                const TAG: KindTag = KindTag::$marker;

                fn of(schema: &Schema) -> &BTreeMap<$key, $value> {
                    &schema.$field
                }

                fn of_mut(schema: &mut Schema) -> &mut BTreeMap<$key, $value> {
                    &mut schema.$field
                }
            }
        )+

        /// Fails to compile when `Schema` gains a map the list does not name.
        ///
        /// The list is only as good as its coverage of `Schema`: a field left
        /// out of it would be skipped by every walk at once, which is the
        /// failure this module exists to remove.
        #[allow(dead_code)]
        fn every_field_of_a_schema_is_a_kind(schema: &Schema) {
            let Schema { $($field: _),+ } = schema;
        }
    };
}

schema_kinds!(__define_kinds);

/// Evaluates `$body` once for every kind of a [`Schema`], with `$K` naming
/// the kind's marker type (a [`SchemaKind`]).
///
/// ```
/// use pbps_model::kinds::SchemaKind;
/// let schema = pbps_model::Schema::default();
/// let mut objects = 0;
/// pbps_model::each_kind!(|K| objects += K::of(&schema).len());
/// assert_eq!(objects, 0);
/// ```
///
/// The body is ordinary code in the caller's scope: it may use `?`, borrow
/// locals and return. It is written once and compiled once per kind, so a
/// call such as `<K as MyQuestion>::answer(..)` needs `MyQuestion` for every
/// kind; that requirement is the point.
#[macro_export]
macro_rules! each_kind {
    (|$K:ident| $body:expr) => {
        $crate::schema_kinds!(__each_kind, $K, $body)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __each_kind {
    (($K:ident, $body:expr) $($marker:ident => $field:ident: $key:ty => $value:ty, $label:literal;)+) => {{
        $(
            {
                #[allow(dead_code, non_camel_case_types)]
                type $K = $crate::kinds::$marker;
                $body;
            }
        )+
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Extension, RoleName, Table, name::ObjectLocalName, name::TableName};

    /// A consumer's question, answered per kind: how many objects it holds.
    trait Count: SchemaKind {
        fn count(schema: &Schema) -> usize {
            Self::of(schema).len()
        }
    }
    impl Count for Extensions {}
    impl Count for Types {}
    impl Count for Search {}
    impl Count for Tables {}
    impl Count for Modules {}
    impl Count for Roles {}

    fn sample() -> Schema {
        let mut schema = Schema::default();
        schema
            .tables
            .insert(TableName::new("app", "a"), Table::default());
        schema
            .tables
            .insert(TableName::new("app", "b"), Table::default());
        schema.extensions.insert(
            ObjectLocalName::new("citext"),
            Extension {
                schema: "public".into(),
                version: "1.6".into(),
            },
        );
        schema
            .roles
            .insert(RoleName::new("reader"), crate::Role::default());
        schema
    }

    #[test]
    fn the_walk_visits_every_kind_once_in_the_listed_order() {
        let mut seen = Vec::new();
        each_kind!(|K| seen.push(K::TAG));
        assert_eq!(seen, KindTag::ALL);
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), seen.len(), "a kind is listed twice");
    }

    #[test]
    fn a_walk_reads_each_kind_through_its_own_map() {
        let schema = sample();
        let mut counts = Vec::new();
        each_kind!(|K| counts.push((K::TAG, <K as Count>::count(&schema))));
        assert_eq!(
            counts,
            [
                (KindTag::Extensions, 1),
                (KindTag::Types, 0),
                (KindTag::Search, 0),
                (KindTag::Tables, 2),
                (KindTag::Modules, 0),
                (KindTag::Roles, 1),
            ]
        );
    }

    #[test]
    fn a_kind_is_changed_through_the_same_map_it_is_read_from() {
        let mut schema = sample();
        Tables::of_mut(&mut schema).clear();
        assert!(schema.tables.is_empty());
        assert_eq!(
            Extensions::of(&schema).len(),
            1,
            "another kind is untouched"
        );
    }

    #[test]
    fn an_empty_schema_has_no_object_of_any_kind() {
        let schema = Schema::default();
        let mut total = 0;
        each_kind!(|K| total += K::of(&schema).len());
        assert_eq!(total, 0);
    }

    #[test]
    fn the_body_can_return_early_from_the_caller() {
        fn first_kind_holding_anything(schema: &Schema) -> Option<KindTag> {
            each_kind!(|K| if !K::of(schema).is_empty() {
                return Some(K::TAG);
            });
            None
        }
        assert_eq!(
            first_kind_holding_anything(&sample()),
            Some(KindTag::Extensions)
        );
        assert_eq!(first_kind_holding_anything(&Schema::default()), None);
    }

    #[test]
    fn every_kind_has_a_label_of_its_own() {
        let mut labels: Vec<_> = KindTag::ALL.iter().map(|kind| kind.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), KindTag::ALL.len());
    }
}
