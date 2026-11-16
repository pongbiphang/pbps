//! Emits the constraint family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(constraint: &ConstraintChange, strategy: Strategy) -> Sql {
    match constraint {
        // An exclusion constraint is PostgreSQL's: SQL Server has no such
        // constraint, and the declaration is refused by name before any plan.
        ConstraintChange::SetExclusion { name, .. } => Err(DialectError::Unsupported {
            dialect: crate::types::DIALECT,
            feature: format!("exclusion constraint {}", name.shown()),
        }),
        ConstraintChange::SetPrimaryKey {
            table,
            from,
            to,
            nonclustered,
        } => {
            let mut out = Vec::new();
            if let Some(pk) = from {
                out.push(drop_primary_key(table, pk)?);
            }
            if let Some(pk) = to {
                out.push(Statement::new(format!(
                    "ALTER TABLE {} ADD {}{};",
                    qualified(table)?,
                    primary_key_clause(pk, !nonclustered)?,
                    online(strategy)
                )));
            }
            Ok(out)
        }
        ConstraintChange::AddUnique {
            table,
            name,
            constraint,
            clustered,
        } => one(format!(
            "ALTER TABLE {} ADD {}{};",
            qualified(table)?,
            unique_clause(name.raw(), constraint, *clustered)?,
            online(strategy)
        )),
        ConstraintChange::AddForeignKey {
            table,
            name,
            constraint,
        } => one(format!(
            "ALTER TABLE {} ADD {};",
            qualified(table)?,
            foreign_key_clause(name.raw(), constraint)?
        )),
        ConstraintChange::AddCheck {
            table,
            name,
            constraint,
        } => one(format!(
            "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({});",
            qualified(table)?,
            quote(name.raw())?,
            verbatim(&constraint.expression)
        )),
        // No ONLINE clause on any of the three. A foreign key and a check are
        // metadata only, where the clause is a syntax error rather than a
        // no-op. A UNIQUE constraint *is* backed by an index, and dropping one
        // takes the option only when that index is clustered (3745 otherwise,
        // measured). The change does not say whether the constraint it drops
        // is the clustered one — the plan's baseline does, and the emitter is
        // handed the change alone — so the clause would be a guess, and a
        // wrong guess is a statement the server rejects. An offline drop of a
        // clustered constraint is always accepted: it rebuilds the table as a
        // heap while holding its lock, which the operational note on the
        // layout change says (#1178). Building one online is a different
        // matter: see AddUnique.
        ConstraintChange::DropUnique { table, name }
        | ConstraintChange::DropForeignKey { table, name }
        | ConstraintChange::DropCheck { table, name } => one(format!(
            "ALTER TABLE {} DROP CONSTRAINT {};",
            qualified(table)?,
            quote(name.raw())?
        )),
    }
}
