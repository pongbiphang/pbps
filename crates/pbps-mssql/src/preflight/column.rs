//! Probes the column family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight<'v, 'p: 'v>(
    change: &Change,
    column: &ColumnChange,
    names: &impl Deref<Target = AsStored<'v, 'p>>,
    unchecked: &mut Vec<Unchecked>,
) -> Result<Vec<Probe>, DialectError> {
    match column {
        ColumnChange::AddColumn {
            table,
            name,
            column,
            ..
        } if !column.nullable && !column.has_required_add_value_source() => {
            let Some(stored) = names.view().table(table) else {
                return Ok(Vec::new());
            };
            // The model's marker is lexical and deliberately conservative: it
            // answers "could this expression mean NULL?" by looking for the
            // word, and `NULLIF` is one of the words. **Measured on SQL Server
            // 2022**, a nonempty table takes
            // `ADD c int NOT NULL DEFAULT NULLIF(1, 2)` and every row reads
            // `1`, while `NULLIF(1, 1)` is refused — one word, both answers.
            // Counting the whole table on the word alone refuses the first.
            //
            // Keep the count only where this module can read the value without
            // running anything: no default, or the literal `NULL`. An
            // expression is the case DECISIONS 124 answers with no probe rather
            // than a guess; asking the engine `(expr) IS NULL` would run the
            // operator's expression before approval, the hazard in #274.
            if let Some(default) = match &column.value_source {
                ValueSource::Default(value) => Some(value.as_str()),
                ValueSource::None
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_)
                | ValueSource::Generated(_) => None,
            } {
                if !crate::rows::is_constant(default) {
                    return Ok(skip(
                        change,
                        "the default cannot be evaluated before apply",
                        unchecked,
                    ));
                }
                if !is_null_default(default) {
                    return Ok(Vec::new());
                }
            }
            Ok(vec![Probe::new(
                format!(
                    "rows that have no value for the new NOT NULL column {}",
                    table.column(name).shown()
                ),
                format!("SELECT COUNT(*) AS n FROM {};", qualified(&stored)?),
            )])
        }
        // The preceding writes may fill or remove stored NULLs. Using the
        // same relation as key probes also handles a tightening before those
        // writes, without copying the planner's sort predicate (DEC-1775.1).
        ColumnChange::AlterColumnNullability {
            column,
            to_nullable: false,
            ..
        } => {
            let Some(rows) = rows_after(
                names,
                &column.table,
                std::slice::from_ref(&column.name),
                names.moved.get(&column.table),
                "c",
                &Applies::AllRows,
            )?
            else {
                return Ok(skip(
                    change,
                    "the planned values of the column cannot be evaluated before apply",
                    unchecked,
                ));
            };
            Ok(vec![null_probe(
                column,
                &format!("(\n{rows}\n) AS r"),
                "r.k0",
            )?])
        }
        ColumnChange::AlterColumnType {
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            to_collation,
            ..
        } => {
            let Some(stored) = names.view().column(column) else {
                return Ok(skip(
                    change,
                    "the column does not exist before apply",
                    unchecked,
                ));
            };
            let mut out = Vec::new();
            // A type change folds a nullability change into itself (§12), so it
            // has to carry that change's probe too.
            if *from_nullable && !*to_nullable {
                out.push(null_probe(
                    column,
                    &qualified(&stored.table)?,
                    &quote(stored.name.raw())?,
                )?);
            }
            // A `timestamp` on either end is asked nothing. The engine refuses
            // the statement whatever the rows hold (4928 out of the type, 4927
            // into it), so there is no data question — and the probe that would
            // otherwise be built answers the wrong one confidently: measured,
            // `CONVERT(timestamp, 0xAB)` succeeds as an expression, so
            // `TRY_CONVERT` finds nothing wrong and the pre-flight line reads
            // as a pass. Classifying the change `Incompatible` is what stops
            // it; a probe beside that would only argue with it
            // (DECISIONS 283).
            let probes_capacity = types::change_risk(from, to).risk_class().is_some();
            if (probes_capacity || character_to_non_unicode(from, to)?)
                && !types::alter_column_is_refused(from)
                && !types::alter_column_is_refused(to)
            {
                out.extend(conversion_probe(
                    column,
                    &stored,
                    from,
                    to,
                    to_collation.as_ref(),
                    probes_capacity,
                )?);
            }
            Ok(out)
        }
        // A persisted computed column is computed over every stored row when
        // it is added, and an expression that fails on one (an overflow, a
        // division by zero) fails the statement. No probe evaluates it before
        // apply; a non-persisted one is computed on read and asks nothing
        // (#1174).
        ColumnChange::AddComputedColumn { computed, .. } if computed.persisted => Ok(skip(
            change,
            "a persisted computed column's values cannot be evaluated before apply",
            unchecked,
        )),
        ColumnChange::AddComputedColumn { .. } => Ok(Vec::new()),
        ColumnChange::AddColumn { .. }
        | ColumnChange::DropColumn { .. }
        | ColumnChange::RenameColumn { .. }
        | ColumnChange::AlterColumnNullability {
            to_nullable: true, ..
        }
        | ColumnChange::AlterColumnDefault { .. }
        | ColumnChange::AlterColumnExpression { .. }
        | ColumnChange::AlterColumnNumbering { .. }
        | ColumnChange::SetColumnDeprecated { .. }
        | ColumnChange::DropComputedColumn { .. } => Ok(Vec::new()),
    }
}
