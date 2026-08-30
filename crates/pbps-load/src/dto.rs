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

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableDto {
    pub table: Spanned<String>,

    #[serde(default)]
    pub description: Option<String>,

    /// One-shot intent: the name this table was renamed from. Removed from the
    /// file once `pbps plan` has absorbed it into the identity file.
    #[serde(default)]
    pub renamed_from: Option<Spanned<String>>,

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

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnDto {
    #[serde(rename = "type")]
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
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum PrimaryKeyDto {
    Columns(Vec<String>),
    Named { name: String, columns: Vec<String> },
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKeyDto {
    pub columns: Vec<String>,

    /// `dbo.region(region_id)` or `dbo.region(a, b)`.
    pub references: Spanned<String>,

    #[serde(default)]
    pub on_delete: ReferentialAction,

    #[serde(default)]
    pub on_update: ReferentialAction,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexDto {
    /// Each entry is `column` or `column desc`.
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
