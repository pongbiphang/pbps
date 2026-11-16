//! Emits the policy family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, policy: &PolicyChange) -> Sql {
    match policy {
        PolicyChange::DropPolicy { table, name, .. } => one(
            pg,
            table,
            format!("DROP POLICY {} ON {};", quote(name)?, qualified(table)?),
        ),
        PolicyChange::CreatePolicy {
            table,
            name,
            policy,
            ..
        } => one(pg, table, create_policy_sql(table, name, policy)?),
        PolicyChange::SetRowSecurity {
            table, from, to, ..
        } => {
            let mut statements = Vec::new();
            if from.enabled != to.enabled {
                statements.extend(one(
                    pg,
                    table,
                    format!(
                        "ALTER TABLE {} {} ROW LEVEL SECURITY;",
                        qualified(table)?,
                        if to.enabled { "ENABLE" } else { "DISABLE" }
                    ),
                )?);
            }
            if from.forced != to.forced {
                statements.extend(one(
                    pg,
                    table,
                    format!(
                        "ALTER TABLE {} {} ROW LEVEL SECURITY;",
                        qualified(table)?,
                        if to.forced { "FORCE" } else { "NO FORCE" }
                    ),
                )?);
            }
            Ok(statements)
        }
    }
}
