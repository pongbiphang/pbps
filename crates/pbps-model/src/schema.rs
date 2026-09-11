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

use crate::data::TableData;
use crate::module::{Module, ModuleId};
use crate::name::TableName;
use crate::types::ColumnType;

/// The complete desired state of a project.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    pub tables: BTreeMap<TableName, Table>,

    /// Views, procedures, functions and triggers (ADR-0002).
    ///
    /// Keyed by [`ModuleId`], not by name: what identifies a module depends on
    /// its kind — a routine by its signature, a trigger by its table
    /// (ADR-0009 §1).
    ///
    /// They live in the same [`Schema`] as the tables because they are part of
    /// the desired state and of the drift comparison — but they carry no data,
    /// so they get none of the identity machinery and never appear in the ids
    /// file.
    ///
    /// Defaulted on read: every state snapshot and plan written before modules
    /// existed is a project with no modules, not a broken file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub modules: BTreeMap<ModuleId, Module>,

    /// Database roles and what they are granted (ADR-0005).
    ///
    /// By name, like modules — but unlike modules they carry identity in the
    /// ids file, because dropping and recreating a role destroys its
    /// membership, which pbps does not manage and cannot restore.
    ///
    /// Defaulted on read: a snapshot written before roles existed describes an
    /// environment with no managed roles, not a broken file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, crate::role::Role>,
}

impl Schema {
    pub fn get(&self, name: &TableName) -> Option<&Table> {
        self.tables.get(name)
    }
}

impl Table {
    /// The single column a declared row is keyed by, where this table has one.
    ///
    /// `None` is not "no key": it is a primary key that is absent or composite,
    /// which a `data:` block cannot be written against at all — the differ
    /// refuses it (`DataWithoutKey`) rather than emitting anything.
    pub fn data_key_column(&self) -> Option<&str> {
        match &self.primary_key {
            Some(pk) if pk.columns.len() == 1 => Some(pk.columns[0].as_str()),
            _ => None,
        }
    }

