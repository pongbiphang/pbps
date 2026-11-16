//! Probes the constraint family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight<'v, 'p: 'v>(
    change: &Change,
    constraint: &ConstraintChange,
    names: &impl Deref<Target = AsStored<'v, 'p>>,
    unchecked: &mut Vec<Unchecked>,
) -> Result<Vec<Probe>, DialectError> {
    match constraint {
        ConstraintChange::AddCheck {
            table,
            name,
            constraint,
        } => {
            let Some(stored) = names.view().table(table) else {
                return Ok(skip(
                    change,
                    "the table does not exist before apply",
                    unchecked,
                ));
            };
            if names.passes_a_name(table) {
                return Ok(skip(
                    change,
                    "a column name in the check's table changes owner before apply",
                    unchecked,
                ));
            }
            // The rows the statement will meet, not the rows standing now.
            // Only preceding row changes contribute to this check's
            // projection (DECISIONS 174, DEC-1775.1).
            let moved = names.moved.get(table);
            // A row this plan writes cannot be spelled against an arbitrary
            // predicate: evaluating it needs every column of the row, and the
            // plan carries only the ones it sets. Unlike a foreign key, whose
            // columns *are* the constraint, so `rows_after` can build them.
            // No answer, as everywhere else one cannot be given.
            if moved.is_some_and(|m| m.has_writes()) {
                return Ok(skip(
                    change,
                    "the check predicate cannot be evaluated over planned row writes",
                    unchecked,
                ));
            }
            let mut sql = format!(
                // A CHECK rejects a row only when its predicate is FALSE;
                // UNKNOWN passes. `WHERE NOT (expr)` has exactly that
                // behaviour, so the count matches what the engine will refuse.
                //
                // The expression is not rewritten for renames: rewriting SQL
                // text by substitution is how a tool that promised never to
                // parse SQL starts parsing it badly. Such a probe fails to run
                // and is reported as unchecked, which is honest.
                "SELECT COUNT(*) AS n FROM {} WHERE NOT ({})",
                qualified(&stored)?,
                verbatim(&constraint.expression)
            );
            // Minus the rows it deletes: they will not be there to violate
            // anything, and counting them refused a plan that cleans up after
            // itself before tightening.
            if let Some(m) = moved
                && !m.deleted.is_empty()
            {
                let Some(key_column) = names.view().column(&table.column(&m.key_column)) else {
                    return Ok(skip(
                        change,
                        "the deleted row key does not exist before apply",
                        unchecked,
                    ));
                };
                let gone: BTreeSet<String> =
                    m.deleted.iter().map(|k| literal(k.as_str())).collect();
                sql.push_str(&format!(
                    " AND {} NOT IN ({})",
                    quote(key_column.name.raw())?,
                    gone.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
            sql.push(';');
            Ok(vec![Probe::new(
                format!(
                    "rows that violate the new check {name}",
                    name = name.shown(),
                ),
                sql,
            )])
        }
        ConstraintChange::AddUnique {
            table,
            name,
            constraint,
            ..
        } => {
            let Some(rows) = rows_after(
                names,
                table,
                &constraint.columns,
                names.moved.get(table),
                "c",
                &Applies::AllRows,
            )?
            else {
                return Ok(skip(
                    change,
                    "the planned unique-key values cannot be evaluated before apply",
                    unchecked,
                ));
            };
            Ok(vec![duplicate_probe(
                &format!("(\n{rows}\n) AS r"),
                &collated_keys(names, table, &constraint.columns)?,
                &format!(
                    "rows that would collide under the new unique constraint {name}",
                    name = name.shown(),
                ),
            )?])
        }
        ConstraintChange::SetPrimaryKey {
            table,
            to: Some(pk),
            ..
        } => {
            // The same relation the unique above is built over, and for the
            // same reason: a primary key follows these row changes, so the
            // rows it will meet are the ones the plan leaves (DECISIONS 175).
            // It subsumes 171's substitution of a column this plan adds —
            // `rows_after` spells one, and a derived table's column can be
            // grouped by where a bare constant cannot.
            let rows = rows_after(
                names,
                table,
                &pk.columns,
                names.moved.get(table),
                "c",
                &Applies::AllRows,
            )?;
            let Some(rows) = rows else {
                return Ok(skip(
                    change,
                    "the planned primary-key values cannot be evaluated before apply",
                    unchecked,
                ));
            };
            let from = format!("(\n{rows}\n) AS r");
            let mut out = Vec::new();
            for (i, declared) in pk.columns.iter().enumerate() {
                out.push(null_probe(
                    &table.column(declared),
                    &from,
                    &format!("r.k{i}"),
                )?);
            }
            out.push(duplicate_probe(
                &from,
                &collated_keys(names, table, &pk.columns)?,
                "rows that would collide under the new primary key",
            )?);
            Ok(out)
        }
        ConstraintChange::AddForeignKey {
            table,
            name,
            constraint,
        } => {
            // Both sides as the plan will leave them, not as they stand:
            // An `AddForeignKey` after the row changes of a plan that
            // supplies the missing parent rows, or repairs the orphaned
            // children, is a plan this constraint will accept (DECISIONS 151).
            let Some(child) = rows_after(
                names,
                table,
                &constraint.columns,
                names.moved.get(table),
                "c",
                &Applies::AllRows,
            )?
            else {
                return Ok(skip(
                    change,
                    "the planned child-key values cannot be evaluated before apply",
                    unchecked,
                ));
            };
            let Some(parent) = rows_after(
                names,
                &constraint.references_table,
                &constraint.references_columns,
                names.moved.get(&constraint.references_table),
                "p",
                &Applies::AllRows,
            )?
            else {
                return Ok(skip(
                    change,
                    "the planned parent-key values cannot be evaluated before apply",
                    unchecked,
                ));
            };

            // A row with any NULL in the key is exempt from the constraint
            // (MATCH SIMPLE, which is what SQL Server implements), so excluding
            // them is not leniency — it is the rule.
            let not_null: Vec<String> = (0..constraint.columns.len())
                .map(|i| format!("r.k{i} IS NOT NULL"))
                .collect();
            // Compared under the collation both sides will have, where this
            // plan changes either (#1175): the rebuilt key is checked by the
            // engine under the columns' new collation, which both sides share
            // once the plan has run (1757). Compared bare, two sides the plan
            // brings to one collation from two others were a collation
            // conflict, and a key moving from case-insensitive to sensitive
            // matched a child `a` to a parent `A` the rebuilt key refuses.
            let join: Vec<String> = constraint
                .columns
                .iter()
                .zip(&constraint.references_columns)
                .enumerate()
                .map(|(i, (child, parent))| {
                    let moved = names.recollated.get(&table.column(child)).or_else(|| {
                        names
                            .recollated
                            .get(&constraint.references_table.column(parent))
                    });
                    let collate = match moved {
                        Some(Some(to)) => crate::emit::collate_clause(Some(to))?,
                        Some(None) => " COLLATE DATABASE_DEFAULT".to_owned(),
                        None => String::new(),
                    };
                    Ok(format!("q.k{i}{collate} = r.k{i}{collate}"))
                })
                .collect::<Result<_, DialectError>>()?;

            Ok(vec![Probe::new(
                format!(
                    "rows with no matching parent for the new foreign key {name}",
                    name = name.shown(),
                ),
                format!(
                    "SELECT COUNT(*) AS n FROM (\n{child}\n) AS r\n WHERE {}\n   AND NOT EXISTS (SELECT 1 FROM (\n{parent}\n) AS q WHERE {});",
                    not_null.join("\n   AND "),
                    join.join(" AND ")
                ),
            )])
        }
        ConstraintChange::SetPrimaryKey { to: None, .. }
        | ConstraintChange::DropUnique { .. }
        | ConstraintChange::DropForeignKey { .. }
        | ConstraintChange::DropCheck { .. }
        | ConstraintChange::SetExclusion { .. } => Ok(Vec::new()),
    }
}
