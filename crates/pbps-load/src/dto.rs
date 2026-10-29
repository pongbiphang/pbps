//! The YAML shape of the declaration files.
//!
//! These structures are deliberately **different** from [`pbps_model`]'s domain
//! model: YAML uses map keys as names, has defaults, and carries one-shot intent
//! annotations such as `renamed_from`. [`crate::convert`] owns the translation,
//! which is what keeps the model clean (see constraints 1 and 2 in CLAUDE.md).
//!
//! Every structure sets `deny_unknown_fields`: a misspelled field name is the
//! most common user error, and silently applying a default would let the
//! declaration and reality diverge without a sound.

use indexmap::IndexMap;
use serde_saphyr::Spanned;
use std::collections::BTreeMap;

use pbps_model::{ColumnName, ReferentialAction};

/// Which kind of declaration a file holds.
///
/// Read first, with unknown fields deliberately **allowed**: this pass only
/// answers "table or module", and the real DTO for that answer is what rejects
/// a misspelled field — with a message naming the right shape rather than
/// "unknown field `colunms`" from whichever variant happened to be tried first.
#[derive(Debug, serde::Deserialize)]
pub struct KindProbe {
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default)]
    pub dependency: Option<String>,
    #[serde(default, rename = "enum")]
    pub enumeration: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub procedure: Option<String>,
    #[serde(default)]
    pub function: Option<String>,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub role: Option<pbps_model::RoleName>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependencyDto {
    pub dependency: pbps_model::dependency::DependencyName,
    pub kind: pbps_model::dependency::DependencyKind,
    #[schemars(extend("minProperties" = 1))]
    pub columns: BTreeMap<ColumnName, DependencyColumnDto>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependencyColumnDto {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default = "dependency_nullable")]
    pub nullable: bool,
}

fn dependency_nullable() -> bool {
    true
}

/// A shared enum: label order is semantic state.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnumTypeDto {
    #[serde(rename = "enum")]
    pub name: String,
    #[serde(default)]
    pub renamed_from: Option<String>,
    #[schemars(schema_with = "enum_values_schema")]
    pub values: Vec<EnumValueDto>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum EnumValueDto {
    Label(String),
    Renamed(EnumValueIntentDto),
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnumValueIntentDto {
    pub name: String,
    #[serde(default)]
    pub renamed_from: Option<String>,
}

fn enum_values_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = generator.subschema_for::<Vec<EnumValueDto>>();
    schema.insert("uniqueItems".into(), true.into());
    schema
}

/// A shared domain, with its column-style base type and named checks.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainTypeDto {
    #[serde(rename = "domain")]
    pub name: String,
    #[serde(default)]
    pub renamed_from: Option<String>,
    #[serde(rename = "type")]
    pub base: String,
    #[serde(default)]
    pub not_null: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub checks: BTreeMap<String, String>,
    #[serde(default)]
    pub collation: Option<String>,
}

/// One database role and its grants (ADR-0005).
///
/// `role: app_reader` names it — unqualified, because a role is a principal
/// and not an object in a schema. Grants are a map of target to permission
/// list: `dbo.customer: [select]`, or `schema::dbo: [select]` for a whole
/// schema.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoleDto {
    #[schemars(with = "String")]
    pub role: Spanned<pbps_model::RoleName>,

    #[serde(default)]
    pub description: Option<String>,

    /// One-shot intent, as for [`TableDto::renamed_from`]: the role keeps its
    /// membership through the rename, which is why it is intent and not
    /// drop + add.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub renamed_from: Option<Spanned<pbps_model::RoleName>>,

    /// Target to permissions. Order carries no meaning; `fmt` sorts both.
    #[serde(default)]
    #[schemars(schema_with = "grants_schema")]
    pub grants: BTreeMap<String, Vec<Spanned<String>>>,
}

/// The `grants` map as the editor sees it: any target, and a permission list
/// over the words the loader accepts. Built from `Permission::ALL` rather than
/// derived on `Permission`, which the model keeps free of `JsonSchema` (see
/// `ReferentialAction`, the one exception); listing the words here by hand
/// would recreate the drift the generation exists to prevent. The list is the
/// union of the engines' (ADR-0010 §6) — which word a dialect refuses is
/// `validate`'s finding, not the editor's.
///
/// Two branches, because the schema has two jobs and one list cannot do both.
/// `Permission::from_str` folds case and reads `_` or a space where the
/// canonical word has `-`, so that a user who types what the engine prints
/// (`VIEW DEFINITION`, `view_definition`) is not corrected. A closed list of
/// the canonical words alone would have an editor flag those, which the loader
/// accepts — so the pattern branch says what is accepted, and the `enum`
/// branch, listing only what `fmt` writes, is what an editor completes from
/// (DECISIONS 213).
fn grants_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let words: Vec<&str> = pbps_model::Permission::ALL
        .iter()
        .map(|p| p.as_str())
        .collect();
    let padding = whitespace_class();
    let accepted = format!(
        "^{padding}*({}){padding}*$",
        words
            .iter()
            .map(|w| spellings_of(w))
            .collect::<Vec<_>>()
            .join("|")
    );
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": {
            "type": "array",
            "items": {
                "type": "string",
                "anyOf": [
                    { "enum": words },
                    { "pattern": accepted }
                ]
            }
        }
    })
}

/// The characters `str::trim` removes, as a regular-expression class.
///
/// Not `\s`. JSON Schema's patterns are ECMA-262, whose `\s` is neither a
/// subset nor a superset of Rust's `char::is_whitespace`: it leaves out U+0085,
/// which `trim` removes, and takes in U+FEFF, which `trim` leaves in place. A
/// pattern written with `\s` would therefore refuse a padded declaration the
/// loader accepts *and* accept one it refuses — both halves of "the schema is
/// the loader" broken by the same two characters (DECISIONS 172, 213).
///
/// Scanned out of `char::is_whitespace` rather than listed, so the class is
/// whatever `trim` does on the compiler that built this binary and cannot go
/// stale against a later Unicode table.
///
/// The characters go in literally rather than as escapes, because the two
/// readers of this pattern share no escape syntax: ECMA-262 spells U+1680
/// `\u1680` and has no `\x{...}`, the `regex` family spells it `\x{1680}` and
/// has no `\u`. A literal character is one both read, and JSON's own escaping
/// keeps the ASCII control characters printable in the published file. None of
/// the set is a regular-expression metacharacter, so none needs escaping
/// inside the class.
fn whitespace_class() -> String {
    let mut class = String::from("[");
    class.extend((char::MIN..=char::MAX).filter(|c| c.is_whitespace()));
    class.push(']');
    class
}

