//! The internal exclusive source keeps the historical, checksum-pinned spelling.
//! Raw options exist only while reading or writing this boundary.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Collation, Column, ColumnType, Generated, Identity, OwnedSequence, ValueSource};

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
    sequence: Option<&'a OwnedSequence>,
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
        let (mut default, mut identity, mut sequence, mut generated) = (None, None, None, None);
        match &self.value_source {
            ValueSource::None => {}
            ValueSource::Default(text) => default = Some(text.as_str()),
            ValueSource::Identity(i) => identity = Some(i),
            ValueSource::Sequence(s) => sequence = Some(s),
            ValueSource::Generated(g) => generated = Some(g),
        }
        ColumnRef {
            ty: &self.ty,
            nullable: self.nullable,
            default,
            identity,
            sequence,
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
    sequence: Option<Option<OwnedSequence>>,
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
        let mut sources = [
            raw.default.flatten().map(ValueSource::Default),
            raw.identity.flatten().map(ValueSource::Identity),
            raw.sequence.flatten().map(ValueSource::Sequence),
            raw.generated.flatten().map(ValueSource::Generated),
        ]
        .into_iter()
        .flatten();
        let value_source = match (sources.next(), sources.next()) {
            (None, _) => ValueSource::None,
            (Some(source), None) => source,
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "a column can have only one of default, identity, sequence or generated",
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
