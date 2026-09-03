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
    /// `ready`, `mid-deployment`, `uninitialized`, `locked`, `lock-unknown`,
    /// `unreachable` or `unconfigured`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edition: Option<String>,
    /// Whether `strategy: online` can be honoured here (ADR-0003 decision 3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_online: Option<bool>,

    /// Whether this server accepts `CREATE OR ALTER`, which every module
    /// statement depends on (ADR-0002).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_create_or_alter: Option<bool>,
    /// The permissions pbps needs and this account does not hold.
    pub missing_permissions: Vec<String>,

    /// Set when the permission query itself failed, so an empty
    /// `missing_permissions` means "not determined" rather than "none".
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub permissions_unknown: bool,

    /// Declared schemas this database does not have.
    ///
    /// A readiness problem rather than a permission one: nothing in the tool
    /// emits `CREATE SCHEMA`, so a plan declaring `app.customer` against a
    /// database with no `app` fails on its first statement.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub absent_schemas: Vec<String>,

    /// Set when the version or edition could not be read, so an absent
    /// `supports_create_or_alter` means "not determined" rather than "fine".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_capabilities_unknown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl EnvDiagnosis {
    /// A diagnosis of an environment nothing has been read from yet.
    ///
    /// Every field is the value that means **not determined**, which is what
    /// makes this safe to share between the three callers: the type already
    /// distinguishes "not determined" from "none" everywhere it matters
    /// (`permissions_unknown` beside an empty `missing_permissions`,
    /// `server_capabilities_unknown` beside an absent `supports_*`), so
    /// starting from here cannot make an unread answer look like a good one.
    /// Written out three times before, which is three places to forget a new
    /// field in — and forgetting one here means shipping a default, not a
    /// compile error.
    fn unknown(environment: String, state: &'static str) -> Self {
        Self {
            environment,
            state,
            server_version: None,
            edition: None,
            supports_online: None,
            supports_create_or_alter: None,
            missing_permissions: Vec::new(),
            permissions_unknown: false,
            absent_schemas: Vec::new(),
            server_capabilities_unknown: None,
            detail: None,
        }
    }

    /// The environment whose connection string could not be resolved at all.
    ///
    /// Distinct from `unreachable`: nothing was attempted, because there was
    /// nothing to attempt it against. An unset `url_env` variable is the
    /// commonest first-run problem there is, and it has its own remedy.
    fn unconfigured(environment: String, detail: String) -> Self {
        Self {
            detail: Some(detail),
            ..Self::unknown(environment, "unconfigured")
        }
    }
}

/// One target named on the command line, resolved or not.
///
/// The resolution is carried rather than unwrapped by the caller because
/// failing to resolve is itself something `doctor` diagnoses: an unset
/// `url_env` variable is the commonest first-run problem, and it has a finding
/// and a remedy here.
pub struct Requested {
    /// The `--env` name, when that is how the target was given.
    pub name: Option<String>,
    pub target: anyhow::Result<db::Target>,
}

