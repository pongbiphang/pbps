use super::*;
use pbps_db::{DbError, Driver, LedgerError, catalog::RowsError};
use pbps_model::TableName;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
enum Wrapper {
    Direct,
    Boxed,
    Ledger,
    Rows,
}
const WRAPPERS: [Wrapper; 4] = [
    Wrapper::Direct,
    Wrapper::Boxed,
    Wrapper::Ledger,
    Wrapper::Rows,
];

fn wrap(error: DbError, wrapper: Wrapper, context: bool) -> anyhow::Error {
    let error = if context {
        error.context("database context")
    } else {
        error
    };
    let error: anyhow::Error = match wrapper {
        Wrapper::Direct => error.into(),
        Wrapper::Boxed => anyhow::Error::new(Box::new(error)),
        Wrapper::Ledger => LedgerError::Db(error).into(),
        Wrapper::Rows => RowsError::Read {
            table: TableName::new("app", "orders"),
            source: Box::new(error),
        }
        .into(),
    };
    error.context("phase context")
}

fn coded(code: Option<&str>) -> DbError {
    DbError::Driver {
        message: "original database cause".into(),
        code: code.map(str::to_owned),
        diagnostic: None,
    }
}

fn assert_report(driver: Driver, error: anyhow::Error, unknown: bool) {
    let cause = format!("{error:#}");
    let mut phase = Phase::on(driver, "candidate_plan", "candidate");
    phase.record(Instant::now(), "test.plan", &Err::<(), _>(error));
    assert_eq!(matches!(phase.outcome, Status::Unanswerable), unknown);
    assert!(matches!(phase.outcome, Status::Failed) || unknown);
    assert_eq!(phase.findings[0].message, cause);
    let mut data = TestData::new(false);
    data.run_id = Some("owned-run".into());
    data.cleanup_confirmed = true;
    data.phases.push(phase);
    let report = data.finish();
    let expected = if unknown { "unanswerable" } else { "findings" };
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["result"], expected);
    assert_eq!(
        json["data"]["phases"][0]["outcome"],
        if unknown { "unanswerable" } else { "failed" }
    );
    assert_eq!(json["findings"][0]["message"], cause);
    assert_eq!(json["data"]["cleanup_confirmed"], true);
    let text = crate::output::human(&report.findings);
    assert!(text.contains(&cause), "{text}");
    let summary = crate::output::human(&report.data.as_ref().unwrap().summary(true));
    assert!(summary.contains(if unknown { "unanswerable" } else { "failed" }));
    assert!(summary.contains("cleanup confirmed: true"));
}

#[test]
fn postgres_infrastructure_codes_have_the_same_verdict_through_every_wrapper() {
    // Each class-57 member and each connection-ending class-25 timeout is
    // explicit here, so broadening one class cannot hide a missing member.
    for code in [
        "08006", "53300", "58030", "XX000", "57000", "57014", "57P01", "57P02", "57P03", "57P04",
        "57P05", "25P03", "25P04",
    ] {
        assert!(engine_fault(Driver::Postgres, &coded(Some(code))), "{code}");
        for wrapper in WRAPPERS {
            for context in [false, true] {
                let error = wrap(coded(Some(code)), wrapper, context);
                assert!(
                    unanswerable(Some(Driver::Postgres), &error),
                    "{code}, {wrapper:?}, context={context}: {error:#}"
                );
                assert_report(Driver::Postgres, error, true);
            }
        }
    }
    // Class 25 also contains ordinary statement/transaction refusals.
    for code in [
        "25001", "25006", "25P02", "42P01", "42501", "23505", "55P03",
    ] {
        assert!(
            !engine_fault(Driver::Postgres, &coded(Some(code))),
            "{code}"
        );
        for wrapper in WRAPPERS {
            for context in [false, true] {
                assert_report(
                    Driver::Postgres,
                    wrap(coded(Some(code)), wrapper, context),
                    false,
                );
            }
        }
    }
}

#[test]
fn transport_and_refusal_verdicts_survive_wrappers_on_both_engines() {
    for driver in [Driver::Postgres, Driver::Mssql] {
        for wrapper in WRAPPERS {
            for context in [false, true] {
                for error in [
                    coded(None),
                    DbError::Connect {
                        addr: "unreachable".into(),
                        reason: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
                    },
                    DbError::DeadlineExceeded("database deadline expired"),
                ] {
                    assert_report(driver, wrap(error, wrapper, context), true);
                }
                assert_report(
                    driver,
                    wrap(
                        DbError::Refused("row requirement failed".into()),
                        wrapper,
                        context,
                    ),
                    false,
                );
                // The engine decides what the same digits mean, after unwrapping.
                assert_report(
                    driver,
                    wrap(coded(Some("53300")), wrapper, context),
                    driver == Driver::Postgres,
                );
                assert_report(driver, wrap(coded(Some("42501")), wrapper, context), false);
            }
        }
    }
}

#[test]
fn non_database_ledger_failures_remain_findings() {
    for error in [
        LedgerError::NotInitialized,
        LedgerError::BadEntry {
            id: 7,
            message: "invalid recorded state".into(),
        },
        LedgerError::Locked(pbps_db::LockInfo {
            locked_by: "other deployer".into(),
            locked_at: "now".into(),
            session_lookup: None,
        }),
    ] {
        assert_report(
            Driver::Postgres,
            anyhow::Error::new(error).context("ledger"),
            false,
        );
    }
}

#[test]
fn an_offline_phase_does_not_guess_the_engine_of_a_wrapped_code() {
    for wrapper in WRAPPERS {
        let mut phase = Phase::offline("input", "offline");
        phase.record(
            Instant::now(),
            "test.input",
            &Err::<(), _>(wrap(coded(Some("57P01")), wrapper, true)),
        );
        assert!(matches!(phase.outcome, Status::Failed));
    }
}
