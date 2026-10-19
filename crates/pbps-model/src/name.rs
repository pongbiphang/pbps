//! Qualified names: `dbo.customer` and `dbo.customer.email`.
//!
//! Names must always be fully qualified. Omitting the schema is not accepted —
//! a "default schema" is a connection-level concept, and letting it leak into
//! declaration files would make one file point at different tables depending on
//! the connection.

use std::fmt;
use std::str::FromStr;

/// One column's original identifier, without implicit human rendering.
///
/// Serialization and identity retain the original bytes (SPEC §4.3). Choose
/// `.shown()` for prose and `.raw()` for SQL quoting and textual keys.
///
/// ```compile_fail,E0277
/// let name = pbps_model::ColumnName::from("column");
/// let _ = format!("{}", name);
/// ```
///
/// ```compile_fail,E0599
/// let name = pbps_model::ColumnName::from("column");
/// let _ = name.to_string();
/// ```
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct ColumnName(String);

impl ColumnName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn raw(&self) -> &str {
        &self.0
    }

    pub fn shown(&self) -> crate::escape::Escaped<'_> {
        crate::escape::Escaped(&self.0)
    }

    pub fn into_raw(self) -> String {
        self.0
    }
}

impl std::borrow::Borrow<str> for ColumnName {
    fn borrow(&self) -> &str {
        self.raw()
    }
}

impl From<String> for ColumnName {
    fn from(name: String) -> Self {
        Self(name)
    }
}

impl From<&str> for ColumnName {
    fn from(name: &str) -> Self {
        Self(name.to_owned())
    }
}

impl From<&String> for ColumnName {
    fn from(name: &String) -> Self {
        Self(name.clone())
    }
}

impl From<&ColumnName> for ColumnName {
    fn from(name: &ColumnName) -> Self {
        name.clone()
    }
}

// The DTO schema remains that of a bare string, without a new definition/ref.
impl schemars::JsonSchema for ColumnName {
    fn inline_schema() -> bool {
        <String as schemars::JsonSchema>::inline_schema()
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        <String as schemars::JsonSchema>::schema_name()
    }
    fn schema_id() -> std::borrow::Cow<'static, str> {
        <String as schemars::JsonSchema>::schema_id()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <String as schemars::JsonSchema>::json_schema(generator)
    }
}

/// One role's original identifier, without implicit human rendering.
///
/// Serialization and identity retain the original bytes (SPEC §4.3). Choose
/// `.shown()` for prose and `.raw()` for SQL quoting and textual keys.
///
/// ```compile_fail,E0277
/// let name = pbps_model::RoleName::from("role");
/// let _ = format!("{}", name);
/// ```
///
/// ```compile_fail,E0599
/// let name = pbps_model::RoleName::from("role");
/// let _ = name.to_string();
/// ```
#[derive(
    Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct RoleName(String);

// Keep String's Debug spelling for byte-identical History collision diagnostics.
impl fmt::Debug for RoleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl RoleName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn raw(&self) -> &str {
        &self.0
    }

    pub fn shown(&self) -> crate::escape::Escaped<'_> {
        crate::escape::Escaped(&self.0)
    }

    pub fn into_raw(self) -> String {
        self.0
    }
}

impl std::borrow::Borrow<str> for RoleName {
    fn borrow(&self) -> &str {
        self.raw()
    }
}

impl From<String> for RoleName {
    fn from(name: String) -> Self {
        Self(name)
    }
}

impl From<&str> for RoleName {
    fn from(name: &str) -> Self {
        Self(name.to_owned())
    }
}

impl From<&String> for RoleName {
    fn from(name: &String) -> Self {
        Self(name.clone())
    }
}

impl From<&RoleName> for RoleName {
    fn from(name: &RoleName) -> Self {
        name.clone()
    }
}

// The DTO schema remains that of a bare string, without a new definition/ref.
impl schemars::JsonSchema for RoleName {
    fn inline_schema() -> bool {
        <String as schemars::JsonSchema>::inline_schema()
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        <String as schemars::JsonSchema>::schema_name()
    }
    fn schema_id() -> std::borrow::Cow<'static, str> {
        <String as schemars::JsonSchema>::schema_id()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <String as schemars::JsonSchema>::json_schema(generator)
    }
}