pub fn cmd_doctor(project: &Project, one: Option<Requested>, json: bool) -> anyhow::Result<()> {
    let dialect = output::or_unanswerable(
        "doctor",
        json,
        "project.unsupported-dialect",
        crate::dialect(project),
    )?;
    let (mut findings, counts) = crate::validate_findings(project, dialect.as_ref(), None);
    // The schemas the permission check asks about. Declarations that do not
    // load leave this empty, which is not a silence: `dbo` is always asked
    // about (the ledger lives there) and the declarations themselves are
    // already reported as findings above.
    let managed_schemas = managed_schemas(project);
    let referenced = referenced_tables(project, &managed_schemas);
    let granted = grant_targets(project);

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
    if let Some(Requested { name, target }) = one {
        output::or_unanswerable(
            "doctor",
            json,
            "project.unsupported-dialect",
            db::require_mssql(project, "doctor"),
        )?;
        let d = match target {
            Ok(target) => {
                // Named by the environment when there is one, and by the
                // *redacted* label otherwise — `db::redact` gives
                // server/database, never the connection string CI passed in.
                let label = name.unwrap_or_else(|| target.label.clone());
                let rt =
                    output::or_unanswerable("doctor", json, "runtime.unavailable", db::runtime())?;
                rt.block_on(examine(
                    &label,
                    target.connection(),
                    &managed_schemas,
                    &referenced,
                    granted.as_ref(),
                ))
            }
            Err(e) => EnvDiagnosis::unconfigured(
                name.unwrap_or_else(|| "the given target".to_owned()),
                format!("{e:#}"),
            ),
        };
        findings.extend(env_findings(&d, counts.modules > 0));
        environments.push(d);
    } else if project.config.environments.is_empty() {
        findings.push(
            output::Finding::warning(
                "project.no-environments",
                "no environments are configured, so nothing connected can be checked",
            )
            .remedy("add `environments:` to pbps.yml with `url_env:` naming the variable"),
        );
    } else {
        let names: Vec<String> = project.config.environments.keys().cloned().collect();
        // Refused before connecting rather than after: the failure is about the
        // project, not the environment, and reporting it once beats reporting it
        // per environment.
        output::or_unanswerable(
            "doctor",
            json,
            "project.unsupported-dialect",
            db::require_mssql(project, "doctor"),
        )?;
        let rt = output::or_unanswerable("doctor", json, "runtime.unavailable", db::runtime())?;
        for name in names {
            let d = match project.connection_string(&name) {
                Ok(conn) => rt.block_on(examine(
                    &name,
                    &conn,
                    &managed_schemas,
                    &referenced,
                    granted.as_ref(),
                )),
                // Each environment is examined independently. One misconfigured
                // variable must not cost the operator the other five answers —
                // being able to see the whole estate at once is what makes this
                // command worth running before a deployment.
                Err(e) => EnvDiagnosis::unconfigured(name.clone(), e.to_string()),
            };
            findings.extend(env_findings(&d, counts.modules > 0));
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

    // Marked before it is printed, so the JSON and the exit code say the same
    // thing: the converter in `scripts/` maps `result` straight to its own exit
    // code, and a report that read `findings` while the process exited 1 would
    // route an unreachable database to the author of the schema change.
    let report = if unanswerable(&report) > 0 {
        report.unanswerable()
    } else {
        report
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report));
    }
    outcome(&report)
}

/// How many findings mean "I could not look", rather than "I looked and found".
fn unanswerable(report: &output::Report<Diagnosis>) -> usize {
    report
        .findings
        .iter()
        .filter(|f| {
            f.severity == output::Severity::Error
                && matches!(
                    f.id.as_str(),
                    "environment.unreachable"
                        | "environment.unconfigured"
                        | "permission.unknown"
                        | "state.lock-unknown"
                        | "server.capabilities-unknown"
                )
        })
        .count()
}

/// Which of the three exit codes this report ends on (SPEC §9.8).
///
/// The default — any error finding means 2 — is wrong for this one command.
/// `doctor` asks "is this environment ready", and for an unreachable or
/// unconfigured target it did not *answer* that question: it could not look.
/// That is exit 1, "the command could not answer", and the distinction is the
/// whole reason the three codes exist — a pipeline running `doctor --env prod`
/// must route a firewall or a missing credential to whoever runs CI, not to the
/// author of the schema change.
///
/// Everything else it found — invalid declarations, a missing permission, an
/// environment mid-deployment — it found by looking, so those stay at 2.
fn outcome(report: &output::Report<Diagnosis>) -> anyhow::Result<()> {
    let unanswerable = unanswerable(report);
    if unanswerable > 0 {
        // The detail is already in the report above; this line is what `main`
        // prints after "error:", so it names the count rather than repeating one
        // environment's message as though it were the only one.
        anyhow::bail!("{unanswerable} environment(s) could not be checked; see the report above");
    }
    report.outcome()
}

/// The schemas this project manages, for the permission check.
///
/// Declarations that do not load give an empty list rather than an error: the
/// load failure is already a finding of its own, and `dbo` is asked about
/// regardless because the ledger lives there. Answering "no schemas" is
/// therefore the same shape as answering "a project with nothing declared yet",
/// which is a real and common state.
fn managed_schemas(project: &Project) -> Vec<String> {
    let Ok(loaded) = crate::load_quiet(project) else {
        return Vec::new();
    };
    let mut out: std::collections::BTreeSet<String> = loaded
        .schema
        .tables
        .keys()
        .map(|t| t.schema.clone())
        .collect();
    out.extend(loaded.schema.modules.keys().map(|m| m.schema.clone()));
    out.into_iter().collect()
}

