//! What a [`Change`] looks like outside the type system: its saved bytes, its
//! `Debug` text and its JSON schema (#2790).
//!
//! `Change` is an enum of families and each family an enum of its kinds, but a
//! change is saved as one flat object tagged by `op`, printed as the kind alone
//! and described by one schema, exactly as it was when every kind was a variant
//! of `Change`. A derive cannot do that for a nested enum, so the three impls
//! are written here, once, and the tests of `tests/change_wire.rs` hold them to
//! the words and bytes the flat enum had.

use super::*;
use schemars::{Schema, SchemaGenerator, json_schema};
use serde::de::{self, Deserializer, IntoDeserializer, MapAccess, SeqAccess, Unexpected, Visitor};
use serde::ser::Serializer;
use serde_json::Value;
use std::borrow::Cow;

/// Every op, in the order the flat enum declared its kinds, which is the order an
/// unknown op is refused with (`expected one of ...`), and the family that holds
/// it. One table for both: the names to list and the enum to ask. A kind added to
/// a family enum needs its row here, and `the_ops_are_the_kinds_of_the_family_enums`
/// fails until it has one.
const OPS: [(&str, ChangeFamily); 67] = [
    ("rebuild_partitioned_table", ChangeFamily::Partitions),
    ("copy_table_rows", ChangeFamily::Partitions),
    ("analyze_table_tree", ChangeFamily::Partitions),
    ("build_partitioned_index", ChangeFamily::Indexes),
    ("add_enum_value", ChangeFamily::TypeValues),
    ("rename_enum_value", ChangeFamily::TypeValues),
    ("alter_domain", ChangeFamily::TypeValues),
    ("enum_rebuild", ChangeFamily::TypeValues),
    ("create_type", ChangeFamily::Types),
    ("drop_type", ChangeFamily::Types),
    ("rename_type", ChangeFamily::Types),
    ("create_search", ChangeFamily::Search),
    ("drop_search", ChangeFamily::Search),
    ("create_extension", ChangeFamily::Extensions),
    ("drop_extension", ChangeFamily::Extensions),
    ("update_extension", ChangeFamily::Extensions),
    ("set_extension_schema", ChangeFamily::Extensions),
    ("create_table", ChangeFamily::Tables),
    ("drop_table", ChangeFamily::Tables),
    ("detach_partition", ChangeFamily::Partitions),
    ("attach_partition", ChangeFamily::Partitions),
    ("rename_table", ChangeFamily::Tables),
    ("add_column", ChangeFamily::Columns),
    ("drop_column", ChangeFamily::Columns),
    ("rename_column", ChangeFamily::Columns),
    ("alter_column_type", ChangeFamily::Columns),
    ("alter_column_nullability", ChangeFamily::Columns),
    ("alter_column_default", ChangeFamily::Columns),
    ("alter_column_expression", ChangeFamily::Columns),
    ("alter_column_numbering", ChangeFamily::Columns),
    ("add_computed_column", ChangeFamily::Columns),
    ("drop_computed_column", ChangeFamily::Columns),
    ("set_column_deprecated", ChangeFamily::Columns),
    ("set_replica_identity", ChangeFamily::TableProperties),
    ("set_comment", ChangeFamily::TableProperties),
    ("set_storage_parameters", ChangeFamily::TableProperties),
    ("set_table_persistence", ChangeFamily::TableProperties),
    ("set_partition_default", ChangeFamily::Partitions),
    ("set_partition_not_null", ChangeFamily::Partitions),
    ("set_index_storage_parameters", ChangeFamily::Indexes),
    ("create_policy", ChangeFamily::Policies),
    ("drop_policy", ChangeFamily::Policies),
    ("set_row_security", ChangeFamily::Policies),
    ("set_primary_key", ChangeFamily::Constraints),
    ("add_unique", ChangeFamily::Constraints),
    ("drop_unique", ChangeFamily::Constraints),
    ("set_exclusion", ChangeFamily::Constraints),
    ("add_foreign_key", ChangeFamily::Constraints),
    ("drop_foreign_key", ChangeFamily::Constraints),
    ("add_check", ChangeFamily::Constraints),
    ("drop_check", ChangeFamily::Constraints),
    ("add_index", ChangeFamily::Indexes),
    ("drop_index", ChangeFamily::Indexes),
    ("insert_row", ChangeFamily::Rows),
    ("update_row", ChangeFamily::Rows),
    ("delete_row", ChangeFamily::Rows),
    ("set_data_mode", ChangeFamily::Rows),
    ("set_ensure_keys", ChangeFamily::Rows),
    ("create_module", ChangeFamily::Modules),
    ("alter_module", ChangeFamily::Modules),
    ("drop_module", ChangeFamily::Modules),
    ("create_role", ChangeFamily::Roles),
    ("drop_role", ChangeFamily::Roles),
    ("rename_role", ChangeFamily::Roles),
    ("grant", ChangeFamily::Permissions),
    ("revoke", ChangeFamily::Permissions),
    ("public_execution", ChangeFamily::Permissions),
];

