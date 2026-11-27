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
    if flag.is_some() || project.config.dev.is_some() {
        crate::engine::require_command_feature(
            db::driver_for(project.config.dialect),
            crate::engine::CommandFeature::DevPreview,
        )?;
    }
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
pub fn rehearse<D: crate::emission::Engine + pbps_cli::resolver::server::PlanningDialect>(
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
    // The scratch-database lifecycle and container setup below are an
    // engine's own (SQL Server's, SPEC §9.3). Dispatching catalog reads by
    // connection cannot make that lifecycle safe for another dialect, so an
    // engine says whether it has one, and the answer is asked before anything
    // else.
    if let Err(unsupported) = pbps_cli::resolver::preview(selected) {
        bail!(
            "{unsupported}. Remove --dev and the dev block in pbps.yml to generate an offline \
             preview."
        );
    }
    let dialect = selected.as_ref();
    let (build, _) =
        crate::rehearsal::lower_from_empty(selected, baseline, baseline_ids, baseline_hints)?;

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
                    pbps_cli::resolver::server::scratch::declared_standard::<D>(standard.as_ref())?;
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
            preview::with_server::<D, _>(connection, externals, &mut cancellation, async |conn| {
                let mut report = crate::rehearsal::rehearse_in::<D>(
                    conn,
                    None,
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
                container::with_started(D::DRIVER, &image, pull, run).await
            }
        }
        // Flattened once, here. `ProduceError::Run` displays the error it also
        // names as its source, so `{:#}` on it prints that error twice, and with
        // it every refusal reason a rehearsal reports (#1920).
        .map_err(|error| anyhow::anyhow!("{error}"))
    })
}

/// What `plan` says about the files it wrote before a rehearsal that did not
/// pass: each exists, and none of them was verified.
///
/// The files are written first and stay written, because the offline part of a
/// plan is identical with or without an engine (SPEC §9.3, DEC-1920.1); so the
/// only honest way to keep a refused or non-converging rehearsal from reading as
/// a verification of them is to say so by name. One function yields both the
/// JSON findings and the human lines, so the wording cannot drift between the two
/// outcomes (refused, and ran without converging) or the two formats.
pub fn output_unverified(written: &[&std::path::Path]) -> Vec<crate::output::Finding> {
    written
        .iter()
        .map(|path| {
            let text = pbps_model::Text::new(path.display().to_string());
            crate::output::Finding::warning(
                "rehearsal.output-unverified",
                format!(
                    "{} was written, and did NOT pass the dev rehearsal",
                    text.shown()
                ),
            )
            .at(path, None)
        })
        .collect()
}

/// `message` followed by one line per written file, and unchanged when none was
/// written, so a run without `--sql` and `--out` prints exactly what it did.
pub fn with_unverified_lines(message: &str, written: &[&std::path::Path]) -> String {
    let mut text = message.to_owned();
    for finding in output_unverified(written) {
        text.push_str(&format!("\nwarning: {}", finding.message));
    }
    text
}

/// A rehearsal step that failed, with the files written before it named.
///
/// In JSON the findings go beside the failure in its `unanswerable` envelope;
/// in human mode the lines follow the error text on the stream it already uses.
/// The error is flattened once for the human text instead of wrapped in context,
/// which would print its top message twice under `{:#}` (DEC-1726.1). Nothing
/// is added when no file was written, and the exit code is untouched.
pub fn unverified_on_failure<T>(
    json: bool,
    written: &[&std::path::Path],
    id: &'static str,
    step: anyhow::Result<T>,
) -> anyhow::Result<T> {
    match step {
        Ok(value) => Ok(value),
        Err(error) if written.is_empty() => {
            crate::output::or_unanswerable("plan", json, id, Err(error))
        }
        Err(error) if json => crate::output::or_unanswerable_beside(
            "plan",
            true,
            id,
            Err(error),
            output_unverified(written),
        ),
        Err(error) => Err(anyhow::anyhow!(
            "{}",
            with_unverified_lines(&format!("{error:#}"), written)
        )),
    }
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
    fn each_written_file_is_named_once_and_no_file_means_no_statement() {
        let out = std::path::Path::new("plan.json");
        let sql = std::path::Path::new("plan.sql");
        let lines = with_unverified_lines("refused", &[out, sql]);
        assert_eq!(lines.matches("did NOT pass the dev rehearsal").count(), 2);
        assert_eq!(lines.matches("plan.json").count(), 1);
        assert_eq!(lines.matches("plan.sql").count(), 1);
        assert!(output_unverified(&[]).is_empty());
        assert_eq!(with_unverified_lines("refused", &[]), "refused");
    }

    #[test]
    fn a_path_with_a_line_break_cannot_start_a_line_of_its_own() {
        let lines = with_unverified_lines("refused", &[std::path::Path::new("a\nerror: forged")]);
        assert_eq!(lines.lines().count(), 2, "{lines}");
    }

    #[test]
    fn a_failed_step_keeps_its_error_and_gains_one_line_per_file() {
        let sql = std::path::Path::new("plan.sql");
        let failed: anyhow::Result<()> = Err(anyhow::anyhow!("no engine"));
        let error = unverified_on_failure(false, &[sql], "rehearsal.unavailable", failed)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("no engine\n"), "{error}");
        assert_eq!(error.matches("plan.sql").count(), 1, "{error}");
        let failed: anyhow::Result<()> = Err(anyhow::anyhow!("no engine"));
        let error = unverified_on_failure(false, &[], "rehearsal.unavailable", failed)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "no engine");
        assert!(unverified_on_failure(false, &[sql], "x", Ok(1)).is_ok());
    }

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
