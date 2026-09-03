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
    baseline_hints: &pbps_model::Hints,
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
        // The baseline's own hints, from the revision it came from. The
        // strategies in them are irrelevant here (nothing has rows yet), but
        // `depends_on:` is the edge the identifier scan could not find — drop it
        // and the rehearsal fails while compiling a baseline that was always
        // valid, before it has said anything about the plan.
        baseline_hints,
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
) -> anyhow::Result<Rehearsal> {
    let mut conn = Conn::connect(connection)
        .await
        .context("cannot reach the dev database")?;

    // A database of its own, so a dev server shared by several developers (or
    // by two runs at once) cannot have one rehearsal walk into another's.
    //
    // The suffix is random rather than derived from the process id and the
    // clock: two CI containers on one host routinely get the same pid within
    // the same second, and the collision shows up as one run failing to create
    // a database the other is using.
    let name = format!("pbps_dev_{}_{}", std::process::id(), random_hex());
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
    // The rehearsal reads the rows back and compares; a spelling the engine
    // reads differently would fail that comparison without saying which
    // spelling to write. Asked first, as every connected command asks
    // (DECISIONS 101).
    crate::deploy::refuse_misspelt(conn, declared).await?;

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

    // The engine side is identified by what the engine actually holds, not by
    // the identity file the declarations carry. Handing both sides the same
    // mapping would make the differ match every uid on both sides and then skip
    // any object the engine is missing — so a plan that failed to create a
    // declared table or column, which is the defect this rehearsal exists to
    // catch, would come back as convergence (the same reason drift needs
    // observed identity, SPEC 8.2).
    let observed = pbps_diff::observed_ids(&scoped.schema, declared_ids);

    // What is left after applying the plan is what has not converged. The
    // comparison is the ordinary one, so the rehearsal cannot disagree with the
    // differ about what a difference is.
    //
    // Rows included, read back under the declared scope (ADR-0004): the DML
    // ran, and whether what it wrote is what was declared — in the engine's
    // own spelling — is exactly what a rehearsal can answer that no offline
    // comparison can.
    let declared_data = declared.data_scopes();
    let read = declared_data
        .iter()
        .map(|(n, s)| (n.clone(), s.rows_to_read()))
        .collect();
    let rows = pbps_mssql::catalog::read_rows(conn, &scoped.schema, &read)
        .await
        .context("cannot read the declared rows back from the dev database")?;
    let engine = scoped
        .schema
        .with_observed_rows(&rows, &declared_data, declared)?;
    let remaining = pbps_diff::diff(
        pbps_diff::Side {
            schema: &engine,
            ids: &observed,
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

    let (structural, spelling) = classify(&remaining, &engine);
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
            } => {
                // A spelling difference needs two spellings. When the engine has
                // no constraint by that name the plan simply did not create it,
                // which is non-convergence — and `converged()` looks only at
                // `structural`, so calling it a spelling would pass the
                // rehearsal with the constraint missing.
                let stored = engine
                    .tables
                    .get(table)
                    .and_then(|t| t.checks.get(name))
                    .map(|c| c.expression.as_str());
                match stored {
                    Some(stored) => spelling.push(format!(
                        "check {name} on {table}: declared `{}`, the engine stores `{stored}`",
                        constraint.expression
                    )),
                    None => structural.push(format!(
                        "check {name} on {table} is declared but the plan did not create it"
                    )),
                }
            }
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
            // A module whose stored body still differs is not layout: the
            // difference already survived `Dialect::normalize_definition`, and
            // the engine stores a module's text verbatim rather than rewriting
            // it the way it rewrites an expression. So it is the emitter or the
            // introspection that is wrong, and swallowing it here would let a
            // view with the wrong body pass as converged.
            Change::AlterModule { name, module, .. } => structural.push(format!(
                "{} {name}: declared `{}`, the engine stores `{}`",
                module.kind,
                module.definition.trim(),
                engine
                    .modules
                    .get(name)
                    .map(|m| m.definition.trim())
                    .unwrap_or("(absent)")
            )),
            // The other half of a check's drop+add pair: not a second finding.
            Change::DropCheck { .. } => {}
            // A row whose every differing cell is "the default" on both sides
            // is the default's own spelling showing through — `N'x'` declared,
            // `'x'` stored — which is the check-constraint case one level
            // down. Any cell with a real value on either side is a row the
            // plan did not write as declared, and stays structural.
            Change::UpdateRow {
                table,
                key,
                columns,
                ..
            } if columns.values().all(|(from, to)| {
                matches!(
                    (from, to),
                    (pbps_model::Cell::Default(_), pbps_model::Cell::Default(_))
                )
            }) =>
            {
                for (column, (from, to)) in columns {
                    spelling.push(format!(
                        "row {key} of {table}, column {column}: declared {to}, the engine stores {from}"
                    ));
                }
            }
            // Deprecation is a fact about the declarations, and the emitter
            // writes nothing for it on purpose. The engine has nowhere to keep
            // it, so it comes back on every rehearsal of every declaration that
            // uses it — as non-convergence it would mean `plan --dev` could
            // never pass on a schema that documents a deprecated column.
            Change::SetColumnDeprecated { .. } => {}
            other => structural.push(crate::report::describe(other)),
        }
    }
    (structural, spelling)
}