/// Tables a declared foreign key points at that lie **outside** the managed
/// schemas, spelled `schema.table` for `HAS_PERMS_BY_NAME`.
///
/// `validate` accepts a foreign key whose target is not declared — the target
/// is somebody else's table, and pbps is not asked to manage it — but the
/// emitter still writes `REFERENCES [shared].[parent]`, which SQL Server
/// authorizes on that table, and the pre-flight probe for the change reads it.
/// Neither is covered by any question about the managed schemas, so a login
/// could pass `doctor` and fail during `apply`.
///
/// Targets *inside* the managed schemas are left out: the schema-scoped
/// `REFERENCES` and `SELECT` already cover them, and asking twice would report
/// the same gap at two securables.
fn referenced_tables(project: &Project, managed: &[String]) -> Vec<String> {
    let Ok(loaded) = crate::load_quiet(project) else {
        return Vec::new();
    };
    let managed: std::collections::BTreeSet<&str> = managed.iter().map(String::as_str).collect();
    let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for table in loaded.schema.tables.values() {
        for fk in table.foreign_keys.values() {
            let target = &fk.references_table;
            if !managed.contains(target.schema.as_str()) {
                out.insert(format!("{}.{}", target.schema, target.name));
            }
        }
    }
    out.into_iter().collect()
}

/// What the declared roles are granted on (ADR-0005), or `None` when the
/// project declares no role — which switches the role requirements off.
fn grant_targets(project: &Project) -> Option<pbps_mssql::doctor::GrantTargets> {
    let loaded = crate::load_quiet(project).ok()?;
    if loaded.schema.roles.is_empty() {
        return None;
    }
    let mut objects = std::collections::BTreeSet::new();
    let mut schemas = std::collections::BTreeSet::new();
    for role in loaded.schema.roles.values() {
        for target in role.grants.keys() {
            match target {
                pbps_model::GrantTarget::Object(o) => {
                    objects.insert(format!("{}.{}", o.schema, o.name));
                }
                pbps_model::GrantTarget::Schema(s) => {
                    schemas.insert(s.clone());
                }
            }
        }
    }
    Some(pbps_mssql::doctor::GrantTargets {
        objects: objects.into_iter().collect(),
        schemas: schemas.into_iter().collect(),
    })
}