/// One permission word as a regular expression matching every spelling
/// `Permission::from_str` folds into it.
///
/// Written out per character rather than with a flag: JSON Schema's patterns
/// are ECMA-262, which has no inline `(?i)`, and a schema that carried one
/// would be a pattern every validator reads differently. `-` admits the two
/// separators the fold also accepts, and the leading and trailing whitespace
/// class the caller wraps this in stands for its `trim`.
fn spellings_of(word: &str) -> String {
    word.chars()
        .map(|c| match c {
            '-' => "[-_ ]".to_owned(),
            c if c.is_ascii_alphabetic() => {
                format!("[{}{}]", c.to_ascii_uppercase(), c)
            }
            // No word has one today — `every_permission_word_is_lowercase_ascii`
            // holds the assumption — and a character class is the spelling that
            // stays literal for the widest set of characters if one appears.
            c => format!("[{c}]"),
        })
        .collect()
}

/// One view, procedure, function or trigger (ADR-0002).
///
/// The leading key is both the kind and the name — `view: dbo.active_customer`
/// — for the same reason `table:` is: the file name carries no meaning, so the
/// identity has to be inside the file.
// The four kind keys are individually optional in Rust because serde has to
// read the file before it can say which one is present — but the loader
// requires exactly one, and a schema that did not say so would bless a file
// with none, or with two, that `convert_module` then refuses. The whole point
// of generating the schema is that it accepts exactly what the loader accepts,
// so the constraint is stated here rather than left to the derive.
//
// Each branch pins the key's *type* as well as its presence: JSON Schema's
// `required` is satisfied by an explicit null, which YAML writes as often as
// not (`view:` with nothing after it), and the loader would read that as
// absent.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
// Each branch pins the kind key's *type* as well as its presence: JSON Schema's
// `required` is satisfied by an explicit null, and YAML writes `view:` with
// nothing after it as often as not — which the loader reads as absent.
//
// `on:` is part of the same choice, not an independent field. The loader
// requires it on a trigger and rejects it on everything else, so a schema that
// left it optional everywhere blessed two documents `pbps validate` refuses: a
// trigger without a table, and a view with one. The principle this schema is
// generated for is that it refuses exactly what the loader refuses.
//
// Every branch pins the other three kind keys — and `on:` — to null, rather
// than leaving them unconstrained. `oneOf` means *exactly one* branch matches,
// so before the `on:` rule existed a two-kind document matched two branches and
// was refused for free. Adding `on: false` broke that: `trigger` + `on` +
// `view` was disqualified from the view branch by `on` and unconstrained in the
// trigger branch, so it matched exactly one and the schema blessed a file
// `convert_module` refuses. The exclusions have to be written out.
//
// `{"type": "null"}` rather than `false`, because that is the loader's own rule:
// a key counts as present only when it holds a string, and YAML writes
// `procedure:` with nothing after it — which serde reads as absent. `false`
// would refuse that line, making the schema stricter than the tool, which is a
// smaller failure than blessing what the tool refuses but a failure all the
// same.
#[schemars(extend("oneOf" = [
    serde_json::json!({
        "required": ["view"],
        "properties": {
            "view": {"type": "string"},
            "procedure": {"type": "null"}, "function": {"type": "null"},
            "trigger": {"type": "null"}, "on": {"type": "null"},
        },
    }),
    serde_json::json!({
        "required": ["procedure"],
        "properties": {
            "procedure": {"type": "string"},
            "view": {"type": "null"}, "function": {"type": "null"},
            "trigger": {"type": "null"}, "on": {"type": "null"},
            "schemabinding": {"type": "null"},
            "indexes": {"type": "null"}, "clustered": {"type": "null"},
        },
    }),
    serde_json::json!({
        "required": ["function"],
        "properties": {
            "function": {"type": "string"},
            "view": {"type": "null"}, "procedure": {"type": "null"},
            "trigger": {"type": "null"}, "on": {"type": "null"},
            "schemabinding": {"type": "null"},
            "indexes": {"type": "null"}, "clustered": {"type": "null"},
        },
    }),
    serde_json::json!({
        "required": ["trigger", "on"],
        "properties": {
            "trigger": {"type": "string"}, "on": {"type": "string"},
            "view": {"type": "null"}, "procedure": {"type": "null"},
            "function": {"type": "null"}, "schemabinding": {"type": "null"},
            "indexes": {"type": "null"}, "clustered": {"type": "null"},
        },
    }),
]))]
pub struct ModuleDto {
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub view: Option<Spanned<String>>,
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub procedure: Option<Spanned<String>>,
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub function: Option<Spanned<String>>,
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub trigger: Option<Spanned<String>>,

    #[serde(default)]
    pub description: Option<String>,

    /// The table a trigger is on. Meaningless — and rejected — on anything else.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub on: Option<Spanned<String>>,

    /// Makes a SQL Server view `WITH SCHEMABINDING` (DEC-2329.1). Meaningless
    /// on anything else and rejected there: a function states the option in
    /// its own header text, and the other kinds have none.
    ///
    /// Written by `fmt` only when `true`, as `public_execute:` is.
    #[serde(default)]
    #[schemars(with = "Option<bool>")]
    pub schemabinding: Option<Spanned<bool>>,

    /// An indexed view's indexes, as a table declares them: plain column keys
    /// under `columns:` or `keys:` (each optionally `desc`, or `order:` under
    /// `keys:`), `include:` and `unique:`. A view's index takes no expression
    /// key, `opclass:`, `where:`, `method:` or `storage_parameters:`; the
    /// dialect refuses them.
    /// Meaningless without `schemabinding: true`, and rejected on every kind
    /// but a view (DEC-2329.3).
    #[serde(default)]
    pub indexes: BTreeMap<pbps_model::ObjectLocalName, IndexDto>,

    /// The name of the unique index among `indexes:` that is clustered: the
    /// engine wants one first, and says nothing about which it is.
    #[serde(default)]
    pub clustered: Option<pbps_model::ObjectLocalName>,

    /// The body. What it starts with depends on the kind; see
    /// [`pbps_model::Module`].
    pub definition: String,

