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

use pbps_model::ReferentialAction;

/// Which kind of declaration a file holds.
///
/// Read first, with unknown fields deliberately **allowed**: this pass only
/// answers "table or module", and the real DTO for that answer is what rejects
/// a misspelled field — with a message naming the right shape rather than
/// "unknown field `colunms`" from whichever variant happened to be tried first.
#[derive(Debug, serde::Deserialize)]
pub struct KindProbe {
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
#[schemars(extend("oneOf" = [
    serde_json::json!({"required": ["view"], "properties": {"view": {"type": "string"}}}),
    serde_json::json!({"required": ["procedure"], "properties": {"procedure": {"type": "string"}}}),
    serde_json::json!({"required": ["function"], "properties": {"function": {"type": "string"}}}),
    serde_json::json!({"required": ["trigger"], "properties": {"trigger": {"type": "string"}}}),
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

    /// The body. What it starts with depends on the kind; see
    /// [`pbps_model::Module`].
    pub definition: String,

    /// Modules that must be created first, where the identifier scan cannot see
    /// the dependency. Persistent, like `strategy:`, and preserved by `fmt`.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub depends_on: Vec<Spanned<String>>,
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
    pub columns: IndexMap<String, ColumnDto>,

    #[serde(default)]
    pub primary_key: Option<PrimaryKeyDto>,

    #[serde(default)]
    pub unique: BTreeMap<String, Vec<String>>,

    #[serde(default)]
    pub foreign_keys: BTreeMap<String, ForeignKeyDto>,

    /// Constraint name to check expression.
    #[serde(default)]
    pub checks: BTreeMap<String, String>,

    #[serde(default)]
    pub indexes: BTreeMap<String, IndexDto>,
}

/// `deny_unknown_fields` is what turns a typo into an error instead of a
/// silent no-op — the whole point of ADR-0003's "validate rejects unknown keys".
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StrategyDto {
    #[serde(default)]
    pub online: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ColumnDto {
    #[serde(rename = "type")]
    #[schemars(with = "String")]
    pub ty: Spanned<String>,

    #[serde(default = "yes")]
    pub nullable: bool,

    /// The default-value expression, kept verbatim.
    #[serde(default)]
    pub default: Option<String>,

    /// `[seed, increment]`
    #[serde(default)]
    pub identity: Option<[i64; 2]>,

    #[serde(default)]
    pub description: Option<String>,

    /// Present means deprecated; the value is the reason. The date comes from
    /// git and is not written here.
    #[serde(default)]
    pub deprecated: Option<String>,

    /// One-shot intent, as for [`TableDto::renamed_from`].
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub renamed_from: Option<Spanned<String>>,
}

const fn yes() -> bool {
    true
}

/// A primary key may be written as a bare column list, leaving the name to the
/// database, or with an explicit constraint name.
///
/// The named form is not there for looks: when `pbps pull` reverse-generates from
/// an existing database, losing the original constraint name would make the next
/// diff want to rename it.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum PrimaryKeyDto {
    Columns(Vec<String>),
    Named { name: String, columns: Vec<String> },
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForeignKeyDto {
    pub columns: Vec<String>,

    /// `dbo.region(region_id)` or `dbo.region(a, b)`.
    #[schemars(with = "String")]
    pub references: Spanned<String>,

    #[serde(default)]
    pub on_delete: ReferentialAction,

    #[serde(default)]
    pub on_update: ReferentialAction,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexDto {
    /// Each entry is `column` or `column desc`.
    #[schemars(with = "Vec<String>")]
    pub columns: Vec<Spanned<String>>,

    #[serde(default)]
    pub include: Vec<String>,

    #[serde(default)]
    pub unique: bool,

    /// The filtered-index predicate. The YAML key is `where`, which is a Rust
    /// keyword, hence the rename.
    #[serde(rename = "where", default)]
    pub filter: Option<String>,
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

/// One pbps declaration file: a table, or one view, procedure, function or
/// trigger. The leading key is both the kind and the name.
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
#[schemars(title = "pbps declaration")]
pub enum DeclarationFile {
    Table(TableDto),
    Module(ModuleDto),
}