    /// The columns a declared row can hold a value in: every one but the
    /// column its key lives in and the engine's own `IDENTITY`s.
    ///
    /// The key is the map key of the row, not a cell in it, and a non-key
    /// `IDENTITY` is the engine's — never written by a row and never read back
    /// (DECISIONS 94). So neither is a cell that can differ, and a table with
    /// none of these can never produce an `UPDATE`: the differ builds one only
    /// from columns that pass this filter and emits it only if the result is
    /// non-empty.
    ///
    /// One spelling, because it is asked from two sides. The differ asks which
    /// cells to compare; `doctor` asks whether an `UPDATE` is possible at all,
    /// and demanding the permission for a table that can never emit one would
    /// report a gap against an account that can run every statement this
    /// declaration can produce.
    pub fn row_columns<'a>(
        &'a self,
        key_column: &'a str,
    ) -> impl Iterator<Item = (&'a String, &'a Column)> {
        self.columns
            .iter()
            .filter(move |(c, spec)| c.as_str() != key_column && spec.identity.is_none())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// An `IndexMap` preserves declaration order, which decides the column
    /// layout of `CREATE TABLE`. Equality ignores order, so reordering is never
    /// mistaken for a schema change — and neither is it by
    /// [`state_checksum`](crate::state_checksum), which sorts these keys before
    /// hashing them for that reason (DECISIONS 238).
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

    /// Declared reference data (ADR-0004).
    ///
    /// `None` — the overwhelmingly common case — is the opt-in switch being
    /// off: pbps touches no row of a table that does not declare one. It lives
    /// inside [`Table`] rather than beside the model because rows are desired
    /// state that the database shows, so `==` must see them.
    ///
    /// Defaulted on read: every snapshot and plan written before `data:`
    /// existed describes a table that declares no rows, not a broken file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<TableData>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// Whether the engine has a declared source for existing rows when this
    /// is added as a required column.
    ///
    /// Expressions stay opaque everywhere else, but a default that explicitly
    /// invokes a NULL-producing construct is not a trustworthy value source.
    /// This is deliberately a conservative lexical check rather than an
    /// attempt to evaluate or normalize SQL.
    ///
    /// Boundaries an identifier by [`crate::module::is_regular_identifier_continue`]
    /// — SQL Server's own rule. That is right for SQL Server's own caller.
    /// It is **not** right for the model's dialect-free risk classification
    /// (`Change::intrinsic_risks`), which runs before a dialect is chosen and
    /// still reads every dialect's defaults by this rule regardless — a known
    /// gap, tracked separately as issue #343, not a decision that this answer
    /// is correct there. A caller that does have a dialect uses
    /// [`Self::has_required_add_value_source_with`] instead, the same split
    /// ADR-0011 Amendment 2 made for `normalize_definition` (DECISIONS 226,
    /// 433).
    pub fn has_required_add_value_source(&self) -> bool {
        self.has_required_add_value_source_with(crate::module::is_regular_identifier_continue)
    }

    /// [`Self::has_required_add_value_source`], with an identifier's end
    /// decided by `continues_ident` instead of assumed to be SQL Server's.
    ///
    /// PostgreSQL's rule is over bytes, so every non-ASCII byte continues a
    /// name there and the shared default's `char::is_alphanumeric` does not
    /// agree with it on all of them — **measured**, a combining mark
    /// (`\u{301}`) is not alphanumeric, so `null\u{301}x` — one PostgreSQL
    /// identifier, accepted unquoted — splits at the mark under the shared
    /// rule into the bare word `null`, and a function call is misread as an
    /// explicit `NULL`.
    pub fn has_required_add_value_source_with(&self, continues_ident: fn(char) -> bool) -> bool {
        self.identity.is_some()
            || self
                .default
                .as_deref()
                .is_some_and(|default| !has_explicit_null_semantics(default, continues_ident))
    }
}

