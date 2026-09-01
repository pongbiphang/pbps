//! `pbps doctor` — is this project, and this environment, ready (SPEC §14.1)?
//!
//! # The problem it solves
//!
//! Connection, engine edition, permissions, paths and ledger readiness each used
//! to fail later, at a different command, one at a time. A user setting pbps up
//! against their first database learned about them in the order the commands
//! happened to need them — often across five runs and two days, each ending in a
//! message about a different subsystem.
//!
//! # Two rules it keeps
//!
//! **It reimplements nothing.** The declaration checks are
//! [`crate::validate_findings`], the same function `validate` runs. A readiness
//! command that disagreed with `validate` about whether the declarations are
//! valid would be worse than one that never looked.
//!
//! **It writes nothing.** This is the command someone runs when they are not yet
//! sure what they are pointed at, which is quite possibly production. So the
//! permissions are asked for rather than tried, and the ledger is read rather
//! than created.

use pbps_config::Project;
use pbps_db::Conn;
use pbps_mssql::edition::Edition;

use crate::{db, output};

/// Everything `doctor` learned, for `--format json`.
#[derive(serde::Serialize)]
pub struct Diagnosis {
    pub project_file: String,
    pub declarations: String,
    pub identity_file: String,
    pub dialect: &'static str,
    pub tables: usize,
    pub modules: usize,
    /// Whether git is available: the default baseline and the operator name both
    /// come from it.
    pub git: bool,
    pub environments: Vec<EnvDiagnosis>,
}

#[derive(serde::Serialize)]
pub struct EnvDiagnosis {
    pub environment: String,
    /// `ready`, `mid-deployment`, `uninitialized`, `locked`, `unreachable` or
    /// `unconfigured`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edition: Option<String>,
    /// Whether `strategy: online` can be honoured here (ADR-0003 decision 3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_online: Option<bool>,
    /// The permissions pbps needs and this account does not hold.
    pub missing_permissions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub fn cmd_doctor(project: &Project, env: Option<&str>, json: bool) -> anyhow::Result<()> {
    let dialect = crate::dialect(project)?;
    let (mut findings, counts) = crate::validate_findings(project, dialect.as_ref());

    if !project.ids_file().exists() {
        findings.push(
            output::Finding::note(
                "project.no-identity-file",
                "no identity file yet; the first `pbps plan` mints one",
            )
            .at(project.ids_file(), None)
            .remedy("pbps plan"),
        );
    }
    let root = project.root();
    let git = db::in_checkout(root);
    if !git {
        // A warning rather than an error: an air-gapped host applying an
        // exported plan legitimately has no checkout (see `db::git_sha`). What
        // it costs is the default baseline and a real operator name, and the
        // user should hear that before the ledger records "unknown".
        findings.push(output::Finding::warning(
            "project.no-git",
            "not inside a git checkout: `plan` has no default baseline (pass --base) and the \
             ledger will record the operator as `unknown`",
        ));
    } else if db::git_sha(root).is_none() {
        // A checkout with no commits: the remedy is the first commit, not
        // installing git, and conflating the two sends the user to the wrong
        // place.
        findings.push(
            output::Finding::note(
                "project.no-commits",
                "this checkout has no commits yet, so `plan`'s default baseline is empty and \
                 everything reads as newly created",
            )
            .remedy("git add -A && git commit"),
        );
    }

    let mut environments = Vec::new();
    if project.config.environments.is_empty() && env.is_none() {
        findings.push(
            output::Finding::warning(
                "project.no-environments",
                "no environments are configured, so nothing connected can be checked",
            )
            .remedy("add `environments:` to pbps.yml with `url_env:` naming the variable"),
        );
    } else {
        let names: Vec<String> = match env {
            Some(name) => vec![name.to_owned()],
            None => project.config.environments.keys().cloned().collect(),
        };
        // Refused before connecting rather than after: the failure is about the
        // project, not the environment, and reporting it once beats reporting it
        // per environment.
        db::require_mssql(project, "doctor")?;
        let rt = db::runtime()?;
        for name in names {
            let d = match project.connection_string(&name) {
                Ok(conn) => rt.block_on(examine(&name, &conn)),
                // Each environment is examined independently. One misconfigured
                // variable must not cost the operator the other five answers —
                // being able to see the whole estate at once is what makes this
                // command worth running before a deployment.
                Err(e) => EnvDiagnosis {
                    environment: name.clone(),
                    state: "unconfigured",
                    server_version: None,
                    edition: None,
                    supports_online: None,
                    missing_permissions: Vec::new(),
                    detail: Some(e.to_string()),
                },
            };
            findings.extend(env_findings(&d));
            environments.push(d);
        }
    }

    let report = output::Report::new(
        "doctor",
        findings,
        Some(Diagnosis {
            project_file: project.config_file().display().to_string(),
            declarations: project.schema_dir().display().to_string(),
            identity_file: project.ids_file().display().to_string(),
            dialect: counts.dialect,
            tables: counts.tables,
            modules: counts.modules,
            git,
            environments,
        }),
    );

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report));
    }
    report.outcome()
}

