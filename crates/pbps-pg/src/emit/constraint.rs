//! Emits the constraint family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, change: &Change, constraint: &ConstraintChange) -> Sql {
    match constraint {
        ConstraintChange::SetPrimaryKey {
            table,
            from,
            to,
            nonclustered,
        } => {
            // Only a SQL Server declaration can make a key nonclustered, and
            // `validate` refuses the selector here; a change that says so
            // anyway came from somewhere else and has no statement.
            if *nonclustered {
                return Err(no_clustered_layout(table));
            }
            let mut out = Vec::new();
            if let Some(pk) = from {
                out.push(Statement::new(drop_primary_key(table, pk)?));
            }
            if let Some(pk) = to {
                out.push(on(
                    pg,
                    table,
                    &add_key_sql(table, &primary_key_clause(pk)?, false)?,
                )?);
            }
            Ok(out)
        }
        // No concurrent path for a unique constraint, and that is measured
        // rather than assumed: this engine builds the backing index under an
        // `ACCESS EXCLUSIVE` lock, and the online spelling is a two-step —
        // `CREATE UNIQUE INDEX CONCURRENTLY` then `ADD CONSTRAINT … USING
        // INDEX` — whose halves commit separately. A plan that half-applied
        // would be a change the gate never approved, so the hint is dropped
        // (see `built_concurrently`) rather than honoured by splitting one
        // declared constraint into two committed steps.
        ConstraintChange::AddUnique { table, .. } if change_is_clustered(change) => {
            Err(no_clustered_layout(table))
        }
        ConstraintChange::AddUnique {
            table,
            name,
            constraint,
            ..
        } => one(
            pg,
            table,
            add_key_sql(table, &unique_clause(name.raw(), constraint)?, false)?,
        ),
        ConstraintChange::AddForeignKey {
            table,
            name,
            constraint,
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} ADD {};",
                qualified(table)?,
                foreign_key_clause(name.raw(), constraint)?
            ),
        ),
        ConstraintChange::AddCheck {
            table,
            name,
            constraint,
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({});",
                qualified(table)?,
                quote(name.raw())?,
                verbatim(&constraint.expression)
            ),
        ),
        ConstraintChange::SetExclusion {
            table,
            name,
            from: None,
            to: Some(x),
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} ADD {};",
                qualified(table)?,
                exclusion_clause(name.raw(), x)?
            ),
        ),
        // Dropping or replacing one needs the retirement record and arrives with
        // it (DEC-2448.3); a replacement is a drop and an add as two changes,
        // each in its own ordering class, never one change carrying both.
        ConstraintChange::SetExclusion { name, .. } => Err(invalid(format!(
            "exclusion constraint `{}` cannot be dropped or replaced yet: that arrives with \
             the retirement record (DEC-2448.3)",
            name.shown()
        ))),
        ConstraintChange::DropUnique { table, name }
        | ConstraintChange::DropForeignKey { table, name }
        | ConstraintChange::DropCheck { table, name } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} DROP CONSTRAINT {};",
                qualified(table)?,
                quote(name.raw())?
            ),
        ),
    }
}