fn has_explicit_null_semantics(expression: &str, continues_ident: fn(char) -> bool) -> bool {
    // String literals and comments are not SQL expressions. Blanking them
    // keeps a harmless default such as 'NULL' from being mistaken for the NULL
    // keyword while still finding it inside CAST(NULL AS int), arithmetic, and
    // other expression shapes.
    crate::module::code_without_quoted_identifiers(expression)
        // Where a word ends is the caller's rule, not always SQL Server's
        // (`is_regular_identifier_continue`) — treating `$`, `@`, or `#` as
        // punctuation there would turn `seq$null` into a false NULL token,
        // and treating a byte PostgreSQL reads as a name as a boundary
        // instead turns a name into one.
        .split(|ch: char| !continues_ident(ch))
        .any(|word| {
            word.eq_ignore_ascii_case("null")
                || word.eq_ignore_ascii_case("nullif")
                || word.eq_ignore_ascii_case("try_cast")
                || word.eq_ignore_ascii_case("try_convert")
                || word.eq_ignore_ascii_case("try_parse")
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub seed: i64,
    pub increment: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimaryKey {
    /// The constraint name. `None` leaves the database to name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UniqueConstraint {
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub references_table: TableName,
    pub references_columns: Vec<String>,

    #[serde(default)]
    pub on_delete: ReferentialAction,
    #[serde(default)]
    pub on_update: ReferentialAction,
}

/// `JsonSchema` is derived here and nowhere else in the model.
///
/// This enum is not merely a domain value: it is a set of literal words a user
/// types into a declaration file, and the editor schema of SPEC §14.1 is
/// generated from the loader's own types precisely so the two cannot drift.
/// Mirroring the variants in `pbps-load` to avoid the derive would recreate the
/// drift the generation exists to prevent.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReferentialAction {
    #[default]
    NoAction,
    Cascade,
    SetNull,
    SetDefault,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConstraint {
    /// The check expression, kept verbatim.
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

    #[test]
    fn an_explicitly_nullable_default_is_not_a_required_add_value_source() {
        let mut column = Column::new(ty("int"));
        assert!(!column.has_required_add_value_source());

        for default in [
            "NULL",
            " null ",
            "(NULL)",
            "((( null )))",
            "CAST(NULL AS int)",
            "CONVERT(int, NULL)",
            "(NULL) + 1",
            "NULLIF(1, 1)",
            "TRY_CONVERT(int, 'not a number')",
            // A bare CR ends a line comment on the engine (a YAML `"\r"`
            // escape gets one there), so what follows it is the keyword.
            "-- carried over\rNULL",
            "-- carried over\r\nNULL",
            "-- carried over\nNULL",
        ] {
            column.default = Some(default.into());
            assert!(!column.has_required_add_value_source(), "{default:?}");
        }

        for default in [
            "0",
            "'NULL'",
            "SYSUTCDATETIME()",
            "NEWID()",
            "NEXT VALUE FOR dbo.[null]",
            "NEXT VALUE FOR dbo.\"try_cast\"",
            "NEXT VALUE FOR dbo.[seq]]null]",
            "NEXT VALUE FOR dbo.\"seq\"\"try_cast\"",
            "NEXT VALUE FOR dbo.seq$null",
            "NEXT VALUE FOR dbo.seq@null",
            "NEXT VALUE FOR dbo.seq#null",
            "NEXT VALUE FOR dbo.序列null",
            "1 /* outer /* nested */ NULL */",
            // These were measured not to end a comment; the engine sees a
            // default of `1` and a long comment, and so must the scan.
            "1 -- note\u{85}NULL",
            "1 -- note\u{2028}NULL",
            "1 -- note\u{0c}NULL",
            "1 -- note\u{0b}NULL",
        ] {
            column.default = Some(default.into());
            assert!(column.has_required_add_value_source(), "{default}");
        }
    }

    /// The keyword scan's word boundary is the caller's rule, not always SQL
    /// Server's. **Measured on PostgreSQL 18.6**, `null\u{301}x` — `null`
    /// followed by a combining acute accent, then `x` — is one identifier:
    /// `CREATE FUNCTION zz.null\u{301}x() ...` is accepted and names the
    /// routine `nulĺx`, callable unquoted. `\u{301}` is not alphanumeric, so
    /// the shared rule (`is_regular_identifier_continue`, `char::is_alphanumeric`)
    /// splits it from `null` and reads a bare `null` where the engine reads
    /// one name — misclassifying a plain function call as an explicit NULL
    /// and, under [`Change::intrinsic_risks`] or an mssql-shaped preflight
    /// probe, the column as having no trustworthy value source.
    #[test]
    fn a_combining_mark_is_read_as_a_name_byte_under_postgresqls_boundary_and_a_gap_under_the_shared_one()
     {
        // PostgreSQL's own rule (`pbps_dialect::continues_ident`, not
        // imported here: the model stays dialect-free by constraint) — every
        // non-ASCII byte continues a name, so the mark stays glued to `null`
        // and the whole thing reads as the one word `null\u{301}x`, which
        // does not match the keyword list.
        fn postgresql_boundary(ch: char) -> bool {
            ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || !ch.is_ascii()
        }

        let mut column = Column::new(ty("int"));
        column.default = Some("zz.null\u{301}x()".into());

        assert!(
            column.has_required_add_value_source_with(postgresql_boundary),
            "a plain call to a name PostgreSQL reads as one identifier"
        );

        // Negative: unparametrized, the answer is unchanged — SQL Server's
        // rule still splits at the mark and still reads the bare word
        // `null`, exactly as it did before `has_required_add_value_source_with`
        // existed.
        assert!(
            !column.has_required_add_value_source(),
            "the shared default keeps SQL Server's answer"
        );
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