const OP_NAMES: [&str; OPS.len()] = {
    let mut names = [""; OPS.len()];
    let mut at = 0;
    while at < OPS.len() {
        names[at] = OPS[at].0;
        at += 1;
    }
    names
};

impl serde::Serialize for Change {
    /// The kind alone: each family enum writes its own `op` tag, so the family
    /// layer leaves no trace in a saved plan.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Change::Table(kind) => kind.serialize(serializer),
            Change::TableProperty(kind) => kind.serialize(serializer),
            Change::Partition(kind) => kind.serialize(serializer),
            Change::Column(kind) => kind.serialize(serializer),
            Change::Constraint(kind) => kind.serialize(serializer),
            Change::Index(kind) => kind.serialize(serializer),
            Change::Policy(kind) => kind.serialize(serializer),
            Change::Row(kind) => kind.serialize(serializer),
            Change::Module(kind) => kind.serialize(serializer),
            Change::Permission(kind) => kind.serialize(serializer),
            Change::TypeValue(kind) => kind.serialize(serializer),
            Change::Role(kind) => kind.serialize(serializer),
            Change::Type(kind) => kind.serialize(serializer),
            Change::Extension(kind) => kind.serialize(serializer),
            Change::Search(kind) => kind.serialize(serializer),
        }
    }
}

impl fmt::Debug for Change {
    /// The kind alone, as the derived `Debug` of the flat enum printed it. The
    /// planner breaks ties by this text and a refusal builds words from it, so a
    /// family name in front of it would change a saved plan's order.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Table(kind) => fmt::Debug::fmt(kind, f),
            Change::TableProperty(kind) => fmt::Debug::fmt(kind, f),
            Change::Partition(kind) => fmt::Debug::fmt(kind, f),
            Change::Column(kind) => fmt::Debug::fmt(kind, f),
            Change::Constraint(kind) => fmt::Debug::fmt(kind, f),
            Change::Index(kind) => fmt::Debug::fmt(kind, f),
            Change::Policy(kind) => fmt::Debug::fmt(kind, f),
            Change::Row(kind) => fmt::Debug::fmt(kind, f),
            Change::Module(kind) => fmt::Debug::fmt(kind, f),
            Change::Permission(kind) => fmt::Debug::fmt(kind, f),
            Change::TypeValue(kind) => fmt::Debug::fmt(kind, f),
            Change::Role(kind) => fmt::Debug::fmt(kind, f),
            Change::Type(kind) => fmt::Debug::fmt(kind, f),
            Change::Extension(kind) => fmt::Debug::fmt(kind, f),
            Change::Search(kind) => fmt::Debug::fmt(kind, f),
        }
    }
}

/// The entries of a change's object as they were read, repeats included, so that
/// the family's own derive sees (and refuses) a field written twice.
struct Entries(Vec<(String, Value)>);

struct EntriesAccess {
    entries: std::vec::IntoIter<(String, Value)>,
    value: Option<Value>,
}

impl<'de> Deserializer<'de> for Entries {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_map(EntriesAccess {
            entries: self.0.into_iter(),
            value: None,
        })
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier
        ignored_any
    }
}

impl<'de> MapAccess<'de> for EntriesAccess {
    type Error = serde_json::Error;

    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        let Some((key, value)) = self.entries.next() else {
            return Ok(None);
        };
        self.value = Some(value);
        seed.deserialize(key.into_deserializer()).map(Some)
    }

    fn next_value_seed<V: de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        match self.value.take() {
            Some(value) => seed.deserialize(value),
            None => Err(de::Error::custom("a value is read after its key")),
        }
    }
}

