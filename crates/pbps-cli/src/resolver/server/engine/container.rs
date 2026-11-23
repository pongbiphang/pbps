//! What an engine needs to run its server in a container pbps owns (#2798).
//!
//! The runner (`scratch::container`), the recovery of a killed run's leftovers
//! (`container::recovery`) and the keeper (`engine::keeper`) used to `match` on
//! the driver each time they needed one of these facts: the label written to
//! the container, its port, the variable that carries the password, how the
//! password is derived, how the loopback connection is spelled, how readiness
//! is judged and how the keeper session is marked and read. They are the fields
//! of one [`ContainerSupport`] per engine, written out in full with no default,
//! so a new engine that claims support owes an answer for every one of them and
//! the compiler lists what it owes.
//!
//! The runner and recovery are the two readers of one table: [`support`] picks
//! an entry from a driver for a run that is starting, and [`by_label`] picks the
//! same entry from the label a container was stamped with, for a leftover found
//! later. Everything that is not an engine's fact stays with them: the runtime
//! inspection, the loops, the deadlines, the journal and the one retry of a
//! crashed start.
//!
//! Compiled, fixed data. Nothing here is loaded, registered at run time or
//! serialized: the label is the one stored value, and it is the string the
//! containers already carry.

use super::keeper::between_reads;
use pbps_db::resolver::Kept;
use pbps_db::{Conn, DbError, Driver};
use std::{fmt, future::Future, pin::Pin};

/// A connected read or write of the keeper, borrowing what it uses.
pub(crate) type Pending<'a, T> = Pin<Box<dyn Future<Output = Result<T, DbError>> + Send + 'a>>;

/// How a started container's server is waited for.
///
/// A closed set of strategies and not a driver: the runner reads the strategy,
/// so an engine that behaves like an existing one names it, and only an engine
/// that needs a third way adds a variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Readiness {
    /// Poll `query` over a connection until it answers, within the run's own
    /// deadline. A container that exited is not diagnosed; the deadline ends it.
    Polled { query: &'static str },
    /// Poll, and when the answer does not come read the runtime: a container
    /// that exited with a dump in its log is a crashed start, the one failure
    /// that earns a second start (DEC-2762.1).
    ExitDiagnosed,
}

/// One engine's support for a scratch server in a container pbps owns.
pub struct ContainerSupport {
    /// The driver the server is reached through. It travels with the support so
    /// that a caller cannot pair one engine's support with another's driver.
    pub driver: Driver,
    /// What a note about this engine's server calls it.
    pub(crate) name: &'static str,
    /// The value of the `pbps.resolver.engine` label, written to every
    /// container and read back by recovery. A stored value and never a display
    /// name: a rename strands the containers already labelled.
    pub(crate) label: &'static str,
    /// The port the server listens on inside the container.
    pub(crate) port: u16,
    /// The environment variable that carries the superuser's password into the
    /// container. Recovery reads the password back from it.
    pub(crate) password_env: &'static str,
    /// The password the server is started with, derived from the entropy pbps
    /// generated. Used once, at launch: launch, connection and redaction then
    /// all take its result, and recovery takes the stored result as it is.
    pub(crate) password: fn(&str) -> String,
    /// The `--env` values of the launch, for the password [`Self::password`]
    /// returned.
    pub(crate) environment: fn(&str) -> Vec<String>,
    /// The superuser connection through the loopback `port` the runtime
    /// published, for a password [`Self::password`] returned.
    pub(crate) loopback: fn(u16, &str) -> String,
    /// How readiness is judged.
    pub(crate) readiness: Readiness,
    /// Holds the run's session in its own server and marks that it connected:
    /// the session first, the persistent mark once it exists. Takes the
    /// session and the run's token.
    pub(crate) hold_keeper: for<'a> fn(&'a mut Conn, &'a str) -> Pending<'a, ()>,
    /// What the server at a connection shows of a run's keeper: takes the
    /// connection, the reader's own name and the keeper's token. `None` when
    /// the reader cannot see itself, which proves nothing about the keeper.
    pub(crate) read_keeper: for<'a> fn(&'a str, &'a str, &'a str) -> Pending<'a, Option<Kept>>,
}

