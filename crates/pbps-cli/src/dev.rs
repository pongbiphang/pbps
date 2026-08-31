//! The optional dev database (SPEC §9.3): a throwaway engine used to raise the
//! fidelity of a **preview**, and never to approve a deployment.
//!
//! # Three uses, all on the preview side
//!
//! 1. **Bootstrap validation** — the declarations have to actually compile.
//! 2. **Convergence rehearsal** — build the baseline, apply the plan,
//!    introspect, and compare against the desired state. That is invariant 3 of
//!    SPEC §11.5 surfaced as a check the user can run before the merge request
//!    is opened.
//! 3. **Normalization round-trip** — the engine is the normalizer (§8.2), so
//!    whatever survives the rehearsal is exactly the spelling that will be
//!    reported as a difference after the first apply. Those are printed with
//!    the form the engine stored, which is the one thing no offline
//!    normalization can produce.
//!
//! # Three stances
//!
//! - **Always optional.** With no dev database the preview degrades to
//!   lightweight normalization and says so. The air-gap promise of §9.1 stands.
//! - **A dev-verified plan is still a preview.** Applyable plans come only from
//!   `plan --db` against the target (§7.3); this does not move that line, which
//!   is why `--dev` and `--db` are refused together rather than combined.
//! - **Edition honesty.** A container runs Developer edition — the Enterprise
//!   feature set — while production may be Standard. So a rehearsal validates
//!   syntax and convergence, never edition capabilities, and the report says so
//!   rather than letting a green run read as a promise (ADR-0003 decision 3).

use anyhow::{Context as _, bail};

use pbps_config::Project;
use pbps_db::Conn;
use pbps_dialect::{Dialect, Statement};
use pbps_model::{Change, ChangeSet, IdsFile, Schema};

use crate::db;

/// Where the throwaway engine comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    /// A server that is already running. pbps creates and drops a database on
    /// it and touches nothing else.
    Connection(String),
    /// A container image to start and throw away.
    Docker(String),
}

/// Resolves `--dev` and `dev:` into one answer.
///
/// The flag wins over the file, as it does for `--db`: a developer trying a
/// different engine version should not have to edit a committed file to do it.
pub fn spec(project: &Project, flag: Option<&str>) -> anyhow::Result<Option<Spec>> {
    if let Some(raw) = flag {
        return Ok(Some(parse(raw)));
    }
    let Some(dev) = &project.config.dev else {
        return Ok(None);
    };
    match (&dev.docker, &dev.url_env) {
        (Some(image), None) => Ok(Some(Spec::Docker(image.clone()))),
        (None, Some(var)) => {
            let url = std::env::var(var).map_err(|_| {
                anyhow::anyhow!(
                    "`dev.url_env` in pbps.yml names ${var}, which is not set.\n\
                     Export it, or pass the throwaway engine directly with --dev."
                )
            })?;
            Ok(Some(Spec::Connection(url)))
        }
        (Some(_), Some(_)) => {
            bail!("`dev:` in pbps.yml names both `docker:` and `url_env:`; pick one")
        }
        (None, None) => bail!("`dev:` in pbps.yml names neither `docker:` nor `url_env:`"),
    }
}

/// `docker://<image>` starts a container; anything else is a connection string.
fn parse(raw: &str) -> Spec {
    match raw.strip_prefix("docker://") {
        Some(image) => Spec::Docker(image.to_owned()),
        None => Spec::Connection(raw.to_owned()),
    }
}

/// What a rehearsal found.
pub struct Rehearsal {
    /// Statements used to build the baseline, then to apply the plan.
    pub built: usize,
    pub applied: usize,

    /// Differences between the desired state and what the engine actually
    /// holds, split by what they mean.
    ///
    /// A **structural** difference is a bug: the plan does not converge, and
    /// applying it for real would leave the database not matching the
    /// declarations. A **spelling** difference is the engine's normalization
    /// showing through — the declaration says `amount > 0` and the catalog
    /// stores `([amount]>(0))` — which costs one rebuilt constraint per apply
    /// and, until the declaration is rewritten, one line in every drift report.
    pub structural: Vec<String>,
    pub spelling: Vec<String>,
}

impl Rehearsal {
    pub fn converged(&self) -> bool {
        self.structural.is_empty()
    }
}

