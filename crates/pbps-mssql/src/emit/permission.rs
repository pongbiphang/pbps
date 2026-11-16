//! Emits the permission family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(permission: &GrantChange) -> Sql {
    match permission {
        GrantChange::Grant {
            role,
            target,
            permissions,
        } => one(format!(
            "GRANT {} ON {} TO {};",
            permission_list(permissions)?,
            securable(target)?,
            quote(role.raw())?
        )),
        GrantChange::Revoke {
            role,
            target,
            permissions,
        } => one(format!(
            "REVOKE {} ON {} FROM {};",
            permission_list(permissions)?,
            securable(target)?,
            quote(role.raw())?
        )),
        GrantChange::PublicExecution { routine, .. } => Err(DialectError::Unsupported {
            dialect: DIALECT,
            feature: format!(
                "settling PUBLIC execution on `{routine}`: this engine grants none to revoke or \
                 keep, and `public` here is an ordinary database role",
                routine = routine.shown()
            ),
        }),
    }
}