impl fmt::Debug for ContainerSupport {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("ContainerSupport")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// What one start of a container is given, from one derivation of the secret.
pub(crate) struct Launch {
    /// The password the server is started with, and the one the connection
    /// and every redaction use.
    pub secret: String,
    /// The `--env` values that carry it.
    pub environment: Vec<String>,
}

impl ContainerSupport {
    /// The launch for `generated`, the entropy pbps drew for this start.
    ///
    /// The one place the password is derived, so that the container, the
    /// connection and the redaction of a diagnostic cannot disagree about it.
    pub(crate) fn launch(&self, generated: &str) -> Launch {
        let secret = (self.password)(generated);
        Launch {
            environment: (self.environment)(&secret),
            secret,
        }
    }

    /// The `NAME=` prefix of the password's line in a container's environment.
    pub(crate) fn password_line_prefix(&self) -> String {
        format!("{}=", self.password_env)
    }
}

static POSTGRES: ContainerSupport = ContainerSupport {
    driver: Driver::Postgres,
    name: "PostgreSQL",
    label: "postgres",
    port: 5432,
    password_env: "POSTGRES_PASSWORD",
    // The generated hexadecimal string meets the image's own rules as it is.
    password: |generated| generated.to_owned(),
    environment: |secret| vec![format!("POSTGRES_PASSWORD={secret}")],
    loopback: |port, secret| {
        format!(
            "host=127.0.0.1 port={port} user=postgres password={secret} dbname=postgres \
             sslmode=disable"
        )
    },
    readiness: Readiness::Polled {
        query: "SELECT 1 AS ok",
    },
    hold_keeper: postgres_hold_keeper,
    read_keeper: postgres_read_keeper,
};

static MSSQL: ContainerSupport = ContainerSupport {
    driver: Driver::Mssql,
    name: "SQL Server",
    label: "mssql",
    port: 1433,
    password_env: "MSSQL_SA_PASSWORD",
    // SQL Server's password policy needs more than the shared hex entropy.
    password: pbps_mssql::resolver::scratch::password,
    environment: |secret| {
        vec![
            // The image refuses to start without the licence accepted.
            "ACCEPT_EULA=Y".into(),
            format!("MSSQL_SA_PASSWORD={secret}"),
        ]
    },
    loopback: |port, secret| {
        format!(
            "Server=127.0.0.1,{port};Database=master;User \
             Id=sa;Password={secret};TrustServerCertificate=true"
        )
    },
    readiness: Readiness::ExitDiagnosed,
    hold_keeper: mssql_hold_keeper,
    read_keeper: mssql_read_keeper,
};

/// Every engine that has support. [`by_label`] reads it, and [`support`] must
/// agree with it (pinned by a test), so that a run's driver and a leftover's
/// label reach the same entry.
static TABLE: [&ContainerSupport; 2] = [&POSTGRES, &MSSQL];

/// The support of the engine `driver` names, or `None` where it has none.
///
/// The one selection for a run that starts. An exhaustive `match`, so that a
/// third engine does not compile until it says whether it has support.
pub const fn support(driver: Driver) -> Option<&'static ContainerSupport> {
    match driver {
        Driver::Postgres => Some(&POSTGRES),
        Driver::Mssql => Some(&MSSQL),
        // A database file has no server to start in a container.
        Driver::Sqlite => None,
    }
}

/// The support that stamped a container with `label`, or `None` for a label no
/// compiled engine writes.
///
/// The one lookup for a leftover. `None` is not an answer about the container:
/// the caller must keep it and show the manual remedy, because a label this
/// build does not know may be a newer build's supported engine, and guessing a
/// driver would run one engine's SQL against another's server.
pub(crate) fn by_label(label: &str) -> Option<&'static ContainerSupport> {
    TABLE.iter().copied().find(|support| support.label == label)
}

/// The refusal of an engine with no container support.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("{engine} does not support owned scratch containers")]
pub struct Unsupported {
    engine: String,
}

/// The support, or the refusal that names the engine.
///
/// A refusal and not a skipped run: a container that cannot be started has
/// answered nothing. Made before a runtime is discovered or anything is
/// created, by the entry that takes the driver.
pub(crate) fn require(
    engine: &str,
    support: Option<&'static ContainerSupport>,
) -> Result<&'static ContainerSupport, Unsupported> {
    support.ok_or_else(|| Unsupported {
        engine: engine.to_owned(),
    })
}

