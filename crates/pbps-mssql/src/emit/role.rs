//! Emits the role family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(role: &RoleChange) -> Sql {
    match role {
        // Roles (ADR-0005). `ALTER ROLE ... WITH NAME` keeps the membership,
        // which is the reason a role rename is intent rather than drop + add.
        RoleChange::CreateRole { name, .. } => Ok(vec![
            Statement::new(format!("CREATE ROLE {};", quote(name.raw())?))
                .creating(Created::Role(name.clone())),
        ]),
        // The members go first, each in a statement of its own, and the role
        // last: the engine refuses to drop a role that still has members, and
        // a plan that listed them is a plan the reviewer saw.
        RoleChange::DropRole { name, members, .. } => {
            let mut out = Vec::new();
            for member in members {
                out.push(Statement::new(format!(
                    "ALTER ROLE {} DROP MEMBER {};",
                    quote(name.raw())?,
                    quote(member.raw())?
                )));
            }
            out.push(Statement::new(format!("DROP ROLE {};", quote(name.raw())?)));
            Ok(out)
        }
        // The statement says what it does to the name (`Statement::renaming_role`),
        // as a table rename does, so a staged checkpoint finds the role again.
        RoleChange::RenameRole { from, to, .. } => Ok(vec![
            Statement::new(format!(
                "ALTER ROLE {} WITH NAME = {};",
                quote(from.raw())?,
                quote(to.raw())?
            ))
            .renaming_role(from.clone(), to.clone()),
        ]),
    }
}
