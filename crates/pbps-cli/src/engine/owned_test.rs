//! What `pbps test` needs of an engine that owns its scratch server (#2798).
//!
//! The four command files that run a test (`plan_test::{execution, report,
//! runner}` and `test_scratch`) used to `match` on the driver to pick a label,
//! a run-record string, the server error codes that mean "infrastructure" and
//! the engine-build read. Those four answers are one [`OwnedTest`] per engine
//! that has them, selected once by [`super::require_owned_test`] before any
//! runtime is discovered, any record is written or any state is reconciled.
//! The commands carry the record and read its fields, so an engine with no
//! owned scratch is refused by name at that one place and cannot reach a
//! path that would borrow another engine's answers.
//!
//! Only the facts that need no per-run state are here, and the container the
//! scratch runs in is not one of them: it is the resolver's shared
//! [`ContainerSupport`], which this record carries and does not copy. The
//! session that provisions identities is the engine's [`TestEngine`], carried
//! the same way. Its grants, fixture and assertion steps still branch on the
//! driver in `plan_test::session` until they move behind it.

use crate::plan_test::session::{MssqlTest, PostgresTest, TestEngine};
use anyhow::Context as _;
use pbps_cli::resolver::server::scratch::container::{ContainerSupport, support};
use pbps_db::{Conn, Driver};
use std::{future::Future, pin::Pin};

/// A connected read that yields the engine build, borrowing the connection.
pub(crate) type BuildRead<'c> = Pin<Box<dyn Future<Output = anyhow::Result<String>> + 'c>>;

/// One engine's support for an owned test scratch.
///
/// Written out in full for each engine, with no default, so a new engine that
/// claims support owes an answer for every field.
pub(crate) struct OwnedTest {
    /// The container support the scratch server starts from, and through it
    /// the driver the server is reached through. It travels with the record so
    /// that a caller cannot pair one engine's support with another engine's
    /// container or driver.
    pub container: &'static ContainerSupport,
    /// The run's session on the owned server: its identities, administrator
    /// connection, incoming-hop preparation and comparison snapshot. It is
    /// chosen with the container so that a run cannot provision one engine's
    /// identities on another engine's server.
    pub session: &'static dyn TestEngine,
    /// What a `provision` phase calls the scratch.
    pub scratch_label: &'static str,
    /// The engine string of the durable run record. It is read back by later
    /// runs' recovery, so it is a stored value and never a display name.
    pub record_engine: &'static str,
    /// Whether a server-coded error is the engine or its host failing, by the
    /// engine's own numbering, so that the run could not look and the SQL is
    /// not blamed.
    pub is_fault_code: fn(&str) -> bool,
    /// The exact build the scratch server reports, recorded in the report so
    /// that two comparison databases of a re-pin can be told to agree.
    pub read_engine_build: for<'c> fn(&'c mut Conn) -> BuildRead<'c>,
}

/// The container support of an engine whose owned test needs one.
///
/// Evaluated at compile time, so that an engine that claims an owned test
/// without container support does not build.
const fn container_of(driver: Driver) -> &'static ContainerSupport {
    match support(driver) {
        Some(container) => container,
        None => panic!("an owned test scratch needs the engine's container support"),
    }
}

pub(super) static POSTGRES: OwnedTest = OwnedTest {
    container: container_of(Driver::Postgres),
    session: &PostgresTest,
    scratch_label: "owned PostgreSQL scratch",
    record_engine: "postgres",
    is_fault_code: postgres_fault_code,
    read_engine_build: postgres_engine_build,
};

pub(super) static MSSQL: OwnedTest = OwnedTest {
    container: container_of(Driver::Mssql),
    session: &MssqlTest,
    scratch_label: "owned SQL Server scratch",
    record_engine: "mssql",
    is_fault_code: pbps_mssql::test_scratch::infrastructure_fault,
    read_engine_build: mssql_engine_build,
};

/// The support an engine's profile offers, or the refusal that names it.
///
/// A refusal and not a skipped or passed test: a run that cannot happen has
/// not answered anything. `engine` is the name the refusal speaks of.
pub(super) fn require(
    engine: &str,
    support: Option<&'static OwnedTest>,
) -> anyhow::Result<&'static OwnedTest> {
    support.with_context(|| format!("{engine} does not support owned scratch for pbps test"))
}

fn postgres_fault_code(code: &str) -> bool {
    ["08", "53", "57", "58", "XX"]
        .iter()
        .any(|p| code.starts_with(p))
        || matches!(code, "25P03" | "25P04")
}

