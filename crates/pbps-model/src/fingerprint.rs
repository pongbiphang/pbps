//! The automatic format fingerprint of the persisted formats (#2412).
//!
//! A format fingerprint is a hash of the JSON Schema generated from the types
//! that are written to disk or to the target database. It exists in no source
//! literal, so there is nothing for two pull requests to collide on, and a
//! change to a format moves it without anyone remembering to.
//!
//! # What moves it, and what does not
//!
//! The question is whether a *reader of the old format* could misread the new
//! one, so the hashed form keeps what decides that and drops what does not:
//!
//! - **Moves it:** a field added, removed, renamed or retyped; a serde tag or
//!   rename changed; a variant added, removed or renamed; a field becoming
//!   optional or required; a default changing; `deny_unknown_fields` changing.
//! - **Leaves it:** doc comments (`description`, `title`), the order of
//!   fields, of `required`, of string-enum values and of the variants of a
//!   tagged enum, and the key order the JSON happens to be built in. None of these is visible to a reader.
//!
//! Both serde contracts are hashed, the one that reads and the one that
//! writes. Types with `try_from` or `into`, and the hand-written codecs
//! (`column_wire.rs`), describe a different shape on each side.
//!
//! # What it cannot see
//!
//! - A *semantic* change that keeps the shape: a string that now means
//!   something else, a checksum algorithm. The increment-rule test of the
//!   later pull requests pins those through the plan checksum.
//! - Nothing about a Rust type's name, except for a type that contains itself.
//!   The schemas are generated with every definition inlined, so a rename of a
//!   shared type does not move the fingerprint. A recursive type cannot be
//!   inlined and keeps a named definition, so renaming one of those moves it.
//!   That errs toward one needless increment, the safe direction.
//! - Anything behind `#[schemars(with = "serde_json::Value")]`: an opaque
//!   field is a hole. `no_persisted_format_has_an_opaque_field` fails if one
//!   appears in a covered format.
//!
//! # Why the published schema set is hashed elsewhere
//!
//! The editor schemas are generated in `pbps-cli` from types in other crates,
//! and they keep their `description` fields because those are content an
//! editor shows (DECISIONS 465). [`fingerprint`] takes the documents and the
//! choice of whether to ignore documentation, and `pbps-cli` supplies them.

use std::sync::OnceLock;

use schemars::JsonSchema;
use schemars::generate::{SchemaGenerator, SchemaSettings};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::plan::SavedPlan;
use crate::state::StateSnapshot;

/// Whether documentation keywords take part in the hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Docs {
    /// Drop `description`, `title` and the other annotations. For the
    /// persisted formats, whose doc comments describe code and not files.
    Ignore,
    /// Keep them. For the published editor schemas, where they are content.
    Keep,
}

/// Keywords whose value is a map from a name to a schema. The names are data
/// (a field called `description` is a field), so only the values are schemas.
const SCHEMA_MAPS: &[&str] = &[
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];

/// Keywords whose value is one schema.
const SCHEMA_VALUES: &[&str] = &[
    "items",
    "additionalProperties",
    "unevaluatedItems",
    "unevaluatedProperties",
    "contains",
    "propertyNames",
    "contentSchema",
    "not",
    "if",
    "then",
    "else",
];

/// Keywords whose value is a list of schemas.
///
/// The order of `anyOf`, `allOf` and `prefixItems` is kept. Schemars writes an
/// untagged enum as `anyOf`, and serde tries its variants in order, so there
/// the order is part of the format. A tagged or externally tagged enum is a
/// `oneOf` whose branches exclude each other, so reordering its variants
/// changes nothing a reader could see and the branches are sorted.
const SCHEMA_LISTS: &[&str] = &["oneOf", "anyOf", "allOf", "prefixItems"];

/// Annotation keywords. They describe a schema to a human and decide nothing.
const ANNOTATIONS: &[&str] = &[
    "description",
    "title",
    "$comment",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
];

/// The canonical form of one schema document: keys sorted, order-free lists
/// sorted, annotations dropped when asked to.
///
/// `serde_json` is built with `preserve_order`, so the key order of a generated
/// schema is the order the derive happened to emit it in, which follows the
/// field order of the Rust source. Sorting is what makes a reordered struct
/// leave the fingerprint alone.
pub fn canonical(schema: &Value, docs: Docs) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = Map::new();
            for key in sorted_keys(map) {
                let value = &map[key];
                if docs == Docs::Ignore && ANNOTATIONS.contains(&key) {
                    continue;
                }
                let canonical = if SCHEMA_MAPS.contains(&key) {
                    if let Value::Object(named) = value {
                        Value::Object(
                            sorted_keys(named)
                                .into_iter()
                                .map(|name| (name.to_owned(), canonical(&named[name], docs)))
                                .collect(),
                        )
                    } else {
                        plain(value)
                    }
                } else if SCHEMA_VALUES.contains(&key) {
                    canonical(value, docs)
                } else if SCHEMA_LISTS.contains(&key) {
                    if let Value::Array(list) = value {
                        let mut branches: Vec<Value> =
                            list.iter().map(|s| canonical(s, docs)).collect();
                        if key == "oneOf" {
                            branches = merge_string_literals(branches);
                            branches.sort_by_cached_key(Value::to_string);
                        }
                        Value::Array(branches)
                    } else {
                        plain(value)
                    }
                } else if matches!(key, "required" | "enum" | "type") {
                    sorted_list(value)
                } else {
                    // `default`, `const` and the like hold data, not schemas.
                    plain(value)
                };
                out.insert(key.to_owned(), canonical);
            }
            hoist_lone_branch(&mut out);
            // Hoisting and merging add keys at the end, so sort once more.
            canonical_map(out)
        }
        // `true` and `false` are schemas too, and nothing else is one.
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
            schema.clone()
        }
    }
}