/// A refusal of a name, which shows the text it was given escaped.
///
/// The text is whatever the file spelled, control characters included: SPEC
/// §4.3 keeps scalar text verbatim and a name that holds one is valid, so a
/// refusal that echoed it raw would let a hand-made plan start a line the tool
/// never wrote (DEC-2032.1, #2035). The error escapes the original text even
/// when it cannot be parsed into a name with its own `.shown()` adapter.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("a table name must have the two parts `schema.table`, got `{}`", crate::escape::Escaped(.0))]
    TableShape(String),

    #[error(
        "a column reference must have the three parts `schema.table.column`, got `{}`",
        crate::escape::Escaped(.0)
    )]
    ColumnShape(String),

    #[error("`{}` contains an empty identifier", crate::escape::Escaped(.0))]
    EmptySegment(String),

    #[error(
        "`{}` contains a `.`, which pbps uses to separate schema, table and column; it cannot \
         be stored as part of a name",
        crate::escape::Escaped(.0)
    )]
    ContainsSeparator(String),
}

/// Whether one already-separated name part — a schema, a table, or a column
/// name — is one [`TableName`] or [`ColumnRef`] can carry.
///
/// Both types cross the JSON/YAML boundary as `.`-joined strings and parse
/// back by splitting on that character (see their `.raw()`/`FromStr` below),
/// so a `.` embedded in a part is not merely an odd identifier — once joined,
/// it is indistinguishable from the separator between two real parts. A
/// column literally named `a.b` on table `dbo.customer` serializes as
/// `dbo.customer.a.b`, which parses back as four segments: exactly the string
/// a mistyped five-part name would also produce, with no way to tell the two
/// apart after the fact (issue #108, DECISIONS 444).
///
/// `FromStr` below never needs this: splitting on `.` cannot leave a `.`
/// inside any one of its own parts, so every part it hands to
/// [`TableName::new`]/[`ColumnRef::new`] is already clean. It matters only
/// where a part is built from something that never went through `FromStr` —
/// a YAML column key (`pbps-load::convert`), which is a mapping key rather
/// than a parsed string, or a name a live database handed back
/// (`pbps-diff::identity::resolve`, fed by a dialect's introspection). By the
/// time either is serialized, refusing is too late: the value has already
/// been written into a plan or an ids file that cannot load it back.
pub fn check_segment(part: &str) -> Result<(), NameError> {
    if part.contains('.') {
        return Err(NameError::ContainsSeparator(part.to_owned()));
    }
    Ok(())
}

/// A fully qualified table name, such as `dbo.customer`.
///
/// `Ord` is decided by `(schema, name)`, which is what makes serialization order
/// stable.
///
/// Choose `.shown()` for a person or `.raw()` for SQL, serialization and keys.
///
/// ```compile_fail,E0277
/// let name = pbps_model::TableName::new("app", "table");
/// let _ = format!("{}", name);
/// ```
///
/// ```compile_fail,E0599
/// let name = pbps_model::TableName::new("app", "table");
/// let _ = name.to_string();
/// ```
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct TableName {
    pub schema: String,
    pub name: String,
}

impl TableName {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
        }
    }

    /// A reference to one column of this table.
    pub fn column(&self, column: impl Into<ColumnName>) -> ColumnRef {
        ColumnRef {
            table: self.clone(),
            name: column.into(),
        }
    }

    /// The qualified name for a person, escaped through [`crate::escape::Escaped`].
    pub fn shown(&self) -> impl fmt::Display + Copy + '_ {
        ShownTableName(self)
    }

    /// The canonical spelling for SQL quoting, serialization and identity keys.
    pub fn raw(&self) -> impl fmt::Display + Copy + '_ {
        RawTableName(self)
    }
}

#[derive(Clone, Copy)]
struct RawTableName<'a>(&'a TableName);

impl fmt::Display for RawTableName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0.schema, self.0.name)
    }
}

#[derive(Clone, Copy)]
struct ShownTableName<'a>(&'a TableName);

impl fmt::Display for ShownTableName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::escape::Escaped;
        write!(f, "{}.{}", Escaped(&self.0.schema), Escaped(&self.0.name))
    }
}

impl FromStr for TableName {
    type Err = NameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('.').collect();
        let [schema, name] = parts.as_slice() else {
            return Err(NameError::TableShape(s.to_owned()));
        };
        if schema.is_empty() || name.is_empty() {
            return Err(NameError::EmptySegment(s.to_owned()));
        }
        Ok(Self::new(*schema, *name))
    }
}

