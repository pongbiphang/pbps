//! Database roles and their grants
//! ([ADR-0005](../../../docs/ADR-0005-roles-and-grants.md)).
//!
//! # The portable unit is the role
//!
//! Naive GRANT-as-code dies on one fact: principals are environment-specific.
//! Dev and prod have different users, and logins are server-level objects no
//! portable declaration can describe. So what pbps manages is the **database
//! role** — its existence and what it is granted — and what stays each
//! environment's own is who holds it. Membership is never declared, never
//! compared, and never touched.
//!
//! # Why roles carry identity
//!
//! The criterion ADR-0005 generalizes from ADR-0002: does drop + add destroy
//! state that lives only in the environment and cannot be restored from the
//! declarations? For a role the answer is yes — its membership — so a role is
//! on the column side of the line: it has a uid, a rename needs intent, and a
//! drop needs a reason and leaves a tombstone.
//!
//! # Grants are data, never SQL
//!
//! A grant is a target and a set of permissions, held in `BTreeMap` /
//! `BTreeSet` so the serialization is deterministic (constraint 5). The
//! emitter renders `GRANT` and `REVOKE`; nothing here knows how either is
//! spelled (constraint 3).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use crate::module::ObjectName;
use crate::name::NameError;

/// One database role: what it is granted. Its name is the key in
/// [`crate::Schema::roles`] (constraint 2).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Role {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Target to the permissions granted on it. An empty set is never stored:
    /// a target with nothing granted is the same declaration as no target.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub grants: BTreeMap<GrantTarget, BTreeSet<Permission>>,
}

/// What a grant applies to.
///
/// Written as `dbo.customer` for an object and `schema::dbo` for a whole
/// schema — the `schema::` prefix is T-SQL's own spelling of the securable
/// class, which is the one every SQL Server user already knows.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub enum GrantTarget {
    /// A table, view, procedure or function: the objects that share one
    /// namespace and one [`ObjectName`] type.
    Object(ObjectName),
    /// Every object in a schema, present and future.
    Schema(String),
}

impl GrantTarget {
    /// The schema this target lives in or is.
    pub fn schema(&self) -> &str {
        match self {
            GrantTarget::Object(o) => &o.schema,
            GrantTarget::Schema(s) => s,
        }
    }
}

impl fmt::Display for GrantTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrantTarget::Object(o) => o.fmt(f),
            GrantTarget::Schema(s) => write!(f, "schema::{s}"),
        }
    }
}

impl FromStr for GrantTarget {
    type Err = NameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        // Case-insensitive on the prefix only: `SCHEMA::dbo` is how T-SQL
        // itself writes it, and refusing it would be a style rule pretending
        // to be a parse error.
        // `get`, not a byte slice: a multibyte character across offset 8
        // (`aaaaaaaé.x`) is a name error, not a panic.
        if let Some(rest) = s
            .get(..8)
            .filter(|p| p.eq_ignore_ascii_case("schema::"))
            .and(s.get(8..))
        {
            let name = rest.trim();
            if name.is_empty() {
                return Err(NameError::EmptySegment(s.to_owned()));
            }
            return Ok(GrantTarget::Schema(name.to_owned()));
        }
        ObjectName::from_str(s).map(GrantTarget::Object)
    }
}

impl TryFrom<String> for GrantTarget {
    type Error = NameError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<GrantTarget> for String {
    fn from(t: GrantTarget) -> String {
        t.to_string()
    }
}

/// An object-level permission this tool models (ADR-0005).
///
/// A closed set. `DENY` is excluded outright, and permissions not listed here
/// (`CONTROL`, `TAKE OWNERSHIP`, `IMPERSONATE`) are reported by `pull` as left
/// alone rather than quietly folded into something they are not.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    Select,
    Insert,
    Update,
    Delete,
    References,
    Execute,
    Alter,
    ViewDefinition,
}

impl Permission {
    /// The word written in a declaration.
    pub const fn as_str(self) -> &'static str {
        match self {
            Permission::Select => "select",
            Permission::Insert => "insert",
            Permission::Update => "update",
            Permission::Delete => "delete",
            Permission::References => "references",
            Permission::Execute => "execute",
            Permission::Alter => "alter",
            Permission::ViewDefinition => "view-definition",
        }
    }

    pub const ALL: [Permission; 8] = [
        Permission::Select,
        Permission::Insert,
        Permission::Update,
        Permission::Delete,
        Permission::References,
        Permission::Execute,
        Permission::Alter,
        Permission::ViewDefinition,
    ];
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Permission {
    type Err = String;

    /// Accepts the declaration spelling and the engine's (`VIEW DEFINITION`,
    /// `view_definition`), case-insensitively: `pull` reads the latter back
    /// from the catalog, and a user who types what the engine prints should
    /// not be corrected.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let folded = s.trim().to_ascii_lowercase().replace([' ', '_'], "-");
        Permission::ALL
            .into_iter()
            .find(|p| p.as_str() == folded)
            .ok_or_else(|| {
                let all: Vec<_> = Permission::ALL.iter().map(|p| p.as_str()).collect();
                format!("unknown permission `{s}`; available: {}", all.join(", "))
            })
    }
}

