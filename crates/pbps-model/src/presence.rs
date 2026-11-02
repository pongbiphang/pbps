//! A field that must be present and may be null (`Option<T>` without a
//! default), for the formats whose reader refuses a missing key.
//!
//! `serde` demands the key of any field with a custom deserializer, but
//! `schemars` cannot see inside it and calls an `Option` field optional. The
//! two halves therefore travel together on every such field:
//!
//! ```ignore
//! #[serde(deserialize_with = "crate::presence::required_option")]
//! #[schemars(required, schema_with = "crate::presence::required_option_schema::<T>")]
//! ```
//!
//! `required` alone would put the key in `required` but also drop `null` from
//! the type, so `schema_with` restores it. The fingerprint tests fail a
//! `deserialize_with` field that lacks the marker (#2412).

use schemars::{JsonSchema, Schema, SchemaGenerator};

pub(crate) fn required_option<'de, D: serde::Deserializer<'de>, T: serde::Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    <Option<T> as serde::Deserialize>::deserialize(d)
}

pub(crate) fn required_option_schema<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<Option<T>>()
}
