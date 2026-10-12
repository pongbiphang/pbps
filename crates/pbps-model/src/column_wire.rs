//! The internal exclusive source keeps the historical, checksum-pinned spelling.
//! Raw options exist only while reading or writing this boundary.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Collation, Column, ColumnType, Generated, Identity, ValueSource};

#[derive(Serialize)]
struct ColumnRef<'a> {
    #[serde(rename = "type")]
    ty: &'a ColumnType,
    nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<&'a Identity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generated: Option<&'a Generated>,
    #[serde(skip_serializing_if = "Option::is_none")]
    collation: Option<&'a Collation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deprecated: Option<&'a str>,
}

impl Serialize for Column {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (default, identity, generated) = match &self.value_source {
            ValueSource::None => (None, None, None),
            ValueSource::Default(text) => (Some(text.as_str()), None, None),
            ValueSource::Identity(identity) => (None, Some(identity), None),
            ValueSource::Generated(generated) => (None, None, Some(generated)),
        };
        ColumnRef {
            ty: &self.ty,
            nullable: self.nullable,
            default,
            identity,
            generated,
            collation: self.collation.as_ref(),
            description: self.description.as_deref(),
            deprecated: self.deprecated.as_deref(),
        }
        .serialize(serializer)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ColumnWire {
    #[serde(rename = "type")]
    ty: ColumnType,
    nullable: bool,
    #[serde(default, deserialize_with = "present")]
    default: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    identity: Option<Option<Identity>>,
    #[serde(default, deserialize_with = "present")]
    generated: Option<Option<Generated>>,
    #[serde(default, deserialize_with = "present")]
    collation: Option<Option<Collation>>,
    #[serde(default, deserialize_with = "present")]
    description: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    deprecated: Option<Option<String>>,
}

// Keep explicit null distinct from an unread field until serde has checked
// duplicate keys; otherwise a first null can hide a second occurrence.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for Column {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = ColumnWire::deserialize(deserializer)?;
        let value_source = match (
            raw.default.flatten(),
            raw.identity.flatten(),
            raw.generated.flatten(),
        ) {
            (None, None, None) => ValueSource::None,
            (Some(text), None, None) => ValueSource::Default(text),
            (None, Some(identity), None) => ValueSource::Identity(identity),
            (None, None, Some(generated)) => ValueSource::Generated(generated),
            (Some(_), Some(_), None)
            | (Some(_), None, Some(_))
            | (None, Some(_), Some(_))
            | (Some(_), Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "a column can have only one of default, identity or generated",
                ));
            }
        };
        Ok(Self {
            ty: raw.ty,
            nullable: raw.nullable,
            value_source,
            collation: raw.collation.flatten(),
            description: raw.description.flatten(),
            deprecated: raw.deprecated.flatten(),
        })
    }
}
