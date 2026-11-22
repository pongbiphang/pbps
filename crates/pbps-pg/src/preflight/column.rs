//! Probes the column family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight<'v, 'p: 'v>(
    change: &Change,
    column: &ColumnChange,
    names: &impl Deref<Target = AsStored<'v, 'p>>,
    unchecked: &mut Vec<Unchecked>,
    sources: Option<&[ColumnRef]>,
) -> Result<Vec<Probe>, DialectError> {
    match column {
        // A new column the declarations require a value in, with nothing to
        // put there. Every existing row breaks it, so the count is the table's
        // own — and it is a count rather than a yes, because "4,213 rows"
        // is what tells an operator what backfilling this will cost.
        ColumnChange::AddColumn {
            table,
            name,
            column,
            ..
        // This engine's own identifier boundary, not the model's default
        // (SQL Server's): measured, a combining mark continues a PostgreSQL
        // name, and the model's own rule does not agree — `null\u{301}x()`
        // is a function call there and, read by the default rule, an
        // expression that "explicitly invokes NULL". Unpinnable today:
        // `constant_default`, just below, already screens every default this
        // distinction could move out of the probe it builds, so this is
        // defence-in-depth against that inner screen ever being relaxed, not
        // a change this arm's own output can prove (DECISIONS 433).
        } if !column.nullable
            && !column.has_required_add_value_source_with(crate::LEXICON.identifier_continues) =>
        {
            let Some(stored) = names.view().table(table) else {
                return Ok(Vec::new());
            };
            // The model's marker is lexical and deliberately conservative: it
            // answers "could this expression mean NULL?" by looking for the
            // word, and `NULLIF` is one of the words. **Measured on 18.6**, a
            // nonempty table takes
            // `ADD COLUMN c integer NOT NULL DEFAULT NULLIF(1, 2)` and every
            // row reads `1`, while `NULLIF(1, 1)` is `contains null values` —
            // one word, both answers. Counting the whole table on the word
            // alone refuses the first.
            //
            // The count is therefore kept only where the value is one this
            // module can read *without running anything*: a built-in type with
            // no default, or a default that is the literal `NULL`. An expression is the
            // case DECISIONS 124 already answers with no probe rather than a
            // guess, and the alternative is worse than silence — asking the
            // engine `(expr) IS NULL` would run the operator's own function
            // before the plan is approved, which is the shape of #274 and
            // something no probe in this crate does.
            // A generated column's value is its expression over each row, and
            // evaluating it here would run the operator's code before the plan
            // is approved, exactly as a default's would. Unchecked, not counted
            // as the whole table: its inputs may never be null, and the engine
            // refuses `contains null values` at `ADD` when they are
            // (DEC-1168.1).
            if match &column.value_source { ValueSource::Generated(_) => true, ValueSource::None | ValueSource::Default(_) | ValueSource::Identity(_) | ValueSource::Sequence(_) => false } {
                return Ok(skip(
                    change,
                    "a generated column's values cannot be evaluated before apply",
                    unchecked,
                ));
            }
            let Some(fill) = backfill_of(&AddedColumn::of(column)) else {
                return Ok(skip(change, "the column's fill cannot be evaluated before apply", unchecked));
            };
            if !crate::rows::unwrapped(&fill).eq_ignore_ascii_case("null") {
                return Ok(Vec::new());
            }
            Ok(vec![Probe::new(
                format!(
                    "rows that have no value for the new NOT NULL column {}",
                    table.column(name).shown()
                ),
                format!("SELECT {} AS n FROM {};", saturated_count("count(*)"), qualified(&stored)?),
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
            if let Some(sources) = sources {
                let (from, reads) = scalar_read(sources)?;
                return Ok(vec![count_probes::nulls(&COUNTS, column, &from, &reads)]);
            }
            let Some(rows) = rows_after(
                names,
                &column.table,
                std::slice::from_ref(&column.name),
                "c",
                &Applies::AllRows,
                StoredConversions::Unchecked,
            )?
            else {
                return Ok(skip(change, "the planned values of the column cannot be evaluated before apply", unchecked));
            };
            Ok(vec![count_probes::nulls(
                &COUNTS,
                column,
                &count_probes::relation(&rows),
                "r.k0",
            )])
        }
        ColumnChange::AlterColumnType {
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            using,
            ..
        } => {
            let Some(stored) = names.column(column) else {
                return Ok(skip(change, "the column does not exist before apply", unchecked));
            };
            if sources.is_some() && using.is_some() {
                return Ok(skip(change, "a membership conversion expression cannot be evaluated over selected stored columns before apply", unchecked));
            }
            let (table, quoted) = match sources {
                Some(sources) => scalar_read(sources)?,
                None => (qualified(&stored.table)?, quote(stored.name.raw())?),
            };
            let mut out = Vec::new();
            // A recorded conversion is evaluated over every stored row, and
            // the probe fails when the engine does: a value the expression
            // cannot convert refuses the plan by name before the first
            // statement runs (DEC-2331.1). Instead of the family's own probe
            // below, which asks whether a value survives the engine's cast
            // and is not the cast the expression performs. The count is
            // always zero: the query is the evaluation, and `count` of the
            // expression is what makes the engine compute it for each row
            // rather than plan it away.
            //
            // **What the engine stores is the expression assigned to the new
            // type**, so the query casts it to that exact type, typmod
            // included: `amount::bigint` into a `smallint` raises on 40000
            // and the bare expression does not (round 1 review). The cast is
            // explicit and the `ALTER` is an assignment, and they part on a
            // bounded string, where the cast truncates and the `ALTER`
            // refuses (see `types::cannot_become`). That target gets the
            // count of rows the assignment would refuse, beside the cast.
            if let Some(expression) = using {
                let material = match change.value_change() {
                    Some(values) => crate::Postgres::new().value_change(&values, &quoted).probe,
                    // A scalar/array crossing has no common leaf shape.
                    None => match crate::types::normalize(to) {
                        Ok(target) => pbps_dialect::ValueProbe::Using {
                            assignment_refuses: crate::types::assignment_refuses(&target, &format!("({expression})")),
                            target, expression: expression.clone(),
                        },
                        Err(_) => pbps_dialect::ValueProbe::Unchecked("the conversion's target type cannot be normalized"),
                    },
                };
                let pbps_dialect::ValueProbe::Using { target, assignment_refuses, .. } = material else {
                    if let pbps_dialect::ValueProbe::Unchecked(reason) = material {
                        unchecked.push(Unchecked::for_change(change, reason));
                    }
                    return Ok(out);
                };
                let value = format!("({expression})");
                out.push(
                    Probe::new(
                        format!(
                            "the conversion `USING {}` of {column}",
                            pbps_model::escape::Escaped(expression),
                            column = column.shown(),
                        ),
                        format!(
                            "SELECT (0 * count({converted}))::int AS n FROM {table};",
                            converted = conversion_value(expression, &target),
                        ),
                    )
                    .refusing_when_it_fails(),
                );
                if let Some(refused) = assignment_refuses {
                    out.push(Probe::new(
                        format!(
                            "values the conversion `USING {}` of {column} makes that {to} would refuse",
                            pbps_model::escape::Escaped(expression),
                            column = column.shown(),
                            to = target.shown(),
                        ),
                        format!(
                            "SELECT {} AS n FROM {table}\n WHERE {value} IS NOT NULL AND ({refused});",
                            saturated_count("count(*)")
                        ),
                    ));
                }
                // The converted value is what `NOT NULL` meets, so the NULLs
                // counted are the expression's, not the stored column's: a
                // `coalesce` that fills them is a valid plan (round 1
                // review). Read through the same expression, so a row it
                // cannot convert fails here by the same name.
                //
                // Whenever the target is `NOT NULL`, **not only when this
                // plan tightens it** (round 4 review). A column that is
                // already required holds no NULL, but `nullif(amount, '')::
                // integer` makes one from a stored value, the engine refuses
                // it, and the `count` above ignores a NULL result. "The stored
                // column already satisfied it" is true of the old value and
                // says nothing about the new one.
                if !*to_nullable {
                    out.push(
                        count_probes::nulls(&COUNTS, column, &table, &value).refusing_when_it_fails(),
                    );
                }
                return Ok(out);
            }
            // One `ALTER TABLE` takes both subcommands (ADR-0011 Amendment 1),
            // so a type change that also tightens the column carries the other
            // change's probe too.
            if *from_nullable && !*to_nullable {
                out.push(count_probes::nulls(&COUNTS, column, &table, &quoted));
            }
            let material = match change.value_change() {
                Some(values) => crate::Postgres::new().value_change(&values, &quoted).probe,
                // The full types retain the scalar/array distinction here.
                None => match (crate::types::normalize(from), crate::types::normalize(to)) {
                    (Ok(from), Ok(to)) => crate::types::cannot_become(&from, &to, &quoted)
                        .map_or(pbps_dialect::ValueProbe::None, |fails| pbps_dialect::ValueProbe::Predicate { target: to, fails }),
                    _ => pbps_dialect::ValueProbe::Unchecked("the conversion types cannot be normalized"),
                },
            };
            if let pbps_dialect::ValueProbe::Unchecked(reason) = &material {
                unchecked.push(Unchecked::for_change(change, *reason));
            }
            if let pbps_dialect::ValueProbe::Predicate { target: to, fails } = material {
                out.push(Probe::new(
                    format!(
                        "values in {column} that cannot become {to}",
                        column = column.shown(),
                        to = to.shown(),
                    ),
                    // The NULLs are excluded here and counted by the probe
                    // above where there is one: a NULL converts to a NULL
                    // whatever the target, and counting it under "cannot
                    // become" would name the wrong problem.
                    format!(
                        "SELECT {} AS n FROM {table}\n WHERE {quoted} IS NOT NULL AND ({fails});",
                        saturated_count("count(*)")
                    ),
                ));
            }
            Ok(out)
        }
        // `SET EXPRESSION` rewrites every stored value, and a NOT NULL, a
        // check or a unique index over the column can refuse what it
        // computes. Evaluating the new expression here would run the
        // operator's code before approval, as a generated column's add would
        // (DEC-1168.1). The plan does not say what reads the column, so the
        // change is reported whatever does: absent from the report would read
        // as checked (#1318).
        ColumnChange::AlterColumnExpression { .. } => Ok(skip(
            change,
            "a generated column's recomputed values cannot be evaluated before apply",
            unchecked,
        )),
        // A kept source whose bounds this plan moves (#1921, DEC-1921.1):
        // measured on 16 and 18, the engine refuses a bound the counter is
        // already past ("RESTART value (10) cannot be greater than MAXVALUE
        // (5)"), mid-apply. Counted here: the sequence's counter outside the
        // bounds the change gives it, or a counter this session cannot read.
        // The counter the engine checks is the stored `last_value`, called
        // or not: `setval(…, false)`, which a source put on by this tool
        // leaves, sets it with no call (#2013 review). `pg_sequence_last_value`
        // answers NULL for a counter not yet called, so it is read from the
        // sequence itself, through `query_to_xml` since the probe is one
        // query and the sequence is found at run time. A sequence this
        // session may not `SELECT` is counted as in the way, never read as
        // inside the bounds (DECISIONS 335); the `CASE` keeps the read from
        // running, and failing, on one it may not.
        ColumnChange::AlterColumnNumbering {
            column, ty, from, to, ..
        } if moved_bounds(from, to).is_some() => {
            let Some((increment, options)) = moved_bounds(from, to) else {
                return Ok(Vec::new());
            };
            let Some(bounds) = crate::types::sequence_range(ty, increment, &options) else {
                return Ok(Vec::new());
            };
            let Some(stored) = names.column(column) else {
                return Ok(skip(change, "the column does not exist before apply", unchecked));
            };
            let counter = "(pg_catalog.xpath('/row/last_value/text()', \
                 pg_catalog.query_to_xml(pg_catalog.format('SELECT last_value FROM %s', \
                 q.seqrelid::pg_catalog.regclass), false, true, '')))[1]::text::numeric";
            Ok(vec![Probe::new(
                format!(
                    "the counter of {column}'s sequence outside the bounds {}..={} this plan \
                     gives it, or not readable",
                    bounds.start(),
                    bounds.end(),
                    column = column.shown(),
                ),
                format!(
                    "SELECT {} AS n FROM pg_catalog.pg_sequence q \
                     WHERE q.seqrelid OPERATOR(pg_catalog.=) \
                     pg_catalog.pg_get_serial_sequence({}, {})::pg_catalog.regclass \
                     AND CASE WHEN NOT pg_catalog.has_sequence_privilege(q.seqrelid, 'SELECT') \
                     THEN true ELSE NOT ({counter} OPERATOR(pg_catalog.>=) {} \
                     AND {counter} OPERATOR(pg_catalog.<=) {}) END;",
                    saturated_count("count(*)"),
                    value_literal(&qualified(&stored.table)?),
                    value_literal(stored.name.raw()),
                    bounds.start(),
                    bounds.end()
                ),
            )])
        }
        // A source put on a column that holds values continues past them
        // (DEC-1889.1), and a value too near the type's bound leaves no
        // number to continue at: the block's `setval` is then refused
        // mid-apply. Counted here, the values that leave no room.
        ColumnChange::AlterColumnNumbering {
            column, ty, from, to, ..
        } if pbps_model::NumberingKind::installed(from, to) => {
            let increment = match to {
                ValueSource::Identity(id) => id.increment,
                ValueSource::Sequence(sequence) => sequence.increment,
                ValueSource::None | ValueSource::Default(_) | ValueSource::Generated(_) => {
                    return Ok(Vec::new());
                }
            };
            // Up to the bound the new source declares, where it sets one
            // (#1921), else the type's.
            let options = match to {
                ValueSource::Identity(id) => id.options,
                ValueSource::Sequence(sequence) => sequence.options,
                ValueSource::None | ValueSource::Default(_) | ValueSource::Generated(_) => {
                    return Ok(Vec::new());
                }
            };
            // One that cycles continues at its other bound instead, as
            // `nextval` would, and nothing is in its way (#2013 review).
            if options.cycle {
                return Ok(Vec::new());
            }
            let Some(held) = crate::types::identity_range(ty) else {
                return Ok(Vec::new());
            };
            let bounds = options.min.unwrap_or(*held.start())..=options.max.unwrap_or(*held.end());
            let Some(stored) = names.column(column) else {
                return Ok(skip(change, "the column does not exist before apply", unchecked));
            };
            // Read before a retype this plan makes first, a value is not yet
            // the one the numbering continues past (#1930 review).
            if names.retyped.contains_key(column) {
                return Ok(skip(
                    change,
                    "the column's values are converted by this plan before numbering reads them",
                    unchecked,
                ));
            }
            // A conversion that counts the same way carries the old source's
            // counter too: the block continues at its next number, which an
            // exhausted counter has past the new source's bound, and `setval`
            // then refuses it mid-apply (#2005). Counted in `numeric`, as the
            // block computes it; a counter this login cannot read counts too,
            // as an unreadable counter does for a bound moved in place.
            let carried = carried_counter(from, to);
            let mut probes = Vec::new();
            if carried.is_some() {
                let read = |field: &str| {
                    format!(
                        "(pg_catalog.xpath('/row/{field}/text()', \
                         pg_catalog.query_to_xml(pg_catalog.format('SELECT {field} FROM %s', \
                         q.seqrelid::pg_catalog.regclass), false, true, '')))[1]::text"
                    )
                };
                let next = format!(
                    "CASE WHEN {}::pg_catalog.bool THEN {}::pg_catalog.numeric \
                     OPERATOR(pg_catalog.+) q.seqincrement ELSE {}::pg_catalog.numeric END",
                    read("is_called"),
                    read("last_value"),
                    read("last_value")
                );
                let (operator, bound) = if increment < 0 {
                    ("<", *bounds.start())
                } else {
                    (">", *bounds.end())
                };
                probes.push(Probe::new(
                    format!(
                        "the counter {column}'s old source carries past the bound {bound} of \
                         its new one, or not readable",
                        column = column.shown(),
                    ),
                    format!(
                        "SELECT {} AS n FROM pg_catalog.pg_sequence q \
                         WHERE q.seqrelid OPERATOR(pg_catalog.=) \
                         pg_catalog.pg_get_serial_sequence({}, {})::pg_catalog.regclass \
                         AND CASE WHEN NOT pg_catalog.has_sequence_privilege(q.seqrelid, 'SELECT') \
                         THEN true ELSE {next} OPERATOR(pg_catalog.{operator}) \
                         {bound}::pg_catalog.numeric END;",
                        saturated_count("count(*)"),
                        value_literal(&qualified(&stored.table)?),
                        value_literal(stored.name.raw()),
                    ),
                ));
            }
            // Nor, where the plan deletes rows of the table, are the values
            // stored now all the ones it reads (#1930 review). The counter
            // is not a row, so its probe stands.
            if names.deleted_from.contains(&column.table) {
                probes.extend(skip(
                    change,
                    "this plan deletes rows of the table, which may be gone before numbering reads it",
                    unchecked,
                ));
                return Ok(probes);
            }
            // The last value from which one more step stays in the type.
            let (operator, last) = if increment < 0 {
                ("<", i128::from(*bounds.start()) - i128::from(increment))
            } else {
                (">", i128::from(*bounds.end()) - i128::from(increment))
            };
            probes.push(Probe::new(
                format!(
                    "values in {column} too near its sequence's bound for numbering to \
                     continue past them",
                    column = column.shown(),
                ),
                format!(
                    "SELECT {} AS n FROM {} WHERE {} OPERATOR(pg_catalog.{operator}) \
                     {last}::pg_catalog.numeric;",
                    saturated_count("count(*)"),
                    qualified(&stored.table)?,
                    quote(stored.name.raw())?
                ),
            ));
            Ok(probes)
        }
        ColumnChange::AddColumn { .. } | ColumnChange::DropColumn { .. } | ColumnChange::RenameColumn { .. } | ColumnChange::AlterColumnNullability {
            to_nullable: true, ..
        } | ColumnChange::AlterColumnDefault { .. } | ColumnChange::AlterColumnNumbering { .. } | ColumnChange::SetColumnDeprecated { .. } | ColumnChange::AddComputedColumn { .. } | ColumnChange::DropComputedColumn { .. } => Ok(Vec::new()),
    }
}