    /// Modules that must be created first, where the identifier scan cannot see
    /// the dependency. Persistent, like `strategy:`, and preserved by `fmt`.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub depends_on: Vec<Spanned<String>>,

    /// Opts this routine back in to the engine's default `EXECUTE` to
    /// `PUBLIC`, where the engine has one (ADR-0010 §5). Persistent, like
    /// `depends_on:`, and preserved by `fmt`.
    ///
    /// Absent — the common case — means the plan takes that default away as
    /// part of creating the routine. Writing `false` says the same thing out
    /// loud and is accepted, so that a declaration can record the decision
    /// where a reader sees it.
    #[serde(default)]
    #[schemars(with = "Option<bool>")]
    pub public_execute: Option<Spanned<bool>>,
}

/// `system_time:` on a SQL Server table (#1176).
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SystemTimeDto {
    /// The period's start and end columns, in that order, each declared
    /// under `columns:` as `datetime2(p)`, not nullable.
    pub period: [ColumnName; 2],

    /// Both period columns `HIDDEN`, left out of `SELECT *`.
    #[serde(default)]
    pub hidden: bool,

    /// `SYSTEM_VERSIONING = ON`. Absent is the period alone.
    #[serde(default)]
    pub versioning: Option<VersioningDto>,
}

/// `versioning:` under `system_time:`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VersioningDto {
    /// The history table, `schema.name`.
    #[schemars(with = "String")]
    pub history: Spanned<String>,

    /// `HISTORY_RETENTION_PERIOD`, as `6 months` or `1 year`. Absent is
    /// INFINITE.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub retention: Option<Spanned<String>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableDto {
    #[schemars(with = "String")]
    pub table: Spanned<String>,

    #[serde(default)]
    pub description: Option<String>,

    /// One-shot intent: the name this table was renamed from. Removed from the
    /// file once `pbps plan` has absorbed it into the identity file.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub renamed_from: Option<Spanned<String>>,

    /// Execution hints (ADR-0003). Persistent, unlike `renamed_from`: `fmt`
    /// keeps it.
    #[serde(default)]
    pub strategy: Option<StrategyDto>,

    /// `IndexMap` preserves the declaration order in the document, which decides
    /// the column layout of `CREATE TABLE`.
    pub columns: IndexMap<ColumnName, ColumnDto>,

    /// SQL Server computed columns: `{expression: "a * 2", persisted: true}`.
    /// No type: the engine infers it (#1174).
    #[serde(default)]
    pub computed: BTreeMap<ColumnName, ComputedDto>,

    #[serde(default)]
    pub primary_key: Option<PrimaryKeyDto>,

    #[serde(default)]
    pub unique: BTreeMap<pbps_model::ObjectLocalName, UniqueDto>,

    #[serde(default)]
    pub foreign_keys: BTreeMap<pbps_model::ObjectLocalName, ForeignKeyDto>,

    /// Constraint name to check expression.
    #[serde(default)]
    pub checks: BTreeMap<pbps_model::ObjectLocalName, String>,

    #[serde(default)]
    pub indexes: BTreeMap<pbps_model::ObjectLocalName, IndexDto>,

    /// Which index holds the table's rows, on SQL Server: `heap`, `{unique:
    /// <constraint>}` or `{index: <index>}`. Absent means the primary key is
    /// the clustered index, or the table is a heap if it has no primary key.
    #[serde(default)]
    pub clustered: Option<ClusteredDto>,

    /// What logical replication carries to name an old row, on PostgreSQL:
    /// `full`, `nothing`, `primary_key`, `{unique: <constraint>}` or
    /// `{index: <index>}`. Absent means the default: the primary key, or
    /// nothing without one.
    #[serde(default)]
    pub replica_identity: Option<ReplicaIdentityDto>,

    /// PostgreSQL heap storage parameters, by name: `fillfactor: 70`,
    /// `autovacuum_enabled: false`, `autovacuum_vacuum_scale_factor: 0.05`,
    /// `vacuum_index_cleanup: auto`. Absent is every parameter at its
    /// default.
    #[serde(default)]
    #[schemars(with = "BTreeMap<String, StorageValueDto>")]
    pub storage_parameters: BTreeMap<String, Spanned<StorageValueDto>>,

    /// A PostgreSQL `UNLOGGED` table: `true` skips the write-ahead log, so a
    /// crash empties it. Absent, or `false`, is a permanent table.
    #[serde(default)]
    pub unlogged: bool,

    /// PostgreSQL's independent ENABLE and FORCE row-security flags.
    #[serde(default)]
    pub row_security: RowSecurityDto,

    /// Table-local PostgreSQL policies, including policies on a disabled table.
    #[serde(default)]
    pub row_policies: BTreeMap<String, RowPolicyDto>,

    /// A PostgreSQL partitioned parent's key columns: `[ts]` for RANGE, or
    /// `{list: [k]}` or `{hash: [a, b]}` (#1632).
    #[serde(default)]
    pub partition_by: Option<PartitionByDto>,

    /// The parent's partitions, by name (qualified when in another schema):
    /// `{from: ['2025-01-01'], to: ['2026-01-01']}`, `MINVALUE`/`MAXVALUE`
    /// for an unbounded end, or `default`. Or `external`: another tool, such
    /// as pg_partman, creates and drops them, and pbps manages the parent
    /// alone.
    #[serde(default)]
    pub partitions: PartitionsDto,

    /// SQL Server `PERIOD FOR SYSTEM_TIME` and system versioning:
    /// `{period: [valid_from, valid_to], hidden: true, versioning: {history:
    /// dbo.orders_history, retention: 6 months}}`. Absent is an ordinary
    /// table.
    #[serde(default)]
    pub system_time: Option<Box<SystemTimeDto>>,

    /// Declared reference data (ADR-0004). Absent on almost every table: it is
    /// the opt-in that lets the tool touch rows at all.
    #[serde(default)]
    pub data: Option<DataDto>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RowSecurityDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub forced: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("allOf" = [
    serde_json::json!({
        "if": {"required": ["command"], "properties": {"command": {"enum": ["select", "delete"]}}},
        "then": {"properties": {"with_check": {"type": "null"}}},
    }),
    serde_json::json!({
        "if": {"required": ["command"], "properties": {"command": {"const": "insert"}}},
        "then": {"properties": {"using": {"type": "null"}}},
    }),
]))]
pub struct RowPolicyDto {
    #[serde(default)]
    pub command: PolicyCommandDto,
    #[serde(default)]
    pub restrictive: bool,
    #[serde(default)]
    pub to: PolicyRolesDto,
    #[serde(default)]
    pub using: Option<String>,
    #[serde(default)]
    pub with_check: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyCommandDto {
    #[default]
    All,
    Select,
    Insert,
    Update,
    Delete,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyPublicDto {
    Public,
}

#[derive(Debug, schemars::JsonSchema)]
#[serde(untagged)]
pub enum PolicyRolesDto {
    Public(PolicyPublicDto),
    Roles(#[schemars(schema_with = "policy_role_list_schema")] Vec<pbps_model::RoleName>),
}

fn policy_role_list_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "minItems": 1,
        "items": {"type": "string", "not": {"enum": ["public", "current_user", "session_user", "current_role"]}},
    })
}

