//! Emits the table_property family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(table_property: &TablePropertyChange) -> Sql {
    match table_property {
        // Refused, not silently rendered. This engine grants no principal
        // `EXECUTE` on a procedure it creates, so
        // `Dialect::creates_public_executable_routines` answers `false` here
        // and the differ never builds one — a plan that carried one anyway
        // would be a plan built for the other engine. Emitting
        // `REVOKE EXECUTE … FROM public` would apply cleanly against SQL
        // Server's real `public` role and take away an access nothing here
        // granted, which is the silent wrong answer
        // `DialectError::Unsupported` exists to prevent — and the decision to
        // *keep* a default this engine never grants is just as wrong a claim.
        // `validate` refuses the field on this engine; a change that carries
        // it anyway came from somewhere else and has no statement here.
        // This engine does not hold descriptions (`holds_descriptions`), so no plan
        // carries one here; a change that does came from somewhere else and has no
        // statement. `MS_Description` follows in its own issue (#2729).
        TablePropertyChange::SetComment { target, .. } => Err(DialectError::Unsupported {
            dialect: DIALECT,
            feature: format!(
                "a comment on `{table}`, which this engine does not hold yet",
                table = target.table().shown()
            ),
        }),
        TablePropertyChange::SetReplicaIdentity { table, .. } => Err(DialectError::Unsupported {
            dialect: DIALECT,
            feature: format!(
                "a replica identity on `{table}`, which is PostgreSQL's",
                table = table.shown()
            ),
        }),
        TablePropertyChange::SetStorageParameters { table, .. } => Err(DialectError::Unsupported {
            dialect: DIALECT,
            feature: format!(
                "storage parameters on `{table}`, which are PostgreSQL's",
                table = table.shown()
            ),
        }),
        TablePropertyChange::SetTablePersistence { table, .. } => Err(DialectError::Unsupported {
            dialect: DIALECT,
            feature: format!(
                "an unlogged `{table}`, which is PostgreSQL's",
                table = table.shown()
            ),
        }),
    }
}
