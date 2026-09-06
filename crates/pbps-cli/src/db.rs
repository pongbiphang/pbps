//! Getting to a database: resolving which one, connecting, and reading its
//! current state.
//!
//! Every connected command shares three problems — which environment, one
//! runtime per command, and "what does this database look like right now" — so
//! they are solved once here rather than five times in five command bodies.

use anyhow::{Context as _, bail};

use pbps_config::{DialectName, Project};

/// A resolved deployment target.
pub struct Target {
    /// What to print. **Never** the connection string: see [`redact`].
    pub label: String,
    connection: String,
    /// The name from `pbps.yml` when the target came from `--env`, and `None`
    /// for a bare `--db` connection string.
    ///
    /// Kept beside the label rather than derived from it: for an `--env` target
    /// the two are the same string, so telling the origins apart by looking at
    /// the label is guesswork, and what depends on the answer is which commands
    /// a message may name. `status` takes no target and walks the configured
    /// environments (SPEC 9.2), so it can only be offered to a caller who named
    /// one (DECISIONS 196).
    environment: Option<String>,
    /// Which driver speaks to it.
    ///
    /// Carried on the target rather than looked up at each `connect`, because
    /// a target *is* "this database, reached this way", and the two answers
    /// have to come from the same `pbps.yml`. Resolved once in [`target`],
    /// where the project is already in hand.
    driver: pbps_db::Driver,
}

impl Target {
    pub fn connection(&self) -> &str {
        &self.connection
    }

    pub fn driver(&self) -> pbps_db::Driver {
        self.driver
    }

    /// `Some` only when this target is an environment `pbps.yml` configures.
    pub fn environment(&self) -> Option<&str> {
        self.environment.as_deref()
    }
}

/// Which driver a dialect is spoken over.
///
/// The one place the two vocabularies meet. `pbps-db` holds no SQL and does not
/// read `pbps.yml`, so it names the drivers it can speak and this match says
/// which belongs to which dialect — exhaustively, so a third engine cannot be
/// added to `DialectName` without an answer here.
pub const fn driver_for(dialect: DialectName) -> pbps_db::Driver {
    match dialect {
        DialectName::Mssql => pbps_db::Driver::Mssql,
        DialectName::Postgres => pbps_db::Driver::Postgres,
    }
}

/// Resolves `--db` / `--env` into one target.
///
/// Exactly one is required. Defaulting to a single configured environment was
/// considered and rejected: "it picked the only one there was" is a rule that
/// stops being safe the day someone adds a second environment, and the day it
/// stops being safe is a deployment.
pub fn target(project: &Project, db: Option<&str>, env: Option<&str>) -> anyhow::Result<Target> {
    match (db, env) {
        (Some(_), Some(_)) => bail!("--db and --env name the same thing; pass one of them"),
        (Some(conn), None) => Ok(Target {
            label: redact(conn),
            connection: conn.to_owned(),
            environment: None,
            driver: driver_for(project.config.dialect),
        }),
        (None, Some(name)) => Ok(Target {
            label: name.to_owned(),
            connection: project.connection_string(name)?,
            environment: Some(name.to_owned()),
            driver: driver_for(project.config.dialect),
        }),
        (None, None) => bail!(
            "this command needs a database: pass --db {} or --env {}",
            crate::report::placeholder("connection string"),
            crate::report::placeholder("name from pbps.yml")
        ),
    }
}

/// A target from a bare connection string, with no project to consult.
///
/// `explain` is the only caller: it answers for a reviewer who may have no
/// checkout, so it cannot go through [`target`], which needs a `Project` to
/// resolve an `--env` name. There is nothing to resolve here — a connection
/// string is already the answer.
pub fn target_from_connection(connection: &str, driver: pbps_db::Driver) -> Target {
    Target {
        label: redact(connection),
        connection: connection.to_owned(),
        // A connection string names no environment, and there may be no
        // `pbps.yml` here at all.
        environment: None,
        // Which is why the driver is passed in: with no project to read, the
        // only thing that knows which engine this plan was computed for is the
        // plan, and `explain` resolves it from there before building a target.
        driver,
    }
}