/// `{"oneOf": [x]}` says what `x` says. Documenting the only variant of an
/// enum is what makes schemars write the first form instead of the second.
fn hoist_lone_branch(schema: &mut Map<String, Value>) {
    let Some(Value::Array(branches)) = schema.get("oneOf") else {
        return;
    };
    let [Value::Object(branch)] = branches.as_slice() else {
        return;
    };
    if branch.keys().any(|key| schema.contains_key(key)) {
        return;
    }
    let branch = branch.clone();
    schema.remove("oneOf");
    schema.extend(branch);
}

/// Merges the branches that are only a string literal into one `enum`.
///
/// Schemars writes the unit variants of an enum as one `{"type":"string",
/// "enum":[...]}` branch, until a single variant gets a doc comment. Then it
/// writes each as its own `{"type":"string","const":...}` branch so that the
/// comment has a place to go. Documentation is ignored, so the two spellings
/// of one format have to hash alike.
fn merge_string_literals(branches: Vec<Value>) -> Vec<Value> {
    let mut literals = Vec::new();
    let mut rest = Vec::new();
    for branch in branches {
        let Some(map) = branch.as_object() else {
            rest.push(branch);
            continue;
        };
        let is_string = map.get("type").and_then(Value::as_str) == Some("string");
        match (is_string, map.len(), map.get("const"), map.get("enum")) {
            (true, 2, Some(literal), None) => literals.push(literal.clone()),
            (true, 2, None, Some(Value::Array(list))) => literals.extend(list.iter().cloned()),
            _ => rest.push(branch),
        }
    }
    if !literals.is_empty() {
        literals.sort_by_cached_key(Value::to_string);
        rest.push(serde_json::json!({"type": "string", "enum": literals}));
    }
    rest
}

