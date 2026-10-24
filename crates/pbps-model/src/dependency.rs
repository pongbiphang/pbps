//! Declared assumptions about objects outside the managed database.
//!
//! These values travel beside Schema: they have no identity, changes or SQL.

use std::{collections::BTreeMap, str::FromStr};

use crate::{ColumnName, ColumnType, TableName, escape::Escaped};

/// Exactly three nonempty identifier parts, serialized as a map-key string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DependencyName {
    database: String,
    object: TableName,
}

impl DependencyName {
    pub fn database(&self) -> &str {
        &self.database
    }

    pub fn object(&self) -> &TableName {
        &self.object
    }

    pub fn raw(&self) -> String {
        [
            self.database.as_str(),
            self.object.schema.as_str(),
            self.object.name.as_str(),
        ]
        .join(".")
    }

    pub fn shown(&self) -> String {
        Escaped(&self.raw()).to_string()
    }
}

impl FromStr for DependencyName {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let fail = || {
            format!(
                "dependency `{}` must be database.schema.object, exactly three parts",
                Escaped(value)
            )
        };
        let parts: Vec<_> = value.split('.').collect();
        if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
            return Err(fail());
        }
        crate::check_segment(parts[0]).map_err(|_| fail())?;
        let object = format!("{}.{}", parts[1], parts[2])
            .parse()
            .map_err(|_| fail())?;
        Ok(Self {
            database: parts[0].into(),
            object,
        })
    }
}

impl TryFrom<String> for DependencyName {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<DependencyName> for String {
    fn from(value: DependencyName) -> Self {
        value.raw()
    }
}

impl schemars::JsonSchema for DependencyName {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "DependencyName".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "pattern": "^[^.]+\\.[^.]+\\.[^.]+$"})
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    Table,
    View,
}

impl DependencyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DependencyColumn {
    #[serde(rename = "type")]
    #[schemars(with = "String")]
    pub ty: ColumnType,
    pub nullable: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub kind: DependencyKind,
    pub columns: BTreeMap<ColumnName, DependencyColumn>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub enum Comparison {
    #[serde(rename = "compared")]
    Compared,
    #[serde(rename = "declared, not compared")]
    NotCompared,
}

impl Comparison {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compared => "compared",
            Self::NotCompared => "declared, not compared",
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct PlannedDependency {
    pub declaration: Dependency,
    pub comparison: Comparison,
}

pub type Dependencies = BTreeMap<DependencyName, Dependency>;
pub type PlannedDependencies = BTreeMap<DependencyName, PlannedDependency>;

pub fn unexamined(declarations: &Dependencies) -> PlannedDependencies {
    declarations
        .iter()
        .map(|(name, declaration)| {
            (
                name.clone(),
                PlannedDependency {
                    declaration: declaration.clone(),
                    comparison: Comparison::NotCompared,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_names_have_exactly_three_parts_including_on_wire() {
        let name: DependencyName = "otherdb.dbo.customers".parse().unwrap();
        assert_eq!(name.database(), "otherdb");
        assert_eq!(
            serde_json::to_string(&name).unwrap(),
            "\"otherdb.dbo.customers\""
        );
        for value in [
            "customers",
            "dbo.customers",
            "a.b.c.d",
            "a..c",
            ".b.c",
            "a.b.",
        ] {
            assert!(value.parse::<DependencyName>().is_err(), "{value}");
            assert!(serde_json::from_value::<DependencyName>(serde_json::json!(value)).is_err());
        }
    }

    #[test]
    fn declarations_and_comparison_outcomes_survive_the_plan_and_change_its_checksum() {
        let mut plan = crate::SavedPlan::new(
            crate::PlanOrigin::Database,
            "mssql",
            "2026-10-24T00:00:00Z",
            crate::PlanBaseline {
                description: "target".into(),
                checksum: "baseline".into(),
                database_collation: None,
                time_zone: None,
            },
            Default::default(),
            Default::default(),
        );
        let empty = plan.wire_fingerprint();
        let name: DependencyName = "otherdb.dbo.t".parse().unwrap();
        plan.dependencies.insert(
            name.clone(),
            PlannedDependency {
                declaration: Dependency {
                    kind: DependencyKind::Table,
                    columns: [(
                        "id".into(),
                        DependencyColumn {
                            ty: "int".parse().unwrap(),
                            nullable: false,
                        },
                    )]
                    .into(),
                },
                comparison: Comparison::Compared,
            },
        );
        let approved = plan.wire_fingerprint();
        assert_ne!(empty, approved);
        assert_eq!(
            plan,
            serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap()
        );
        let mut changed = plan.clone();
        changed.dependencies.get_mut(&name).unwrap().comparison = Comparison::NotCompared;
        assert_ne!(approved, changed.wire_fingerprint());
        let mut changed = plan.clone();
        changed
            .dependencies
            .get_mut(&name)
            .unwrap()
            .declaration
            .columns
            .get_mut("id")
            .unwrap()
            .nullable = true;
        assert_ne!(approved, changed.wire_fingerprint());
        let mut malformed = serde_json::to_value(&plan).unwrap();
        malformed["dependencies"][name.raw()]["comparison"] = "verified".into();
        assert!(serde_json::from_value::<crate::SavedPlan>(malformed).is_err());

        // Wire hashing must retain both evidence families; dialect admission
        // remains a separate boundary from this untrusted DTO.
        let mut mixed = plan.clone();
        mixed.declared_type_pins.push(crate::DeclaredTypePin {
            uid: "y_aaaaaa".parse().unwrap(),
            name: "app.mood".parse().unwrap(),
            definition: crate::ManagedType::Enum {
                values: vec!["active".into()],
            },
            before: None,
        });
        let mixed_checksum = mixed.wire_fingerprint();
        assert_ne!(approved, mixed_checksum);
        assert_eq!(
            mixed,
            serde_json::from_str(&serde_json::to_string(&mixed).unwrap()).unwrap()
        );
        let mut changed = mixed.clone();
        changed.declared_type_pins[0].definition = crate::ManagedType::Enum {
            values: vec!["inactive".into()],
        };
        assert_ne!(mixed_checksum, changed.wire_fingerprint());
        let mut changed = mixed;
        changed.dependencies.get_mut(&name).unwrap().comparison = Comparison::NotCompared;
        assert_ne!(mixed_checksum, changed.wire_fingerprint());
    }
}