/// Everything one environment can be asked without writing to it.
async fn examine(
    name: &str,
    connection: &str,
    schemas: &[String],
    referenced: &[String],
    granted: Option<&pbps_mssql::doctor::GrantTargets>,
) -> EnvDiagnosis {
    // `unreachable` until a connection says otherwise: every early return below
    // is a database that could not be read, and the state each of them leaves
    // behind has to say so rather than inherit an optimistic default.
    let mut d = EnvDiagnosis::unknown(name.to_owned(), "unreachable");
    let mut conn = match Conn::connect(connection).await {
        Ok(c) => c,
        Err(e) => {
            // `redact` has already reduced the label; the driver's own message
            // names an address and a cause, never the string it was given.
            d.detail = Some(e.to_string());
            return d;
        }
    };

    // `.ok()` and `if let Ok` would be the third instance in this function of
    // an error read as good news: with the version or the edition unread,
    // `supports_create_or_alter` is never computed, so the 2016-SP1 gate does
    // not run — and `doctor` could still say `ready` for a server that will
    // reject every module statement in the plan.
    match pbps_mssql::doctor::server_version(&mut conn).await {
        Ok(v) => d.server_version = Some(v),
        Err(e) => d.server_capabilities_unknown = Some(format!("{e}")),
    }
    match pbps_mssql::edition::edition(&mut conn).await {
        Err(e) => d.server_capabilities_unknown = Some(format!("{e}")),
        Ok(ed) => {
            d.supports_online = Some(ed.supports_online());
            // Asked of the version *and* the edition together: Azure reports
            // 12.0.x and supports the syntax regardless (see the dialect
            // function).
            d.supports_create_or_alter = d
                .server_version
                .as_deref()
                .map(|v| pbps_mssql::doctor::supports_create_or_alter(v, ed.name()));
            d.edition = Some(match &ed {
                // Named as unrecognised rather than passed through: the tool is
                // about to treat it as limited, and an operator reading their
                // own edition string back without comment would not know that.
                Edition::Unknown(raw) => format!("{raw} (unrecognised; treated as limited)"),
                known @ (Edition::Full(_) | Edition::Limited(_)) => known.name().to_owned(),
            });
        }
    }
    match pbps_mssql::doctor::permissions(&mut conn, schemas, referenced, granted).await {
        Ok(held) => {
            d.missing_permissions = pbps_mssql::doctor::missing(&held)
                .into_iter()
                // The securable is part of the answer, not decoration: "you
                // lack ALTER" sends someone to ask for it on the database,
                // which is the over-grant this check exists to avoid.
                .map(|g| format!("{} on {} — {}", g.permission, g.securable(), g.why))
                .collect();
            d.absent_schemas = held.absent_schemas.iter().cloned().collect();
        }
        // Not merely noted in `detail`: with the list left empty, a successful
        // ledger read could go on to set `ready`, and `doctor` would print
        // "Nothing to report; this project is ready" having never established
        // whether the account can deploy at all. Silence in the one direction
        // that matters is the worst answer this command can give.
        Err(e) => {
            d.permissions_unknown = true;
            d.detail = Some(format!("could not read this account's permissions: {e}"));
        }
    }

    // The ledger last, because it is the only question whose answer changes
    // between two runs a minute apart, and it is the one that decides the state.
    //
    // The lock is asked **first**, ahead of `is_initialized`. It used to be
    // asked only when the state table existed, because `lock_holder` selected
    // from `__pbps_lock` unconditionally and a never-initialized database has
    // neither table. It now checks for its own table and answers `None`, so the
    // ordering that guarded against that is no longer needed — and it was
    // hiding the half-present ledger this command already reports on elsewhere:
    // `dbo.__pbps_state` dropped by hand while a live lock survives. `doctor`
    // called that "uninitialized" and could exit 0, with the next apply blocked
    // by a lock nothing had mentioned.
    let lock = pbps_mssql::state::lock_holder(&mut conn).await;
    d.state = match lock {
        // A lock this could not read is not an absent lock. Falling through
        // here let a denied or damaged `__pbps_lock` be reported as `ready` —
        // `doctor` saying no deployment is active without ever having
        // established it, which is the same mistake as an empty
        // `missing_permissions` meaning "none missing".
        Err(e) => {
            d.detail = Some(format!("could not read the deployment lock: {e}"));
            "lock-unknown"
        }
        Ok(Some(lock)) => {
            d.detail = Some(format!(
                "held by {} since {}; an apply is running, or one died without releasing",
                lock.locked_by, lock.locked_at
            ));
            "locked"
        }
        Ok(None) => match pbps_mssql::state::is_initialized(&mut conn).await {
            Ok(false) => "uninitialized",
            Ok(true) => match pbps_mssql::state::latest(&mut conn).await {
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
            Err(e) => {
                d.detail = Some(e.to_string());
                "unreachable"
            }
        },
    };
    d
}

fn env_findings(d: &EnvDiagnosis, declares_modules: bool) -> Vec<output::Finding> {
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
            // Every per-environment remedy names the environment. `baseline`,
            // `apply` and `unlock` each require exactly one of --db / --env, so
            // a remedy without one is a command that fails the moment it is
            // pasted — and this one is aimed at a first-time user, who has the
            // least standing to work out why.
            // Quoted: an environment name is a YAML map key, so `US West` is
            // valid and interpolated verbatim becomes two arguments.
            .remedy(format!(
                "pbps baseline --env {} --reason \"adopting this environment\"",
                crate::report::env_arg(&d.environment)
            )),
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
            .remedy(format!(
                "pbps apply --env {} --plan <plan.json> --staged --resume",
                crate::report::env_arg(&d.environment)
            )),
        ),
        // Unanswerable, like `permission.unknown`: `doctor` could not establish
        // whether a deployment is running, and "probably not" is not an answer
        // this command is allowed to give.
        "lock-unknown" => out.push(
            output::Finding::error(
                "state.lock-unknown",
                format!(
                    "{}: {}",
                    d.environment,
                    d.detail
                        .as_deref()
                        .unwrap_or("the deployment lock could not be read")
                ),
            )
            .remedy("grant SELECT on dbo.__pbps_lock, or check that the table is intact"),
        ),
        // An error, not a warning. `doctor` answers "can I deploy from here",
        // and while the lock is held an apply is refused — so a readiness check
        // that passed would be answering a different question than the one it
        // was asked. Whether the holder is a live deployment or a process that
        // died is exactly what the remedy is for; both mean "not now".
        "locked" => out.push(
            output::Finding::error(
                "state.locked",
                format!(
                    "{}: {}",
                    d.environment,
                    d.detail.as_deref().unwrap_or("the lock is held")
                ),
            )
            .remedy(format!(
                "if no apply is running: pbps unlock --env {}",
                crate::report::env_arg(&d.environment)
            )),
        ),
        _ => {}
    }
    if let Some(why) = &d.server_capabilities_unknown {
        out.push(output::Finding::error(
            "server.capabilities-unknown",
            format!(
                "{}: this server's version or edition could not be read ({why}), so whether it \
                 accepts `CREATE OR ALTER` and online index operations is undetermined",
                d.environment
            ),
        ));
    }
    if d.permissions_unknown {
        out.push(
            output::Finding::error(
                "permission.unknown",
                format!(
                    "{}: this account's permissions could not be read, so whether it can deploy \
                     here is undetermined",
                    d.environment
                ),
            )
            .remedy("grant VIEW DEFINITION, or check what the login is mapped to in this database"),
        );
    }
    for gap in &d.missing_permissions {
        out.push(output::Finding::error(
            "permission.missing",
            format!("{}: the account lacks {gap}", d.environment),
        ));
    }
    for schema in &d.absent_schemas {
        // An error, not a note: nothing in this tool creates a schema, so the
        // first `CREATE TABLE [schema].[...]` fails. `doctor` exiting 0 here
        // was the readiness command clearing a deployment it could see would
        // break.
        let mut finding = output::Finding::error(
            "schema.absent",
            format!(
                "{}: the declarations use schema `{schema}`, which this database does not \
                 have — pbps never creates a schema, so the first table in it will fail",
                d.environment
            ),
        );
        // Through the dialect's own quoting, exactly like the emitter. A remedy
        // is advertised as copy-pastable, so an unescaped `]` in a schema name
        // turns one statement into several — the same lesson `shell_arg`
        // learned three times, in SQL instead of a shell. Where the name cannot
        // be quoted at all, no command is offered rather than a broken one: the
        // message already names the schema.
        if let Ok(quoted) = pbps_mssql::ident::quote(schema) {
            finding = finding.remedy(format!("CREATE SCHEMA {quoted};"));
        }
        out.push(finding);
    }
    // Only when this project actually has modules. The emitter writes
    // `CREATE OR ALTER` for every one of them and for nothing else, so on a
    // project of plain tables an old server is perfectly deployable — and a
    // readiness error there would be the check crying wolf.
    if declares_modules && d.supports_create_or_alter == Some(false) {
        out.push(
            output::Finding::error(
                "server.no-create-or-alter",
                format!(
                    "{}: this server predates SQL Server 2016 SP1, so it will reject the \
                     `CREATE OR ALTER` every module statement uses (ADR-0002). This project \
                     declares module(s), so an apply would fail here",
                    d.environment
                ),
            )
            .remedy("upgrade the server to 2016 SP1 or later, or remove the declared modules"),
        );
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
            if e.supports_create_or_alter == Some(false) {
                out.push_str("                 no CREATE OR ALTER (pre-2016 SP1)\n");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn absent(schema: &str) -> EnvDiagnosis {
        EnvDiagnosis {
            absent_schemas: vec![schema.to_owned()],
            ..EnvDiagnosis::unknown("prod".to_owned(), "ready")
        }
    }

    fn remedy(schema: &str) -> Option<String> {
        env_findings(&absent(schema), false)
            .into_iter()
            .find(|f| f.id == "schema.absent")
            .and_then(|f| f.remedy)
    }

    /// The remedy is advertised as copy-pastable, so it goes through the
    /// dialect's own quoting. An unescaped `]` turns one statement into several
    /// — the lesson `shell_arg` learned three times, in SQL this time.
    #[test]
    fn the_create_schema_remedy_is_quoted_by_the_dialect() {
        assert_eq!(remedy("app").as_deref(), Some("CREATE SCHEMA [app];"));
        assert_eq!(
            remedy("sales]archive").as_deref(),
            Some("CREATE SCHEMA [sales]]archive];")
        );
        // The shape that made this urgent: pasted unescaped it would run a
        // second statement. Counting semicolons is the wrong property — this
        // name contains two of its own, and they are harmless *inside* the
        // brackets. The property that matters is that the whole name is one
        // identifier, which is exactly "unescaping the interior gives the name
        // back".
        let name = "x]; DROP TABLE audit;--";
        let crafted = remedy(name).unwrap();
        let interior = crafted
            .strip_prefix("CREATE SCHEMA [")
            .and_then(|r| r.strip_suffix("];"))
            .unwrap_or_else(|| panic!("not a single quoted identifier: {crafted}"));
        assert_eq!(interior.replace("]]", "]"), name, "{crafted}");
    }

    /// A name the dialect cannot quote at all gets no command rather than a
    /// broken one. The message already names the schema, so nothing is lost.
    #[test]
    fn a_schema_name_that_cannot_be_quoted_is_offered_no_command() {
        assert_eq!(remedy("with\0nul"), None);
        // And the finding itself is still reported.
        assert!(
            env_findings(&absent("with\0nul"), false)
                .iter()
                .any(|f| f.id == "schema.absent")
        );
    }
}