/// Builds the baseline, applies the plan, and compares against the declarations.
#[allow(clippy::too_many_arguments)]
pub fn rehearse(
    project: &Project,
    spec: &Spec,
    baseline: &Schema,
    baseline_ids: &IdsFile,
    declared: &Schema,
    declared_ids: &IdsFile,
    plan: &[Statement],
    dialect: &dyn Dialect,
    hints: &pbps_model::Hints,
) -> anyhow::Result<Rehearsal> {
    db::require_mssql(project, "plan --dev")?;

    // Building the baseline is the same operation as `bootstrap`: the plan from
    // nothing. Reusing the differ rather than a second code path is what keeps
    // the rehearsal a rehearsal of the real thing.
    let build = pbps_diff::diff(
        pbps_diff::Side {
            schema: &Schema::default(),
            ids: &IdsFile::default(),
        },
        pbps_diff::Side {
            schema: baseline,
            ids: baseline_ids,
        },
        dialect,
        &pbps_model::Hints::default(),
    )
    .map_err(|errs| {
        anyhow::anyhow!(
            "the baseline cannot be expressed: {} problem(s)",
            errs.len()
        )
    })?;
    let build = crate::statements(&build, dialect)?;

    let container = match spec {
        Spec::Docker(image) => Some(Container::start(image)?),
        Spec::Connection(_) => None,
    };
    let connection = match (&container, spec) {
        (Some(c), _) => c.connection.clone(),
        (None, Spec::Connection(url)) => url.clone(),
        // Unreachable by construction: a container is started for exactly the
        // Docker case.
        (None, Spec::Docker(image)) => bail!("the container for `{image}` was not started"),
    };

    let result = db::runtime()?.block_on(run(
        &connection,
        &build,
        plan,
        declared,
        declared_ids,
        dialect,
        hints,
        project.config.unmanaged,
    ));
    // The container goes whatever happened; leaving one behind on a failure is
    // how a developer's machine fills up with servers they did not know about.
    drop(container);
    result
}

