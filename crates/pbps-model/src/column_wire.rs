//! The internal exclusive source keeps the historical, checksum-pinned spelling.
//! Raw options exist only while reading or writing this boundary.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Collation, Column, ColumnType, Generated, Identity, OwnedSequence, Text, ValueSource};

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
    description: Option<&'a Text>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deprecated: Option<&'a Text>,
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
            description: self.description.as_ref(),
            deprecated: self.deprecated.as_ref(),
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
    description: Option<Option<Text>>,
    #[serde(default, deserialize_with = "present")]
    deprecated: Option<Option<Text>>,
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

/// A source on its own, as a change carries one (#1889): the column's keys
/// for it, at most one of them, and an empty map for none.
#[derive(Serialize)]
struct SourceRef<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<&'a Identity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sequence: Option<&'a OwnedSequence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generated: Option<&'a Generated>,
}

impl Serialize for ValueSource {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut wire = SourceRef {
            default: None,
            identity: None,
            sequence: None,
            generated: None,
        };
        match self {
            ValueSource::None => {}
            ValueSource::Default(text) => wire.default = Some(text.as_str()),
            ValueSource::Identity(i) => wire.identity = Some(i),
            ValueSource::Sequence(s) => wire.sequence = Some(s),
            ValueSource::Generated(g) => wire.generated = Some(g),
        }
        wire.serialize(serializer)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceWire {
    #[serde(default, deserialize_with = "present")]
    default: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    identity: Option<Option<Identity>>,
    #[serde(default, deserialize_with = "present")]
    sequence: Option<Option<OwnedSequence>>,
    #[serde(default, deserialize_with = "present")]
    generated: Option<Option<Generated>>,
}

impl<'de> Deserialize<'de> for ValueSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = SourceWire::deserialize(deserializer)?;
        let mut sources = [
            raw.default.flatten().map(ValueSource::Default),
            raw.identity.flatten().map(ValueSource::Identity),
            raw.sequence.flatten().map(ValueSource::Sequence),
            raw.generated.flatten().map(ValueSource::Generated),
        ]
        .into_iter()
        .flatten();
        match (sources.next(), sources.next()) {
            (None, _) => Ok(ValueSource::None),
            (Some(source), None) => Ok(source),
            (Some(_), Some(_)) => Err(serde::de::Error::custom(
                "a value source is only one of default, identity, sequence or generated",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Identity, OwnedSequence, ValueSource};

    /// A numbering change carries both sources in the saved plan (#1889):
    /// each round-trips in the column's own spelling, none as an empty map,
    /// and two at once are refused rather than one silently kept.
    #[test]
    fn a_value_source_round_trips_alone_and_refuses_two() {
        for source in [
            ValueSource::None,
            ValueSource::Default("0".into()),
            ValueSource::Identity(Identity {
                seed: 5,
                increment: -1,
                options: Default::default(),
            }),
            ValueSource::Sequence(OwnedSequence {
                start: 9,
                increment: 3,
                name: Some("numbers".into()),
                options: Default::default(),
            }),
            ValueSource::Generated(crate::Generated {
                expression: "n * 2".into(),
                stored: true,
            }),
        ] {
            let text = serde_json::to_string(&source).unwrap();
            assert_eq!(
                serde_json::from_str::<ValueSource>(&text).unwrap(),
                source,
                "{text}"
            );
        }
        assert_eq!(serde_json::to_string(&ValueSource::None).unwrap(), "{}");
        let two = r#"{"default": "0", "identity": {"seed": 1, "increment": 1}}"#;
        let err = serde_json::from_str::<ValueSource>(two).unwrap_err();
        assert!(err.to_string().contains("only one of"), "{err}");
        assert!(serde_json::from_str::<ValueSource>(r#"{"extra": 1}"#).is_err());
    }
}
