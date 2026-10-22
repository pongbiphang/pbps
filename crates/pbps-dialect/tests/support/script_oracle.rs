// Frozen a7352b65 script rendering for byte comparisons; test-only source.
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
        previous_own_batch = s.own_batch;
    }
    out
}
