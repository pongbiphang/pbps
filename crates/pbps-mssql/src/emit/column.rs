//! Emits the column family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(column: &ColumnChange, strategy: Strategy) -> Sql {
    match column {
        ColumnChange::AddColumn {
            table,
            name,
            column,
            ..
        } => {
            // Its own batch: a statement later in the same batch that mentions
            // the new column fails to compile, because the batch is parsed
            // before any of it runs.
            Ok(vec![
                Statement::new(format!(
                    "ALTER TABLE {} ADD {};",
                    qualified(table)?,
                    column_definition(table, name.raw(), column)?
                ))
                .own_batch()
                .creating(Created::Column(table.clone(), name.clone())),
            ])
        }
        ColumnChange::DropColumn { column, .. } => Ok(vec![
            Statement::new(format!(
                "{}\nALTER TABLE {} DROP COLUMN {};",
                drop_default_block(&column.table, column.name.raw())?,
                qualified(&column.table)?,
                quote(column.name.raw())?
            ))
            .own_batch(),
        ]),
        // The column's generated default is renamed in the same batch, when it
        // still has the name built from the old column (#975). A plan does not
        // say whether the column has a default, so the batch asks, and does
        // nothing for a column without one or with a name `pbps` did not give.
        // When the plan also renamed the table, the default has the name built
        // from the new table, or — if the table rename found its target taken
        // and left it — the one built from the old table. Both are tried; the
        // second finds nothing once the first has renamed it.
        ColumnChange::RenameColumn {
            table,
            from,
            to,
            table_was,
            ..
        } => {
            let mut sql = format!(
                "EXEC sp_rename {}, {}, 'COLUMN';",
                literal(&format!("{}.{}", qualified(table)?, quote(from.raw())?)),
                literal(to.raw())
            );
            let new = default_constraint_name(table, to.raw());
            let fallback = fallback_default_constraint_name(table, to.raw());
            let olds = std::iter::once(table)
                .chain(table_was)
                .flat_map(|t| generated_default_names(t, from.raw()));
            for old in olds {
                // After `sp_rename`, so the column is found by its new name.
                if let Some(rename) =
                    rename_generated_default(table, to.raw(), &old, &new, &fallback)?
                {
                    sql.push('\n');
                    sql.push_str(&rename);
                }
            }
            Ok(vec![Statement::new(sql).own_batch()])
        }
        ColumnChange::AlterColumnType {
            column,
            to,
            to_nullable,
            to_collation,
            ..
        } => {
            let normalized = types::normalize(to)?;
            let alter = format!(
                "ALTER TABLE {} ALTER COLUMN {} {}{} {}{};",
                qualified(&column.table)?,
                quote(column.name.raw())?,
                normalized.raw(),
                collate_clause(to_collation.as_ref())?,
                null_clause(*to_nullable),
                online(strategy)
            );
            Ok(vec![
                Statement::new(preserve_default_during_retype(
                    &column.table,
                    column.name.raw(),
                    &alter,
                )?)
                .own_batch(),
            ])
        }
        ColumnChange::AlterColumnNullability {
            column,
            ty,
            to_nullable,
            collation,
            ..
        } => {
            // `CREATE TABLE` accepts nullable timestamp / rowversion, but the
            // engine refuses every ALTER COLUMN that names either spelling —
            // including one whose only change is nullability (DECISIONS 298).
            if types::alter_column_is_refused(ty) {
                return Err(DialectError::Invalid {
                    dialect: DIALECT,
                    message: format!(
                        "column `{column}` has type `{ty}`, which SQL Server refuses to name in \
                         ALTER COLUMN; its nullability cannot be changed",
                        column = column.shown(),
                        ty = ty.shown(),
                    ),
                });
            }
            let normalized = types::normalize(ty)?;
            one(format!(
                "ALTER TABLE {} ALTER COLUMN {} {}{} {}{};",
                qualified(&column.table)?,
                quote(column.name.raw())?,
                normalized.raw(),
                collate_clause(collation.as_ref())?,
                null_clause(*to_nullable),
                online(strategy)
            ))
        }
        // A computed column (#1174). The expression is the declaration's,
        // verbatim, inside the parentheses `AS` takes. Neither the expression
        // nor the persistence changes in place (`ALTER COLUMN … AS` is a
        // syntax error, measured on 17.0), so a change of either is this
        // statement after the drop below, and the column is re-added at the
        // end of its table.
        ColumnChange::AddComputedColumn {
            table,
            name,
            computed,
        } => one(format!(
            "ALTER TABLE {} ADD {};",
            qualified(table)?,
            computed_definition(name.raw(), computed)?
        )),
        // No default can stand on a computed column, so none is looked for.
        ColumnChange::DropComputedColumn { table, name, .. } => one(format!(
            "ALTER TABLE {} DROP COLUMN {};",
            qualified(table)?,
            quote(name.raw())?
        )),
        // Refused by validation: this model holds generated columns for
        // PostgreSQL only (DEC-1168.1). An error rather than a statement for a
        // column this engine was never given.
        ColumnChange::AlterColumnExpression { column, .. } => Err(DialectError::Invalid {
            dialect: DIALECT,
            message: format!(
                "{column} is a generated column, which this model holds for PostgreSQL only",
                column = column.shown()
            ),
        }),
        // PostgreSQL only (#1889): the differ refuses an identity change on
        // this engine, which has no `ALTER` for one.
        ColumnChange::AlterColumnNumbering { column, .. } => Err(DialectError::Invalid {
            dialect: DIALECT,
            message: format!(
                "{column}: a change to its identity has no ALTER on SQL Server, and this model \
                 changes one on PostgreSQL only",
                column = column.shown()
            ),
        }),
        ColumnChange::AlterColumnDefault {
            column, from, to, ..
        } => {
            let table = qualified(&column.table)?;
            let mut sql = String::new();
            if from.is_some() {
                sql.push_str(&drop_default_block(&column.table, column.name.raw())?);
                sql.push('\n');
            }
            if let Some(expr) = to {
                sql.push_str(&format!(
                    "ALTER TABLE {table} ADD CONSTRAINT {} DEFAULT ({}) FOR {};",
                    quote(default_constraint_name(&column.table, column.name.raw()).raw())?,
                    verbatim(expr),
                    quote(column.name.raw())?
                ));
            }
            let sql = sql.trim_end().to_owned();
            // Dropping used a variable, so the batch has to stand alone.
            Ok(vec![if from.is_some() {
                Statement::new(sql).own_batch()
            } else {
                Statement::new(sql)
            }])
        }
        // Deprecation is a fact about the declarations, not about the database.
        // Recording it as an extended property is a Phase 5 decision; until then
        // the honest output is nothing at all, rather than a statement that
        // pretends to do something.
        ColumnChange::SetColumnDeprecated { .. } => Ok(Vec::new()),
    }
}
