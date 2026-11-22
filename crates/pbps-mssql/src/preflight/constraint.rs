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
            let from = qualified(&stored)?;
            // Minus the rows it deletes: they will not be there to violate
            // anything, and counting them refused a plan that cleans up after
            // itself before tightening.
            let mut gone = None;
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
                let literals: BTreeSet<String> =
                    m.deleted.iter().map(|k| literal(k.as_str())).collect();
                gone = Some((quote(key_column.name.raw())?, literals));
            }
            Ok(vec![count_probes::check_violations(
                &COUNTS,
                name,
                &from,
                &verbatim(&constraint.expression),
                gone.as_ref().map(|(key, literals)| Gone { key, literals }),
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
            Ok(vec![count_probes::duplicates(
                &COUNTS,
                &count_probes::relation(&rows),
                &collated_keys(names, table, &constraint.columns)?,
                &format!(
                    "rows that would collide under the new unique constraint {name}",
                    name = name.shown(),
                ),
            )])
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
            let from = count_probes::relation(&rows);
            let mut out = Vec::new();
            for (i, declared) in pk.columns.iter().enumerate() {
                out.push(count_probes::nulls(
                    &COUNTS,
                    &table.column(declared),
                    &from,
                    &format!("r.k{i}"),
                ));
            }
            out.push(count_probes::duplicates(
                &COUNTS,
                &from,
                &collated_keys(names, table, &pk.columns)?,
                "rows that would collide under the new primary key",
            ));
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
                // A row with any NULL in the key is exempt from the
                // constraint (MATCH SIMPLE, which is what SQL Server
                // implements), and `orphaned` leaves those out.
                format!(
                    "SELECT {} AS n {};",
                    COUNTS.rows,
                    count_probes::orphaned(&child, &parent, constraint.columns.len(), &join)
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