/// [`require`] for the engine `driver` names.
///
/// The name is spoken only when support is absent, and is the one thing here
/// that is not an entry of the table, so it is written next to [`support`]: an
/// engine added without support adds its name here.
pub(crate) fn select(driver: Driver) -> Result<&'static ContainerSupport, Unsupported> {
    let name = match driver {
        Driver::Postgres => POSTGRES.name,
        Driver::Mssql => MSSQL.name,
        Driver::Sqlite => "SQLite",
    };
    require(name, support(driver))
}

/// The support of an engine that has it, for a test that starts nothing.
#[cfg(test)]
pub(crate) fn owned(driver: Driver) -> &'static ContainerSupport {
    support(driver).expect("both engines support owned scratch containers")
}

fn postgres_hold_keeper<'a>(conn: &'a mut Conn, _: &'a str) -> Pending<'a, ()> {
    Box::pin(async move {
        pbps_pg::resolver::scratch::keep(conn).await?;
        pbps_pg::resolver::recovery::mark_kept(conn).await?;
        Ok(())
    })
}

fn postgres_read_keeper<'a>(
    connection: &'a str,
    own: &'a str,
    token: &'a str,
) -> Pending<'a, Option<Kept>> {
    Box::pin(async move {
        let mut conn =
            Conn::connect_named(Driver::Postgres, connection, &format!("pbps-{own}")).await?;
        let ever = pbps_pg::resolver::recovery::kept(&mut conn).await?;
        between_reads(token).await;
        let alive = pbps_pg::resolver::recovery::alive(&mut conn, own, &[token.to_owned()]).await?;
        Ok(alive.map(|alive| Kept {
            alive: alive.contains(token),
            ever,
        }))
    })
}

fn mssql_hold_keeper<'a>(conn: &'a mut Conn, token: &'a str) -> Pending<'a, ()> {
    Box::pin(async move {
        pbps_mssql::resolver::scratch::mark(conn, token).await?;
        pbps_mssql::resolver::scratch::mark_kept(conn, token).await?;
        Ok(())
    })
}