/// The parts of a connection string that are safe to print.
///
/// Command output lands in CI logs, in tickets and in chat. A connection string
/// carries a password, so what identifies the target — server and database — is
/// extracted and everything else is dropped. Anything unrecognizable degrades to
/// a placeholder rather than being echoed on the chance it was harmless.
pub fn redact(connection: &str) -> String {
    let mut server = None;
    let mut database = None;
    for part in connection.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "server" | "data source" | "addr" | "address" => server = Some(value),
            "database" | "initial catalog" => database = Some(value),
            _ => {}
        }
    }
    match (server, database) {
        (Some(s), Some(d)) => format!("{s}/{d}"),
        (Some(s), None) => s.to_owned(),
        _ => "the database given on the command line".to_owned(),
    }
}

/// Opens a connection to `target`.
///
/// The context line is the reason this is a function. It was written out at
/// eight call sites, which is eight chances for the ninth to say something
/// slightly different — and this message is the first thing an operator sees
/// when a deployment cannot start, so it is worth saying the same way every
/// time. The driver's own message names an address and a cause; the label is
/// deliberately not added here, because the callers that want it in a *finding*
/// (`verify`, `doctor`) compose it themselves with the redacted form.
pub async fn connect(target: &Target) -> anyhow::Result<pbps_db::Conn> {
    pbps_db::Conn::connect(target.driver(), target.connection())
        .await
        .context("cannot connect to the database")
}

/// One runtime per command. The tool is a CLI, not a server; only the driver
/// needs async at all.
pub fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// Refuses politely on a dialect whose connected half does not exist yet.
pub fn require_mssql(project: &Project, command: &str) -> anyhow::Result<()> {
    if project.config.dialect != DialectName::Mssql {
        bail!(
            "`pbps {command}` is only implemented for mssql (this project's pbps.yml selects `{}`)",
            project.config.dialect
        );
    }
    Ok(())
}

/// Whether `dir` is inside a git checkout at all.
///
/// Distinct from [`git_sha`] returning `None`, which a checkout with no commits
/// yet also does. The two have different remedies — install git and clone, or
/// make the first commit — and `doctor` has to give the right one.
pub fn in_checkout(dir: &std::path::Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--git-dir"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The commit the declarations were read from, when there is one.
///
/// Absent outside a checkout, which is legitimate: an air-gapped host applying
/// an exported plan has no git. The ledger records `None` rather than a lie.
///
/// Asked about the *project* directory, not the process's. `--project` points
/// somewhere else, and a sha read from the shell's location would stamp a plan
/// with a commit that has nothing to do with the declarations in it.
pub fn git_sha(dir: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!sha.is_empty()).then_some(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing this function exists to prevent: a password reaching a log.
    #[test]
    fn redaction_never_leaks_the_password() {
        let s = "Server=db.internal,1433;Database=app;User Id=deploy;Password=hunter2;TrustServerCertificate=true";
        let label = redact(s);
        assert_eq!(label, "db.internal,1433/app");
        assert!(!label.contains("hunter2"));
        assert!(!label.contains("deploy"));
    }

    #[test]
    fn redaction_accepts_the_other_spellings() {
        assert_eq!(
            redact("Data Source=srv;Initial Catalog=db;Password=x"),
            "srv/db"
        );
        assert_eq!(redact("SERVER=srv;PASSWORD=x"), "srv");
    }

    /// An unparseable string must degrade to a placeholder, never be echoed:
    /// "I did not recognize it" is not a reason to assume it is harmless.
    #[test]
    fn an_unrecognized_string_is_not_echoed() {
        let label = redact("this-is-not-a-connection-string-Password=hunter2");
        assert!(!label.contains("hunter2"), "{label}");
    }
}
