// Frozen a7352b65 script rendering for byte comparisons, plus the `after_vet`
// tail below; test-only source.
use pbps_dialect::{Dialect, Statement};
use pbps_model::SessionZone;

pub fn render(
    statements: &[Statement],
    dialect: &(impl Dialect + ?Sized),
    zone: &SessionZone,
) -> String {
    let Some(pins) = dialect
        .session_pins(zone)
        .filter(|_| !statements.is_empty())
    else {
        return render_batches(statements, dialect.batch_separator());
    };
    let prepared: Vec<_> = std::iter::once(Statement::new(pins).own_batch())
        .chain(statements.iter().cloned())
        .collect();
    format!(
        "-- Execute statements/batches in order, not as one query.\n\
         -- Session settings must take effect before the following SQL is parsed.\n\n{}",
        render_batches(&prepared, dialect.batch_separator())
    )
}

fn render_batches(statements: &[Statement], separator: Option<&str>) -> String {
    let mut out = String::new();
    let mut previous_own_batch = false;
    for (i, s) in statements.iter().enumerate() {
        if i > 0 {
            if let Some(sep) = separator
                && (s.own_batch || previous_own_batch)
            {
                out.push_str(sep);
                out.push('\n');
            }
            out.push('\n');
        }
        out.push_str(&s.sql);
        out.push('\n');
        // The one rendering added since the freeze: what runs after the vet
        // is shown where it runs (DEC-1635.8).
        if let Some(tail) = s
            .declared_type
            .as_ref()
            .and_then(|declared| declared.after_vet.as_deref())
        {
            out.push_str(
                "\n-- pbps vets the bound code of this declaration at this point, then runs:\n",
            );
            out.push_str(tail);
            out.push('\n');
        }
        previous_own_batch = s.own_batch;
    }
    out
}
