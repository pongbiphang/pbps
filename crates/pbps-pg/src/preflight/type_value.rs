//! Probes the type_value family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight<'v, 'p: 'v>(
    change: &Change,
    type_value: &TypeValueChange,
    names: &impl Deref<Target = AsStored<'v, 'p>>,
    unchecked: &mut Vec<Unchecked>,
    label_arrays: usize,
) -> Result<Vec<Probe>, DialectError> {
    match type_value {
        // The cast to the replacement refuses a label it does not have, naming
        // the row only at apply: count them now, by the labels as stored today
        // (DEC-1635.7). An array counts its elements.
        TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Retype { column, values, .. },
            ..
        } => {
            let Some(stored) = names.column(column) else {
                return Ok(skip(
                    change,
                    "the column does not exist before apply",
                    unchecked,
                ));
            };
            let table = qualified(&stored.table)?;
            let quoted = quote(stored.name.raw())?;
            let verdict = crate::Postgres::new().value_change(values, &quoted);
            let mut probes = Vec::new();
            let removed = match values.as_ref() {
                pbps_model::ValueChange::Cast { arrays, .. } => {
                    // Existing derived-array rebuilds retain their unchecked
                    // report; root array narrowing is refused by the differ.
                    if *arrays > 0 && verdict.risk != pbps_dialect::TypeChangeRisk::Safe {
                        unchecked.push(Unchecked::for_change(
                            change,
                            "the elements of an array are not checked against a narrower base",
                        ));
                    } else if let pbps_dialect::ValueProbe::Predicate { target, fails } =
                        verdict.probe
                    {
                        probes.push(Probe::new(
                            format!("values in {column} that cannot become {to}", column = column.shown(), to = target.shown()),
                            format!("SELECT {} AS n FROM {table}\n WHERE {quoted} IS NOT NULL AND ({fails});", saturated_count("count(*)")),
                        ));
                    } else if let pbps_dialect::ValueProbe::Unchecked(reason) = verdict.probe {
                        unchecked.push(Unchecked::for_change(change, reason));
                    }
                    return Ok(probes);
                }
                pbps_model::ValueChange::Relabel { removed } => removed,
            };
            if removed.is_empty() {
                return Ok(probes);
            }
            let levels = label_arrays;
            let labels = format!(
                "ARRAY[{}]::pg_catalog.text[]",
                removed
                    .iter()
                    .map(|l| value_literal(l))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            // One unnest per level of array in the stored values, so a
            // domain over `e[]` is read by its elements and not by the
            // array's text.
            let (what, sql) = if levels > 0 {
                let mut joins = String::new();
                let mut source = format!("t.{quoted}");
                for level in 1..=levels {
                    joins.push_str(&format!(
                        " CROSS JOIN LATERAL pg_catalog.unnest({source}) AS x{level}(v{level})"
                    ));
                    source = format!("x{level}.v{level}");
                }
                (
                    "elements",
                    format!(
                        "SELECT {} AS n FROM {table} AS t{joins} \
                         WHERE {source}::pg_catalog.text OPERATOR(pg_catalog.=) ANY ({labels});",
                        saturated_count("count(*)")
                    ),
                )
            } else {
                (
                    "rows",
                    format!(
                        "SELECT {} AS n FROM {table} WHERE {quoted}::pg_catalog.text \
                         OPERATOR(pg_catalog.=) ANY ({labels});",
                        saturated_count("count(*)")
                    ),
                )
            };
            probes.push(Probe::new(
                format!(
                    "{what} of {column} holding a label this plan removes ({})",
                    removed
                        .iter()
                        .map(|l| pbps_model::escape::Escaped(l).to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    column = column.shown(),
                ),
                sql,
            ));
            Ok(probes)
        }
        TypeValueChange::AddEnumValue { .. }
        | TypeValueChange::AlterDomain { .. }
        | TypeValueChange::RenameEnumValue { .. }
        | TypeValueChange::EnumRebuild { .. } => Ok(Vec::new()),
    }
}
