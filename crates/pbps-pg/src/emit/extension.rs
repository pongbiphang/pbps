//! Emits the extension family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(extension: &ExtensionChange) -> Sql {
    match extension {
        // Roles (ADR-0005, ADR-0010 §3). The principal is a **cluster** object
        // here, not a database one, so `Dialect::manages_roles` is `false` and
        // the differ builds none of these three on this dialect: a declared
        // role the cluster lacks is refused by a connected check with the
        // `CREATE ROLE` to run, and a dropped role's grants are revoked while
        // the role itself is left standing.
        //
        // Each arm is therefore the second lock on the same door — a plan that
        // arrived some other way names the statement a human has to run,
        // rather than becoming SQL that widens or narrows the whole cluster's
        // idea of who a principal is.
        // Exactly what the declaration pins: no `IF NOT EXISTS`, which would
        // adopt a package that is already there, and no `CASCADE`, which would
        // install a prerequisite at a version nobody approved (DEC-2447.6).
        ExtensionChange::CreateExtension {
            uid,
            name,
            definition,
            ..
        } => {
            let mut statement = Statement::new(format!(
                "CREATE EXTENSION {} SCHEMA {} VERSION {};",
                quote(name.raw())?,
                quote(&definition.schema)?,
                literal(&definition.version)
            ));
            statement.extension = Some(pbps_dialect::ExtensionStatement {
                uid: uid.clone(),
                name: name.clone(),
                phase: pbps_dialect::ExtensionPhase::Create,
            });
            Ok(vec![statement])
        }
        // `RESTRICT` is the engine's default and is written anyway, and there
        // is no `IF EXISTS` or `CASCADE`: a package the target lost is an
        // error, and what the drop reaches was sealed into the plan and is
        // checked by the executor (DEC-2447.8).
        ExtensionChange::DropExtension { uid, name, .. } => {
            let mut statement =
                Statement::new(format!("DROP EXTENSION {} RESTRICT;", quote(name.raw())?));
            statement.extension = Some(pbps_dialect::ExtensionStatement {
                uid: uid.clone(),
                name: name.clone(),
                phase: pbps_dialect::ExtensionPhase::Drop,
            });
            Ok(vec![statement])
        }
        // The version is named, never the default, and a path the engine
        // cannot walk is its own error: no downgrade guess, no drop and
        // recreate (DEC-2447.10).
        // Only the schema is named: the package keeps its version and every
        // member keeps its object number (DEC-2447.11).
        ExtensionChange::SetExtensionSchema {
            uid,
            name,
            from,
            to,
            ..
        } => {
            let mut statement = Statement::new(format!(
                "ALTER EXTENSION {} SET SCHEMA {};",
                quote(name.raw())?,
                quote(&to.schema)?
            ));
            statement.extension = Some(pbps_dialect::ExtensionStatement {
                uid: uid.clone(),
                name: name.clone(),
                phase: pbps_dialect::ExtensionPhase::SetSchema { from: from.clone() },
            });
            Ok(vec![statement])
        }
        ExtensionChange::UpdateExtension {
            uid,
            name,
            from,
            to,
            ..
        } => {
            let mut statement = Statement::new(format!(
                "ALTER EXTENSION {} UPDATE TO {};",
                quote(name.raw())?,
                literal(&to.version)
            ));
            statement.extension = Some(pbps_dialect::ExtensionStatement {
                uid: uid.clone(),
                name: name.clone(),
                phase: pbps_dialect::ExtensionPhase::Update { from: from.clone() },
            });
            Ok(vec![statement])
        }
    }
}