impl Default for PolicyRolesDto {
    fn default() -> Self {
        Self::Public(PolicyPublicDto::Public)
    }
}

impl<'de> serde::Deserialize<'de> for PolicyRolesDto {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Dispatch by YAML shape, without buffering an untagged value: names
        // must reach the same String reader as other declared identifiers.
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = PolicyRolesDto;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("the scalar `public`, or a list of role names")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                match value {
                    "public" => Ok(PolicyRolesDto::default()),
                    _ => Err(E::custom(
                        "write the scalar `public`, or a list of role names",
                    )),
                }
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                seq: A,
            ) -> Result<Self::Value, A::Error> {
                serde::Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
                    .map(PolicyRolesDto::Roles)
            }
        }
        d.deserialize_any(V)
    }
}

/// The `data:` block.
///
/// `mode` is required rather than defaulted. The two modes differ by whether a
/// row the declaration does not mention is deleted, and a default would decide
/// that for the user in the file where it is least visible.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataDto {
    #[schemars(with = "String")]
    pub mode: Spanned<String>,

    /// Primary-key value to the row's other columns.
    ///
    /// The key is read as a string whatever the YAML scalar looked like, which
    /// is why an integer key (`1:`) works: the primary-key column's declared
    /// type is what says how to render it, and it says so in one place
    /// (`pbps_model::RowKey`).
    pub rows: BTreeMap<String, BTreeMap<ColumnName, ValueDto>>,
}

/// One cell, as written: `null`, a boolean, an integer, or text. A decimal
/// must be quoted (`'1.50'`) — see `pbps_model::Value` for why.
///
/// The variants are tried in order and the order is the whole design. `Text`
/// is last so that it catches only what nothing else did; `Float` sits ahead
/// of it so that a bare `1.50` is *caught and refused* rather than silently
/// becoming the string `"1.50"` in some YAML parsers and the number 1.5 in
/// others. `convert` turns that arm into a diagnostic asking for quotes.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ValueDto {
    Null,
    Bool(bool),
    Int(i64),
    /// Exists to be refused with a good message. **Kept out of the published
    /// schema**: an editor validating against it would otherwise accept a
    /// bare `1.50` that every `pbps` command then rejects, and the whole point
    /// of generating the schema from these types is that the two agree.
    #[schemars(skip)]
    Float(f64),
    Text(String),
}

/// `deny_unknown_fields` is what turns a typo into an error instead of a
/// silent no-op — the whole point of ADR-0003's "validate rejects unknown keys".
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StrategyDto {
    #[serde(default)]
    pub online: bool,
}

/// A key that is there holds a boolean; only a key that is not is `None`.
/// `Option<bool>` alone reads an explicit null as the key left out.
fn a_boolean<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    <bool as serde::Deserialize>::deserialize(deserializer).map(Some)
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ColumnDto {
    #[serde(rename = "type")]
    #[schemars(with = "String")]
    pub ty: Spanned<String>,

    /// Absent is `true`, except on a `serial`, which PostgreSQL makes NOT NULL.
    // Present, it is a boolean: `nullable: null` is refused, as it was while
    // the field was a plain `bool`, not read as absent (#1631).
    #[serde(default, deserialize_with = "a_boolean")]
    #[schemars(with = "bool")]
    pub nullable: Option<bool>,

    /// The default-value expression, kept verbatim.
    #[serde(default)]
    pub default: Option<String>,

    /// `[seed, increment]`, or a mapping where the identity's sequence has
    /// more to say: `{seed, increment, min, max, cycle, cache}` (PostgreSQL,
    /// #1921).
    #[serde(default)]
    pub identity: Option<IdentityDto>,

    /// What a `serial` column's sequence says other than `serial`'s own: its
    /// `start`, its `increment`, and a `name` other than the engine's
    /// `<table>_<column>_seq` (PostgreSQL). Only beside a `type: serial`,
    /// `bigserial` or `smallserial`.
    #[serde(default)]
    pub sequence: Option<SequenceDto>,

    /// A generation expression (PostgreSQL): `{expression: "a * 2", stored:
    /// true}`. The value is computed from the row on every write, where a
    /// default is computed once; never both. `stored` is required, because a
    /// generation expression with no kind is a virtual one on PostgreSQL 18.
    #[serde(default)]
    pub generated: Option<GeneratedDto>,

    /// An explicit collation, on SQL Server (`Latin1_General_CS_AS`). Absent
    /// means the collation of the database the column is created in.
    #[serde(default)]
    pub collation: Option<String>,

    #[serde(default)]
    pub description: Option<String>,

    /// Present means deprecated; the value is the reason. The date comes from
    /// git and is not written here.
    #[serde(default)]
    pub deprecated: Option<String>,

    /// One-shot intent, as for [`TableDto::renamed_from`].
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub renamed_from: Option<Spanned<ColumnName>>,
}

/// A primary key may be written as a bare column list, leaving the name to the
/// database, or with an explicit constraint name.
///
/// The named form is not there for looks: when `pbps pull` reverse-generates from
/// an existing database, losing the original constraint name would make the next
/// diff want to rename it.
#[derive(Debug, schemars::JsonSchema)]
#[serde(untagged)]
pub enum PrimaryKeyDto {
    Columns(Vec<ColumnName>),
    Spec(PrimaryKeySpecDto),
}