/// Everything one environment can be asked without writing to it.
async fn examine(name: &str, connection: &str) -> EnvDiagnosis {
    let mut d = EnvDiagnosis {
        environment: name.to_owned(),
        state: "unreachable",
        server_version: None,
        edition: None,
        supports_online: None,
        missing_permissions: Vec::new(),
        detail: None,
    };
    let mut conn = match Conn::connect(connection).await {
        Ok(c) => c,
        Err(e) => {
            // `redact` has already reduced the label; the driver's own message
            // names an address and a cause, never the string it was given.
            d.detail = Some(e.to_string());
            return d;
        }
    };

    d.server_version = pbps_mssql::doctor::server_version(&mut conn).await.ok();
    if let Ok(ed) = pbps_mssql::edition::edition(&mut conn).await {
        d.supports_online = Some(ed.supports_online());
        d.edition = Some(match &ed {
            // Named as unrecognised rather than passed through: the tool is
            // about to treat it as limited, and an operator reading their own
            // edition string back without comment would not know that.
            Edition::Unknown(raw) => format!("{raw} (unrecognised; treated as limited)"),
            known @ (Edition::Full(_) | Edition::Limited(_)) => known.name().to_owned(),
        });
    }
    match pbps_mssql::doctor::permissions(&mut conn).await {
        Ok(held) => {
            d.missing_permissions = pbps_mssql::doctor::missing(&held)
                .into_iter()
                .map(|(name, why)| format!("{name} — {why}"))
                .collect();
        }
        Err(e) => d.detail = Some(format!("could not read this account's permissions: {e}")),
    }

    // The ledger last, because it is the only question whose answer changes
    // between two runs a minute apart, and it is the one that decides the state.
    d.state = match pbps_mssql::state::is_initialized(&mut conn).await {
        Ok(false) => "uninitialized",
        Ok(true) => match pbps_mssql::state::lock_holder(&mut conn).await {
            Ok(Some(lock)) => {
                d.detail = Some(format!(
                    "held by {} since {}; an apply is running, or one died without releasing",
                    lock.locked_by, lock.locked_at
                ));
                "locked"
            }
            _ => match pbps_mssql::state::latest(&mut conn).await {
                Ok(Some(entry)) if entry.snapshot.staged.is_some() => {
                    let p = entry.snapshot.staged.as_ref().expect("just matched");
                    d.detail = Some(format!(
                        "a staged apply stopped after {} of {} statement(s)",
                        p.completed, p.total
                    ));
                    "mid-deployment"
                }
                Ok(Some(_)) => "ready",
                Ok(None) => "uninitialized",
                Err(e) => {
                    d.detail = Some(e.to_string());
                    "unreachable"
                }
            },
        },
        Err(e) => {
            d.detail = Some(e.to_string());
            "unreachable"
        }
    };
    d
}

