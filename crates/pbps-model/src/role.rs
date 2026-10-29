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
#[serde(deny_unknown_fields)]
pub struct Role {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<crate::Text>,

    /// Target to the permissions granted on it. An empty set is never stored:
    /// a target with nothing granted is the same declaration as no target.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub grants: BTreeMap<GrantTarget, BTreeSet<Permission>>,
}

impl Role {
    /// The grants a declaration can spell. Derived permissions belong to the
    /// recorded state, never to YAML or the data catalogue (DEC-1892.1).
    pub fn declarable_grants(&self) -> impl Iterator<Item = (&GrantTarget, BTreeSet<Permission>)> {
        self.grants.iter().filter_map(|(target, permissions)| {
            let declared: BTreeSet<_> = permissions
                .iter()
                .copied()
                .filter(|p| *p != Permission::SerialSequences)
                .collect();
            (!declared.is_empty()).then_some((target, declared))
        })
    }
}

/// What a grant applies to.
///
/// ```compile_fail,E0277
/// let target: pbps_model::GrantTarget = "schema::app".parse().unwrap();
/// let _ = format!("{}", target);
/// ```
///
/// ```compile_fail,E0599
/// let target: pbps_model::GrantTarget = "schema::app".parse().unwrap();
/// let _ = target.to_string();
/// ```
///
/// Written as `dbo.customer` for an object, `app.f(integer,text)` for a
/// routine the engine identifies by signature, and `schema::dbo` for a whole
/// schema — the `schema::` prefix is T-SQL's own spelling of the securable
/// class, which is the one every SQL Server user already knows.
///
/// The routine spelling exists because `GRANT EXECUTE ON FUNCTION app.f` is an
/// error on PostgreSQL where `app.f` is overloaded (ADR-0009 §1, measured):
/// a grant has to be able to name which one. On SQL Server nothing overloads,
/// so nothing there writes it.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub enum GrantTarget {
    /// A table, view, procedure or function: the objects that share one
    /// namespace and one [`ObjectName`] type.
    Object(ObjectName),
    /// One overload of a function or procedure, by signature.
    Routine(crate::module::RoutineId),
    /// Every object in a schema, present and future.
    Schema(String),
    /// The owned sequence of one declared serial column (DEC-2199.1).
    Sequence(crate::ColumnRef),
}

impl GrantTarget {
    /// The schema this target lives in or is.
    pub fn schema(&self) -> &str {
        match self {
            GrantTarget::Object(o) => &o.schema,
            GrantTarget::Routine(r) => &r.name.schema,
            GrantTarget::Schema(s) => s,
            GrantTarget::Sequence(c) => &c.table.schema,
        }
    }
}

impl GrantTarget {
    pub fn raw(&self) -> impl fmt::Display + Copy + '_ {
        RawGrantTarget(self)
    }

    pub fn shown(&self) -> impl fmt::Display + Copy + '_ {
        ShownGrantTarget(self)
    }
}

#[derive(Clone, Copy)]
struct RawGrantTarget<'a>(&'a GrantTarget);

impl fmt::Display for RawGrantTarget<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            GrantTarget::Object(o) => o.raw().fmt(f),
            GrantTarget::Routine(r) => r.raw().fmt(f),
            GrantTarget::Schema(s) => write!(f, "schema::{s}"),
            GrantTarget::Sequence(c) => {
                f.write_str("sequence::")?;
                c.raw().fmt(f)
            }
        }
    }
}

#[derive(Clone, Copy)]
struct ShownGrantTarget<'a>(&'a GrantTarget);

impl fmt::Display for ShownGrantTarget<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            GrantTarget::Object(o) => o.shown().fmt(f),
            GrantTarget::Routine(r) => r.shown().fmt(f),
            GrantTarget::Schema(s) => write!(f, "schema::{}", crate::escape::Escaped(s)),
            GrantTarget::Sequence(c) => write!(f, "sequence::{}", c.shown()),
        }
    }
}

impl FromStr for GrantTarget {
    type Err = NameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.get(..10)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sequence::"))
        {
            return s[10..].parse().map(GrantTarget::Sequence);
        }

