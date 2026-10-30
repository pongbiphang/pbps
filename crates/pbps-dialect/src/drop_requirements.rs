//! Shared guard transport and refusal; execution stays with each engine (DEC-2400.2).
use crate::{DialectError, Statement};
use pbps_model::{DropGuard, DropRequirement, PlanMode, escape::Escaped};

/// Prepared lowering must carry the guard too: callers can bypass `emit`.
pub fn emit(
    guard: Option<&DropGuard>,
    mode: PlanMode,
    dialect: &'static str,
    lower: impl FnOnce() -> Result<Vec<Statement>, DialectError>,
) -> Result<Vec<Statement>, DialectError> {
    let invalid = |message: &str| DialectError::Invalid {
        dialect,
        message: message.into(),
    };
    if guard.is_some() && mode == PlanMode::Staged {
        return Err(invalid(
            "guarded drops require a transactional plan; --staged is unsupported",
        ));
    }
    let mut statements = lower()?;
    if let Some(guard) = guard {
        let first = statements
            .first_mut()
            .ok_or_else(|| invalid("a guarded drop emitted no statement"))?;
        first.drop_guard = Some(guard.clone());
    }
    Ok(statements)
}

/// Missing, NULL, mistyped and negative evidence never mean zero violations.
pub fn check_count(
    count: Option<i64>,
    guard: &DropGuard,
    requirement: &DropRequirement,
) -> Result<(), String> {
    let count = count
        .filter(|n| *n >= 0)
        .ok_or("the row requirement returned no readable count; nothing was removed")?;
    if count > 0 {
        return Err(format!(
            "{count} row(s) of {} violate the row requirement `{}`; nothing was removed",
            guard.table.shown(),
            Escaped(&requirement.predicate)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn guard() -> DropGuard {
        DropGuard {
            table_uid: "t_000001".parse().unwrap(),
            table: "app.t".parse().unwrap(),
            detach_from: None,
            requirements: [(
                "c_000001".parse().unwrap(),
                DropRequirement {
                    table_uid: "t_000001".parse().unwrap(),
                    predicate: "x\n=1\u{1b}\u{202e}".into(),
                    required_at: "2026-10-30".into(),
                    operator: "reviewer".into(),
                },
            )]
            .into(),
        }
    }
    #[test]
    fn guard_transport_covers_prepared_lowering_and_refuses_staged_or_empty_emission() {
        let guard = guard();
        let out = emit(Some(&guard), PlanMode::Transactional, "mssql", || {
            Ok(vec![Statement::new("one"), Statement::new("two")])
        })
        .unwrap();
        assert_eq!(out[0].drop_guard.as_ref(), Some(&guard));
        assert!(out[1].drop_guard.is_none());
        assert!(
            emit(Some(&guard), PlanMode::Staged, "mssql", || panic!(
                "staged guard must refuse before lowering"
            ))
            .unwrap_err()
            .to_string()
            .contains("transactional plan")
        );
        assert!(
            emit(Some(&guard), PlanMode::Transactional, "mssql", || Ok(
                vec![]
            ))
            .unwrap_err()
            .to_string()
            .contains("no statement")
        );
        assert!(
            emit(None, PlanMode::Staged, "mssql", || Ok(vec![]))
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn only_readable_zero_counts_permit_removal_and_refusals_show_operands_once() {
        let g = guard();
        let r = g.requirements.values().next().unwrap();
        for n in [None, Some(-1)] {
            assert_eq!(
                check_count(n, &g, r).unwrap_err(),
                "the row requirement returned no readable count; nothing was removed"
            );
        }
        assert!(check_count(Some(0), &g, r).is_ok());
        assert_eq!(
            check_count(Some(2), &g, r).unwrap_err(),
            r"2 row(s) of app.t violate the row requirement `x\n=1\u{1b}\u{202e}`; nothing was removed"
        );
    }
}
