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
            let moved = names.moved.get(table);
            // A row this plan writes cannot be spelled against an arbitrary
            // predicate: evaluating one needs every column of the row, and the
            // plan carries only the cells it sets. Unlike a foreign key, whose
            // columns *are* the constraint, so [`rows_after`] can build them.
            //
            // And a column this plan **retypes** takes the probe with it. The
            // conversion precedes this check in the projected prefix, so
            // the engine tests the converted value and a probe over the stored
            // one tests a different value. **Measured on 18.6**: a
            // `numeric(10,2)` holding `1.50` and `2.25`, converted to
            // `numeric(10,0)` and then given `CHECK (v = round(v))` — the
            // engine stores `2` and `2` and accepts the constraint, while
            // `WHERE NOT (v = round(v))` over the stored values counts **both
            // rows** and refuses the plan.
            //
            // A skip and not a projection: the predicate is arbitrary SQL that
            // names its columns itself, and supplying converted values would
            // mean rewriting that text by substitution — the thing this module
            // refuses to do. A skip and not a failing probe, which is how the
            // rename and added-column cases are handled two comments down:
            // those make the probe *fail to run*, and the runner says so by
            // name. A retype leaves the probe able to run and quietly wrong,
            // which is the one outcome no report can catch (DECISIONS 410).
            if moved.is_some_and(|m| m.has_writes()) || names.retypes_in(table) {
                return Ok(skip(
                    change,
                    "the check predicate cannot be evaluated over planned row writes or retyped columns",
                    unchecked,
                ));
            }
            if names.passes_a_name(table) {
                return Ok(skip(
                    change,
                    "the check predicate may spell a name this plan gives another column",
                    unchecked,
                ));
            }
            let mut sql = format!(
                // A CHECK rejects a row only when its predicate is FALSE;
                // UNKNOWN passes. `WHERE NOT (expr)` has exactly that
                // behaviour — measured, a row holding NULL is accepted by
                // `CHECK (v > 0)` and `WHERE NOT (v > 0)` counts none of them.
                //
                // The expression is not rewritten for renames: rewriting SQL
                // text by substitution is how a tool that promised never to
                // parse SQL starts parsing it badly. Such a probe fails to run
                // and is reported as unchecked, which is honest.
                //
                // An unqualified name in it binds under the write path the
                // emitted statement carries (DECISIONS 259) and this probe
                // does not, because a probe is one `SELECT` and a `SET` cannot
                // ride in one. A column reference needs no path and is the
                // common case; an unqualified *function* or table makes the
                // probe unchecked, and the runner says so.
                "SELECT {} AS n FROM {} WHERE NOT ({})",
                saturated_count("count(*)"),
                qualified(&stored)?,
                constraint.expression
            );
            // Minus the rows it deletes: they will not be there to violate
            // anything, and counting them refuses a plan that cleans up after
            // itself before tightening.
            if let Some(m) = moved
                && !m.deleted.is_empty()
            {
                let Some(key_column) = names.column(&table.column(&m.key_column)) else {
                    return Ok(skip(
                        change,
                        "the deleted row key does not exist before apply",
                        unchecked,
                    ));
                };
                let gone: BTreeSet<String> = m
                    .deleted
                    .iter()
                    .map(|k| value_literal(k.as_str()))
                    .collect();
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
        // The engine's own build checks every pair of rows under the table's
        // lock, and the rehearsal runs that build. A pairwise probe here would
        // be expensive and stale by apply, and the operator may be any the
        // engine accepts, so it is reported as unchecked (DEC-2448.3).
        ConstraintChange::SetExclusion { to: Some(_), .. } => Ok(skip(
            change,
            "an exclusion constraint's conflicts are found by the engine while it builds the \
             constraint, which the rehearsal runs",
            unchecked,
        )),
        ConstraintChange::SetExclusion { to: None, .. } => Ok(Vec::new()),
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
                "c",
                &Applies::AllRows,
                StoredConversions::Unchecked,
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
                &keys(constraint.columns.len()),
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
            let Some(rows) = rows_after(
                names,
                table,
                &pk.columns,
                "c",
                &Applies::AllRows,
                StoredConversions::Unchecked,
            )?
            else {
                return Ok(skip(
                    change,
                    "the planned primary-key values cannot be evaluated before apply",
                    unchecked,
                ));
            };
            let from = format!("(\n{rows}\n) AS r");
            // Both questions, because a primary key asks both: this engine
            // sets `NOT NULL` on every column of one itself — measured, which
            // is why a nullable primary key column is refused offline
            // (DECISIONS 266) — and then the key has to be unique.
            let mut out: Vec<Probe> = pk
                .columns
                .iter()
                .enumerate()
                .map(|(i, declared)| null_probe(&table.column(declared), &from, &format!("r.k{i}")))
                .collect();
            out.push(duplicate_probe(
                &from,
                &keys(pk.columns.len()),
                "rows that would collide under the new primary key",
            ));
            Ok(out)
        }
        ConstraintChange::AddForeignKey {
            table,
            name,
            constraint,
        } => {
            let probes = orphan_probe(names, table, name, constraint)?;
            if probes.is_empty() {
                return Ok(skip(
                    change,
                    "the planned foreign-key values cannot be evaluated before apply",
                    unchecked,
                ));
            }
            Ok(probes)
        }
        ConstraintChange::SetPrimaryKey { to: None, .. }
        | ConstraintChange::DropUnique { .. }
        | ConstraintChange::DropForeignKey { .. }
        | ConstraintChange::DropCheck { .. } => Ok(Vec::new()),
    }
}