fn sorted_keys(map: &Map<String, Value>) -> Vec<&str> {
    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

/// A list whose order carries no meaning (`required`, `enum`, `type`).
fn sorted_list(value: &Value) -> Value {
    if let Value::Array(list) = value {
        let mut items: Vec<Value> = list.iter().map(plain).collect();
        items.sort_by_cached_key(Value::to_string);
        Value::Array(items)
    } else {
        plain(value)
    }
}

/// A data value with its object keys sorted and nothing else touched.
fn plain(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            sorted_keys(map)
                .into_iter()
                .map(|key| (key.to_owned(), plain(&map[key])))
                .collect(),
        ),
        Value::Array(list) => Value::Array(list.iter().map(plain).collect()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

/// The hash of named schema documents, as 64 lowercase hex digits.
///
/// The name is part of the hash, so moving a schema from one document to
/// another is a change. The order of `documents` is not.
pub fn fingerprint<'a>(
    documents: impl IntoIterator<Item = (&'a str, &'a Value)>,
    docs: Docs,
) -> String {
    let mut all = Map::new();
    for (name, schema) in documents {
        all.insert(name.to_owned(), canonical(schema, docs));
    }
    let bytes = serde_json::to_vec(&canonical_map(all)).expect("a JSON value always serializes");
    let mut hasher = Sha256::new();
    // A tag, so that deliberately changing what is hashed changes every
    // fingerprint at once and nobody has to guess why they all moved.
    hasher.update(b"pbps-format-fingerprint-1\n");
    hasher.update(&bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn canonical_map(map: Map<String, Value>) -> Value {
    Value::Object(
        sorted_keys(&map)
            .into_iter()
            .map(|key| (key.to_owned(), map[key].clone()))
            .collect(),
    )
}

/// The generated schema of `T` as the reader sees it and as the writer
/// writes it, in that order.
pub fn contract_schemas<T: JsonSchema>() -> [Value; 2] {
    [
        generator(SchemaSettings::draft2020_12().for_deserialize()),
        generator(SchemaSettings::draft2020_12().for_serialize()),
    ]
    .map(|g| g.into_root_schema_for::<T>().to_value())
}

/// Every definition is inlined so that the hash does not depend on what a Rust
/// type is called. The cost is a schema several times larger, which is
/// generated once per process.
fn generator(settings: SchemaSettings) -> SchemaGenerator {
    settings
        .with(|settings| settings.inline_subschemas = true)
        .into_generator()
}

/// The fingerprint of the persisted format rooted at `T`.
pub fn format_fingerprint<T: JsonSchema>() -> String {
    let [read, write] = contract_schemas::<T>();
    fingerprint([("read", &read), ("write", &write)], Docs::Ignore)
}

/// The fingerprint of the saved-plan file format.
pub fn saved_plan() -> &'static str {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(format_fingerprint::<SavedPlan>)
}

/// The fingerprint of the ledger snapshot format.
pub fn state_snapshot() -> &'static str {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(format_fingerprint::<StateSnapshot>)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The schema positions that accept anything: `true`, or an empty schema.
    /// Such a position is a hole in the fingerprint, because whatever is stored
    /// there can change shape without the hash noticing.
    fn opaque_positions(schema: &Value, path: &str, found: &mut Vec<String>) {
        match schema {
            Value::Bool(true) => found.push(path.to_owned()),
            Value::Object(map) if map.is_empty() => found.push(path.to_owned()),
            Value::Object(map) => {
                for (key, value) in map {
                    let here = format!("{path}/{key}");
                    if SCHEMA_MAPS.contains(&key.as_str()) {
                        if let Value::Object(named) = value {
                            for (name, sub) in named {
                                opaque_positions(sub, &format!("{here}/{name}"), found);
                            }
                        }
                    } else if SCHEMA_VALUES.contains(&key.as_str()) {
                        // `additionalProperties: true` is the one `true` that is
                        // not a hole in the shape: it is what the absence of
                        // `deny_unknown_fields` says.
                        if key != "additionalProperties" || value.is_object() {
                            opaque_positions(value, &here, found);
                        }
                    } else if SCHEMA_LISTS.contains(&key.as_str())
                        && let Value::Array(list) = value
                    {
                        for (i, sub) in list.iter().enumerate() {
                            opaque_positions(sub, &format!("{here}/{i}"), found);
                        }
                    }
                }
            }
            Value::Null
            | Value::Bool(false)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_) => {}
        }
    }

    /// A fixture format: the items are wrapped in a module of their own so that
    /// each variant of the same format can reuse the same type names.
    macro_rules! fixture {
        ($m:ident { $($items:item)* }) => {
            #[allow(dead_code)]
            mod $m {
                use schemars::JsonSchema;
                use serde::{Deserialize, Serialize};
                $($items)*
            }
        };
    }

    fixture!(base {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    // --- Changes of format -------------------------------------------------

    fixture!(field_added {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
            pub extra: u32,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(field_removed {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
    });

    fixture!(field_renamed {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub title: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(field_renamed_on_the_wire_only {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            #[serde(rename = "label")]
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(field_retyped {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u64,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(nested_field_retyped {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: u32 },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(field_made_optional {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: Option<String>,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(field_given_a_default {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            #[serde(default)]
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(unknown_fields_allowed {
        #[derive(Serialize, Deserialize, JsonSchema)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(tag_renamed {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(tag_removed {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(variant_added {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
            Rename { from: String, to: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(variant_removed {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(variant_renamed {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Remove { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(unit_variant_added {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
            Online,
        }
    });

    fixture!(field_named_description {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
            pub description: String,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    // --- Edits that are not a change of format ------------------------------

    fixture!(documented {
        /// The root of the format, with a doc comment the base does not have.
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            /// The version.
            pub version: u32,
            /// The name, and now a sentence about it.
            pub name: String,
            /// What the step is.
            pub step: Step,
            pub mode: Mode,
        }
        /// A step.
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            /// Adds one.
            Add {
                /// The table.
                table: String,
            },
            Drop { table: String },
        }
        /// How it runs.
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            /// In one transaction.
            Transactional,
            Staged,
        }
    });

    fixture!(reordered {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub mode: Mode,
            pub step: Step,
            pub name: String,
            pub version: u32,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Drop { table: String },
            Add { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Staged,
            Transactional,
        }
    });

    fixture!(with_code_around_it {
        #[derive(Debug, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub version: u32,
            pub name: String,
            pub step: Step,
            pub mode: Mode,
        }
        impl Root {
            fn a_private_helper(&self) -> u32 {
                self.version + 1
            }
            pub fn a_public_method(&self) -> u32 {
                self.a_private_helper()
            }
        }
        #[derive(Debug, Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Step {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Debug, Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Mode {
            Transactional,
            Staged,
        }
    });

    fixture!(types_renamed {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Plan {
            pub version: u32,
            pub name: String,
            pub step: Action,
            pub mode: Strategy,
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum Action {
            Add { table: String },
            Drop { table: String },
        }
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum Strategy {
            Transactional,
            Staged,
        }
    });

    fn fp<T: JsonSchema>() -> String {
        format_fingerprint::<T>()
    }

    macro_rules! moves {
        ($($test:ident: $changed:ident),+ $(,)?) => {$(
            #[test]
            fn $test() {
                assert_ne!(
                    fp::<base::Root>(),
                    fp::<$changed::Root>(),
                    "`{}` is a change of format and must move the fingerprint",
                    stringify!($changed)
                );
            }
        )+};
    }

    macro_rules! stays {
        ($($test:ident: $edited:ident::$root:ident),+ $(,)?) => {$(
            #[test]
            fn $test() {
                assert_eq!(
                    fp::<base::Root>(),
                    fp::<$edited::$root>(),
                    "`{}` is not a change of format and must not move the fingerprint",
                    stringify!($edited)
                );
            }
        )+};
    }

    moves! {
        adding_a_field_moves_the_fingerprint: field_added,
        removing_a_field_moves_the_fingerprint: field_removed,
        renaming_a_field_moves_the_fingerprint: field_renamed,
        renaming_a_field_on_the_wire_only_moves_the_fingerprint: field_renamed_on_the_wire_only,
        retyping_a_field_moves_the_fingerprint: field_retyped,
        retyping_a_field_of_a_nested_variant_moves_the_fingerprint: nested_field_retyped,
        making_a_field_optional_moves_the_fingerprint: field_made_optional,
        giving_a_field_a_default_moves_the_fingerprint: field_given_a_default,
        accepting_unknown_fields_moves_the_fingerprint: unknown_fields_allowed,
        renaming_a_serde_tag_moves_the_fingerprint: tag_renamed,
        removing_a_serde_tag_moves_the_fingerprint: tag_removed,
        adding_a_tagged_variant_moves_the_fingerprint: variant_added,
        removing_a_variant_moves_the_fingerprint: variant_removed,
        renaming_a_variant_moves_the_fingerprint: variant_renamed,
        adding_a_unit_variant_moves_the_fingerprint: unit_variant_added,
        adding_a_field_called_description_moves_the_fingerprint: field_named_description,
    }

    stays! {
        a_doc_comment_does_not_move_the_fingerprint: documented::Root,
        reordering_fields_and_variants_does_not_move_the_fingerprint: reordered::Root,
        a_helper_and_a_derive_do_not_move_the_fingerprint: with_code_around_it::Root,
        renaming_the_types_does_not_move_the_fingerprint: types_renamed::Plan,
    }

    #[test]
    fn every_mutation_class_gives_a_different_fingerprint() {
        // Two mutations that happened to cancel each other would let one
        // `moves!` case pass for the wrong reason.
        let all = [
            fp::<field_added::Root>(),
            fp::<field_removed::Root>(),
            fp::<field_renamed::Root>(),
            fp::<field_renamed_on_the_wire_only::Root>(),
            fp::<field_retyped::Root>(),
            fp::<nested_field_retyped::Root>(),
            fp::<field_made_optional::Root>(),
            fp::<field_given_a_default::Root>(),
            fp::<unknown_fields_allowed::Root>(),
            fp::<tag_renamed::Root>(),
            fp::<tag_removed::Root>(),
            fp::<variant_added::Root>(),
            fp::<variant_removed::Root>(),
            fp::<variant_renamed::Root>(),
            fp::<unit_variant_added::Root>(),
            fp::<field_named_description::Root>(),
            fp::<base::Root>(),
        ];
        let distinct: std::collections::BTreeSet<_> = all.iter().collect();
        assert_eq!(distinct.len(), all.len());
    }

    // --- The two contracts ---------------------------------------------------

    macro_rules! wire {
        ($m:ident, from $from:literal, into $into:literal) => {
            #[allow(dead_code)]
            mod $m {
                use schemars::JsonSchema;
                use serde::{Deserialize, Serialize};

                #[derive(Clone, Serialize, Deserialize, JsonSchema)]
                #[serde(try_from = $from, into = $into)]
                pub struct Wrapped(String);

                impl TryFrom<String> for Wrapped {
                    type Error = String;
                    fn try_from(value: String) -> Result<Self, String> {
                        Ok(Self(value))
                    }
                }
                impl TryFrom<u32> for Wrapped {
                    type Error = String;
                    fn try_from(value: u32) -> Result<Self, String> {
                        Ok(Self(value.to_string()))
                    }
                }
                impl From<Wrapped> for String {
                    fn from(value: Wrapped) -> String {
                        value.0
                    }
                }
                impl From<Wrapped> for u32 {
                    fn from(value: Wrapped) -> u32 {
                        value.0.len() as u32
                    }
                }
            }
        };
    }

    wire!(wire_string_string, from "String", into "String");
    wire!(wire_number_string, from "u32", into "String");
    wire!(wire_string_number, from "String", into "u32");

    #[test]
    fn a_change_to_the_reading_side_alone_moves_the_fingerprint() {
        assert_ne!(
            fp::<wire_string_string::Wrapped>(),
            fp::<wire_number_string::Wrapped>()
        );
    }

    #[test]
    fn a_change_to_the_writing_side_alone_moves_the_fingerprint() {
        assert_ne!(
            fp::<wire_string_string::Wrapped>(),
            fp::<wire_string_number::Wrapped>()
        );
    }

    // --- The hashed form -----------------------------------------------------

    #[test]
    fn documentation_is_ignored_for_a_persisted_format_and_kept_for_a_published_one() {
        let plain = json!({"type": "string"});
        let documented = json!({"type": "string", "description": "a name", "title": "Name"});
        let reworded = json!({"type": "string", "description": "a longer name", "title": "Name"});
        let hash = |schema: &Value, docs| fingerprint([("s", schema)], docs);

        assert_eq!(hash(&plain, Docs::Ignore), hash(&documented, Docs::Ignore));
        assert_eq!(
            hash(&documented, Docs::Ignore),
            hash(&reworded, Docs::Ignore)
        );
        assert_ne!(hash(&plain, Docs::Keep), hash(&documented, Docs::Keep));
        assert_ne!(hash(&documented, Docs::Keep), hash(&reworded, Docs::Keep));
    }

    #[test]
    fn a_property_called_description_is_not_taken_for_documentation() {
        let without = json!({"type": "object", "properties": {"id": {"type": "string"}}});
        let with = json!({"type": "object", "properties": {
            "id": {"type": "string"},
            "description": {"type": "string"},
            "title": {"type": "integer"}
        }});
        assert_ne!(
            fingerprint([("s", &without)], Docs::Ignore),
            fingerprint([("s", &with)], Docs::Ignore)
        );
    }

    #[test]
    fn key_order_and_the_order_of_required_names_are_not_part_of_the_hash() {
        let a: Value = serde_json::from_str(
            r#"{"type":"object","required":["a","b"],"properties":{"a":{"type":"string"},"b":{"enum":["x","y"]}}}"#,
        )
        .unwrap();
        let b: Value = serde_json::from_str(
            r#"{"properties":{"b":{"enum":["y","x"]},"a":{"type":"string"}},"required":["b","a"],"type":"object"}"#,
        )
        .unwrap();
        assert_eq!(
            fingerprint([("s", &a)], Docs::Ignore),
            fingerprint([("s", &b)], Docs::Ignore)
        );
    }

    #[test]
    fn the_order_of_an_untagged_enums_variants_is_part_of_the_hash() {
        // Serde tries untagged variants in order; schemars writes them as `anyOf`.
        let a = json!({"anyOf": [{"type": "string"}, {"type": "integer"}]});
        let b = json!({"anyOf": [{"type": "integer"}, {"type": "string"}]});
        assert_ne!(
            fingerprint([("s", &a)], Docs::Ignore),
            fingerprint([("s", &b)], Docs::Ignore)
        );
    }

    #[test]
    fn the_name_of_a_document_is_part_of_the_hash_and_their_order_is_not() {
        let x = json!({"type": "string"});
        let y = json!({"type": "integer"});
        assert_ne!(
            fingerprint([("a", &x), ("b", &y)], Docs::Keep),
            fingerprint([("a", &y), ("b", &x)], Docs::Keep)
        );
        assert_eq!(
            fingerprint([("a", &x), ("b", &y)], Docs::Keep),
            fingerprint([("b", &y), ("a", &x)], Docs::Keep)
        );
    }

    /// Names that cross JSON as one string must be described as one string, on
    /// both sides: a derive that described their fields would document a shape
    /// the wire never has, and a change of the real shape would go unseen.
    #[test]
    fn names_that_are_strings_on_the_wire_are_strings_in_both_contracts() {
        use crate::{
            CollationName, ColumnRef, ColumnType, ModuleId, Reason, RoutineId, TableName, TypeRef,
            Uid,
        };

        fn assert_string<T: JsonSchema>(name: &str) {
            for schema in contract_schemas::<T>() {
                assert_eq!(schema["type"], "string", "{name}: {schema}");
                assert!(schema.get("properties").is_none(), "{name}: {schema}");
            }
        }
        assert_string::<TypeRef>("TypeRef");
        assert_string::<CollationName>("CollationName");
        assert_string::<ModuleId>("ModuleId");
        assert_string::<RoutineId>("RoutineId");
        assert_string::<TableName>("TableName");
        assert_string::<ColumnRef>("ColumnRef");
        assert_string::<ColumnType>("ColumnType");
        assert_string::<Uid>("Uid");
        assert_string::<Reason>("Reason");
    }

    #[test]
    fn a_fingerprint_is_sixty_four_lowercase_hex_digits() {
        for fp in [saved_plan(), state_snapshot()] {
            assert_eq!(fp.len(), 64);
            assert!(
                fp.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            );
        }
    }

    // --- The real formats ----------------------------------------------------

    #[test]
    fn the_plan_and_the_snapshot_have_different_stable_fingerprints() {
        assert_ne!(saved_plan(), state_snapshot());
        assert_eq!(saved_plan(), format_fingerprint::<SavedPlan>());
        assert_eq!(state_snapshot(), format_fingerprint::<StateSnapshot>());
    }

    #[test]
    fn the_real_schemas_reach_the_deepest_types_of_both_formats() {
        // A derive missing somewhere would not compile, but a field hidden
        // behind `with` or `skip` would: name something from each corner.
        let plan = serde_json::to_string(&contract_schemas::<SavedPlan>()).unwrap();
        let state = serde_json::to_string(&contract_schemas::<StateSnapshot>()).unwrap();
        for name in ["database_collation", "add_column", "analysis", "time_zone"] {
            assert!(
                plan.contains(name),
                "the saved plan's schema lacks `{name}`"
            );
        }
        for name in ["baseline_evidence", "module_deps", "tombstones", "moves"] {
            assert!(state.contains(name), "the snapshot's schema lacks `{name}`");
        }
    }

    #[test]
    fn no_persisted_format_has_an_opaque_field() {
        fixture!(opaque {
            #[derive(Serialize, Deserialize, JsonSchema)]
            pub struct Root {
                pub fine: String,
                #[schemars(with = "serde_json::Value")]
                pub hidden: crate::drift::DriftReport,
            }
        });

        let mut found = Vec::new();
        for schema in contract_schemas::<opaque::Root>() {
            opaque_positions(&schema, "", &mut found);
        }
        assert!(
            found.iter().any(|p| p.contains("hidden")),
            "the detector must see a field hidden behind `with`: {found:?}"
        );

        for (format, schemas) in [
            ("SavedPlan", contract_schemas::<SavedPlan>()),
            ("StateSnapshot", contract_schemas::<StateSnapshot>()),
        ] {
            let mut found = Vec::new();
            for schema in &schemas {
                opaque_positions(schema, "", &mut found);
            }
            assert!(
                found.is_empty(),
                "{format} has fields its fingerprint cannot see: {found:?}"
            );
        }
    }

    /// One edit to a schema document: where it is and what it does.
    enum Site {
        RenameProperty(Vec<String>, String),
        Retype(Vec<String>),
        AddEnumValue(Vec<String>),
    }

    fn sites(value: &Value, path: &mut Vec<String>, out: &mut Vec<Site>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    path.push(key.clone());
                    match (key.as_str(), child) {
                        ("properties", Value::Object(props)) => {
                            for name in props.keys() {
                                out.push(Site::RenameProperty(path.clone(), name.clone()));
                            }
                        }
                        ("type", Value::String(_)) => out.push(Site::Retype(path.clone())),
                        ("enum", Value::Array(_)) => out.push(Site::AddEnumValue(path.clone())),
                        _ => {}
                    }
                    sites(child, path, out);
                    path.pop();
                }
            }
            Value::Array(list) => {
                for (i, child) in list.iter().enumerate() {
                    path.push(i.to_string());
                    sites(child, path, out);
                    path.pop();
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    fn at<'a>(value: &'a mut Value, path: &[String]) -> &'a mut Value {
        path.iter().fold(value, |v, step| match v {
            Value::Array(list) => &mut list[step.parse::<usize>().unwrap()],
            Value::Object(map) => map.get_mut(step.as_str()).expect("a path of the schema"),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                panic!("a path cannot enter a scalar")
            }
        })
    }

    fn apply(schema: &Value, site: &Site) -> Value {
        let mut edited = schema.clone();
        match site {
            Site::RenameProperty(path, name) => {
                let props = at(&mut edited, path).as_object_mut().unwrap();
                let value = props.remove(name).unwrap();
                props.insert(format!("{name}_renamed"), value);
            }
            Site::Retype(path) => {
                let slot = at(&mut edited, path);
                *slot = Value::String(format!("{}_retyped", slot.as_str().unwrap()));
            }
            Site::AddEnumValue(path) => {
                at(&mut edited, path)
                    .as_array_mut()
                    .unwrap()
                    .push(Value::String("added_by_the_test".into()));
            }
        }
        edited
    }

    /// The fixture schema is small enough to mutate at every site, so this is
    /// the exhaustive version of the claim: no field, type or enum value of a
    /// schema is invisible to the hash.
    #[test]
    fn every_site_of_a_fixture_schema_moves_the_fingerprint() {
        let schema = contract_schemas::<base::Root>()[0].clone();
        let mut found = Vec::new();
        sites(&schema, &mut Vec::new(), &mut found);
        assert!(found.len() >= 10, "{} sites", found.len());
        let before = fingerprint([("s", &schema)], Docs::Ignore);
        for site in &found {
            let after = fingerprint([("s", &apply(&schema, site))], Docs::Ignore);
            assert_ne!(before, after);
        }
    }

    /// The same claim on the real formats, which have thousands of sites. Every
    /// 97th keeps the test fast and reaches every part of the schema.
    #[test]
    fn sampled_sites_of_the_real_schemas_move_the_fingerprint() {
        for (format, schemas) in [
            ("SavedPlan", contract_schemas::<SavedPlan>()),
            ("StateSnapshot", contract_schemas::<StateSnapshot>()),
        ] {
            for schema in &schemas {
                let mut found = Vec::new();
                sites(schema, &mut Vec::new(), &mut found);
                assert!(found.len() > 300, "{format}: {} sites", found.len());
                let before = fingerprint([("s", schema)], Docs::Ignore);
                for site in found.iter().step_by(97) {
                    let after = fingerprint([("s", &apply(schema, site))], Docs::Ignore);
                    assert_ne!(before, after, "{format}");
                }
            }
        }
    }

    // --- Presence is not nullability ------------------------------------------

    fixture!(optional_field {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            pub a: Option<String>,
        }
    });

    fixture!(required_nullable_field {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            #[serde(deserialize_with = "crate::presence::required_option")]
            #[schemars(
                required,
                schema_with = "crate::presence::required_option_schema::<String>"
            )]
            pub a: Option<String>,
        }
    });

    // The marker alone drops `null` from the type: the key is required, and the
    // codec accepts a null, so the schema would be wrong the other way.
    fixture!(required_only {
        #[derive(Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct Root {
            #[serde(deserialize_with = "crate::presence::required_option")]
            #[schemars(required)]
            pub a: Option<String>,
        }
    });

    #[test]
    fn a_required_nullable_field_and_an_optional_one_have_different_fingerprints() {
        assert_ne!(
            fp::<optional_field::Root>(),
            fp::<required_nullable_field::Root>()
        );
    }

    #[test]
    fn the_schema_of_a_required_nullable_field_agrees_with_its_codec_on_both_sides() {
        // The codec: a missing key is refused, an explicit null is accepted.
        assert!(serde_json::from_value::<required_nullable_field::Root>(json!({})).is_err());
        assert!(
            serde_json::from_value::<required_nullable_field::Root>(json!({"a": null})).is_ok()
        );
        assert!(serde_json::from_value::<optional_field::Root>(json!({})).is_ok());

        // The schema says the same on the reading side, and the two fields are
        // written alike (null included), so the writing side does not differ.
        let [read_optional, write_optional] = contract_schemas::<optional_field::Root>();
        let [read_required, write_required] = contract_schemas::<required_nullable_field::Root>();
        assert_eq!(read_required["required"], json!(["a"]));
        assert!(read_optional.get("required").is_none());
        assert_eq!(write_required["required"], json!(["a"]));
        assert_eq!(write_optional["required"], json!(["a"]));
        assert_eq!(
            canonical(&write_required, Docs::Ignore),
            canonical(&write_optional, Docs::Ignore)
        );
        // And the key stays nullable on both sides.
        for schema in [&read_required, &write_required] {
            let kinds = canonical(&schema["properties"]["a"], Docs::Ignore);
            assert_eq!(kinds["type"], json!(["null", "string"]));
        }
    }

    #[test]
    fn the_required_marker_without_its_schema_function_would_forbid_a_null() {
        let [read, write] = contract_schemas::<required_only::Root>();
        for schema in [read, write] {
            assert_eq!(schema["properties"]["a"]["type"], json!("string"));
        }
        assert_ne!(
            fp::<required_only::Root>(),
            fp::<required_nullable_field::Root>()
        );
    }

    /// The attribute blocks that put a field behind a custom deserializer
    /// without saying so to the schema. `serde` demands the key of such a field
    /// unless it has a default, however its type reads, and `schemars` cannot
    /// see inside the deserializer; `schemars(required, schema_with = ..)` tells it.
    fn unmarked_custom_deserializers(source: &str) -> Vec<String> {
        // Only the code of a line counts: brackets inside a string, as in a
        // `regex(pattern = "^[a-z]")`, are not part of the nesting.
        fn depth_change(line: &str) -> i32 {
            let mut quoted = false;
            let mut depth = 0;
            for c in line.chars() {
                match c {
                    '"' => quoted = !quoted,
                    '(' | '[' if !quoted => depth += 1,
                    ')' | ']' if !quoted => depth -= 1,
                    _ => {}
                }
            }
            depth
        }
        fn offending(block: &str) -> bool {
            let has = |word: &str| {
                block
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(|token| token == word)
            };
            has("deserialize_with")
                && !has("default")
                && !block.replace(' ', "").contains("schemars(required")
        }

        let mut found = Vec::new();
        let mut block = String::new();
        let mut depth = 0;
        for line in source.lines().map(str::trim) {
            if depth == 0 && !line.starts_with("#[") {
                // Comments may sit between the attributes of one field.
                if line.starts_with("//") {
                    continue;
                }
                if offending(&block) {
                    found.push(block.clone());
                }
                block.clear();
                continue;
            }
            block.push_str(line);
            block.push(' ');
            depth += depth_change(line);
        }
        found
    }

    #[test]
    fn a_custom_deserializer_without_a_default_must_be_marked_required() {
        let marked = r#"
            #[derive(Deserialize)]
            struct A {
                /// Null is allowed; absence is not.
                #[serde(deserialize_with = "required_option")]
                #[schemars(
                    required,
                    schema_with = "required_option_schema::<String>"
                )]
                a: Option<String>,
                #[serde(
                    default,
                    skip_serializing_if = "Option::is_none",
                    deserialize_with = "names"
                )]
                #[schemars(regex(pattern = "^[a-z]+$"))]
                b: Option<String>,
                c: Option<String>,
            }
        "#;
        assert_eq!(unmarked_custom_deserializers(marked), Vec::<String>::new());

        let unmarked = r#"
            struct A {
                #[serde(deserialize_with = "required_option")]
                a: Option<String>,
                #[serde(
                    skip_serializing_if = "Option::is_none",
                    deserialize_with = "names"
                )]
                #[schemars(regex(pattern = "^[a-z]+$"))]
                b: Option<String>,
            }
        "#;
        assert_eq!(unmarked_custom_deserializers(unmarked).len(), 2);
    }

    #[test]
    fn every_custom_deserializer_of_the_model_is_marked_required_or_has_a_default() {
        fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("the source directory is readable") {
                let path = entry.expect("a directory entry is readable").path();
                if path.is_dir() {
                    sources(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        sources(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        assert!(files.len() > 30, "only {} source files found", files.len());

        let mut found = Vec::new();
        for file in files {
            // This file's own fixtures and samples show the shape on purpose.
            if file.file_name().is_some_and(|n| n == "fingerprint.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).expect("a source file is readable");
            for block in unmarked_custom_deserializers(&text) {
                found.push(format!("{}: {block}", file.display()));
            }
        }
        assert!(
            found.is_empty(),
            "serde requires the key of these fields, which the schema calls optional; \
             add the marker of `presence.rs`: {found:#?}"
        );
    }

    /// Every object schema, at any depth, that has all of the named properties.
    fn objects_with<'a>(schema: &'a Value, properties: &[&str], found: &mut Vec<&'a Value>) {
        if let Value::Object(map) = schema {
            if let Some(Value::Object(own)) = map.get("properties")
                && properties.iter().all(|name| own.contains_key(*name))
            {
                found.push(schema);
            }
            for value in map.values() {
                objects_with(value, properties, found);
            }
        } else if let Value::Array(list) = schema {
            for value in list {
                objects_with(value, properties, found);
            }
        }
    }

    /// The same schema with `names` no longer required where the object has all
    /// of `properties`.
    fn loosened(schema: &Value, properties: &[&str], names: &[&str]) -> Value {
        let mut out = schema.clone();
        fn walk(value: &mut Value, properties: &[&str], names: &[&str]) {
            if let Value::Object(map) = value {
                let hit = matches!(map.get("properties"), Some(Value::Object(own))
                    if properties.iter().all(|name| own.contains_key(*name)));
                if hit && let Some(Value::Array(required)) = map.get_mut("required") {
                    required.retain(|r| !names.iter().any(|n| r.as_str() == Some(*n)));
                }
                for child in map.values_mut() {
                    walk(child, properties, names);
                }
            } else if let Value::Array(list) = value {
                for child in list {
                    walk(child, properties, names);
                }
            }
        }
        walk(&mut out, properties, names);
        out
    }

    #[test]
    fn the_required_nullable_fields_of_the_real_formats_are_required_and_move_the_fingerprint() {
        use crate::resolver::CandidateSet;

        // The codec first: the key of `name` is demanded even when it is null.
        let present = json!({"class": "table", "namespace": null, "name": null});
        assert!(serde_json::from_value::<CandidateSet>(present).is_ok());
        let missing = json!({"class": "table", "namespace": null});
        assert!(serde_json::from_value::<CandidateSet>(missing).is_err());

        let formats = [
            ("SavedPlan", contract_schemas::<SavedPlan>()),
            ("StateSnapshot", contract_schemas::<StateSnapshot>()),
        ];
        let fields: [(&str, &[&str], &[&str]); 3] = [
            (
                "CandidateSet",
                &["class", "namespace", "name"],
                &["namespace", "name"],
            ),
            (
                "RoutineLookup",
                &["signature", "search_path", "kind", "resolved"],
                &["resolved"],
            ),
            (
                "SurfaceResolution",
                &["surface", "current", "desired"],
                &["current", "desired"],
            ),
        ];
        for (type_name, properties, nullable) in fields {
            for side in 0..2 {
                let mut reached = 0;
                for (format, schemas) in &formats {
                    let schema = &schemas[side];
                    let mut found = Vec::new();
                    objects_with(schema, properties, &mut found);
                    for object in &found {
                        let required: Vec<&str> = object["required"]
                            .as_array()
                            .expect("an object with required fields lists them")
                            .iter()
                            .filter_map(Value::as_str)
                            .collect();
                        for name in nullable {
                            assert!(
                                required.contains(name),
                                "{type_name}.{name} is required by serde but not by the \
                                 {format} schema (side {side})"
                            );
                            // Required, yet still allowed to be null.
                            let kinds = object["properties"][*name].to_string();
                            assert!(
                                kinds.contains("null"),
                                "{type_name}.{name} lost its null in {format} (side {side}): {kinds}"
                            );
                        }
                    }
                    if !found.is_empty() {
                        let before = fingerprint([("s", schema)], Docs::Ignore);
                        let after = fingerprint(
                            [("s", &loosened(schema, properties, nullable))],
                            Docs::Ignore,
                        );
                        assert_ne!(before, after, "{type_name} in {format} (side {side})");
                    }
                    reached += found.len();
                }
                assert!(
                    reached > 0,
                    "{type_name} is in neither format (side {side})"
                );
            }
        }
    }
}
