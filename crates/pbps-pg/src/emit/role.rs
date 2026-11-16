//! Emits the role family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(role: &RoleChange) -> Sql {
    match role {
        RoleChange::CreateRole { name, .. } => {
            let statement = format!("CREATE ROLE {};", quote(name.raw())?);
            Err(by_hand(
                format!(
                    "`{name}` is a role, which on this engine lives in `pg_authid` and is shared by                  every database in the cluster; pbps manages what a role is granted here and not                  whether it exists (ADR-0010 section 3)",
                    name = name.shown(),
                ),
                statement,
            ))
        }
        RoleChange::DropRole { name, .. } => {
            let statement = format!("DROP ROLE {};", quote(name.raw())?);
            Err(by_hand(
                format!(
                    "`{name}` is a role, and dropping one is a cluster-wide act this database's                  catalog cannot even see the reasons for: measured, with every grant *this*                  database holds revoked, the engine still refuses with `1 object in database                  otherdb`. pbps revokes what the role was granted here and leaves the role                  (ADR-0010 section 3, section 4)",
                    name = name.shown(),
                ),
                statement,
            ))
        }
        RoleChange::RenameRole { from, to, .. } => {
            let statement = format!(
                "ALTER ROLE {} RENAME TO {};",
                quote(from.raw())?,
                quote(to.raw())?
            );
            Err(by_hand(
                format!(
                    "`{from}` is a role, and renaming one renames it in every database of the                  cluster; pbps does not own the principal (ADR-0010 section 3). The grants follow the                  role's oid rather than its name, so nothing here has to be re-granted afterwards",
                    from = from.shown(),
                ),
                statement,
            ))
        }
    }
}