/// A primary key written out: its columns, and optionally its constraint
/// name and its index's storage parameters (PostgreSQL B-tree:
/// `fillfactor`, `deduplicate_items`).
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrimaryKeySpecDto {
    #[serde(default)]
    pub name: Option<pbps_model::ObjectLocalName>,
    pub columns: Vec<ColumnName>,
    #[serde(default)]
    #[schemars(with = "BTreeMap<String, StorageValueDto>")]
    pub storage_parameters: BTreeMap<String, Spanned<StorageValueDto>>,
}

/// A UNIQUE constraint: its column list, or written out with its index's
/// storage parameters (PostgreSQL B-tree: `fillfactor`,
/// `deduplicate_items`).
#[derive(Debug, schemars::JsonSchema)]
#[serde(untagged)]
pub enum UniqueDto {
    Columns(Vec<ColumnName>),
    Spec(UniqueSpecDto),
}

// A list or a mapping, read by hand rather than `#[serde(untagged)]`, which
// buffers the value without a type first: a column named `n`, `y` or `on`
// then reads as a YAML boolean and matches neither variant (#1442; the trap
// `IndexKeyDto` documents). Here the list's entries are asked for as
// strings, and a mapping is read as the spec it is.
macro_rules! list_or_spec {
    ($dto:ident, $spec:ident, $what:literal) => {
        impl<'de> serde::Deserialize<'de> for $dto {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> serde::de::Visitor<'de> for V {
                    type Value = $dto;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str($what)
                    }
                    fn visit_seq<A: serde::de::SeqAccess<'de>>(
                        self,
                        mut seq: A,
                    ) -> Result<$dto, A::Error> {
                        let mut columns = Vec::new();
                        while let Some(column) = seq.next_element::<ColumnName>()? {
                            columns.push(column);
                        }
                        Ok($dto::Columns(columns))
                    }
                    fn visit_map<A: serde::de::MapAccess<'de>>(
                        self,
                        map: A,
                    ) -> Result<$dto, A::Error> {
                        serde::Deserialize::deserialize(
                            serde::de::value::MapAccessDeserializer::new(map),
                        )
                        .map($dto::Spec)
                    }
                }
                d.deserialize_any(V)
            }
        }
    };
}
// Read by hand rather than `#[serde(untagged)]`, for the reason
// `list_or_spec!` is: buffered first, a bound value would be matched as
// whatever YAML type it buffered as, and the error would name no variant.

/// A partition: `default`, or a mapping of its bound, `{from: [...], to:
/// [...]}` with one value per key column, `{in: [...]}` (#1632), `{modulus: m,
/// remainder: r}` (#1632), or `default: true`, and what it has of its own
/// beside its parent's: `checks:` and `indexes:` (#1577) and `foreign_keys:`
/// (#1632), written as a table's.
#[derive(Debug, schemars::JsonSchema)]
#[serde(untagged)]
pub enum PartitionDto {
    /// `default`.
    Default(String),
    Entry(PartitionEntryDto),
}

// One bound, as the loader takes them: `from:` and `to:` both, `in:`,
// `modulus:` and `remainder:` both (#1632), or `default: true` alone. Optional
// fields alone would bless `{}` and `{from: [1]}`, which `convert` refuses;
// written out as `ModuleDto`'s exclusions are, each alternative's own keys
// set and every other bound's key absent or `null`, which serde reads as
// absent, so that exactly one alternative matches what the loader takes.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("oneOf" = [
    serde_json::json!({
        "required": ["from", "to"],
        "properties": {
            "from": {"type": "array"}, "to": {"type": "array"},
            "in": {"type": "null"}, "modulus": {"type": "null"}, "remainder": {"type": "null"},
            "default": {"const": false},
        },
    }),
    serde_json::json!({
        "required": ["default"],
        "properties": {
            "default": {"const": true},
            "from": {"type": "null"}, "to": {"type": "null"},
            "in": {"type": "null"}, "modulus": {"type": "null"}, "remainder": {"type": "null"},
        },
    }),
    serde_json::json!({
        "required": ["in"],
        "properties": {
            "in": {"type": "array"},
            "from": {"type": "null"}, "to": {"type": "null"},
            "modulus": {"type": "null"}, "remainder": {"type": "null"},
            "default": {"const": false},
        },
    }),
    serde_json::json!({
        "required": ["modulus", "remainder"],
        "properties": {
            "modulus": {"type": "integer"}, "remainder": {"type": "integer"},
            "from": {"type": "null"}, "to": {"type": "null"}, "in": {"type": "null"},
            "default": {"const": false},
        },
    }),
]))]
pub struct PartitionEntryDto {
    #[serde(default)]
    pub from: Option<Vec<BoundValueDto>>,
    #[serde(default)]
    pub to: Option<Vec<BoundValueDto>>,
    /// A LIST partition's values (#1632); `NULL` takes the rows whose key
    /// is NULL.
    #[serde(default, rename = "in")]
    pub values: Option<Vec<ListValueDto>>,
    /// A HASH partition's modulus (#1632), with `remainder:`.
    #[serde(default)]
    pub modulus: Option<u32>,
    /// A HASH partition's remainder (#1632), with `modulus:`.
    #[serde(default)]
    pub remainder: Option<u32>,
    /// The DEFAULT partition, written as a mapping so that it can carry what
    /// it has of its own.
    #[serde(default)]
    pub default: bool,
    /// Constraint name to check expression, the partition's own.
    #[serde(default)]
    pub checks: BTreeMap<pbps_model::ObjectLocalName, String>,
    /// The partition's own indexes; the parent's reach it without being
    /// written here.
    #[serde(default)]
    pub indexes: BTreeMap<pbps_model::ObjectLocalName, IndexDto>,
    /// The partition's own foreign keys (#1632); the parent's reach it as
    /// clones without being written here, and one equal to a parent's is
    /// refused, since the engine may take it for the parent's clone.
    #[serde(default)]
    pub foreign_keys: BTreeMap<pbps_model::ObjectLocalName, ForeignKeyDto>,
    /// What the partition holds of its parent's columns otherwise than its
    /// parent: a default, a NOT NULL (#1578). Never a type, which is the
    /// parent's.
    #[serde(default)]
    pub columns: BTreeMap<ColumnName, PartitionColumnDto>,
    /// The partition's own persistence, as a table's (#1580): the parent
    /// has none to give it.
    #[serde(default)]
    pub unlogged: bool,
    /// The partition's own heap storage parameters, as a table's (#1580):
    /// the parent takes none.
    #[serde(default)]
    #[schemars(with = "BTreeMap<String, StorageValueDto>")]
    pub storage_parameters: BTreeMap<String, Spanned<StorageValueDto>>,
}