        // Not trimmed, on either side of the prefix. Measured on SQL Server
        // 2025: `CREATE SCHEMA [ app]` keeps its padding, `pull` renders the
        // target as a quoted `"schema:: app"`, and a trimmed reload named a
        // schema the database does not have — the next connected plan then
        // failing, or touching the wrong securable. A name is what the
        // database spells it; `trim()` asks whether there is one and nothing
        // more (DECISIONS 177, 178).
        //
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
            if rest.trim().is_empty() {
                return Err(NameError::EmptySegment(s.to_owned()));
            }
            return Ok(GrantTarget::Schema(rest.to_owned()));
        }
        // Parentheses mean a signature, the same shape rule `ModuleId` reads.
        if s.contains('(') {
            return match s.parse::<crate::module::ModuleId>() {
                Ok(crate::module::ModuleId::Routine(r)) => Ok(GrantTarget::Routine(r)),
                _ => Err(NameError::TableShape(s.to_owned())),
            };
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
        t.raw().to_string()
    }
}

/// An object-level permission this tool models (ADR-0005, ADR-0010 §6).
///
/// A closed set, and the **union** of what the supported engines grant, not
/// any one engine's list: inviolable constraint 1 needs one model in which two
/// semantically identical schemas compare equal, so the dialect stays outside
/// this type and `Dialect::validate_role` refuses the words its engine lacks
/// (DECISIONS 210). The first eight are SQL Server's; `usage`, `create`,
/// `truncate`, `trigger` and `maintain` are PostgreSQL's, which has no `alter`
/// and no `view-definition`. `DENY` is excluded outright, and permissions not
/// listed here (`CONTROL`, `TAKE OWNERSHIP`, `IMPERSONATE`) are reported by
/// `pull` as left alone rather than quietly folded into something they are not.
///
/// New words are appended: the derived order is the order `fmt` writes a
/// grant's permissions in, and inserting one would reorder every role file.
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
    Usage,
    Create,
    Truncate,
    Trigger,
    Maintain,
    /// USAGE on every sequence the table's `serial` columns own
    /// (`pg_depend.deptype = 'a'`). Derived, never declared (#1892,
    /// DEC-1892.1): engine-neutral data that only PostgreSQL derives.
    SerialSequences,
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
            Permission::Usage => "usage",
            Permission::Create => "create",
            Permission::Truncate => "truncate",
            Permission::Trigger => "trigger",
            Permission::Maintain => "maintain",
            Permission::SerialSequences => "serial-sequences",
        }
    }

    /// Declarable words only: the loader must never accept a derived marker.
    pub const ALL: [Permission; 13] = [
        Permission::Select,
        Permission::Insert,
        Permission::Update,
        Permission::Delete,
        Permission::References,
        Permission::Execute,
        Permission::Alter,
        Permission::ViewDefinition,
        Permission::Usage,
        Permission::Create,
        Permission::Truncate,
        Permission::Trigger,
        Permission::Maintain,
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

/// The serial columns whose sequences an INSERT target uses (DEC-2202.1).
/// Partitions own no columns or sequences: even a sub-partition uses the root.
pub fn serial_grant_columns(
    table: &crate::TableName,
    tables: &BTreeMap<crate::TableName, crate::Table>,
) -> BTreeSet<crate::ColumnRef> {
    let Some((name, root)) =
        crate::partitions::ancestor_chain(table, tables).and_then(|chain| chain.last().copied())
    else {
        return BTreeSet::new();
    };
    root.columns
        .iter()
        .filter(|(_, c)| matches!(c.value_source, crate::ValueSource::Sequence(_)))
        .map(|(column, _)| name.column(column))
        .collect()
}

/// Adds the sequence access implied by a declared INSERT on a serial tree.
/// Only declarations pass here: inferring access from a live INSERT would
/// hide the very missing sequence privilege the comparison must find.
pub fn implied_serial_grants(schema: &mut crate::Schema) {
    let serial_tables: BTreeMap<_, _> = schema
        .tables
        .keys()
        .filter_map(|name| {
            let columns = serial_grant_columns(name, &schema.tables);
            (!columns.is_empty()).then_some((name, columns))
        })
        .collect();
    for role in schema.roles.values_mut() {
        for (target, permissions) in &mut role.grants {
            if matches!(target, GrantTarget::Object(table) if serial_tables.contains_key(table))
                && permissions.contains(&Permission::Insert)
            {
                permissions.insert(Permission::SerialSequences);
            }
        }
        let implied: BTreeSet<_> = role
            .grants
            .iter()
            .filter_map(|(target, rights)| match target {
                GrantTarget::Object(table) if rights.contains(&Permission::SerialSequences) => {
                    serial_tables.get(table)
                }
                GrantTarget::Object(_)
                | GrantTarget::Sequence(_)
                | GrantTarget::Routine(_)
                | GrantTarget::Schema(_) => None,
            })
            .flatten()
            .cloned()
            .collect();
        role.grants.retain(|target, rights| {
            if let GrantTarget::Sequence(column) = target
                && implied.contains(column)
            {
                rights.remove(&Permission::Usage);
            }
            !rights.is_empty()
        });
    }
}

/// Expand the table marker into the actual column permissions for comparisons.
/// The marker and explicit USAGE describe one ACL entry, never two revocations.
pub fn effective_grants(
    role: &Role,
    tables: &BTreeMap<crate::TableName, crate::Table>,
) -> BTreeMap<GrantTarget, BTreeSet<Permission>> {
    let mut grants = role.grants.clone();
    for (target, rights) in &role.grants {
        if let GrantTarget::Object(table) = target
            && rights.contains(&Permission::SerialSequences)
        {
            for column in serial_grant_columns(table, tables) {
                grants
                    .entry(GrantTarget::Sequence(column))
                    .or_default()
                    .insert(Permission::Usage);
            }
            if let Some(rights) = grants.get_mut(target) {
                rights.remove(&Permission::SerialSequences);
            }
        }
    }
    grants.retain(|_, rights| !rights.is_empty());
    grants
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
        if name.raw().trim().is_empty() {
            problems.push("a role must have a name".to_owned());
        }
        for (target, permissions) in &role.grants {
            if permissions.is_empty() {
                problems.push(format!(
                    "role `{name}`: `{target}` is listed with no permission — remove the line \
                     or name one",
                    name = name.shown(),
                    target = target.shown()
                ));
            }
            match target {
                GrantTarget::Object(object)
                    if !schema.tables.contains_key(object)
                        && !schema
                            .modules
                            .keys()
                            .any(|id| id.referenced_name().as_ref() == Some(object)) =>
                {
                    problems.push(format!(
                        "role `{name}`: grants on `{object}`, which the declarations do not have; \
                         declare the object, or grant on `schema::{}` if it is outside pbps",
                        object.schema,
                        object = object.shown(),
                        name = name.shown()
                    ));
                }
                // A signature names one overload, so nothing but that exact
                // routine satisfies it: a grant on `app.f(integer)` where only
                // `app.f(text)` is declared is a grant on an object pbps will
                // never see, exactly as the object case is.
                GrantTarget::Routine(routine)
                    if !schema.modules.keys().any(
                        |id| matches!(id, crate::module::ModuleId::Routine(r) if r == routine),
                    ) =>
                {
                    problems.push(format!(
                        "role `{name}`: grants on `{routine}`, which the declarations do not \
                         have; declare the routine with that signature, or grant on \
                         `schema::{}` if it is outside pbps",
                        routine.name.schema,
                        routine = routine.shown(),
                        name = name.shown()
                    ));
                }
                GrantTarget::Sequence(column) => {
                    if !schema
                        .tables
                        .get(&column.table)
                        .filter(|table| table.partition_of.is_none())
                        .and_then(|table| table.columns.get(&column.name))
                        .is_some_and(|c| matches!(c.value_source, crate::ValueSource::Sequence(_)))
                    {
                        problems.push(format!(
                            "role `{}`: grants on `{}`, which must name a declared serial column with an owned sequence",
                            name.shown(), target.shown()
                        ));
                    }
                    for permission in permissions {
                        if ![Permission::Usage, Permission::Select, Permission::Update]
                            .contains(permission)
                        {
                            problems.push(format!(
                                "role `{}`: `{permission}` on `{}` is not a sequence permission; use usage, select or update",
                                name.shown(), target.shown()
                            ));
                        }
                    }
                }
                GrantTarget::Object(_) | GrantTarget::Routine(_) | GrantTarget::Schema(_) => {}
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
    fn unknown_role_fields_are_refused_instead_of_defaulting_grants() {
        for json in [
            r#"{"grnats": {}}"#,
            r#"{"grants": {}, "descripton": "reader"}"#,
        ] {
            let error = serde_json::from_str::<Role>(json).unwrap_err().to_string();
            assert!(error.contains("unknown field"), "{error}");
            assert!(
                error.contains("grnats") || error.contains("descripton"),
                "{error}"
            );
        }
        for json in ["{}", r#"{"grants": {}}"#] {
            assert_eq!(serde_json::from_str::<Role>(json).unwrap(), Role::default());
        }
    }

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
    fn serial_sequence_access_is_derived_only_for_insert_on_a_serial_table() {
        use crate::{Column, Identity, OwnedSequence, Schema, Table, ValueSource};
        for (source, permission, expected) in [
            (
                ValueSource::Sequence(OwnedSequence::serial()),
                Permission::Insert,
                true,
            ),
            (
                ValueSource::Sequence(OwnedSequence::serial()),
                Permission::Select,
                false,
            ),
            (
                ValueSource::Identity(Identity::new(1, 1)),
                Permission::Insert,
                false,
            ),
            (ValueSource::None, Permission::Insert, false),
        ] {
            let name = TableName::new("app", "t");
            let mut column = Column::new("integer".parse().unwrap());
            column.value_source = source;
            let mut table = Table::default();
            table.columns.insert("id".into(), column);
            let target = GrantTarget::Object(name.clone());
            let mut schema = Schema::default();
            schema.tables.insert(name, table);
            schema.roles.insert(
                "writer".into(),
                Role {
                    grants: [
                        (target.clone(), [permission].into()),
                        (
                            GrantTarget::Schema("app".into()),
                            [Permission::Insert].into(),
                        ),
                        (
                            GrantTarget::Object(TableName::new("app", "absent")),
                            [Permission::Insert].into(),
                        ),
                    ]
                    .into(),
                    ..Role::default()
                },
            );
            implied_serial_grants(&mut schema);
            let once = schema.clone();
            implied_serial_grants(&mut schema);
            assert_eq!(schema, once, "derivation is idempotent");
            let role = &schema.roles["writer"];
            assert_eq!(
                role.grants[&target].contains(&Permission::SerialSequences),
                expected
            );
            assert!(role.grants[&target].contains(&permission));
            for (other, held) in &role.grants {
                if other != &target {
                    assert!(!held.contains(&Permission::SerialSequences));
                }
            }
        }
    }

    #[test]
    fn partition_insert_derives_only_the_roots_serial_access_at_every_depth() {
        use crate::{Identity, OwnedSequence, PartitionBound, PartitionOf, ValueSource};
        for source in [
            ValueSource::Sequence(OwnedSequence::serial()),
            ValueSource::None,
            ValueSource::Identity(Identity::new(1, 1)),
        ] {
            for permission in [Permission::Insert, Permission::Select] {
                let expected =
                    matches!(source, ValueSource::Sequence(_)) && permission == Permission::Insert;
                let mut schema = schema_with_table();
                schema
                    .tables
                    .get_mut(&"dbo.customer".parse().unwrap())
                    .unwrap()
                    .columns["id"]
                    .value_source = source.clone();
                for (name, parent) in [("dbo.p1", "dbo.customer"), ("dbo.p2", "dbo.p1")] {
                    schema.tables.insert(
                        name.parse().unwrap(),
                        Table {
                            partition_of: Some(PartitionOf {
                                parent: parent.parse().unwrap(),
                                bound: PartitionBound::Default,
                                columns: Default::default(),
                            }),
                            ..Default::default()
                        },
                    );
                }
                for target in ["dbo.p1", "dbo.p2"] {
                    schema.roles.insert(
                        target.into(),
                        Role {
                            grants: [
                                (target.parse().unwrap(), [permission].into()),
                                (
                                    "sequence::dbo.customer.id".parse().unwrap(),
                                    [Permission::Usage, Permission::Select].into(),
                                ),
                            ]
                            .into(),
                            ..Default::default()
                        },
                    );
                }
                implied_serial_grants(&mut schema);
                for target in ["dbo.p1", "dbo.p2"] {
                    let role = &schema.roles[target];
                    assert_eq!(
                        role.grants[&target.parse().unwrap()]
                            .contains(&Permission::SerialSequences),
                        expected
                    );
                    assert_eq!(
                        role.grants[&"sequence::dbo.customer.id".parse().unwrap()]
                            .contains(&Permission::Usage),
                        !expected
                    );
                    assert_eq!(
                        effective_grants(role, &schema.tables)
                            [&"sequence::dbo.customer.id".parse().unwrap()],
                        [Permission::Usage, Permission::Select].into()
                    );
                    assert!(
                        !effective_grants(role, &schema.tables)
                            .contains_key(&format!("sequence::{target}.id").parse().unwrap())
                    );
                }
                let once = schema.clone();
                implied_serial_grants(&mut schema);
                assert_eq!(schema, once);
                // A missing parent and a cycle must prove no implication.
                schema
                    .tables
                    .get_mut(&"dbo.p1".parse().unwrap())
                    .unwrap()
                    .partition_of
                    .as_mut()
                    .unwrap()
                    .parent = "dbo.missing".parse().unwrap();
                assert!(
                    serial_grant_columns(&"dbo.p2".parse().unwrap(), &schema.tables).is_empty()
                );
                schema
                    .tables
                    .get_mut(&"dbo.p1".parse().unwrap())
                    .unwrap()
                    .partition_of
                    .as_mut()
                    .unwrap()
                    .parent = "dbo.p2".parse().unwrap();
                assert!(
                    serial_grant_columns(&"dbo.p2".parse().unwrap(), &schema.tables).is_empty()
                );
            }
        }
    }

    #[test]
    fn an_explicit_sequence_target_cannot_name_a_partitions_inherited_column() {
        let mut schema = schema_with_table();
        schema.roles.insert(
            "writer".into(),
            Role {
                grants: [(
                    "sequence::dbo.customer.id".parse().unwrap(),
                    [Permission::Usage].into(),
                )]
                .into(),
                ..Default::default()
            },
        );
        let table = schema
            .tables
            .get_mut(&"dbo.customer".parse().unwrap())
            .unwrap();
        table.columns["id"].value_source =
            crate::ValueSource::Sequence(crate::OwnedSequence::serial());
        assert!(check(&schema).is_empty());
        schema
            .tables
            .get_mut(&"dbo.customer".parse().unwrap())
            .unwrap()
            .partition_of = Some(crate::PartitionOf {
            parent: "dbo.parent".parse().unwrap(),
            bound: crate::PartitionBound::Default,
            columns: Default::default(),
        });
        assert!(
            check(&schema)
                .iter()
                .any(|p| p.contains("sequence::dbo.customer.id") && p.contains("owned sequence"))
        );
        schema
            .tables
            .get_mut(&"dbo.customer".parse().unwrap())
            .unwrap()
            .columns
            .clear();
        assert!(
            check(&schema)
                .iter()
                .any(|p| p.contains("sequence::dbo.customer.id"))
        );
    }

    #[test]
    fn the_serial_marker_round_trips_in_state_but_has_no_declaration_word() {
        assert!(!Permission::ALL.contains(&Permission::SerialSequences));
        for word in ["serial-sequences", "SERIAL SEQUENCES", "serial_sequences"] {
            assert!(word.parse::<Permission>().is_err(), "{word}");
        }
        let json = serde_json::to_string(&Permission::SerialSequences).unwrap();
        assert_eq!(json, "\"serial-sequences\"");
        assert_eq!(
            serde_json::from_str::<Permission>(&json).unwrap(),
            Permission::SerialSequences
        );
        assert_eq!("USAGE".parse::<Permission>().unwrap(), Permission::Usage);
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

    /// The set is the union of the engines' (ADR-0010 §6): PostgreSQL's five
    /// parse and render like SQL Server's eight, in the declaration spelling
    /// and the engine's, and the ones an engine lacks are its dialect's to
    /// refuse — not the model's, which would put the dialect inside it.
    #[test]
    fn every_permission_of_either_engine_parses_and_renders_by_its_own_word() {
        for p in Permission::ALL {
            assert_eq!(p.as_str().parse::<Permission>().unwrap(), p);
            assert_eq!(
                p.as_str()
                    .to_ascii_uppercase()
                    .parse::<Permission>()
                    .unwrap(),
                p,
                "the engine's spelling"
            );
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
            assert_eq!(serde_json::from_str::<Permission>(&json).unwrap(), p);
        }
        for (word, p) in [
            ("usage", Permission::Usage),
            ("create", Permission::Create),
            ("truncate", Permission::Truncate),
            ("trigger", Permission::Trigger),
            ("maintain", Permission::Maintain),
        ] {
            assert_eq!(word.parse::<Permission>().unwrap(), p);
        }
        assert_eq!(Permission::ALL.len(), 13);
        // Still outside: PostgreSQL's `connect`, `temporary` and `set` are
        // database-level, and SQL Server's `control` is not a grant this tool
        // will ever write (ADR-0005).
        for word in ["connect", "temporary", "set", "control"] {
            assert!(word.parse::<Permission>().is_err(), "{word}");
        }
    }

    fn schema_with_table() -> Schema {
        let mut s = Schema::default();
        let mut t = Table::default();
        t.columns.insert(
            "id".into(),
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
        s.roles
            .insert(("app_reader".to_owned()).into(), role.clone());
        assert_eq!(check(&s), Vec::<String>::new());

        role.grants.insert(
            "dbo.ghost".parse().unwrap(),
            [Permission::Select].into_iter().collect(),
        );
        s.roles.insert(("app_reader".to_owned()).into(), role);
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
        s.roles.insert(("r".to_owned()).into(), role);
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
        a.roles.insert(("r".to_owned()).into(), role);
        assert_ne!(a, b);
        let mut role = Role::default();
        role.grants.insert(
            "dbo.customer".parse().unwrap(),
            [Permission::Select, Permission::Insert]
                .into_iter()
                .collect(),
        );
        b.roles.insert(("r".to_owned()).into(), role);
        assert_eq!(a, b);
    }
    #[test]
    fn sequence_targets_round_trip_and_require_a_declared_serial_and_sequence_words() {
        let target: GrantTarget = "SEQUENCE::dbo.customer.id".parse().unwrap();
        assert_eq!(
            target,
            GrantTarget::Sequence("dbo.customer.id".parse().unwrap())
        );
        assert_eq!(target.raw().to_string(), "sequence::dbo.customer.id");
        assert_eq!(target.schema(), "dbo");
        assert_eq!(
            serde_json::from_str::<GrantTarget>(&serde_json::to_string(&target).unwrap()).unwrap(),
            target
        );
        for bad in [
            "sequence::",
            "sequence::dbo.customer",
            "sequence::dbo..id",
            "sequence::dbo.customer.id.extra",
            "sequencë::dbo.customer.id",
        ] {
            assert!(bad.parse::<GrantTarget>().is_err(), "{bad}");
        }
        let mut schema = schema_with_table();
        schema.roles.insert(
            "reader".into(),
            Role {
                grants: [(target.clone(), [Permission::Usage].into())].into(),
                ..Default::default()
            },
        );
        for source in [
            crate::ValueSource::None,
            crate::ValueSource::Identity(crate::Identity::new(1, 1)),
            crate::ValueSource::Sequence(crate::OwnedSequence::serial()),
        ] {
            schema
                .tables
                .get_mut(&"dbo.customer".parse().unwrap())
                .unwrap()
                .columns["id"]
                .value_source = source.clone();
            for permission in Permission::ALL {
                schema
                    .roles
                    .get_mut("reader")
                    .unwrap()
                    .grants
                    .insert(target.clone(), [permission].into());
                let problems = check(&schema);
                let valid_source = matches!(source, crate::ValueSource::Sequence(_));
                let valid_word = [Permission::Usage, Permission::Select, Permission::Update]
                    .contains(&permission);
                assert_eq!(
                    problems.is_empty(),
                    valid_source && valid_word,
                    "{source:?} {permission}: {problems:?}"
                );
                assert!(
                    problems
                        .iter()
                        .all(|p| p.contains("sequence::dbo.customer.id"))
                );
                if !valid_word {
                    assert!(problems.iter().any(|p| p.contains(permission.as_str())));
                }
            }
        }
        schema
            .tables
            .get_mut(&"dbo.customer".parse().unwrap())
            .unwrap()
            .columns
            .clear();
        assert!(
            check(&schema)
                .iter()
                .any(|p| p.contains("sequence::dbo.customer.id"))
        );
        schema.tables.clear();
        assert!(
            check(&schema)
                .iter()
                .any(|p| p.contains("sequence::dbo.customer.id"))
        );
    }

    #[test]
    fn explicit_usage_and_insert_have_one_effective_sequence_privilege() {
        let mut schema = schema_with_table();
        schema
            .tables
            .get_mut(&"dbo.customer".parse().unwrap())
            .unwrap()
            .columns["id"]
            .value_source = crate::ValueSource::Sequence(crate::OwnedSequence::serial());
        schema.roles.insert(
            "writer".into(),
            Role {
                grants: [
                    ("dbo.customer".parse().unwrap(), [Permission::Insert].into()),
                    (
                        "sequence::dbo.customer.id".parse().unwrap(),
                        [Permission::Usage, Permission::Select].into(),
                    ),
                ]
                .into(),
                ..Default::default()
            },
        );
        implied_serial_grants(&mut schema);
        let role = &schema.roles["writer"];
        assert_eq!(
            role.grants[&"sequence::dbo.customer.id".parse().unwrap()],
            [Permission::Select].into()
        );
        assert_eq!(
            effective_grants(role, &schema.tables)[&"sequence::dbo.customer.id".parse().unwrap()],
            [Permission::Usage, Permission::Select].into()
        );
        assert!(
            !effective_grants(role, &schema.tables)[&"dbo.customer".parse().unwrap()]
                .contains(&Permission::SerialSequences)
        );
        let mut plain = schema.clone();
        plain
            .roles
            .get_mut("writer")
            .unwrap()
            .grants
            .remove(&"dbo.customer".parse().unwrap());
        assert_eq!(
            effective_grants(&plain.roles["writer"], &plain.tables)
                [&"sequence::dbo.customer.id".parse().unwrap()],
            [Permission::Select].into()
        );
    }
    #[test]
    fn the_insert_implication_does_not_hide_invalid_explicit_targets() {
        let mut schema = schema_with_table();
        let table = schema
            .tables
            .get_mut(&"dbo.customer".parse().unwrap())
            .unwrap();
        let mut serial = Column::new("integer".parse().unwrap());
        serial.value_source = crate::ValueSource::Sequence(crate::OwnedSequence::serial());
        table.columns.insert("serial_id".into(), serial);
        for invalid in ["id", "missing"] {
            schema.roles.insert(
                "writer".into(),
                Role {
                    grants: [
                        ("dbo.customer".parse().unwrap(), [Permission::Insert].into()),
                        (
                            GrantTarget::Sequence(
                                "dbo.customer".parse::<TableName>().unwrap().column(invalid),
                            ),
                            [Permission::Usage].into(),
                        ),
                    ]
                    .into(),
                    ..Default::default()
                },
            );
            implied_serial_grants(&mut schema);
            assert!(!check(&schema).is_empty(), "{invalid}");
        }
    }
}
