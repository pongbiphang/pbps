//! Qualified names: `dbo.customer` and `dbo.customer.email`.
//!
//! Names must always be fully qualified. Omitting the schema is not accepted —
//! a "default schema" is a connection-level concept, and letting it leak into
//! declaration files would make one file point at different tables depending on
//! the connection.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("a table name must have the two parts `schema.table`, got `{0}`")]
    TableShape(String),

    #[error("a column reference must have the three parts `schema.table.column`, got `{0}`")]
    ColumnShape(String),

    #[error("`{0}` contains an empty identifier")]
    EmptySegment(String),
}

/// A fully qualified table name, such as `dbo.customer`.
///
/// `Ord` is decided by `(schema, name)`, which is what makes serialization order
/// stable.
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
    pub fn column(&self, column: impl Into<String>) -> ColumnRef {
        ColumnRef {
            table: self.clone(),
            name: column.into(),
        }
    }
}

impl fmt::Display for TableName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.schema, self.name)
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
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct ColumnRef {
    pub table: TableName,
    pub name: String,
}

impl ColumnRef {
    pub fn new(table: TableName, name: impl Into<String>) -> Self {
        Self {
            table,
            name: name.into(),
        }
    }
}

impl fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.table, self.name)
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
                v.to_string()
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
    fn table_round_trips() {
        let t: TableName = "dbo.customer".parse().unwrap();
        assert_eq!(t.schema, "dbo");
        assert_eq!(t.name, "customer");
        assert_eq!(t.to_string(), "dbo.customer");
    }

    #[test]
    fn column_round_trips() {
        let c: ColumnRef = "dbo.customer.email".parse().unwrap();
        assert_eq!(c.table.to_string(), "dbo.customer");
        assert_eq!(c.name, "email");
        assert_eq!(c.to_string(), "dbo.customer.email");
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

    #[test]
    fn ordering_is_schema_then_name() {
        let mut v = [
            TableName::new("dbo", "z"),
            TableName::new("app", "a"),
            TableName::new("dbo", "a"),
        ];
        v.sort();
        assert_eq!(
            v.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["app.a", "dbo.a", "dbo.z"]
        );
    }
}