/// One of a partition's columns, where it is not as its parent's (#1578):
/// `default:` of its own, `nullable: false` where the parent's is nullable.
/// `nullable: true` would undo a NOT NULL the parent's column has, which the
/// engine does not let a partition do, so the schema takes `false` alone.
// A `default:` that is text or `nullable: false`, as the loader takes them: a
// key YAML leaves empty reads as absent, so `{default: null}` declares
// nothing and `minProperties` alone would bless it.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("anyOf" = [
    serde_json::json!({"required": ["default"], "properties": {"default": {"type": "string"}}}),
    serde_json::json!({"required": ["nullable"], "properties": {"nullable": {"const": false}}}),
]))]
pub struct PartitionColumnDto {
    /// The default-value expression, kept verbatim, where it is not the
    /// parent's.
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    #[schemars(extend("enum" = [false, null]))]
    pub nullable: Option<bool>,
}

/// A parent's `partitions:`: its partitions by name, or `external` (#1533).
#[derive(Debug, schemars::JsonSchema)]
#[serde(untagged)]
pub enum PartitionsDto {
    Declared(IndexMap<String, PartitionDto>),
    /// `external`: any other word is refused by the loader, by name, as a
    /// partition's `default` is.
    #[schemars(schema_with = "external_schema")]
    External(String),
}

impl Default for PartitionsDto {
    fn default() -> Self {
        Self::Declared(IndexMap::new())
    }
}

fn external_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "const": "external",
        "description": "Another tool, such as pg_partman, creates and drops this parent's \
                        partitions; pbps manages the parent alone."
    })
}

impl<'de> serde::Deserialize<'de> for PartitionsDto {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // A word or a mapping, told apart by what the document holds, as
        // `PartitionDto` is: an untagged derive would report a misspelt
        // mapping as "matches no variant", not by its own error.
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = PartitionsDto;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a mapping of partitions by name, or `external`")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<PartitionsDto, E> {
                Ok(PartitionsDto::External(v.to_owned()))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<PartitionsDto, A::Error> {
                serde::Deserialize::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(PartitionsDto::Declared)
            }
        }
        d.deserialize_any(V)
    }
}

impl<'de> serde::Deserialize<'de> for PartitionDto {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = PartitionDto;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("`default`, or a mapping of `from:` and `to:` or `default: true`")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<PartitionDto, E> {
                Ok(PartitionDto::Default(v.to_owned()))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<PartitionDto, A::Error> {
                serde::Deserialize::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(PartitionDto::Entry)
            }
        }
        d.deserialize_any(V)
    }
}

/// One key column's end of a range: a value as text or a whole number, or
/// `MINVALUE`/`MAXVALUE`. Not a decimal or a boolean, whose YAML reading is
/// not the text the engine prints: quote those.
#[derive(Debug, schemars::JsonSchema)]
#[serde(untagged)]
pub enum BoundValueDto {
    Text(String),
    Int(i64),
}

impl<'de> serde::Deserialize<'de> for BoundValueDto {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = BoundValueDto;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a bound value: text, a whole number, MINVALUE or MAXVALUE (quote a decimal)",
                )
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<BoundValueDto, E> {
                Ok(BoundValueDto::Text(v.to_owned()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<BoundValueDto, E> {
                Ok(BoundValueDto::Int(v))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<BoundValueDto, E> {
                i64::try_from(v)
                    .map(BoundValueDto::Int)
                    .map_err(|_| E::custom("a bound value out of range: quote it"))
            }
        }
        d.deserialize_any(V)
    }
}

/// One value of a LIST partition (#1632): text, a whole number, a boolean,
/// or YAML's `null`, which is SQL's `NULL`.
#[derive(Debug, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ListValueDto {
    Text(String),
    Int(i64),
    Bool(bool),
    Null,
}

impl<'de> serde::Deserialize<'de> for ListValueDto {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = ListValueDto;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a list value: text, a whole number, a boolean, or NULL (quote a decimal)",
                )
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<ListValueDto, E> {
                Ok(ListValueDto::Text(v.to_owned()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<ListValueDto, E> {
                Ok(ListValueDto::Int(v))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<ListValueDto, E> {
                i64::try_from(v)
                    .map(ListValueDto::Int)
                    .map_err(|_| E::custom("a list value out of range: quote it"))
            }
            // A boolean key's value, as the engine spells it.
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<ListValueDto, E> {
                Ok(ListValueDto::Bool(v))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<ListValueDto, E> {
                Ok(ListValueDto::Null)
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<ListValueDto, E> {
                Ok(ListValueDto::Null)
            }
        }
        d.deserialize_any(V)
    }
}

/// A parent's `partition_by:` (#1632): a column list, which is RANGE, or a
/// mapping naming LIST or HASH.
#[derive(Debug, schemars::JsonSchema)]
#[serde(untagged)]
pub enum PartitionByDto {
    Range(Vec<ColumnName>),
    Strategy(PartitionStrategyDto),
}

/// `{list: [k]}` or `{hash: [a, b]}`: one of the two.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
// The one named is a list and the other absent or `null`, which serde reads
// as absent: `required` alone matched both alternatives for `{list: [k],
// hash: null}`, which the loader takes (#1632 review).
#[schemars(extend("oneOf" = [
    serde_json::json!({
        "required": ["list"],
        "properties": {"list": {"type": "array"}, "hash": {"type": "null"}},
    }),
    serde_json::json!({
        "required": ["hash"],
        "properties": {"hash": {"type": "array"}, "list": {"type": "null"}},
    }),
]))]
pub struct PartitionStrategyDto {
    #[serde(default)]
    pub list: Option<Vec<ColumnName>>,
    #[serde(default)]
    pub hash: Option<Vec<ColumnName>>,
}

impl<'de> serde::Deserialize<'de> for PartitionByDto {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // A list or a mapping, told apart by what the document holds, as
        // `PartitionsDto` is.
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = PartitionByDto;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a list of key columns, or a mapping of `list:` or `hash:`")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                seq: A,
            ) -> Result<PartitionByDto, A::Error> {
                serde::Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
                    .map(PartitionByDto::Range)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<PartitionByDto, A::Error> {
                serde::Deserialize::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(PartitionByDto::Strategy)
            }
        }
        d.deserialize_any(V)
    }
}