/// What a role declaration must satisfy before anything is generated from it.
///
/// A rule of the model, not of a dialect: a grant on an object nobody
/// declares is the foreign-key-target rule applied to permissions. Either the
/// object is managed here and can be named, or it is not — and a role that
/// named it would have pbps managing a permission on something it will never
/// see, whose drift it could never report. Schema-level targets pass: a schema
/// is not an object the declarations create.
///
/// Returns one message per problem, all of them, in a stable order.
pub fn check(schema: &crate::schema::Schema) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, role) in &schema.roles {
        if name.trim().is_empty() {
            problems.push("a role must have a name".to_owned());
        }
        for (target, permissions) in &role.grants {
            if permissions.is_empty() {
                problems.push(format!(
                    "role `{name}`: `{target}` is listed with no permission — remove the line \
                     or name one"
                ));
            }
            if let GrantTarget::Object(object) = target
                && !schema.tables.contains_key(object)
                && !schema.modules.contains_key(object)
            {
                problems.push(format!(
                    "role `{name}`: grants on `{object}`, which the declarations do not have; \
                     declare the object, or grant on `schema::{}` if it is outside pbps",
                    object.schema
                ));
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::name::TableName;
    use crate::schema::{Column, Schema, Table};
    use crate::types::ColumnType;

    #[test]
    fn a_target_is_an_object_or_a_whole_schema() {
        assert_eq!(
            "dbo.customer".parse::<GrantTarget>().unwrap(),
            GrantTarget::Object(TableName::new("dbo", "customer"))
        );
        assert_eq!(
            "schema::dbo".parse::<GrantTarget>().unwrap(),
            GrantTarget::Schema("dbo".to_owned())
        );
        assert_eq!(
            "SCHEMA::app".parse::<GrantTarget>().unwrap(),
            GrantTarget::Schema("app".to_owned())
        );
        // The negative cases: a bare word is neither, and an empty schema is
        // not a schema.
        assert!("customer".parse::<GrantTarget>().is_err());
        assert!("schema::".parse::<GrantTarget>().is_err());
        // A multibyte character across byte 8 is not a panic: this text
        // comes straight from a hand-written role file, and byte-slicing the
        // prefix used to crash on it. The first is an ordinary object name;
        // the second is neither.
        assert_eq!(
            "aaaaaaaé.x".parse::<GrantTarget>().unwrap(),
            GrantTarget::Object(TableName::new("aaaaaaaé", "x"))
        );
        assert!("schemä::dbo".parse::<GrantTarget>().is_err());
    }

    #[test]
    fn targets_round_trip_as_json_map_keys() {
        let mut role = Role::default();
        role.grants.insert(
            GrantTarget::Schema("dbo".into()),
            [Permission::Select].into_iter().collect(),
        );
        role.grants.insert(
            GrantTarget::Object(TableName::new("dbo", "t")),
            [Permission::Select, Permission::ViewDefinition]
                .into_iter()
                .collect(),
        );
        let json = serde_json::to_string(&role).unwrap();
        assert!(json.contains("\"schema::dbo\""), "{json}");
        assert!(json.contains("\"view-definition\""), "{json}");
        let back: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(back, role);
    }

    #[test]
    fn a_permission_is_read_in_its_own_spelling_and_the_engines() {
        assert_eq!(
            "view-definition".parse::<Permission>().unwrap(),
            Permission::ViewDefinition
        );
        assert_eq!(
            "VIEW DEFINITION".parse::<Permission>().unwrap(),
            Permission::ViewDefinition
        );
        assert_eq!("Select".parse::<Permission>().unwrap(), Permission::Select);
        // Not modelled, and refused by name rather than folded into something.
        let e = "control".parse::<Permission>().unwrap_err();
        assert!(e.contains("control"), "{e}");
        assert!(e.contains("select"), "the available set is named: {e}");
    }

    fn schema_with_table() -> Schema {
        let mut s = Schema::default();
        let mut t = Table::default();
        t.columns.insert(
            "id".to_owned(),
            Column::new("int".parse::<ColumnType>().unwrap()),
        );
        s.tables.insert(TableName::new("dbo", "customer"), t);
        s
    }

    #[test]
    fn a_grant_on_an_undeclared_object_is_refused_and_a_schema_grant_is_not() {
        let mut s = schema_with_table();
        let mut role = Role::default();
        role.grants.insert(
            "dbo.customer".parse().unwrap(),
            [Permission::Select].into_iter().collect(),
        );
        role.grants.insert(
            "schema::legacy".parse().unwrap(),
            [Permission::Select].into_iter().collect(),
        );
        s.roles.insert("app_reader".to_owned(), role.clone());
        assert_eq!(check(&s), Vec::<String>::new());

        role.grants.insert(
            "dbo.ghost".parse().unwrap(),
            [Permission::Select].into_iter().collect(),
        );
        s.roles.insert("app_reader".to_owned(), role);
        let p = check(&s);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("dbo.ghost"), "{p:?}");
        assert!(p[0].contains("schema::dbo"), "the remedy is named: {p:?}");
    }

    #[test]
    fn a_target_with_no_permission_is_refused() {
        let mut s = schema_with_table();
        let mut role = Role::default();
        role.grants
            .insert("dbo.customer".parse().unwrap(), BTreeSet::new());
        s.roles.insert("r".to_owned(), role);
        let p = check(&s);
        assert!(p.iter().any(|m| m.contains("no permission")), "{p:?}");
    }

    /// Roles are part of the desired state, so two schemas differing only in a
    /// grant must not be `==` — and identical ones must be, whatever order the
    /// permissions arrived in.
    #[test]
    fn a_grant_takes_part_in_schema_equality() {
        let mut a = schema_with_table();
        let mut b = a.clone();
        assert_eq!(a, b);
        let mut role = Role::default();
        role.grants.insert(
            "dbo.customer".parse().unwrap(),
            [Permission::Insert, Permission::Select]
                .into_iter()
                .collect(),
        );
        a.roles.insert("r".to_owned(), role);
        assert_ne!(a, b);
        let mut role = Role::default();
        role.grants.insert(
            "dbo.customer".parse().unwrap(),
            [Permission::Select, Permission::Insert]
                .into_iter()
                .collect(),
        );
        b.roles.insert("r".to_owned(), role);
        assert_eq!(a, b);
    }
}
