//! The structure of the desired state.
//!
//! # Two deliberate design constraints
//!
//! **Containers hold names; elements do not.** `Table` has no `name` field and
//! `Column` has no `name` field — the name is the key in the parent map. This
//! eliminates an entire class of bugs that cannot detect themselves: a map key
//! disagreeing with the name stored inside.
//!
//! **The model expresses state, never intent.** Transient annotations such as
//! `renamed_from` do not live here; `pbps-load` returns them separately. The
//! reason is that `Schema` must satisfy "two semantically identical schemas are
//! equal", which both diff and drift detection are built on. Mixing in one-shot
//! intent would make the same state compare unequal depending on whether an
//! annotation happened to be present.

use std::collections::BTreeMap;

use indexmap::IndexMap;

use crate::module::{Module, ObjectName};
use crate::name::TableName;
use crate::types::ColumnType;

/// The complete desired state of a project.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Schema {
    pub tables: BTreeMap<TableName, Table>,

    /// Views, procedures, functions and triggers (ADR-0002).
    ///
    /// They live in the same [`Schema`] as the tables because they are part of
    /// the desired state and of the drift comparison — but they carry no data,
    /// so they get none of the identity machinery and never appear in the ids
    /// file.
    ///
    /// Defaulted on read: every state snapshot and plan written before modules
    /// existed is a project with no modules, not a broken file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub modules: BTreeMap<ObjectName, Module>,
}

impl Schema {
    pub fn get(&self, name: &TableName) -> Option<&Table> {
        self.tables.get(name)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Table {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// An `IndexMap` preserves declaration order, which decides the column
    /// layout of `CREATE TABLE`. Equality ignores order, so reordering is never
    /// mistaken for a schema change.
    pub columns: IndexMap<String, Column>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<PrimaryKey>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unique: BTreeMap<String, UniqueConstraint>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub foreign_keys: BTreeMap<String, ForeignKey>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<String, CheckConstraint>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub indexes: BTreeMap<String, Index>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Column {
    #[serde(rename = "type")]
    pub ty: ColumnType,

    /// Defaults to `true`, matching SQL's own default.
    pub nullable: bool,

    /// The default-value expression, kept verbatim (`0`, `SYSUTCDATETIME()`).
    ///
    /// It is not parsed into a structure: expression syntax is dialect knowledge,
    /// and all we ever need from it is to hand it to the database untouched and
    /// to compare whether it changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Present means deprecated; the value is the reason.
    ///
    /// No date is stored — git already records it, so asking the user to write
    /// one by hand is redundant and guaranteed to drift from the real time
    /// (SPEC §4.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<String>,
}

impl Column {
    /// The most common shape: nullable, no default.
    pub fn new(ty: ColumnType) -> Self {
        Self {
            ty,
            nullable: true,
            default: None,
            identity: None,
            description: None,
            deprecated: None,
        }
    }

    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    pub fn is_deprecated(&self) -> bool {
        self.deprecated.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub seed: i64,
    pub increment: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrimaryKey {
    /// The constraint name. `None` leaves the database to name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UniqueConstraint {
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub references_table: TableName,
    pub references_columns: Vec<String>,

    #[serde(default)]
    pub on_delete: ReferentialAction,
    #[serde(default)]
    pub on_update: ReferentialAction,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferentialAction {
    #[default]
    NoAction,
    Cascade,
    SetNull,
    SetDefault,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CheckConstraint {
    /// The check expression, kept verbatim.
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Index {
    pub columns: Vec<IndexColumn>,

    /// Payload columns that are not part of the key (SQL Server's INCLUDE).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,

    #[serde(default)]
    pub unique: bool,

    /// The filtered-index predicate, kept verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexColumn {
    pub name: String,
    #[serde(default)]
    pub descending: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    fn sample() -> Table {
        let mut columns = IndexMap::new();
        columns.insert("customer_id".into(), Column::new(ty("bigint")).not_null());
        columns.insert("email".into(), Column::new(ty("nvarchar(255)")));
        Table {
            columns,
            primary_key: Some(PrimaryKey {
                name: Some("pk_customer".into()),
                columns: vec!["customer_id".into()],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn nullable_defaults_to_true() {
        assert!(Column::new(ty("int")).nullable);
        assert!(!Column::new(ty("int")).not_null().nullable);
    }

    /// Column order affects CREATE TABLE output, so it has to be preserved.
    #[test]
    fn column_order_is_preserved() {
        let t = sample();
        assert_eq!(
            t.columns.keys().collect::<Vec<_>>(),
            ["customer_id", "email"]
        );
    }

    /// ...but a different order must not count as a schema change, or every
    /// reshuffle would produce a phantom diff.
    #[test]
    fn column_order_does_not_affect_equality() {
        let a = sample();
        let mut columns = IndexMap::new();
        columns.insert("email".into(), Column::new(ty("nvarchar(255)")));
        columns.insert("customer_id".into(), Column::new(ty("bigint")).not_null());
        let b = Table {
            columns,
            ..a.clone()
        };
        assert_eq!(a, b);
    }

    /// Serialization must be deterministic: state snapshots go into git and into
    /// the database, and shifting order would manufacture phantom diffs.
    #[test]
    fn serialisation_is_deterministic() {
        let mut schema = Schema::default();
        schema
            .tables
            .insert(TableName::new("dbo", "customer"), sample());
        schema
            .tables
            .insert(TableName::new("app", "region"), Table::default());

        let first = serde_json::to_string(&schema).unwrap();
        for _ in 0..20 {
            assert_eq!(serde_json::to_string(&schema).unwrap(), first);
        }
        // BTreeMap ordering: app.region comes before dbo.customer
        assert!(first.find("app.region").unwrap() < first.find("dbo.customer").unwrap());
    }

    #[test]
    fn schema_round_trips_through_json() {
        let mut schema = Schema::default();
        schema
            .tables
            .insert(TableName::new("dbo", "customer"), sample());
        let json = serde_json::to_string(&schema).unwrap();
        let back: Schema = serde_json::from_str(&json).unwrap();
        assert_eq!(schema, back);
    }

    /// Empty collections must not pollute the output, or identity files and
    /// snapshots would fill up with `{}`.
    #[test]
    fn empty_collections_are_omitted() {
        let json = serde_json::to_string(&Table::default()).unwrap();
        assert_eq!(json, r#"{"columns":{}}"#);
    }
}