list_or_spec!(
    PrimaryKeyDto,
    PrimaryKeySpecDto,
    "a column list, or a mapping of `columns:` with optional `name:` and `storage_parameters:`"
);
list_or_spec!(
    UniqueDto,
    UniqueSpecDto,
    "a column list, or a mapping of `columns:` and `storage_parameters:`"
);

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UniqueSpecDto {
    pub columns: Vec<ColumnName>,
    #[serde(default)]
    #[schemars(with = "BTreeMap<String, StorageValueDto>")]
    pub storage_parameters: BTreeMap<String, Spanned<StorageValueDto>>,
}

// A mirror of `pbps_model::Clustered`, which keeps `JsonSchema` off the model
// the way every type but `ReferentialAction` does; the one conversion is in
// `convert`. Tagged by kind because a UNIQUE constraint and an index may share
// a name in the declarations, and the selector must not have to guess which
// one it means. The doc comment below is the editor's description.

/// A table layout other than the default, on SQL Server: no clustered index
/// although the table has a primary key (`heap`), or the named UNIQUE
/// constraint (`{unique: <name>}`) or index (`{index: <name>}`) as the
/// clustered one.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ClusteredDto {
    Heap,
    Unique(pbps_model::ObjectLocalName),
    Index(pbps_model::ObjectLocalName),
}

/// A storage parameter's value as written. Any of the four, since the
/// parameters are booleans, integers, reals and one word list; `convert`
/// reads each into its canonical spelling by the engine's own rules, so
/// `off`, `false` and `0` are one value (DEC-1441.1).
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum StorageValueDto {
    Bool(bool),
    Int(i64),
    Real(f64),
    Text(String),
}

// A mirror of `pbps_model::ReplicaIdentity`, for the reason `ClusteredDto`
// is one, and tagged by kind for the same reason.