/// Reads the family's enum from the object's entries, which carry the `op` the
/// family's derive dispatches on.
fn family_of(
    family: ChangeFamily,
    entries: Vec<(String, Value)>,
) -> Result<Change, serde_json::Error> {
    use serde::Deserialize as _;
    let entries = Entries(entries);
    Ok(match family {
        ChangeFamily::Tables => Change::Table(TableChange::deserialize(entries)?),
        ChangeFamily::TableProperties => {
            Change::TableProperty(TablePropertyChange::deserialize(entries)?)
        }
        ChangeFamily::Partitions => Change::Partition(PartitionChange::deserialize(entries)?),
        ChangeFamily::Columns => Change::Column(ColumnChange::deserialize(entries)?),
        ChangeFamily::Constraints => Change::Constraint(ConstraintChange::deserialize(entries)?),
        ChangeFamily::Indexes => Change::Index(IndexChange::deserialize(entries)?),
        ChangeFamily::Policies => Change::Policy(PolicyChange::deserialize(entries)?),
        ChangeFamily::Rows => Change::Row(RowChange::deserialize(entries)?),
        ChangeFamily::Modules => Change::Module(ModuleChange::deserialize(entries)?),
        ChangeFamily::Permissions => Change::Permission(GrantChange::deserialize(entries)?),
        ChangeFamily::TypeValues => Change::TypeValue(TypeValueChange::deserialize(entries)?),
        ChangeFamily::Roles => Change::Role(RoleChange::deserialize(entries)?),
        ChangeFamily::Types => Change::Type(TypeChange::deserialize(entries)?),
        ChangeFamily::Extensions => Change::Extension(ExtensionChange::deserialize(entries)?),
        ChangeFamily::Search => Change::Search(SearchChange::deserialize(entries)?),
    })
}

/// The `op`, read as the derived enum read its tag: an identifier. By name, or, where
/// the decoder offers one (a plan read through `PlannedChange`'s flattening), by the
/// index of the variant in the order `OPS` keeps.
struct Op(usize);

impl<'de> de::Deserialize<'de> for Op {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(OpVisitor)
    }
}

struct OpVisitor;

impl Visitor<'_> for OpVisitor {
    type Value = Op;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("variant identifier")
    }

    fn visit_u64<E: de::Error>(self, index: u64) -> Result<Op, E> {
        match usize::try_from(index) {
            Ok(at) if at < OPS.len() => Ok(Op(at)),
            _ => Err(de::Error::invalid_value(
                Unexpected::Unsigned(index),
                &format!("variant index 0 <= i < {}", OPS.len()).as_str(),
            )),
        }
    }

    fn visit_str<E: de::Error>(self, name: &str) -> Result<Op, E> {
        match OPS.iter().position(|(op, _)| *op == name) {
            Some(at) => Ok(Op(at)),
            None => Err(de::Error::unknown_variant(name, &OP_NAMES)),
        }
    }
}

/// What the first pass found: the family and every entry, not yet read as a kind.
struct Pending(ChangeFamily, Vec<(String, Value)>);

struct ChangeVisitor;