fn env_findings(d: &EnvDiagnosis) -> Vec<output::Finding> {
    let mut out = Vec::new();
    match d.state {
        // Unreachable and unconfigured are errors: this is the command whose
        // whole job is to answer "can I deploy from here", and it cannot.
        "unreachable" => out.push(
            output::Finding::error(
                "environment.unreachable",
                format!(
                    "{}: {}",
                    d.environment,
                    d.detail.as_deref().unwrap_or("cannot connect")
                ),
            )
            .remedy("check the variable named by `url_env:`, the host, and the firewall"),
        ),
        "unconfigured" => out.push(output::Finding::error(
            "environment.unconfigured",
            format!(
                "{}: {}",
                d.environment,
                d.detail.as_deref().unwrap_or("no connection string")
            ),
        )),
        // Not an error: a database pbps has never touched is the ordinary state
        // of the environment someone is about to adopt, and telling them it is
        // broken would be wrong on their first run.
        "uninitialized" => out.push(
            output::Finding::note(
                "state.uninitialized",
                format!("{}: pbps has recorded no state here yet", d.environment),
            )
            .remedy("pbps baseline --reason \"adopting this environment\""),
        ),
        "mid-deployment" => out.push(
            output::Finding::error(
                "state.mid-deployment",
                format!(
                    "{}: {}",
                    d.environment,
                    d.detail
                        .as_deref()
                        .unwrap_or("a staged apply is unfinished")
                ),
            )
            .remedy("pbps apply --plan <plan.json> --staged --resume"),
        ),
        "locked" => out.push(
            output::Finding::warning(
                "state.locked",
                format!(
                    "{}: {}",
                    d.environment,
                    d.detail.as_deref().unwrap_or("the lock is held")
                ),
            )
            .remedy("if no apply is running: pbps unlock"),
        ),
        _ => {}
    }
    for gap in &d.missing_permissions {
        out.push(output::Finding::error(
            "permission.missing",
            format!("{}: the account lacks {gap}", d.environment),
        ));
    }
    if d.supports_online == Some(false) {
        out.push(output::Finding::note(
            "edition.no-online",
            format!(
                "{}: this edition has no online index operations, so `strategy: online` cannot \
                 be honoured here",
                d.environment
            ),
        ));
    }
    out
}

fn render(report: &output::Report<Diagnosis>) -> String {
    let Some(d) = &report.data else {
        return String::new();
    };
    let mut out = String::from("Project\n");
    out.push_str(&format!("  config        {}\n", d.project_file));
    out.push_str(&format!("  declarations  {}\n", d.declarations));
    out.push_str(&format!("  identity      {}\n", d.identity_file));
    out.push_str(&format!(
        "  dialect       {} — {} table(s), {} module(s)\n",
        d.dialect, d.tables, d.modules
    ));
    out.push_str(&format!(
        "  git           {}\n",
        if d.git { "yes" } else { "no" }
    ));

    if !d.environments.is_empty() {
        out.push_str("\nEnvironments\n");
        for e in &d.environments {
            out.push_str(&format!("  {:<14} {}\n", e.environment, e.state));
            if let Some(v) = &e.server_version {
                out.push_str(&format!("                 server  {v}\n"));
            }
            if let Some(ed) = &e.edition {
                out.push_str(&format!("                 edition {ed}\n"));
            }
            if let Some(dd) = &e.detail {
                out.push_str(&format!("                 {dd}\n"));
            }
            for gap in &e.missing_permissions {
                out.push_str(&format!("                 missing {gap}\n"));
            }
        }
    }

    out.push_str("\nFindings\n");
    if report.findings.is_empty() {
        out.push_str("  Nothing to report; this project is ready.\n");
    } else {
        out.push_str(&output::human(&report.findings));
    }
    out
}