fn postgres_engine_build(conn: &mut Conn) -> BuildRead<'_> {
    Box::pin(async move {
        let rows = conn.query("SELECT version() AS version").await?;
        Ok(rows
            .first()
            .context("engine build is unreadable")?
            .try_get::<&str>("version")?
            .context("missing engine build")?
            .into())
    })
}

fn mssql_engine_build(conn: &mut Conn) -> BuildRead<'_> {
    Box::pin(async move { Ok(pbps_mssql::test_scratch::engine_build(conn).await?) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::profile::profile;

    const DRIVERS: [Driver; 2] = [Driver::Mssql, Driver::Postgres];

    #[test]
    fn a_missing_support_record_is_refused_by_the_engines_name() {
        let refusal = require("Third", None)
            .err()
            .expect("no support is a refusal")
            .to_string();
        assert_eq!(
            refusal,
            "Third does not support owned scratch for pbps test"
        );
    }

    #[test]
    fn a_support_record_is_admitted_as_it_is() {
        let admitted = require("PostgreSQL", Some(&POSTGRES)).unwrap();
        assert!(std::ptr::eq(admitted, &POSTGRES));
    }

    #[test]
    fn each_engine_keeps_the_label_and_record_string_it_had_before_the_move() {
        // The record string is stored in a run record that a later run reads
        // back, so a rename here would strand an old run's container.
        let (pg, ms) = (&POSTGRES, &MSSQL);
        assert_eq!(pg.scratch_label, "owned PostgreSQL scratch");
        assert_eq!(pg.record_engine, "postgres");
        assert_eq!(ms.scratch_label, "owned SQL Server scratch");
        assert_eq!(ms.record_engine, "mssql");
    }

    #[test]
    fn both_engines_are_supported_and_each_record_carries_its_own_container() {
        for driver in DRIVERS {
            let support =
                crate::engine::require_owned_test(driver).expect("both engines own a test scratch");
            assert_eq!(support.container.driver, driver);
            assert!(std::ptr::eq(
                support.container,
                pbps_cli::resolver::server::scratch::container::support(driver).unwrap()
            ));
            assert!(std::ptr::eq(
                support,
                profile(driver).owned_test.expect("the profile carries it")
            ));
        }
    }

    #[test]
    fn each_engine_record_carries_its_own_session_implementation() {
        use std::any::Any;
        let session = |record: &OwnedTest| -> &dyn Any { record.session };
        assert!(session(&POSTGRES).is::<PostgresTest>());
        assert!(session(&MSSQL).is::<MssqlTest>());
        // Negative: neither record answers with the other engine's session, so
        // one engine's identities are never provisioned on the other's server.
        assert!(!session(&POSTGRES).is::<MssqlTest>());
        assert!(!session(&MSSQL).is::<PostgresTest>());
    }

    #[test]
    fn the_profile_selects_the_session_of_the_engine_it_names() {
        use std::any::Any;
        let selected = |driver| -> &'static dyn Any {
            crate::engine::require_owned_test(driver).unwrap().session
        };
        assert!(selected(Driver::Postgres).is::<PostgresTest>());
        assert!(selected(Driver::Mssql).is::<MssqlTest>());
        assert!(!selected(Driver::Postgres).is::<MssqlTest>());
        assert!(!selected(Driver::Mssql).is::<PostgresTest>());
    }

    #[test]
    fn postgres_reads_its_infrastructure_faults_by_sqlstate_class() {
        for code in [
            "08006", "53200", "57014", "58030", "XX000", "25P03", "25P04",
        ] {
            assert!((POSTGRES.is_fault_code)(code), "{code}");
        }
        // The user's own SQL: syntax, a missing relation, a permission, a
        // serialization failure and an unrelated 25 class member.
        for code in ["42601", "42P01", "42501", "40001", "25P02", "23505"] {
            assert!(!(POSTGRES.is_fault_code)(code), "{code}");
        }
    }

    #[test]
    fn sql_server_reads_faults_by_number_and_does_not_borrow_sqlstate_prefixes() {
        for code in ["701", "823", "9002"] {
            assert!((MSSQL.is_fault_code)(code), "{code}");
        }
        // A PostgreSQL class prefix is a plain unknown number here, and the
        // user's permission error (229) is a finding.
        for code in ["08006", "53200", "229", "3906"] {
            assert!(!(MSSQL.is_fault_code)(code), "{code}");
        }
    }
}