impl<'de> Visitor<'de> for ChangeVisitor {
    type Value = Pending;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("internally tagged enum Change")
    }

    /// First pass over the object. The `op` is read where it is met, as the derived
    /// enum read its tag: a second one is refused, and a value that is not an
    /// identifier or names no kind is refused with the position just after it
    /// (listing every op in the order of `OPS`). The rest is kept whole for the
    /// family's own derive, whose field wording and refusal of an unknown field
    /// are the ones the flat enum gave. No `op` at all is refused at the end.
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Pending, A::Error> {
        let mut family: Option<ChangeFamily> = None;
        let mut entries: Vec<(String, Value)> = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            if key == "op" {
                if family.is_some() {
                    return Err(de::Error::duplicate_field("op"));
                }
                let Op(at) = map.next_value()?;
                family = Some(OPS[at].1);
                // The kind's own enum is told the op by name, whatever the index was.
                entries.push((key, Value::String(OPS[at].0.to_owned())));
            } else {
                let value: Value = map.next_value()?;
                entries.push((key, value));
            }
        }
        match family {
            Some(family) => Ok(Pending(family, entries)),
            None => Err(de::Error::missing_field("op")),
        }
    }

    /// A sequence is read as the derived enum read it: the first element is the
    /// tag, an identifier. Only the refusals are reproduced, which is all a
    /// sequence of anything pbps writes can give.
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Pending, A::Error> {
        match seq.next_element::<Op>()? {
            None => Err(de::Error::missing_field("op")),
            Some(Op(at)) => Err(de::Error::custom(format_args!(
                "a change is an object tagged by `op`, not a sequence starting with `{}`",
                OPS[at].0
            ))),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Change {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Two passes, as the derived enum made two: the first, inside the decoder,
        // finds the `op` (and gets the decoder's position on a refusal); the second
        // reads the kind after the decoder has returned, so a refusal of a field
        // carries no position, as the flat enum's did.
        let Pending(family, entries) = deserializer.deserialize_any(ChangeVisitor)?;
        family_of(family, entries).map_err(de::Error::custom)
    }
}

/// The documentation of the flat enum, which is the description its schema had.
const SCHEMA_DESCRIPTION: &str = "Every variant carries the UID of the object it affects, so that applying a\nplan never has to match on names — names are exactly what may be changing.";

impl JsonSchema for Change {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Change")
    }

    /// One `oneOf`, the variants of every family in the order of `OPS`, as the
    /// flat enum's derive wrote it.
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut variants: Vec<(usize, Value)> = Vec::new();
        for schema in [
            TableChange::json_schema(generator),
            TablePropertyChange::json_schema(generator),
            PartitionChange::json_schema(generator),
            ColumnChange::json_schema(generator),
            ConstraintChange::json_schema(generator),
            IndexChange::json_schema(generator),
            PolicyChange::json_schema(generator),
            RowChange::json_schema(generator),
            ModuleChange::json_schema(generator),
            GrantChange::json_schema(generator),
            TypeValueChange::json_schema(generator),
            RoleChange::json_schema(generator),
            TypeChange::json_schema(generator),
            ExtensionChange::json_schema(generator),
            SearchChange::json_schema(generator),
        ] {
            let Some(Value::Array(own)) = schema.get("oneOf") else {
                continue;
            };
            for variant in own {
                let op = variant["properties"]["op"]["const"]
                    .as_str()
                    .unwrap_or_default();
                if let Some(at) = OPS.iter().position(|(name, _)| *name == op) {
                    variants.push((at, variant.clone()));
                }
            }
        }
        variants.sort_by_key(|(at, _)| *at);
        let variants: Vec<Value> = variants.into_iter().map(|(_, variant)| variant).collect();
        json_schema!({
            "description": SCHEMA_DESCRIPTION,
            "oneOf": variants,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The ops a family's enum writes, read from its own schema, which the derive
    /// writes from the variants: so the table cannot say what the enums do not.
    fn ops_of<T: JsonSchema>() -> BTreeSet<String> {
        let schema = T::json_schema(&mut SchemaGenerator::default());
        let Some(Value::Array(variants)) = schema.get("oneOf") else {
            panic!("a family enum is a oneOf of its kinds");
        };
        variants
            .iter()
            .map(|v| {
                v["properties"]["op"]["const"]
                    .as_str()
                    .expect("an op")
                    .to_owned()
            })
            .collect()
    }

    fn table_ops(family: ChangeFamily) -> BTreeSet<String> {
        OPS.iter()
            .filter(|(_, held)| *held == family)
            .map(|(op, _)| (*op).to_owned())
            .collect()
    }

    /// Every op of every family enum is in the table under its family, and the
    /// table holds nothing else. A kind added to an enum fails here until its row
    /// is added, and so does a row with no kind.
    #[test]
    fn the_ops_are_the_kinds_of_the_family_enums() {
        let families = [
            (ChangeFamily::Tables, ops_of::<TableChange>()),
            (
                ChangeFamily::TableProperties,
                ops_of::<TablePropertyChange>(),
            ),
            (ChangeFamily::Partitions, ops_of::<PartitionChange>()),
            (ChangeFamily::Columns, ops_of::<ColumnChange>()),
            (ChangeFamily::Constraints, ops_of::<ConstraintChange>()),
            (ChangeFamily::Indexes, ops_of::<IndexChange>()),
            (ChangeFamily::Policies, ops_of::<PolicyChange>()),
            (ChangeFamily::Rows, ops_of::<RowChange>()),
            (ChangeFamily::Modules, ops_of::<ModuleChange>()),
            (ChangeFamily::Permissions, ops_of::<GrantChange>()),
            (ChangeFamily::TypeValues, ops_of::<TypeValueChange>()),
            (ChangeFamily::Roles, ops_of::<RoleChange>()),
            (ChangeFamily::Types, ops_of::<TypeChange>()),
            (ChangeFamily::Extensions, ops_of::<ExtensionChange>()),
            (ChangeFamily::Search, ops_of::<SearchChange>()),
        ];
        let mut counted = 0;
        for (family, ops) in &families {
            assert_eq!(&table_ops(*family), ops, "{family:?}");
            counted += ops.len();
        }
        assert_eq!(
            counted,
            OPS.len(),
            "a row of the table names no family enum"
        );
        let distinct: BTreeSet<_> = OPS.iter().map(|(op, _)| op).collect();
        assert_eq!(distinct.len(), OPS.len(), "an op is in the table twice");
    }

    /// Negative: a table that lacked one of a family's kinds is not equal to it.
    #[test]
    fn a_family_with_a_kind_the_table_lacks_is_not_the_table() {
        let mut short = table_ops(ChangeFamily::Tables);
        short.remove("drop_table");
        assert_ne!(short, ops_of::<TableChange>());
    }
}
