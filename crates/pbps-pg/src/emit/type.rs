//! Emits the type family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, r#type: &TypeChange) -> Sql {
    match r#type {
        TypeChange::CreateType {
            uid,
            name,
            definition,
        } => {
            let address = TableName::new(name.schema(), name.name());
            let sql = match definition.as_ref() {
                pbps_model::ManagedType::Enum { values } => format!(
                    "CREATE TYPE {} AS ENUM ({});",
                    qualified(&address)?,
                    values
                        .iter()
                        .map(|v| literal(v))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                domain @ pbps_model::ManagedType::Domain { .. } => create_domain_sql(name, domain)?,
            };
            let mut statement = scoped(pg, name.schema(), &sql)?;
            statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
                uid: uid.clone(),
                name: name.clone(),
                definition: Some((**definition).clone()),
                altered: false,
                after_vet: None,
            });
            Ok(vec![statement])
        }
        TypeChange::DropType {
            uid,
            name,
            definition,
        } => {
            let mut statement = scoped(
                pg,
                name.schema(),
                &format!(
                    "DROP {} {}.{};",
                    if matches!(definition.as_ref(), pbps_model::ManagedType::Domain { .. }) {
                        "DOMAIN"
                    } else {
                        "TYPE"
                    },
                    quote(name.schema())?,
                    quote(name.name())?
                ),
            )?;
            statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
                uid: uid.clone(),
                name: name.clone(),
                definition: None,
                altered: false,
                after_vet: None,
            });
            Ok(vec![statement])
        }
        TypeChange::RenameType {
            uid,
            from,
            to,
            definition,
        } => {
            let kind = if matches!(definition.as_ref(), pbps_model::ManagedType::Domain { .. }) {
                "DOMAIN"
            } else {
                "TYPE"
            };
            let mut at = from.clone();
            let mut out = Vec::new();
            if from.schema() != to.schema() {
                let mut statement = scoped(
                    pg,
                    at.schema(),
                    &format!(
                        "ALTER {kind} {}.{} SET SCHEMA {};",
                        quote(at.schema())?,
                        quote(at.name())?,
                        quote(to.schema())?
                    ),
                )?;
                at = pbps_model::TypeRef::new(to.schema(), from.name())
                    .map_err(|e| invalid(e.to_string()))?;
                statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
                    uid: uid.clone(),
                    name: at.clone(),
                    definition: Some((**definition).clone()),
                    altered: false,
                    after_vet: None,
                });
                out.push(statement);
            }
            if at.name() != to.name() {
                let mut statement = scoped(
                    pg,
                    at.schema(),
                    &format!(
                        "ALTER {kind} {}.{} RENAME TO {};",
                        quote(at.schema())?,
                        quote(at.name())?,
                        quote(to.name())?
                    ),
                )?;
                statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
                    uid: uid.clone(),
                    name: to.clone(),
                    definition: Some((**definition).clone()),
                    altered: false,
                    after_vet: None,
                });
                out.push(statement);
            }
            Ok(out)
        }
    }
}