#[allow(clippy::too_many_arguments)]
async fn run(
    connection: &str,
    build: &[Statement],
    plan: &[Statement],
    declared: &Schema,
    declared_ids: &IdsFile,
    dialect: &dyn Dialect,
    hints: &pbps_model::Hints,
    unmanaged: pbps_config::Unmanaged,
) -> anyhow::Result<Rehearsal> {
    let _ = unmanaged;
    let mut conn = Conn::connect(connection)
        .await
        .context("cannot reach the dev database")?;

    // A database of its own, so a dev server shared by several developers (or
    // by two runs at once) cannot have one rehearsal walk into another's.
    let name = format!(
        "pbps_dev_{}_{}",
        std::process::id(),
        crate::unix_seconds().rem_euclid(100_000)
    );
    conn.execute(&format!("CREATE DATABASE [{name}];"))
        .await
        .with_context(|| format!("cannot create the scratch database `{name}`"))?;
    let outcome = rehearse_in(
        &mut conn,
        &name,
        build,
        plan,
        declared,
        declared_ids,
        dialect,
        hints,
    )
    .await;
    // Dropping is best effort: the verdict of the rehearsal must not be
    // replaced by a cleanup failure on a database nobody will look at again.
    let _ = conn
        .execute(&format!(
            "USE master; ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
        ))
        .await;
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn rehearse_in(
    conn: &mut Conn,
    name: &str,
    build: &[Statement],
    plan: &[Statement],
    declared: &Schema,
    declared_ids: &IdsFile,
    dialect: &dyn Dialect,
    hints: &pbps_model::Hints,
) -> anyhow::Result<Rehearsal> {
    conn.execute(&format!("USE [{name}];")).await?;

    for (i, stmt) in build.iter().enumerate() {
        conn.execute(&stmt.sql).await.map_err(|e| {
            anyhow::anyhow!(
                "the dev database rejected statement {} of the baseline:\n{}\n\n{e}\n\n\
                 The baseline itself does not compile, so nothing can be said about the plan.",
                i + 1,
                stmt.sql
            )
        })?;
    }
    for (i, stmt) in plan.iter().enumerate() {
        conn.execute(&stmt.sql).await.map_err(|e| {
            anyhow::anyhow!(
                "the dev database rejected statement {} of the plan:\n{}\n\n{e}\n\n\
                 This plan would fail at apply time; nothing was changed anywhere real.",
                i + 1,
                stmt.sql
            )
        })?;
    }

    let built = pbps_mssql::catalog::introspect(conn)
        .await
        .context("cannot read the dev database back")?;
    let modules: std::collections::BTreeSet<_> = declared.modules.keys().cloned().collect();
    let scoped = pbps_diff::scope(&built.schema, declared_ids, &modules);

    // What is left after applying the plan is what has not converged. The
    // comparison is the ordinary one, so the rehearsal cannot disagree with the
    // differ about what a difference is.
    let remaining = pbps_diff::diff(
        pbps_diff::Side {
            schema: &scoped.schema,
            ids: declared_ids,
        },
        pbps_diff::Side {
            schema: declared,
            ids: declared_ids,
        },
        dialect,
        hints,
    )
    .map_err(|errs| {
        anyhow::anyhow!(
            "the rehearsal produced {} inexpressible difference(s)",
            errs.len()
        )
    })?;

    let (structural, spelling) = classify(&remaining, &scoped.schema);
    Ok(Rehearsal {
        built: build.len(),
        applied: plan.len(),
        structural,
        spelling,
    })
}

/// Splits what is left over into "the plan is wrong" and "the engine spells it
/// differently", and describes each in the terms that make it actionable.
#[allow(clippy::wildcard_enum_match_arm)]
fn classify(remaining: &ChangeSet, engine: &Schema) -> (Vec<String>, Vec<String>) {
    let mut structural = Vec::new();
    let mut spelling = Vec::new();

    for p in &remaining.changes {
        // Exhaustiveness would be the wrong instrument here: a change kind
        // added later is structural until somebody shows it is a spelling, and
        // the safe default is the one that fails the rehearsal loudly.
        match &p.change {
            Change::AddCheck {
                table,
                name,
                constraint,
            } => spelling.push(format!(
                "check {name} on {table}: declared `{}`, the engine stores `{}`",
                constraint.expression,
                engine
                    .tables
                    .get(table)
                    .and_then(|t| t.checks.get(name))
                    .map(|c| c.expression.as_str())
                    .unwrap_or("(absent)")
            )),
            Change::AlterColumnDefault { column, to, .. } => spelling.push(format!(
                "default on {column}: declared `{}`, the engine stores `{}`",
                to.as_deref().unwrap_or("(none)"),
                engine
                    .tables
                    .get(&column.table)
                    .and_then(|t| t.columns.get(&column.name))
                    .and_then(|c| c.default.as_deref())
                    .unwrap_or("(none)")
            )),
            // The other half of a check's drop+add pair, and a module the engine
            // stored with different layout: neither is a second finding.
            Change::DropCheck { .. } | Change::AlterModule { .. } => {}
            other => structural.push(crate::report::describe(other)),
        }
    }
    (structural, spelling)
}

/// A container started for one rehearsal and removed when it ends.
struct Container {
    id: String,
    connection: String,
}

/// How long to wait for a server that is still starting.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

impl Container {
    fn start(image: &str) -> anyhow::Result<Container> {
        // A password per run, never one written in the source: the container is
        // throwaway, but a fixed credential in a published binary is the kind
        // of thing that ends up somewhere it was not meant to.
        let password = format!("Pbps!{}{}", std::process::id(), crate::unix_seconds());
        eprintln!("dev: starting {image} (a throwaway container)...");

        let out = std::process::Command::new("docker")
            .args(["run", "-d", "-e", "ACCEPT_EULA=Y"])
            .arg("-e")
            .arg(format!("MSSQL_SA_PASSWORD={password}"))
            // Port 0 lets the host pick, so a rehearsal never collides with the
            // SQL Server a developer already has running.
            .args(["-p", "0:1433", image])
            .output()
            .map_err(|e| {
                anyhow::anyhow!(
                    "cannot run docker: {e}\n\
                     A dev database is optional — drop --dev and the preview falls back to \
                     lightweight normalization."
                )
            })?;
        if !out.status.success() {
            bail!(
                "docker could not start `{image}`:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();

        let container = Container {
            id: id.clone(),
            connection: String::new(),
        };
        let port = match Self::published_port(&id) {
            Ok(p) => p,
            Err(e) => {
                drop(container);
                return Err(e);
            }
        };
        let connection = format!(
            "Server=127.0.0.1,{port};User Id=sa;Password={password};TrustServerCertificate=true"
        );
        let container = Container { id, connection };
        container.wait_until_ready()?;
        Ok(container)
    }

    fn published_port(id: &str) -> anyhow::Result<u16> {
        let out = std::process::Command::new("docker")
            .args(["port", id, "1433/tcp"])
            .output()
            .map_err(|e| anyhow::anyhow!("cannot ask docker for the published port: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        // `0.0.0.0:49154` — and on a dual-stack host, one line per family.
        text.lines()
            .filter_map(|l| l.rsplit(':').next())
            .find_map(|p| p.trim().parse::<u16>().ok())
            .ok_or_else(|| anyhow::anyhow!("docker published no port for 1433: {}", text.trim()))
    }

    /// Polls until the server answers, because a container is "running" long
    /// before SQL Server is listening.
    fn wait_until_ready(&self) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        let rt = db::runtime()?;
        loop {
            if rt.block_on(async {
                match Conn::connect(&self.connection).await {
                    Ok(mut c) => c.query("SELECT 1 AS ok;").await.is_ok(),
                    Err(_) => false,
                }
            }) {
                return Ok(());
            }
            if started.elapsed() > READY_TIMEOUT {
                bail!(
                    "the dev container did not accept connections within {}s",
                    READY_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if self.id.is_empty() {
            return;
        }
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.id])
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scheme is what tells a container apart from a server that is already
    /// running; a connection string must never be mistaken for an image name.
    #[test]
    fn the_docker_scheme_selects_a_container() {
        assert_eq!(
            parse("docker://mcr.microsoft.com/mssql/server:2022-latest"),
            Spec::Docker("mcr.microsoft.com/mssql/server:2022-latest".into())
        );
        assert_eq!(
            parse("Server=localhost,1433;User Id=sa;Password=x"),
            Spec::Connection("Server=localhost,1433;User Id=sa;Password=x".into())
        );
    }

    #[test]
    fn the_published_port_is_read_from_dockers_output() {
        // Not a docker call: the parsing is what can be wrong, and a
        // dual-stack host prints two lines.
        let text = "0.0.0.0:49154\n[::]:49154\n";
        let port = text
            .lines()
            .filter_map(|l| l.rsplit(':').next())
            .find_map(|p| p.trim().parse::<u16>().ok());
        assert_eq!(port, Some(49154));
    }
}
