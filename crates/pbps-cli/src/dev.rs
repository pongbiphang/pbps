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

use anyhow::bail;

use pbps_config::Project;
use pbps_model::{IdsFile, Schema};

use crate::db;

/// Where the throwaway engine comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    /// A supplied server; the account determines the shared scratch layout.
    Connection(String),
    /// A container image to start and throw away.
    Docker(String),
    Configured(Box<pbps_config::resolver::ResolverProfile>, Option<String>),
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
    if let Some(name) = &dev.resolver {
        if dev.docker.is_some() || dev.url_env.is_some() {
            bail!(
                "`dev.resolver` selects a shared entry; do not combine it with `docker` or `url_env`"
            );
        }
        let selected = project
            .config
            .select_resolver(Some(name), None)?
            .expect("an explicit name selects an entry");
        let (pbps_config::resolver::ResolverProfile::Server { externals, .. }
        | pbps_config::resolver::ResolverProfile::Docker { externals, .. }) = &selected.profile;
        let externals = externals
            .as_ref()
            .map(|path| {
                crate::resolution::read_externals(project.root(), path).map_err(|why| {
                    anyhow::anyhow!("the dev externals file {} {why}", path.display())
                })
            })
            .transpose()?;
        return Ok(Some(Spec::Configured(
            Box::new(selected.profile),
            externals,
        )));
    }
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

/// Builds the baseline, applies the plan, and compares against the declarations.
#[allow(clippy::too_many_arguments)]
pub fn rehearse<D: crate::emission::Engine>(
    spec: &Spec,
    baseline: &Schema,
    baseline_ids: &IdsFile,
    baseline_hints: &pbps_model::Hints,
    declared: &Schema,
    declared_ids: &IdsFile,
    plan: &crate::emission::LoweredPlan,
    selected: &D,
    hints: &pbps_model::Hints,
) -> anyhow::Result<crate::rehearsal::Findings> {
    let dialect = selected.as_ref();
    // The scratch-database lifecycle and container setup below are SQL
    // Server-specific (SPEC §9.3). Dispatching catalog reads by connection
    // cannot make that lifecycle safe for another dialect.
    if dialect.name() != "mssql" {
        bail!(
            "dev rehearsal is not supported for dialect `{}`; it currently requires SQL Server. \
             Remove --dev and the dev block in pbps.yml to generate an offline preview.",
            dialect.name()
        );
    }
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
    // Through the schedule as bootstrap is (#1955): whichever passes an
    // engine has for a plan built from nothing, the rehearsal's baseline gets.
    // SQL Server has none today.
    let build = crate::dependents::schedule::schedule(
        build,
        &crate::dependents::schedule::Declarations {
            schema: baseline,
            sources: crate::dependents::Sources {
                before: pbps_diff::Side {
                    schema: &Schema::default(),
                    ids: &IdsFile::default(),
                },
                after: baseline_ids,
            },
            module_deps: &baseline_hints.module_deps,
        },
        &crate::dependents::schedule::Facts::default(),
        dialect,
    )?
    .changes;
    let build = crate::statements(
        &build.ordered(pbps_model::PlanMode::Transactional, dialect),
        selected,
    )?;

    let (source, externals) = match spec {
        Spec::Docker(image) => (
            Source::Docker(image.clone(), pbps_config::resolver::PullPolicy::IfMissing),
            None,
        ),
        Spec::Connection(url) => (Source::Server(url.clone()), None),
        Spec::Configured(profile, externals) => {
            use pbps_config::resolver::ResolverProfile;
            let source = match profile.as_ref() {
                ResolverProfile::Docker { image, pull, .. } => Source::Docker(image.clone(), *pull),
                ResolverProfile::Server {
                    url_env, standard, ..
                } => {
                    pbps_cli::resolver::server::scratch::declared_standard(
                        pbps_db::Driver::Mssql,
                        standard.as_ref(),
                    )?;
                    Source::Server(std::env::var(url_env).map_err(|error| {
                        let reason = match error {
                            std::env::VarError::NotPresent => "not set",
                            std::env::VarError::NotUnicode(_) => "not Unicode text",
                        };
                        anyhow::anyhow!("scratch credential variable ${url_env} is {reason}; export UTF-8 scratch credentials or pass --dev explicitly")
                    })?)
                }
            };
            (source, externals.as_deref())
        }
    };
    db::runtime()?.block_on(async {
        use pbps_cli::resolver::server::scratch::{container, preview};
        let mut cancellation = preview::Cancellation::new()?;
        let mut run = async |connection: &str| {
            preview::with_server(connection, externals, &mut cancellation, async |conn| {
                let mut report = crate::rehearsal::rehearse_in(
                    conn,
                    crate::rehearsal::Base::Baseline {
                        statements: &build,
                        ids: baseline_ids,
                    },
                    plan,
                    declared,
                    declared_ids,
                    dialect,
                    hints,
                )
                .await?;
                report.externals_staged = externals.is_some();
                Ok(report)
            })
            .await
        };
        match source {
            Source::Server(url) => run(&url).await,
            Source::Docker(image, pull) => {
                container::with_started(pbps_db::Driver::Mssql, &image, pull, run).await
            }
        }
        .map_err(Into::into)
    })
}

enum Source {
    Server(String),
    Docker(String, pbps_config::resolver::PullPolicy),
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
}
