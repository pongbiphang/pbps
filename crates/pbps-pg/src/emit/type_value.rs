//! Emits the type_value family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, type_value: &TypeValueChange) -> Sql {
    match type_value {
        TypeValueChange::AddEnumValue {
            uid,
            name,
            value,
            placement,
            definition,
            ..
        } => {
            let position = match placement {
                Some(pbps_model::EnumPlacement::Before(value)) => {
                    format!(" BEFORE {}", value_literal(value))
                }
                Some(pbps_model::EnumPlacement::After(value)) => {
                    format!(" AFTER {}", value_literal(value))
                }
                None => String::new(),
            };
            let mut statement = scoped(
                pg,
                name.schema(),
                &format!(
                    "ALTER TYPE {}.{} ADD VALUE {}{};",
                    quote(name.schema())?,
                    quote(name.name())?,
                    value_literal(value),
                    position
                ),
            )?;
            statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
                uid: uid.clone(),
                name: name.clone(),
                definition: Some((**definition).clone()),
                altered: false,
                after_vet: None,
            });
            Ok(vec![statement])
        }
        TypeValueChange::AlterDomain {
            uid,
            name,
            from,
            to,
            half,
        } => alter_domain(pg, uid, name, (from, to), *half),
        TypeValueChange::RenameEnumValue {
            uid,
            name,
            from,
            to,
            definition,
            ..
        } => {
            let mut statement = scoped(
                pg,
                name.schema(),
                &format!(
                    "ALTER TYPE {}.{} RENAME VALUE {} TO {};",
                    quote(name.schema())?,
                    quote(name.name())?,
                    value_literal(from),
                    value_literal(to)
                ),
            )?;
            statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
                uid: uid.clone(),
                name: name.clone(),
                definition: Some((**definition).clone()),
                altered: false,
                after_vet: None,
            });
            Ok(vec![statement])
        }
        TypeValueChange::EnumRebuild {
            uid,
            name,
            tmp,
            step,
        } => emit_enum_rebuild(pg, uid, name, tmp, step),
    }
}