/// A PostgreSQL replica identity other than the default: every old column
/// (`full`), none (`nothing`), or the index of the primary key
/// (`primary_key`), of the named UNIQUE constraint (`{unique: <name>}`) or
/// the named unique index (`{index: <name>}`).
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplicaIdentityDto {
    Full,
    Nothing,
    PrimaryKey,
    Unique(pbps_model::ObjectLocalName),
    Index(pbps_model::ObjectLocalName),
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForeignKeyDto {
    pub columns: Vec<ColumnName>,

    /// `dbo.region(region_id)` or `dbo.region(a, b)`.
    #[schemars(with = "String")]
    pub references: Spanned<String>,

    #[serde(default)]
    pub on_delete: ReferentialAction,

    #[serde(default)]
    pub on_update: ReferentialAction,
}

/// How a generated column is computed (DEC-1168.1).
/// A SQL Server computed column (#1174).
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComputedDto {
    /// The expression, kept verbatim.
    pub expression: String,
    /// `true` for a value computed on write and stored (`PERSISTED`).
    #[serde(default)]
    pub persisted: bool,
    /// `true` for `PERSISTED NOT NULL`. Requires `persisted`.
    #[serde(default)]
    pub not_null: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SequenceDto {
    /// The first value. Absent is 1.
    #[serde(default)]
    pub start: Option<i64>,
    /// The step. Absent is 1.
    #[serde(default)]
    pub increment: Option<i64>,
    /// The sequence's name in the table's schema, where it is not the one the
    /// engine generates.
    #[serde(default)]
    pub name: Option<String>,
    /// The lowest value. Absent is the engine's for the type and direction.
    #[serde(default)]
    pub min: Option<i64>,
    /// The highest value. Absent is the engine's for the type and direction.
    #[serde(default)]
    pub max: Option<i64>,
    /// Whether the sequence starts again at the other bound once it reaches
    /// one. Absent is `false`.
    #[serde(default)]
    pub cycle: bool,
    /// How many numbers a session takes at once. Absent is 1.
    #[serde(default)]
    pub cache: Option<i64>,
}

/// A column's `identity:`: `[seed, increment]`, or a mapping (#1921).
#[derive(Debug, schemars::JsonSchema)]
#[serde(untagged)]
pub enum IdentityDto {
    Pair([i64; 2]),
    Options(IdentityOptionsDto),
}

/// An `identity:` written as a mapping, with its sequence's options
/// (PostgreSQL, #1921).
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IdentityOptionsDto {
    /// The first value. Absent is 1.
    #[serde(default)]
    pub seed: Option<i64>,
    /// The step. Absent is 1.
    #[serde(default)]
    pub increment: Option<i64>,
    /// The lowest value. Absent is the engine's for the type and direction.
    #[serde(default)]
    pub min: Option<i64>,
    /// The highest value. Absent is the engine's for the type and direction.
    #[serde(default)]
    pub max: Option<i64>,
    /// Whether the sequence starts again at the other bound once it reaches
    /// one. Absent is `false`.
    #[serde(default)]
    pub cycle: bool,
    /// How many numbers a session takes at once. Absent is 1.
    #[serde(default)]
    pub cache: Option<i64>,
}

impl<'de> serde::Deserialize<'de> for IdentityDto {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // A pair or a mapping, told apart by what the document holds, as
        // `PartitionsDto` is: an untagged derive would report a misspelt
        // option as "matches no variant", not by its own error.
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = IdentityDto;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("`[seed, increment]`, or a mapping of `seed`, `increment`, `min`, `max`, `cycle` and `cache`")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                seq: A,
            ) -> Result<IdentityDto, A::Error> {
                serde::Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
                    .map(IdentityDto::Pair)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<IdentityDto, A::Error> {
                serde::Deserialize::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(IdentityDto::Options)
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GeneratedDto {
    /// The expression, kept verbatim.
    pub expression: String,
    /// `true` for a value computed on write and stored. PostgreSQL holds only
    /// stored generated columns in this model.
    pub stored: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexDto {
    /// The keys, where every one is a column. Each entry is `column`,
    /// optionally followed by an operator class (PostgreSQL), then optionally
    /// by `asc` or `desc`: `column`, `column desc`, `body jsonb_path_ops`.
    /// An index names its keys here or under `keys:`, not both.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub columns: Vec<Spanned<String>>,

    /// The keys, one mapping each, where any is an expression (PostgreSQL):
    /// `{column: id}` or `{expression: "lower(email)"}`, each with optional
    /// `opclass:` and `order: asc|desc`. An index names its keys here or
    /// under `columns:`, not both.
    #[serde(default)]
    #[schemars(with = "Vec<IndexKeyDto>")]
    pub keys: Vec<Spanned<IndexKeyDto>>,

    #[serde(default)]
    pub include: Vec<ColumnName>,

    #[serde(default)]
    pub unique: bool,

    /// The filtered-index predicate. The YAML key is `where`, which is a Rust
    /// keyword, hence the rename.
    #[serde(rename = "where", default)]
    pub filter: Option<String>,

    /// The access method: `btree` (the default, and what an absent key
    /// means) or `gin` (PostgreSQL, over `jsonb` columns).
    #[serde(default)]
    pub method: pbps_model::IndexMethod,

    /// The index's storage parameters, by its method (PostgreSQL): a B-tree
    /// takes `fillfactor` and `deduplicate_items`, a GIN index `fastupdate`
    /// and `gin_pending_list_limit`.
    #[serde(default)]
    #[schemars(with = "BTreeMap<String, StorageValueDto>")]
    pub storage_parameters: BTreeMap<String, Spanned<StorageValueDto>>,
}

/// One index key under `keys:`: a column or an expression, and which of the
/// two is what its key says, never inferred from parentheses (DEC-1169.2). A
/// mapping of named fields and not a string-or-mapping list entry: read
/// without a type, a plain `n` or `on` is a YAML boolean, and a column may be
/// named either.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexKeyDto {
    /// A column of the table.
    #[serde(default)]
    pub column: Option<ColumnName>,
    /// An expression over the table's columns, kept verbatim.
    #[serde(default)]
    pub expression: Option<String>,
    /// The operator class, where it is not the key's default.
    #[serde(default)]
    pub opclass: Option<String>,
    /// `asc`, the default, or `desc`.
    #[serde(default)]
    pub order: Option<IndexOrder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum IndexOrder {
    Asc,
    Desc,
}

// The doc comment below becomes the schema's own `description`, which an editor
// shows the user — so it says what a declaration file is, not how this type is
// used. The maintainer's note goes here instead:
//
// Only `JsonSchema` is derived. The loader never deserializes through this,
// because `KindProbe` reads the leading key first so that a misspelled field is
// reported against the shape the user meant rather than whichever variant
// happened to be tried first. This type exists so the *schema* says what the
// loader accepts, in one place, generated from the very structures it reads.

/// One pbps declaration file: a table, module, role, enum or domain.
/// The leading key is both the kind and the name.
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
#[schemars(title = "pbps declaration")]
pub enum DeclarationFile {
    Extension(Box<ExtensionDto>),
    Dependency(Box<DependencyDto>),
    // Boxed for their sizes: the enum is only ever a schema, and a box is
    // transparent to it.
    Table(Box<TableDto>),
    Module(Box<ModuleDto>),
    Role(Box<RoleDto>),
    Enum(Box<EnumTypeDto>),
    Domain(Box<DomainTypeDto>),
}

#[cfg(test)]
mod tests {
    /// The editor schema must refuse what the loader refuses. A `number` arm
    /// here would let an editor bless `pct: 1.50` that `validate` then rejects.
    #[test]
    fn the_published_value_schema_has_no_floating_point_arm() {
        let v = serde_json::to_value(schemars::schema_for!(super::ValueDto)).unwrap();
        let text = v.to_string();
        assert!(!text.contains("\"number\""), "{text}");
        assert!(!text.contains("double"), "{text}");
        // And the negative case: the arms that are valid are still there.
        for arm in ["\"null\"", "\"boolean\"", "\"integer\"", "\"string\""] {
            assert!(text.contains(arm), "{arm} missing from {text}");
        }
    }

    /// `spellings_of` writes a character class per letter and leaves anything
    /// else as one; a word carrying a regex metacharacter would reach the
    /// pattern as syntax. No word does, and this is where that stops being an
    /// assumption.
    #[test]
    fn every_permission_word_is_lowercase_ascii() {
        for permission in pbps_model::Permission::ALL {
            let word = permission.as_str();
            assert!(
                word.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "`{word}` is not lowercase ASCII and `-`"
            );
        }
    }

    /// The class the pattern pads with holds exactly the characters `trim`
    /// removes, and each of them literally.
    ///
    /// The exactness is the point in both directions: a character missing from
    /// it makes the schema refuse a declaration the loader accepts, and one
    /// too many makes it bless a declaration the loader refuses. This is where
    /// `\s` would fail — it is short U+0085 and long U+FEFF — so the class is
    /// compared against `char::is_whitespace` itself, which is the question
    /// `trim` asks.
    #[test]
    fn the_padding_class_holds_exactly_the_characters_trim_removes() {
        let class = super::whitespace_class();
        let inside = class
            .strip_prefix('[')
            .and_then(|c| c.strip_suffix(']'))
            .expect("the padding is a character class");
        let expected: String = (char::MIN..=char::MAX)
            .filter(|c| c.is_whitespace())
            .collect();
        assert_eq!(inside, expected);
        assert!(
            inside.contains('\u{85}'),
            "U+0085, which ECMA-262 `\\s` omits"
        );
        assert!(
            !inside.contains('\u{feff}'),
            "U+FEFF, which ECMA-262 `\\s` takes and `trim` does not"
        );
        // A metacharacter would change what the class matches rather than being
        // one of its members; none of the set is one today, and the class is
        // written without escaping on that basis.
        assert!(!inside.contains(['\\', ']', '^', '-']));
    }

    /// The pattern is the loader's fold written out: a letter matches either
    /// case, a `-` matches the two separators the fold also reads, and the
    /// caller's whitespace class stands for its `trim`.
    #[test]
    fn a_word_becomes_a_class_per_letter_and_the_separators_it_folds() {
        assert_eq!(super::spellings_of("usage"), "[Uu][Ss][Aa][Gg][Ee]");
        assert_eq!(
            super::spellings_of("view-definition"),
            "[Vv][Ii][Ee][Ww][-_ ][Dd][Ee][Ff][Ii][Nn][Ii][Tt][Ii][Oo][Nn]"
        );
    }
}

/// Exact PostgreSQL package declaration. Rename is deliberately not a field.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtensionDto {
    pub extension: pbps_model::ObjectLocalName,
    pub schema: String,
    #[schemars(with = "String", extend("minLength" = 1))]
    pub version: serde_saphyr::Spanned<String>,
}