fn mssql_read_keeper<'a>(
    connection: &'a str,
    own: &'a str,
    token: &'a str,
) -> Pending<'a, Option<Kept>> {
    Box::pin(async move {
        use pbps_mssql::resolver::scratch as sql;
        let mut conn = Conn::connect(Driver::Mssql, connection).await?;
        sql::mark(&mut conn, own).await?;
        if !sql::marker_present(&mut conn, own).await? {
            return Ok(None);
        }
        let ever = sql::kept(&mut conn).await?;
        between_reads(token).await;
        Ok(Some(Kept {
            alive: sql::marker_present(&mut conn, token).await?,
            ever,
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRIVERS: [Driver; 2] = [Driver::Mssql, Driver::Postgres];
    const ENTROPY: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_table_and_the_selection_reach_the_same_entry_for_each_engine() {
        for driver in DRIVERS {
            let selected = support(driver).expect("both engines have support");
            assert_eq!(selected.driver, driver);
            assert!(std::ptr::eq(
                by_label(selected.label).expect("its own label is found"),
                selected
            ));
        }
        // Each entry is reachable from its own driver, and the labels are
        // distinct, or one label would name two engines.
        for entry in TABLE {
            assert!(std::ptr::eq(support(entry.driver).unwrap(), entry));
        }
        assert_eq!(TABLE.len(), DRIVERS.len());
        assert_ne!(TABLE[0].label, TABLE[1].label);
    }

    #[test]
    fn each_engine_keeps_the_stored_label_it_had_before_the_move() {
        // The label is written to containers that a later run's recovery reads
        // back, so a rename here would strand an old run's container.
        assert_eq!(POSTGRES.label, "postgres");
        assert_eq!(MSSQL.label, "mssql");
    }

    #[test]
    fn a_label_no_engine_writes_is_not_a_support_record() {
        for label in ["", "oracle", "Postgres", "MSSQL", "postgres ", "mssql\n"] {
            assert!(by_label(label).is_none(), "{label:?}");
        }
    }

    #[test]
    fn a_missing_support_is_refused_by_the_engines_name() {
        let refusal = require("Third", None).expect_err("no support is a refusal");
        assert_eq!(
            refusal.to_string(),
            "Third does not support owned scratch containers"
        );
    }

    #[test]
    fn a_support_is_admitted_as_it_is() {
        let admitted = require("PostgreSQL", Some(&POSTGRES)).unwrap();
        assert!(std::ptr::eq(admitted, &POSTGRES));
        for driver in DRIVERS {
            assert!(std::ptr::eq(
                select(driver).unwrap(),
                support(driver).unwrap()
            ));
        }
    }

    #[test]
    fn the_launch_connection_and_redaction_use_the_one_derived_secret() {
        for entry in TABLE {
            let launch = entry.launch(ENTROPY);
            // What the server is started with is the derived secret, in the
            // variable recovery reads it back from.
            let line = format!("{}{}", entry.password_line_prefix(), launch.secret);
            assert!(launch.environment.contains(&line), "{line}");
            // And the connection carries that same secret, once.
            let connection = (entry.loopback)(41417, &launch.secret);
            assert_eq!(connection.matches(launch.secret.as_str()).count(), 1);
            assert!(connection.contains("41417"));
        }
        // SQL Server's secret is derived, so the entropy alone is not it and a
        // diagnostic redacting the entropy would leave the password readable.
        assert_ne!(MSSQL.launch(ENTROPY).secret, ENTROPY);
        assert_eq!(POSTGRES.launch(ENTROPY).secret, ENTROPY);
    }

    #[test]
    fn a_recovered_secret_is_used_as_it_was_stored_and_never_derived_again() {
        // The environment of a container holds the derived secret. A connection
        // built from it must carry it unchanged: deriving it again would
        // prefix SQL Server's twice and the keeper could never be proven.
        let stored = MSSQL.launch(ENTROPY).secret;
        let connection = (MSSQL.loopback)(41417, &stored);
        assert!(connection.contains(&format!("Password={stored};")));
        assert!(!connection.contains(&(MSSQL.password)(&stored)));
        let stored = POSTGRES.launch(ENTROPY).secret;
        assert!((POSTGRES.loopback)(41417, &stored).contains(&format!("password={stored} ")));
    }

    /// The keeper reads need a connected server, so the live recovery tests pin
    /// their behaviour (`docker2410_tests`, `docker2410_mssql_tests`). This pins
    /// the order they depend on in the source: the persistent mark is read
    /// before the session, with the pause between them, and a reader that
    /// cannot see itself answers nothing.
    #[test]
    fn each_keeper_read_takes_the_persistent_mark_before_the_session() {
        let source = include_str!("container.rs");
        let body = |name: &str| {
            let from = source.find(&format!("fn {name}<'a>(")).expect(name);
            let rest = &source[from..];
            &rest[..rest.find("\n}\n").expect("the function ends")]
        };
        let before = |text: &str, first: &str, second: &str| {
            let (a, b) = (text.find(first), text.find(second));
            assert!(
                a.is_some() && b.is_some() && a < b,
                "{first} before {second}"
            );
        };
        let postgres = body("postgres_read_keeper");
        before(postgres, "recovery::kept(", "between_reads(");
        before(postgres, "between_reads(", "recovery::alive(");
        let mssql = body("mssql_read_keeper");
        before(mssql, "return Ok(None)", "sql::kept(");
        before(mssql, "sql::kept(", "between_reads(");
        before(mssql, "between_reads(", "marker_present(&mut conn, token)");
        // Negative: a read that could not see itself is never `alive: false`.
        assert!(!mssql.contains("alive: false"));
    }

    #[test]
    fn each_engines_launch_environment_and_connection_are_what_they_were() {
        let postgres = POSTGRES.launch("secret");
        assert_eq!(postgres.environment, ["POSTGRES_PASSWORD=secret"]);
        assert_eq!(
            (POSTGRES.loopback)(41417, "secret"),
            "host=127.0.0.1 port=41417 user=postgres password=secret dbname=postgres sslmode=disable"
        );
        let mssql = MSSQL.launch(ENTROPY);
        let password = pbps_mssql::resolver::scratch::password(ENTROPY);
        assert_eq!(
            mssql.environment,
            [
                "ACCEPT_EULA=Y".to_owned(),
                format!("MSSQL_SA_PASSWORD={password}")
            ]
        );
        assert_eq!(
            (MSSQL.loopback)(41417, &mssql.secret),
            format!(
                "Server=127.0.0.1,41417;Database=master;User \
                 Id=sa;Password={password};TrustServerCertificate=true"
            )
        );
        assert_eq!((POSTGRES.port, MSSQL.port), (5432, 1433));
        assert_eq!(
            POSTGRES.readiness,
            Readiness::Polled {
                query: "SELECT 1 AS ok"
            }
        );
        assert_eq!(MSSQL.readiness, Readiness::ExitDiagnosed);
    }
}