/// A fully qualified column reference, such as `dbo.customer.email`.
///
/// ```compile_fail,E0277
/// let name = pbps_model::TableName::new("app", "table").column("column");
/// let _ = format!("{}", name);
/// ```
///
/// ```compile_fail,E0599
/// let name = pbps_model::TableName::new("app", "table").column("column");
/// let _ = name.to_string();
/// ```
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct ColumnRef {
    pub table: TableName,
    pub name: ColumnName,
}

impl ColumnRef {
    pub fn new(table: TableName, name: impl Into<ColumnName>) -> Self {
        Self {
            table,
            name: name.into(),
        }
    }

    /// The qualified reference for a person, with every part escaped once.
    pub fn shown(&self) -> impl fmt::Display + Copy + '_ {
        ShownColumnRef(self)
    }

    /// The canonical spelling for SQL quoting, serialization and identity keys.
    pub fn raw(&self) -> impl fmt::Display + Copy + '_ {
        RawColumnRef(self)
    }
}

#[derive(Clone, Copy)]
struct RawColumnRef<'a>(&'a ColumnRef);

impl fmt::Display for RawColumnRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0.table.raw(), self.0.name.raw())
    }
}

#[derive(Clone, Copy)]
struct ShownColumnRef<'a>(&'a ColumnRef);

impl fmt::Display for ShownColumnRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0.table.shown(), self.0.name.shown())
    }
}

impl FromStr for ColumnRef {
    type Err = NameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('.').collect();
        let [schema, table, column] = parts.as_slice() else {
            return Err(NameError::ColumnShape(s.to_owned()));
        };
        if schema.is_empty() || table.is_empty() || column.is_empty() {
            return Err(NameError::EmptySegment(s.to_owned()));
        }
        Ok(Self::new(TableName::new(*schema, *table), *column))
    }
}

// ---- serde bridge: both cross the JSON boundary as strings ----

macro_rules! string_serde {
    ($t:ty) => {
        impl TryFrom<String> for $t {
            type Error = NameError;
            fn try_from(s: String) -> Result<Self, Self::Error> {
                s.parse()
            }
        }
        impl From<$t> for String {
            fn from(v: $t) -> String {
                v.raw().to_string()
            }
        }
    };
}

