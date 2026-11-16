//! Emits the column family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, change: &Change, column: &ColumnChange) -> Sql {
    match column {
        // No `own_batch`, and that is the difference from T-SQL rather than an
        // omission: measured, `ALTER TABLE t ADD COLUMN c int; ALTER TABLE t
        // ADD CONSTRAINT ck CHECK (c > 0); CREATE INDEX ix ON t (c);` is
        // accepted as one batch here, because PostgreSQL analyses each
        // statement of a simple query when it reaches it. What it *does* read
        // up front is the whole batch's *lexis*, which is why
        // `standard_conforming_strings` is pinned by the framing and not here.
        ColumnChange::AddColumn {
            table,
            name,
            column,
            ..
        } => {
            // A `serial`'s sequence before the column and its ownership after,
            // in the column's own statement, so no staged checkpoint falls
            // between them (#1631). Logged: the differ refuses a `serial`
            // added to an `UNLOGGED` table, whose persistence this change does
            // not carry.
            let add = format!(
                "ALTER TABLE {} ADD COLUMN {};",
                qualified(table)?,
                column_definition(table, name.raw(), column)?
            );
            let body = match owned_sequence_statements(table, name.raw(), column, false)? {
                Some((create, owned, sequence)) => {
                    let owner = sequence_to_table_owner(table, &sequence)?;
                    vec![create, owner, add, owned]
                }
                None => vec![add],
            };
            Ok(vec![
                scoped_items(pg, &table.schema, body)?
                    .creating(Created::Column(table.clone(), name.clone())),
            ])
        }
        ColumnChange::DropColumn { column, .. } => one(
            pg,
            &column.table,
            format!(
                "ALTER TABLE {} DROP COLUMN {};",
                qualified(&column.table)?,
                quote(column.name.raw())?
            ),
        ),
        ColumnChange::RenameColumn {
            table, from, to, ..
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {};",
                qualified(table)?,
                quote(from.raw())?,
                quote(to.raw())?
            ),
        ),
        ColumnChange::AlterColumnType {
            column,
            to_collation: Some(collation),
            ..
        }
        | ColumnChange::AlterColumnType {
            column,
            from_collation: Some(collation),
            ..
        }
        | ColumnChange::AlterColumnNullability {
            column,
            collation: Some(collation),
            ..
        } => Err(no_column_collation(column.shown(), collation)),
        ColumnChange::AlterColumnType {
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            through_external_partitions,
            using,
            ..
        } => {
            let normalized = types::normalize(to)?;
            let was = types::normalize(from)?;
            if from.type_ref().is_some() || to.type_ref().is_some() {
                return Err(DialectError::InvalidVerbatim {
                    dialect: DIALECT,
                    message: format!(
                        "column `{}` cannot be changed from `{}` to `{}`: retyping a \
                         user-defined type reference is not supported. Add the new column, fill \
                         it in a declared step, and drop the old one.",
                        column.name.shown(),
                        was.shown(),
                        normalized.shown(),
                    ),
                });
            }
            // No `USING`, ever (ADR-0012 §5). Where the engine would need one,
            // the change is refused here with the clause named, rather than
            // carried to the server to fail there — or, worse, performed under
            // a cast pbps chose, which is a data transformation nobody
            // declared, nobody reviewed and nobody can find in git.
            //
            // The one exception is a conversion a human recorded for exactly
            // this change (`pbps convert`, DEC-2331.1): it is the declared,
            // reviewed transformation that section names, so the refusals
            // below that ask for one do not apply, and its expression is the
            // only text a `USING` is ever made from.
            //
            // Both ends normalized first, and that is not a formality: the
            // catalogue's families are keyed on the spelling the engine gives
            // back, so `varchar(10)` unnormalized is a type it does not know
            // and every change from one reads as `Incompatible` — a widening
            // refused for needing a clause it does not need.
            // An array's element change is supported where the element change
            // is safe, and nowhere else yet (#1167, DEC-1167.1): narrowing one
            // needs element-wise probes over NULL arrays, NULL elements,
            // bounds and shape, which nothing counts today.
            let verdict = change
                .value_change()
                .map(|values| pbps_dialect::Dialect::value_change(pg, &values, ""));
            if verdict.as_ref().is_some_and(|v| {
                v.needs_using == Some(pbps_dialect::ConversionRequired::ArrayElements)
            }) {
                return Err(invalid(format!(
                    "column `{}` cannot be changed from `{was}` to `{normalized}`: an array's \
                     element type is changed only where every element keeps its value, and \
                     this change narrows or converts them. Add the new column, fill it in a \
                     declared step, and drop the old one.",
                    column.name.shown(),
                    was = was.shown(),
                    normalized = normalized.shown(),
                )));
            }
            if verdict.as_ref().map_or_else(
                || {
                    using.is_none()
                        && types::change_risk(&was, &normalized)
                            == pbps_dialect::TypeChangeRisk::Incompatible
                },
                |v| v.needs_using == Some(pbps_dialect::ConversionRequired::Incompatible),
            ) {
                return Err(invalid(format!(
                    "column `{}` cannot be changed from `{}` to `{normalized}`: this engine \
                     refuses the conversion outright -- `column \"{}\" cannot be cast \
                     automatically` -- and its remedy is a `USING` clause, which pbps does not \
                     emit. A `USING` expression says what the data becomes, and that is a \
                     transformation to declare and review, not one for a tool to choose \
                     (ADR-0012 section 5). Record the conversion with `pbps convert {} --using -` \
                     (the expression is read from standard input) and plan again, or add the new \
                     column, fill it in a declared step, and drop the old one.",
                    column.name.shown(),
                    types::normalize(from)?.shown(),
                    column.name.shown(),
                    column.shown(),
                    normalized = normalized.shown(),
                )));
            }
            // The same refusal, for the same reason, on a change the risk
            // classes cannot say no to. Gaining or losing the time zone is a
            // conversion the applying session's `TimeZone` decides — measured,
            // `12:00` becomes `12:00:00+00` from a `UTC` session and
            // `17:00:00+00` from `America/New_York` — so two operators running
            // one approved plan store two different instants. `change_risk`
            // knows the shape and answers `Narrowing`, but `Narrowing` is a
            // risk a human clears at the gate, and what the human cleared was
            // the loss, not a zone that was never in the plan. The framing
            // pins the target's recorded zone for everything pbps applies
            // (DEC-1916.1), which makes the result *reproducible* — it does
            // not make it *declared*, and a silent reinterpretation of every
            // stored value in that zone is exactly the undeclared
            // transformation ADR-0012 §5 refuses. Whether the recorded zone
            // changes that answer is #1919.
            if verdict.as_ref().is_some_and(|v| {
                v.needs_using == Some(pbps_dialect::ConversionRequired::SessionTimeZone)
            }) {
                return Err(invalid(format!(
                    "column `{}` cannot be changed from `{was}` to `{normalized}`: the \
                     conversion gains or loses the time zone, and what each stored value \
                     becomes is then read from a session setting rather than from anything \
                     declared -- the same value converts to a different instant depending on \
                     the zone the applying session happens to hold. Say what the values mean \
                     instead: record the conversion with the zone written out in it \
                     (`pbps convert {} --using -`, `AT TIME ZONE \'...\'`), or add the new column, \
                     fill it in a declared step with the zone written out, and drop the old one.",
                    column.name.shown(),
                    column.shown(),
                    was = was.shown(),
                    normalized = normalized.shown(),
                )));
            }
            // One `ALTER TABLE` takes both subcommands (ADR-0011, Amendment 1),
            // and the nullability is restated only when it moves: unlike SQL
            // Server, a `TYPE` subcommand here leaves `NOT NULL` where it was,
            // so restating it always would put a line in plan.sql that changes
            // nothing.
            let mut parts = vec![format!(
                "ALTER COLUMN {} TYPE {normalized}{}",
                quote(column.name.raw())?,
                using
                    .as_ref()
                    .map(|expression| format!(" USING {expression}"))
                    .unwrap_or_default(),
                normalized = normalized.raw(),
            )];
            if from_nullable != to_nullable {
                parts.push(format!(
                    "ALTER COLUMN {} {}",
                    quote(column.name.raw())?,
                    if *to_nullable {
                        "DROP NOT NULL"
                    } else {
                        "SET NOT NULL"
                    }
                ));
            }
            let body = format!(
                "ALTER TABLE {} {};",
                qualified(&column.table)?,
                parts.join(", ")
            );
            let sql = if using.is_some() {
                // The vetter and row pre-flight bind on the empty path. A
                // schema-scoped ALTER could call code neither judged (DEC-2378.2).
                // Keep the pin in the emitted statement for rehearsal, plan
                // tests and standalone SQL replay too.
                let setup = "SET search_path = '';".to_owned();
                let mut statement = Statement::new(format!("{setup}\n{body}"));
                statement.execution = Some(pbps_dialect::StatementExecution {
                    commands: vec![setup, body],
                    cleanup: None,
                });
                vec![statement]
            } else {
                one(pg, &column.table, body)?
            };
            if !from_nullable && *to_nullable && *through_external_partitions {
                clear_external_partitions(&column.table, column.name.raw(), sql)
            } else {
                Ok(sql)
            }
        }
        // `ty` is carried for SQL Server, which restates the whole column
        // definition and reads an omitted `NULL` as nullable. This engine has a
        // subcommand for exactly this and needs no type, so the field is unused
        // here — deliberately, and not because it was missed.
        ColumnChange::AlterColumnNullability {
            column,
            to_nullable,
            through_external_partitions,
            ..
        } => {
            let sql = one(
                pg,
                &column.table,
                format!(
                    "ALTER TABLE {} ALTER COLUMN {} {};",
                    qualified(&column.table)?,
                    quote(column.name.raw())?,
                    if *to_nullable {
                        "DROP NOT NULL"
                    } else {
                        "SET NOT NULL"
                    }
                ),
            )?;
            if *to_nullable && *through_external_partitions {
                clear_external_partitions(&column.table, column.name.raw(), sql)
            } else {
                Ok(sql)
            }
        }
        // A default is not a named object here — it is a property of the column
        // — so there is nothing to drop by name and no generated constraint
        // name to guess. `SET DEFAULT` replaces whatever was there.
        ColumnChange::AlterColumnNumbering {
            column,
            ty,
            from,
            to,
            ..
        } => numbering(pg, column, ty, from, to),
        ColumnChange::AlterColumnDefault { column, to, .. } => one(
            pg,
            &column.table,
            match to {
                Some(expr) => format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {};",
                    qualified(&column.table)?,
                    quote(column.name.raw())?,
                    verbatim(expr)
                ),
                None => format!(
                    "ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT;",
                    qualified(&column.table)?,
                    quote(column.name.raw())?
                ),
            },
        ),
        // Recomputes every row under the new expression. PostgreSQL 17 and
        // later only: 16 has no in-place form, and the connected path refuses
        // a plan that needs one there rather than emitting a drop and re-add
        // that would take the column's dependents with it (DEC-1168.1).
        // Refused by validation: computed columns are SQL Server's (#1174),
        // and PostgreSQL's own are generated columns. An error rather than a
        // statement for a column this engine was never given.
        ColumnChange::AddComputedColumn { table, name, .. }
        | ColumnChange::DropComputedColumn { table, name, .. } => Err(DialectError::Invalid {
            dialect: crate::types::DIALECT,
            message: format!(
                "{} is a computed column, which this model holds for SQL Server only",
                table.column(name).shown()
            ),
        }),
        ColumnChange::AlterColumnExpression { column, to, .. } => one(
            pg,
            &column.table,
            format!(
                "ALTER TABLE {} ALTER COLUMN {} SET EXPRESSION AS ({});",
                qualified(&column.table)?,
                quote(column.name.raw())?,
                verbatim(to)
            ),
        ),
        // Deprecation is a fact about the declarations, not about the database.
        // `COMMENT ON` would make it one, and that is a decision with its own
        // round trip to design; until then the honest output is nothing at all
        // rather than a statement that pretends to do something.
        ColumnChange::SetColumnDeprecated { .. } => Ok(Vec::new()),
    }
}