/// Sixteen hex digits from the OS.
///
/// `RandomState` is seeded per process by the operating system and is in std,
/// which is why there is no `rand` dependency here for two uses of it. Each
/// call builds a fresh hasher, so two calls do not repeat.
fn random_hex() -> String {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    // Without a write the hasher would return its seed, which is per
    // `RandomState` rather than per call.
    h.write_usize(std::process::id() as usize);
    format!("{:016x}", h.finish())
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
        //
        // From OS randomness, not the process id and the clock. The port is
        // published, so on a shared host anything else that can reach it could
        // derive the password from a launch second and a small pid range, and
        // then connect as sa while the rehearsal is deciding whether the plan
        // converges.
        let password = format!("Pbps!{}{}", random_hex(), random_hex());
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

        // **One** guard, mutated in place. It used to be built twice — once with
        // an empty connection string to own the cleanup while the port was
        // looked up, then again, shadowing the first, once the connection string
        // was known. A shadowed binding is not dropped early: it lives to the
        // end of the function, so the first guard's `Drop` ran `docker rm -f` on
        // the container the second had just handed to the caller. `rehearse`
        // then connected to a container that no longer existed.
        //
        // Building it before the port lookup is the point of the guard, not an
        // accident of ordering: `docker run` has already created the container
        // by then, so every path out of this function from here on has to be one
        // that removes it. `?` below is such a path precisely because there is
        // only one owner now.
        let mut container = Container {
            id,
            connection: String::new(),
        };
        let port = Self::published_port(&container.id)?;
        container.connection = format!(
            "Server=127.0.0.1,{port};User Id=sa;Password={password};TrustServerCertificate=true"
        );
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

    fn planned(c: Change) -> pbps_model::PlannedChange {
        pbps_model::PlannedChange::new(c)
    }

    fn view(definition: &str) -> pbps_model::Module {
        pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            description: None,
            on: None,
            definition: definition.into(),
        }
    }

    /// A body that still differs after the plan has run is the emitter or the
    /// introspection being wrong, not the engine spelling things its own way:
    /// unlike an expression, a module's text is stored verbatim. Swallowing it
    /// would let a view with the wrong body rehearse as converged.
    #[test]
    fn a_module_the_engine_stored_differently_fails_the_rehearsal() {
        let name: pbps_model::ObjectName = "dbo.v".parse().unwrap();
        let mut engine = Schema::default();
        engine.modules.insert(name.clone(), view("SELECT 1 AS one"));

        let remaining = ChangeSet {
            changes: vec![planned(Change::AlterModule {
                name: name.clone(),
                module: Box::new(view("SELECT 2 AS two")),
            })],
        };
        let (structural, spelling) = classify(&remaining, &engine);
        assert!(spelling.is_empty(), "{spelling:?}");
        assert_eq!(structural.len(), 1, "{structural:?}");
        // Both sides have to be in the message, or the reader cannot tell which
        // half is wrong.
        assert!(structural[0].contains("SELECT 2 AS two"), "{structural:?}");
        assert!(structural[0].contains("SELECT 1 AS one"), "{structural:?}");
    }

    /// Deprecation is a fact about the declarations; the emitter writes nothing
    /// for it on purpose and the engine has nowhere to keep it. Counting it as
    /// non-convergence would mean `plan --dev` could never pass on a schema
    /// that documents a deprecated column.
    #[test]
    fn a_deprecation_never_fails_the_rehearsal() {
        let remaining = ChangeSet {
            changes: vec![planned(Change::SetColumnDeprecated {
                uid: "c_a1b2c3".parse().unwrap(),
                column: "dbo.t.old".parse().unwrap(),
                reason: Some("superseded by email".into()),
            })],
        };
        let (structural, spelling) = classify(&remaining, &Schema::default());
        assert!(structural.is_empty(), "{structural:?}");
        assert!(spelling.is_empty(), "{spelling:?}");
    }

    /// A spelling difference needs two spellings. When the engine has nothing
    /// by that name the plan did not create the constraint at all — and
    /// `converged()` looks only at `structural`, so calling it a spelling would
    /// pass the rehearsal with the check missing from the database.
    #[test]
    fn a_check_the_plan_never_created_is_structural() {
        let table: pbps_model::TableName = "dbo.t".parse().unwrap();
        let remaining = ChangeSet {
            changes: vec![planned(Change::AddCheck {
                table: table.clone(),
                name: "ck_positive".into(),
                constraint: pbps_model::CheckConstraint {
                    expression: "qty > 0".into(),
                },
            })],
        };

        // Nothing in the engine: the plan failed to create it.
        let (structural, spelling) = classify(&remaining, &Schema::default());
        assert_eq!(structural.len(), 1, "{structural:?}");
        assert!(spelling.is_empty(), "{spelling:?}");

        // The same name present, spelled the engine's way: a spelling after all.
        let mut engine = Schema::default();
        let mut t = pbps_model::Table::default();
        t.checks.insert(
            "ck_positive".into(),
            pbps_model::CheckConstraint {
                expression: "([qty]>(0))".into(),
            },
        );
        engine.tables.insert(table, t);
        let (structural, spelling) = classify(&remaining, &engine);
        assert!(structural.is_empty(), "{structural:?}");
        assert_eq!(spelling.len(), 1, "{spelling:?}");
    }

    /// The safe default is the loud one: a change kind nobody has classified
    /// must fail the rehearsal rather than pass quietly.
    #[test]
    fn an_unclassified_change_is_structural() {
        let remaining = ChangeSet {
            changes: vec![planned(Change::DropColumn {
                uid: "c_a1b2c3".parse().unwrap(),
                column: "dbo.t.gone".parse().unwrap(),
            })],
        };
        let (structural, spelling) = classify(&remaining, &Schema::default());
        assert_eq!(structural.len(), 1, "{structural:?}");
        assert!(spelling.is_empty(), "{spelling:?}");
    }

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