string_serde!(TableName);
string_serde!(ColumnRef);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_qualified_part_is_shown_once_and_raw_bytes_are_preserved() {
        let part = "a\nb\r\u{1b}[31m\\\u{200b}";
        let shown = r"a\nb\r\u{1b}[31m\\\u{200b}";
        let table = TableName::new(part, part);
        let column = table.column(part);
        assert_eq!(table.shown().to_string(), format!("{shown}.{shown}"));
        assert_eq!(
            column.shown().to_string(),
            format!("{shown}.{shown}.{shown}")
        );
        assert_eq!(table.raw().to_string(), format!("{part}.{part}"));
        assert_eq!(column.raw().to_string(), format!("{part}.{part}.{part}"));
        assert_eq!(
            serde_json::to_string(&table).unwrap(),
            serde_json::to_string(&format!("{part}.{part}")).unwrap()
        );
        assert_eq!(
            serde_json::to_string(&column).unwrap(),
            serde_json::to_string(&format!("{part}.{part}.{part}")).unwrap()
        );
        // Module and grant identities serialize the same raw qualified spelling.
        for (id, expected) in [
            (
                crate::ModuleId::Named(table.clone()),
                format!("{part}.{part}"),
            ),
            (
                crate::ModuleId::Routine(crate::RoutineId::new(table.clone(), vec![])),
                format!("{part}.{part}()"),
            ),
            (
                crate::ModuleId::Trigger {
                    on: table.clone(),
                    name: part.into(),
                },
                format!("{part}.{part}.{part}"),
            ),
        ] {
            assert_eq!(
                serde_json::to_string(&id).unwrap(),
                serde_json::to_string(&expected).unwrap()
            );
        }
        assert_eq!(
            serde_json::to_string(&crate::GrantTarget::Object(table)).unwrap(),
            serde_json::to_string(&format!("{part}.{part}")).unwrap()
        );
        // Ordinary Unicode letters and visible punctuation keep their spelling.
        let unicode = TableName::new("名前", "ünï").column("emoji🙂");
        assert_eq!(unicode.shown().to_string(), "名前.ünï.emoji🙂");
    }

    #[test]
    fn table_round_trips() {
        let t: TableName = "dbo.customer".parse().unwrap();
        assert_eq!(t.schema, "dbo");
        assert_eq!(t.name, "customer");
        assert_eq!(t.raw().to_string(), "dbo.customer");
    }

    #[test]
    fn column_round_trips() {
        let c: ColumnRef = "dbo.customer.email".parse().unwrap();
        assert_eq!(c.table.raw().to_string(), "dbo.customer");
        assert_eq!(c.name, "email".into());
        assert_eq!(c.raw().to_string(), "dbo.customer.email");
    }

    #[test]
    fn unqualified_names_are_rejected() {
        assert_eq!(
            "customer".parse::<TableName>().unwrap_err(),
            NameError::TableShape("customer".into())
        );
        assert_eq!(
            "dbo.customer".parse::<ColumnRef>().unwrap_err(),
            NameError::ColumnShape("dbo.customer".into())
        );
    }

    #[test]
    fn empty_segments_are_rejected() {
        assert_eq!(
            "dbo.".parse::<TableName>().unwrap_err(),
            NameError::EmptySegment("dbo.".into())
        );
    }

    /// Too many segments must be rejected, never silently truncated.
    #[test]
    fn over_qualified_names_are_rejected() {
        assert!("db.dbo.customer".parse::<TableName>().is_err());
        assert!("db.dbo.customer.email".parse::<ColumnRef>().is_err());
    }

    /// A column named `a.b` on `dbo.customer` would serialize as
    /// `dbo.customer.a.b`, which is exactly the string a mistyped five-part
    /// name also produces — `FromStr` cannot tell the two apart and must keep
    /// refusing this shape (issue #108). The refusal has to happen where the
    /// name is built (`check_segment`, used by the loader and by `pull`'s
    /// identity resolution), not here — but this pins that `FromStr` itself
    /// never grows a way to let it through.
    #[test]
    fn a_column_name_containing_the_separator_cannot_round_trip() {
        assert!("dbo.customer.a.b".parse::<ColumnRef>().is_err());
    }

    #[test]
    fn check_segment_rejects_an_embedded_separator() {
        assert_eq!(
            check_segment("a.b").unwrap_err(),
            NameError::ContainsSeparator("a.b".into())
        );
    }

    #[test]
    fn check_segment_accepts_an_ordinary_part() {
        assert!(check_segment("customer").is_ok());
        // Emptiness is a different problem, with its own message at each
        // caller ("a column must have a name", "a role must have a name")
        // — this check is only about the separator.
        assert!(check_segment("").is_ok());
    }

    #[test]
    fn ordering_is_schema_then_name() {
        let mut v = [
            TableName::new("dbo", "z"),
            TableName::new("app", "a"),
            TableName::new("dbo", "a"),
        ];
        v.sort();
        assert_eq!(
            v.iter()
                .map(|name| name.shown().to_string())
                .collect::<Vec<_>>(),
            ["app.a", "dbo.a", "dbo.z"]
        );
    }

    /// A name out of a file may hold a line break or a terminal sequence, and a
    /// refusal that echoed it raw would start a line of its own or rewrite the
    /// ones before it (DEC-2032.1, #2035). Serialization stays raw; the error
    /// escapes the text even when it is not a valid name.
    #[test]
    fn a_refusal_shows_the_name_it_was_given_escaped() {
        let hostile = "app\n\u{1b}[31m.t\r\u{202e}";
        for refusal in [
            NameError::TableShape(hostile.into()),
            NameError::ColumnShape(hostile.into()),
            NameError::EmptySegment(hostile.into()),
            NameError::ContainsSeparator(hostile.into()),
        ] {
            let text = refusal.to_string();
            assert!(!text.chars().any(char::is_control), "{text:?}");
            assert!(
                text.contains("app\\n\\u{1b}[31m.t\\r\\u{202e}"),
                "the escaped name is shown: {text}"
            );
        }
        // The refusals `FromStr` raises carry the same text.
        let refusal = "a\nb".parse::<TableName>().unwrap_err().to_string();
        assert!(
            refusal.contains("`a\\nb`") && !refusal.contains('\n'),
            "{refusal:?}"
        );
        // Negative: the serialization is not touched, so a name round-trips.
        let table = TableName::new("app", "a\nb");
        assert_eq!(table.raw().to_string(), "app.a\nb");
        // Negative: a Unicode name is shown as spelled.
        let refusal = NameError::TableShape("名前_ünï".into()).to_string();
        assert!(refusal.contains("`名前_ünï`"), "{refusal}");
    }
}
