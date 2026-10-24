//! Declared cross-database rehearsal inputs and their typed handoff (DEC-2323.1).

use super::scratch;
use pbps_db::{Conn, DbError};
use pbps_model::dependency::{
    Comparison, Dependencies, Dependency, DependencyKind, DependencyName,
};
use std::collections::{BTreeMap, BTreeSet};

mod catalog;
mod indexes;
mod rebuild;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectName {
    pub database: String,
    pub schema: String,
    pub name: String,
}

impl From<&DependencyName> for ObjectName {
    fn from(name: &DependencyName) -> Self {
        Self {
            database: name.database().into(),
            schema: name.object().schema.clone(),
            name: name.object().name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Limitation {
    HiddenObject {
        object: ObjectName,
    },
    UnsupportedObject {
        object: ObjectName,
        kind: String,
    },
    UnresolvedReference {
        object: ObjectName,
    },
    CatalogRead {
        object: ObjectName,
        code: Option<String>,
    },
    Replay {
        object: ObjectName,
        code: Option<String>,
        reason: String,
    },
    MetadataMismatch {
        object: ObjectName,
        columns: Vec<String>,
    },
    Index {
        object: ObjectName,
        index: String,
        code: Option<String>,
        reason: String,
        cause: Option<Box<Limitation>>,
    },
    LineageUnavailable {
        object: ObjectName,
    },
    FallbackInClosure {
        object: ObjectName,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    Verified,
    PartiallyUnverified { limitation: Limitation },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeSource {
    Catalog,
    Declaration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rehearsal {
    BoundAgainstRealDatabase,
    CompanionRendered {
        source: ShapeSource,
        verification: Verification,
    },
}

pub type Rehearsals = BTreeMap<DependencyName, Rehearsal>;

struct Input {
    declaration: Dependency,
    catalog: Option<catalog::Closure>,
    verification: Verification,
}

/// Catalog contents remain engine-private, as they do in automatic externals.
pub struct Inputs {
    values: BTreeMap<DependencyName, Input>,
    real: bool,
}

fn refused(name: &DependencyName, reason: impl std::fmt::Display) -> DbError {
    DbError::Refused(format!("dependency `{}`: {reason}", name.shown()))
}

pub async fn read(
    target: &mut Conn,
    declarations: &Dependencies,
    real: bool,
) -> Result<Inputs, DbError> {
    let compared = crate::dependencies::compare(target, declarations).await?;
    let mut inputs = BTreeMap::new();
    for (name, planned) in compared {
        let address = ObjectName::from(&name);
        let catalog = if !real && planned.comparison == Comparison::Compared {
            match catalog::relation(target, &address).await? {
                Some(root) => Some(catalog::closure(target, &address, root).await?),
                None => None,
            }
        } else {
            None
        };
        let verification = match &catalog {
            Some(catalog) => match &catalog.limitation {
                Some(limitation) => Verification::PartiallyUnverified {
                    limitation: limitation.clone(),
                },
                None => Verification::Verified,
            },
            None => Verification::PartiallyUnverified {
                limitation: Limitation::HiddenObject { object: address },
            },
        };
        inputs.insert(
            name,
            Input {
                declaration: planned.declaration,
                catalog,
                verification,
            },
        );
    }
    if !real {
        crate::dependencies::compare(target, declarations).await?;
    }
    Ok(Inputs {
        values: inputs,
        real,
    })
}

impl Inputs {
    pub fn databases(&self) -> BTreeSet<String> {
        let mut databases: BTreeSet<_> = self
            .values
            .keys()
            .map(|name| name.database().to_owned())
            .collect();
        for input in self.values.values() {
            if let Some(catalog) = &input.catalog
                && catalog.limitation.is_none()
            {
                databases.extend(catalog.relations.keys().map(|name| name.database.clone()));
            }
        }
        databases
    }

    pub fn rehearsals(&self) -> Rehearsals {
        self.values
            .iter()
            .map(|(name, input)| {
                let rehearsal = if self.real {
                    Rehearsal::BoundAgainstRealDatabase
                } else {
                    Rehearsal::CompanionRendered {
                        source: if input.catalog.is_some() {
                            ShapeSource::Catalog
                        } else {
                            ShapeSource::Declaration
                        },
                        verification: input.verification.clone(),
                    }
                };
                (name.clone(), rehearsal)
            })
            .collect()
    }

    /// DDL connections have entered one irreversible sandbox per companion.
    /// The separate scratch administrator is used only for catalog/lineage reads.
    pub async fn stage(
        &mut self,
        sessions: &mut BTreeMap<String, Conn>,
        observer: &mut Conn,
        token: &str,
    ) -> Result<(), DbError> {
        rebuild::stage(self, sessions, observer, token).await
    }

    /// Recheck the reconstructed closure on the companion administrator's
    /// read connection. A changed shape can only lose verification.
    pub async fn verify(&mut self, observer: &mut Conn) -> Result<(), DbError> {
        rebuild::verify(self, observer).await
    }
}

fn declaration(
    name: &DependencyName,
    shape: &Dependency,
    private: &str,
) -> Result<Vec<String>, DbError> {
    let object = name.object();
    let qualified = |local: &str| {
        format!(
            "{}.{}",
            scratch::ident(&object.schema),
            scratch::ident(local)
        )
    };
    let columns = shape
        .columns
        .iter()
        .map(|(column, shape)| {
            let ty = crate::types::normalize(&shape.ty).map_err(|error| refused(name, error))?;
            Ok(format!(
                "{} {} {}",
                scratch::ident(column.raw()),
                ty.raw(),
                if shape.nullable { "NULL" } else { "NOT NULL" }
            ))
        })
        .collect::<Result<Vec<_>, DbError>>()?
        .join(", ");
    let table = match shape.kind {
        DependencyKind::Table => object.name.as_str(),
        DependencyKind::View => private,
    };
    let mut statements = vec![format!("CREATE TABLE {}({columns});", qualified(table))];
    if shape.kind == DependencyKind::View {
        let selected = shape
            .columns
            .keys()
            .map(|column| scratch::ident(column.raw()))
            .collect::<Vec<_>>()
            .join(", ");
        statements.push(format!(
            "CREATE VIEW {} AS SELECT {selected} FROM {};",
            qualified(&object.name),
            qualified(private)
        ));
    }
    Ok(statements)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn declaration_preserves_view_kind_nullability_and_quoted_names() {
        let name: DependencyName = "other.dbo.v]".parse().unwrap();
        let mut shape = Dependency {
            kind: DependencyKind::View,
            columns: [(
                "i]".into(),
                pbps_model::dependency::DependencyColumn {
                    ty: "integer".parse().unwrap(),
                    nullable: false,
                },
            )]
            .into(),
        };
        let sql = declaration(&name, &shape, "private").unwrap();
        assert_eq!(
            sql,
            [
                "CREATE TABLE [dbo].[private]([i]]] int NOT NULL);",
                "CREATE VIEW [dbo].[v]]] AS SELECT [i]]] FROM [dbo].[private];"
            ]
        );
        shape.columns.get_mut("i]").unwrap().ty = "not_a_type".parse().unwrap();
        assert!(declaration(&name, &shape, "private").is_err());
    }
}
