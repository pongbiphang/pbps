//! `pbps` — declarative database schema version control.

mod baseline;
mod db;
mod declaration_file;
mod deploy;
mod dev;
mod doctor;
mod explain;
mod hooks;
mod init;
mod integration;
mod output;
mod prompt;
mod report;
mod status;

/// The command ran correctly and found something the user must act on.
///
/// Not a failure of the tool, so it must not look like one. A scheduled
/// drift-watch pipeline (SPEC §10) needs to tell "the database moved" from "the
/// tool could not reach it": the first pages the schema owner, the second pages
/// whoever runs CI. One exit code for both would send every alert to the wrong
/// person half the time — and the same split holds for every read-only command,
/// which is why this is not specific to drift (SPEC §14.1).
///
/// Three codes in total: 0 success, [`EXIT_FINDING`] a finding, 1 anything that
/// stopped the tool from answering.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Found(String);

impl Found {
    /// A finding whose detail has already been printed.
    ///
    /// The empty message is load-bearing: [`main`] prints nothing for it, so a
    /// report that has already listed twelve problems is not followed by a
    /// thirteenth line restating that there were problems.
    pub fn reported() -> Self {
        Found(String::new())
    }

    /// A finding whose one-line summary is the whole of the output.
    pub fn new(message: impl Into<String>) -> Self {
        Found(message.into())
    }
}

/// The exit code for [`Found`].
const EXIT_FINDING: i32 = 2;

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::{Args, Parser, Subcommand};

use pbps_config::{DialectName, Project};
use pbps_dialect::Dialect;
use pbps_diff::{Context, Side};
use pbps_model::{ColumnRef, IdsFile, Intent, TableName};

#[derive(Parser)]
#[command(
    name = "pbps",
    version,
    about = "Declarative database schema version control"
)]
pub struct Cli {
    /// Project directory. Existing commands search upwards for pbps.yml; init
    /// creates it here. Defaults to the current directory.
    #[arg(long, global = true)]
    project: Option<PathBuf>,

    /// Never ask a question, even on a terminal.
    ///
    /// It declines the prompt; it can never answer one. No flag may supply
    /// rename or drop intent (SPEC §14.3), so this only ever makes the run more
    /// conservative — which is why it is safe to put in a shell alias.
    #[arg(long, global = true)]
    no_input: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new project, optionally adopting an existing database
    Init(init::InitArgs),

    /// Compare the declarations against a baseline and produce a change plan
    Plan {
        /// Compute an applyable plan against this environment as queried
        #[command(flatten)]
        target: TargetArgs,

        /// Which git revision to use as the baseline
        #[arg(long, default_value = "HEAD")]
        since: String,

        /// Use the named state snapshot as the baseline instead (for use without git)
        #[arg(long, conflicts_with = "since")]
        base: Option<PathBuf>,

        /// CI mode: change no files, and fail if the identity file is out of date
        #[arg(long)]
        check: bool,

        /// Write the change set out as JSON
        #[arg(long)]
        out: Option<PathBuf>,

        /// Write the plan as a SQL script (a preview; never hand-edited)
        #[arg(long)]
        sql: Option<PathBuf>,

        /// Plan one logical change for a staged apply: run outside a transaction,
        /// one statement at a time, with a checkpoint in the ledger. Needs --db/--env
        #[arg(long)]
        staged: bool,

        /// Rehearse the plan against a throwaway engine: "docker://<image>", or a
        /// connection string to a server pbps may create a scratch database on
        #[arg(long)]
        dev: Option<String>,

        /// human (default) or json. Only meaningful with --check, which is the
        /// read-only file check CI runs
        #[arg(long, default_value = "human")]
        format: OutputFormat,
    },

    /// Print the JSON Schema for the declaration format or for pbps.yml
    ///
    /// For editors, including air-gapped ones: `pbps schema --kind declaration
    /// --out .pbps-declaration.schema.json` needs no network and no service.
    Schema {
        /// declaration (default) or config
        #[arg(long, default_value = "declaration")]
        kind: integration::SchemaKind,

        /// Where to write. Defaults to standard output
        #[arg(long, short)]
        out: Option<PathBuf>,
    },

    /// Print a shell completion script
    Completions {
        /// bash, zsh, fish, elvish or powershell
        shell: clap_complete::Shell,
    },

    /// Write one man page per command into a directory
    Man {
        #[arg(long, short, default_value = "man")]
        out: PathBuf,
    },

    /// Check whether this project and its environments are ready
    Doctor {
        /// Check only this target. Without one, every configured environment.
        /// `--db` is what CI passes from a secret, as for every other connected
        /// command
        #[command(flatten)]
        target: TargetArgs,

        /// human (default) or json
        #[arg(long, default_value = "human")]
        format: OutputFormat,
    },

    /// Explain a saved plan to whoever has to approve it
    Explain {
        /// The plan.json written by `pbps plan --out`
        #[arg(long)]
        plan: PathBuf,

        /// Optionally check whether that environment is mid-deployment — the one
        /// question the plan file cannot answer
        #[command(flatten)]
        target: TargetArgs,

        /// human (default) or json
        #[arg(long, default_value = "human")]
        format: OutputFormat,
    },

    /// Check that the declarations are valid, without comparing to a baseline
    Validate {
        /// human (default) or json
        #[arg(long, default_value = "human")]
        format: OutputFormat,
    },

    /// Rewrite the declarations in canonical form
    Fmt {
        /// Only check; exit non-zero if any file needs rewriting, and change nothing
        #[arg(long)]
        check: bool,

        /// human (default) or json
        #[arg(long, default_value = "human")]
        format: OutputFormat,
    },

    /// Record a column rename
    Rename {
        /// The old fully qualified column name, e.g. dbo.customer.customer_name
        from: String,
        /// The new column name, without the table
        to: String,
    },

    /// Record a table rename
    RenameTable { from: String, to: String },

    /// Record a column drop
    Drop {
        /// The fully qualified column name, e.g. dbo.customer.national_id
        column: String,
        /// Why it is being dropped; required for audit
        #[arg(long)]
        reason: String,
    },

    /// Record a table drop
    DropTable {
        table: String,
        #[arg(long)]
        reason: String,
    },

    /// Render documentation and an ERD from the declarations
    Docs {
        /// markdown, html or erd
        #[arg(long, default_value = "markdown")]
        format: pbps_docs::Format,

        /// Where to write. Defaults to standard output
        #[arg(long, short)]
        out: Option<PathBuf>,

        /// Heading for the document
        #[arg(long, default_value = "Database schema")]
        title: String,
    },

    /// Reverse-generate declarations from an existing database
    Pull {
        #[command(flatten)]
        target: TargetArgs,

        /// Overwrite existing declarations and identity file
        #[arg(long)]
        force: bool,
    },

    /// Run a saved plan against the environment it was computed for
    Apply {
        #[command(flatten)]
        target: TargetArgs,

        /// The plan.json written by `pbps plan --db --out`
        #[arg(long)]
        plan: PathBuf,

        /// Risk classes this deployment is approved for, comma-separated
        #[arg(long, value_delimiter = ',')]
        allow: Vec<pbps_model::RiskClass>,

        /// Apply a staged plan: outside a transaction, one statement at a time
        #[arg(long)]
        staged: bool,

        /// Continue a staged apply that stopped part-way through
        #[arg(long, requires = "staged")]
        resume: bool,
    },

    /// Check the live database against its recorded state
    Verify {
        #[command(flatten)]
        target: TargetArgs,

        /// text (default) or json
        #[arg(long, default_value = "human")]
        format: OutputFormat,
    },

    /// Record the database's current state in its ledger
    Snapshot {
        #[command(flatten)]
        target: TargetArgs,

        /// Record even when the database differs from the last recorded state
        #[arg(long)]
        force: bool,
    },

    /// Take the database as it stands as the new starting point
    Baseline {
        #[command(flatten)]
        target: TargetArgs,

        /// Why the difference is not being pursued; required for audit
        #[arg(long)]
        reason: String,
    },

    /// Build the whole schema from the declarations, into an empty database
    Bootstrap {
        #[command(flatten)]
        target: TargetArgs,

        /// Write the CREATE script here (useful with no database at all)
        #[arg(long)]
        sql: Option<PathBuf>,
    },

    /// Maintenance of the in-database state ledger
    State {
        #[command(subcommand)]
        command: StateCommand,
    },

    /// One screen across every configured environment
    Status {
        /// text (default) or json
        #[arg(long, default_value = "human")]
        format: OutputFormat,
    },

    /// Release a lock left behind by a process that died mid-apply
    Unlock {
        #[command(flatten)]
        target: TargetArgs,
    },
}

impl Command {
    /// The command name to put in an envelope, when this invocation asked for
    /// JSON and speaks one.
    ///
    /// Matched exhaustively on purpose: a new read-only command with a
    /// `--format` flag has to be added here, and the compiler says so. A
    /// wildcard arm is how the other escapes in this pattern stayed hidden for
    /// eleven review rounds.
    fn json_envelope(&self) -> Option<&'static str> {
        let (name, format) = match self {
            Command::Plan { format, .. } => ("plan", *format),
            Command::Doctor { format, .. } => ("doctor", *format),
            Command::Validate { format } => ("validate", *format),
            Command::Fmt { format, .. } => ("fmt", *format),
            Command::Verify { format, .. } => ("verify", *format),
            Command::Status { format, .. } => ("status", *format),
            // Everything else either speaks no envelope (the write commands,
            // `docs`, the intent commands) or returns before discovery.
            // `explain` never reaches discovery: it returns above, with an
            // undiscoverable project reported as an unresolved *target* rather
            // than as a failure of the whole command.
            Command::Explain { .. }
            | Command::Init(_)
            | Command::Schema { .. }
            | Command::Completions { .. }
            | Command::Man { .. }
            | Command::Rename { .. }
            | Command::RenameTable { .. }
            | Command::Drop { .. }
            | Command::DropTable { .. }
            | Command::Docs { .. }
            | Command::Pull { .. }
            | Command::Apply { .. }
            | Command::Snapshot { .. }
            | Command::Baseline { .. }
            | Command::Bootstrap { .. }
            | Command::State { .. }
            | Command::Unlock { .. } => return None,
        };
        (format == OutputFormat::Json).then_some(name)
    }
}

#[derive(Subcommand)]
enum StateCommand {
    /// Delete all but the newest snapshots
    Prune {
        #[command(flatten)]
        target: TargetArgs,

        /// How many to keep
        #[arg(long, default_value_t = pbps_db::ledger::DEFAULT_KEEP)]
        keep: u32,
    },
}

/// How a read-only command should speak.
///
/// `text` is kept as an alias for `human`: it is what the connected commands
/// took before this became the one spelling across all of them (SPEC §14.1),
/// and a pipeline that already passes it should not break to gain a synonym.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    #[value(name = "human", alias = "text")]
    Human,
    Json,
}

/// Which database to act on.
///
/// Two ways in, and they are not interchangeable in practice: `--db` is what CI
/// passes from a secret, and `--env` is what a human types. Neither is a default
/// for the other — see [`db::target`].
#[derive(Args)]
struct TargetArgs {
    /// ADO.NET-style connection string, e.g.
    /// "Server=host,1433;Database=app;User Id=u;Password=p;TrustServerCertificate=true"
    #[arg(long)]
    db: Option<String>,

    /// Name of an environment declared in pbps.yml
    #[arg(long)]
    env: Option<String>,
}

impl TargetArgs {
    fn resolve(&self, project: &Project) -> anyhow::Result<db::Target> {
        db::target(project, self.db.as_deref(), self.env.as_deref())
    }
}

fn main() {
    if let Err(e) = run() {
        if let Some(found) = e.downcast_ref::<Found>() {
            // A report that already listed what it found prints nothing more;
            // repeating "error: drift found" underneath it would add noise, not
            // information.
            let message = found.to_string();
            if !message.is_empty() {
                eprintln!("{message}");
            }
            std::process::exit(EXIT_FINDING);
        }
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let start = match &cli.project {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };

    // `init` is the one command whose job is to create pbps.yml, so making it
    // pass discovery first would turn the first-run path into an impossibility.
    // Every other command keeps the ordinary upward discovery behaviour.
    if let Command::Init(args) = &cli.command {
        return init::cmd_init(&start, args);
    }
    // Like `init`, these three answer without a project. Requiring one would
    // mean a user could not get completions installed until after they had
    // succeeded at the thing completions are meant to help them do.
    //
    // Written as three `if let`s rather than one match with a catch-all: the
    // catch-all is what `wildcard_enum_match_arm` exists to refuse, and here it
    // would be right to refuse it — a new command that needs no project would
    // otherwise fall silently into "discover a project first".
    if let Command::Schema { kind, out } = &cli.command {
        return integration::cmd_schema(*kind, out.as_deref());
    }
    if let Command::Completions { shell } = &cli.command {
        return integration::cmd_completions(*shell);
    }
    if let Command::Man { out } = &cli.command {
        return integration::cmd_man(out);
    }
    // `explain` needs a project only to look an --env name up in pbps.yml. The
    // reviewer this command exists for may have been handed nothing but
    // plan.json, in a directory with no project at all — and the plan carries
    // its own dialect, so there is nothing else to discover. Requiring a
    // checkout here would break the command's one promise.
    if let Command::Explain {
        plan,
        target,
        format,
    } = &cli.command
    {
        // Every form of `explain` returns here, including `--env`. The `--env`
        // form does need a project — the name only means something inside a
        // `pbps.yml` — but that is a fact about the *environment*, not about
        // the explanation, whose file half is the entire point of the command
        // and must survive a reviewer with no checkout. Discovery failing used
        // to suppress the whole report, the same mistake an unset `url_env`
        // made one layer down and for the same reason.
        let resolved = match (&target.db, &target.env) {
            (Some(db), _) => explain::Target::Reachable(db::target_from_connection(db)),
            (None, Some(_)) => explain::Target::from(
                Project::discover(&start)
                    .map_err(anyhow::Error::from)
                    .and_then(|project| target.resolve(&project)),
            ),
            (None, None) => explain::Target::None,
        };
        return explain::cmd_explain(
            plan,
            resolved,
            target.env.as_deref(),
            *format == OutputFormat::Json,
        );
    }
    // Discovery is the one failure no command body can catch: it happens before
    // dispatch, so `validate --format json` in a directory with no readable
    // `pbps.yml` printed nothing at all and the converter reported its own
    // generic "produced no output". It is also the first failure a new user
    // meets. Which command was asked for, and whether it wanted JSON, is known
    // here — which is all the envelope needs.
    let project = match cli.command.json_envelope() {
        Some(command) => output::or_unanswerable(
            command,
            true,
            "project.undiscoverable",
            Project::discover(&start).map_err(anyhow::Error::from),
        )?,
        None => Project::discover(&start)?,
    };

    match cli.command {
        Command::Init(_)
        | Command::Schema { .. }
        | Command::Completions { .. }
        | Command::Man { .. }
        | Command::Explain { .. } => {
            unreachable!("these return before project discovery")
        }
        Command::Plan {
            target,
            since,
            base,
            check,
            out,
            sql,
            staged,
            dev,
            format,
        } => {
            // Two commands under one name, because to a user they are one
            // question asked in two places (SPEC §7.3): the MR wants a preview,
            // the deployment wants the plan for that environment.
            if target.db.is_some() || target.env.is_some() {
                if check {
                    bail!(
                        "--check is the CI file check; it never connects, so it cannot take --db"
                    );
                }
                if base.is_some() {
                    bail!("--base and --db name two different baselines; pass one of them");
                }
                if dev.is_some() {
                    // A rehearsal answers "would this compile and converge",
                    // which is a preview's question. `plan --db` produces the
                    // artifact the deployment gate approves, and mixing the two
                    // would invite a dev-verified plan to be read as a
                    // target-verified one (SPEC §9.3).
                    bail!(
                        "--dev rehearses a preview and --db computes the plan for a real \
                         environment; run them separately"
                    );
                }
                let target = target.resolve(&project)?;
                return deploy::cmd_plan_db(
                    &project,
                    &target,
                    out.as_deref(),
                    sql.as_deref(),
                    staged,
                );
            }
            if staged {
                // Staged execution is a property of a plan that is going to be
                // applied, and an offline plan never is (SPEC §7.3).
                bail!(
                    "--staged describes how a plan is applied, so it needs the target: pass --db or --env"
                );
            }
            let source = match base {
                Some(p) => baseline::Source::File(p),
                None if since == "HEAD" => baseline::default_source(&project),
                None => baseline::Source::Git { rev: since },
            };
            cmd_plan(
                &project,
                &source,
                PlanOptions {
                    check,
                    out: out.as_deref(),
                    sql: sql.as_deref(),
                    dev: dev.as_deref(),
                    // `--check` is the CI file check and changes nothing, so it
                    // must not be able to ask a question either: a prompt there
                    // would hang a pipeline on a run that was supposed to be
                    // read-only.
                    may_prompt: !cli.no_input && !check,
                    format,
                },
            )
        }
        Command::Apply {
            target,
            plan,
            allow,
            staged,
            resume,
        } => {
            let target = target.resolve(&project)?;
            deploy::cmd_apply(
                &project,
                &target,
                &plan,
                &allow.into_iter().collect(),
                staged,
                resume,
            )
        }
        Command::Doctor { target, format } => {
            // The only connected command whose target is optional: with none it
            // surveys every configured environment, which is what makes it
            // worth running before a deployment.
            //
            // A resolution failure is *not* propagated here. An unset `url_env`
            // variable is the most common first-run problem there is, and
            // `doctor` has a diagnosis for it — `unconfigured`, with the remedy
            // — which the all-environments path already produced. Failing at
            // `?` instead made the single-environment path, the one a person
            // onboarding actually types, the one that answered worst.
            let one = match (&target.db, &target.env) {
                (None, None) => None,
                _ => Some(doctor::Requested {
                    name: target.env.clone(),
                    target: target.resolve(&project),
                }),
            };
            doctor::cmd_doctor(&project, one, format == OutputFormat::Json)
        }
        Command::Validate { format } => cmd_validate(&project, format),
        Command::Fmt { check, format } => cmd_fmt(&project, check, format),
        Command::Rename { from, to } => {
            let col: ColumnRef = from.parse()?;
            cmd_intent(
                &project,
                Intent::RenameColumn {
                    table: col.table.clone(),
                    from: col.name,
                    to,
                },
            )
        }
        Command::RenameTable { from, to } => cmd_intent(
            &project,
            Intent::RenameTable {
                from: from.parse()?,
                to: to.parse()?,
            },
        ),
        Command::Drop { column, reason } => cmd_intent(
            &project,
            Intent::DropColumn {
                column: column.parse()?,
                reason,
            },
        ),
        Command::DropTable { table, reason } => cmd_intent(
            &project,
            Intent::DropTable {
                table: table.parse::<TableName>()?,
                reason,
            },
        ),
        Command::Pull { target, force } => {
            let target = target.resolve(&project)?;
            cmd_pull(&project, &target, force)
        }
        Command::Docs { format, out, title } => cmd_docs(&project, format, out.as_deref(), &title),
        Command::Verify { target, format } => {
            let json = format == OutputFormat::Json;
            // Resolving the target reads `url_env`, and an unset variable is the
            // commonest first-run failure. It happens before the command body,
            // so without this it would escape the envelope the command promises.
            let target = output::or_unanswerable(
                "verify",
                json,
                "environment.unconfigured",
                target.resolve(&project),
            )?;
            deploy::cmd_verify(&project, &target, json)
        }
        Command::Snapshot { target, force } => {
            let target = target.resolve(&project)?;
            deploy::cmd_snapshot(&project, &target, force)
        }
        Command::Baseline { target, reason } => {
            let target = target.resolve(&project)?;
            deploy::cmd_baseline(&project, &target, &reason)
        }
        Command::Bootstrap { target, sql } => {
            // The only command with an optional target: writing the script needs
            // no database, and on an air-gapped host there is not one to give.
            let target = match (&target.db, &target.env) {
                (None, None) => None,
                _ => Some(target.resolve(&project)?),
            };
            deploy::cmd_bootstrap(&project, target.as_ref(), sql.as_deref())
        }
        Command::State { command } => match command {
            StateCommand::Prune { target, keep } => {
                let target = target.resolve(&project)?;
                deploy::cmd_prune(&project, &target, keep)
            }
        },
        Command::Status { format } => status::cmd_status(&project, format == OutputFormat::Json),
        Command::Unlock { target } => {
            let target = target.resolve(&project)?;
            deploy::cmd_unlock(&project, &target)
        }
    }
}

/// Renders documentation from the declarations, offline (SPEC §9.4).
///
/// The ids file is read but not required: without it there is simply no
/// graveyard section. Documentation must not become the one command that fails
/// on a project that has never run `plan`.
fn cmd_docs(
    project: &Project,
    format: pbps_docs::Format,
    out: Option<&std::path::Path>,
    title: &str,
) -> anyhow::Result<()> {
    let loaded = load(project)?;
    let ids = read_ids_opt(project)?.unwrap_or_default();
    let rendered = pbps_docs::render(&loaded.schema, &ids, format, title);

    match out {
        Some(path) => {
            std::fs::write(path, &rendered)
                .with_context(|| format!("cannot write `{}`", path.display()))?;
            // To stderr: stdout may be the document itself when --out is absent,
            // and a progress line in a piped file would corrupt it.
            eprintln!("wrote {} ({})", path.display(), format.as_str());
        }
        None => print!("{rendered}"),
    }
    Ok(())
}

/// Reverse-generates declarations from a live database — the adoption path.
///
/// `pull` designates one source-of-truth environment (SPEC §9.2): everything the
/// database has and the model can express becomes YAML, everything it cannot
/// express is printed as a warning, and a fresh identity file is minted so the
/// next `plan` starts from "no changes".
fn cmd_pull(project: &Project, target: &db::Target, force: bool) -> anyhow::Result<()> {
    db::require_mssql(project, "pull")?;

    let dir = project.schema_dir();
    if !force {
        // A listing that *failed* is not an empty directory. This guard is the
        // only thing standing between an unforced `pull` and the user's
        // declarations, so reading "I could not look" as "there is nothing
        // there" is the one mistake it must not make — the consequence is
        // overwriting files nobody ever saw.
        let existing = if dir.is_dir() {
            pbps_load::schema_files(&dir)
                .with_context(|| format!("cannot list `{}`", dir.display()))?
        } else if dir.exists() {
            // Present and not a directory: the project is misconfigured, and
            // "there are no declarations here" is not the right reading of it.
            // Taken as one, an unforced pull would go on to create the
            // directory beside it or fail halfway through writing.
            bail!(
                "`{}` exists and is not a directory; pbps cannot tell what declarations this \
                 project has",
                dir.display()
            );
        } else {
            // Genuinely absent, which is the ordinary adoption case: a project
            // that has never had declarations is exactly what `pull` is for.
            Vec::new()
        };
        // `init` deliberately creates a versioned empty identity file. That is
        // still a pristine project, not user data for pull to overwrite. A
        // non-empty or malformed file remains a hard stop before connecting.
        let identities_exist = read_ids_opt(project)?.is_some_and(|ids| {
            !ids.tables.is_empty() || !ids.columns.is_empty() || !ids.tombstones.is_empty()
        });
        if !existing.is_empty() || identities_exist {
            bail!(
                "this project already has declarations; pull would overwrite them.\n                 Re-run with --force if that is what you want, or pull into a fresh project and merge."
            );
        }
    }

    let pulled = db::runtime()?.block_on(async {
        let mut conn = pbps_db::Conn::connect(target.connection()).await?;
        pbps_mssql::catalog::introspect(&mut conn).await
    })?;

    for w in &pulled.warnings {
        eprintln!("warning: {w}");
    }

    // What pbps cannot manage it still names (ADR-0002): an encrypted module or
    // one whose shape the emitter cannot reproduce is left alone, and a pull
    // that stayed quiet about it would tell the user the database is fully
    // covered when it is not.
    if !pulled.unmanaged_modules.is_empty() {
        eprintln!(
            "note: {} object(s) in this database cannot be managed:",
            pulled.unmanaged_modules.len()
        );
        for m in &pulled.unmanaged_modules {
            eprintln!("  {} {} — {}", m.kind, m.name, m.why);
        }
        eprintln!(
            "  They are left untouched: pbps will neither change nor drop them, and they do not appear in any plan."
        );
    }

    // Mint fresh identity for everything pulled. resolve with an empty baseline
    // can produce no blockers (nothing disappears from empty), so a failure here
    // is a bug, not a user problem.
    let res = pbps_diff::resolve(&pulled.schema, &IdsFile::default(), &[], &context())
        .map_err(|b| anyhow::anyhow!("pull could not mint identities: {} blocker(s)", b.len()))?;

    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create `{}`", dir.display()))?;
    let mut written: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for (name, table) in &pulled.schema.tables {
        let path = declaration_file::path(&dir, name, None)?;
        std::fs::write(&path, pbps_load::render(name, table, &[], None))
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        written.insert(path);
    }
    // Modules go into files of their own, named for the kind as well as the
    // object: a view and a table cannot collide in the database, so they must
    // not collide on disk either (ADR-0002).
    for (name, module) in &pulled.schema.modules {
        let path = declaration_file::path(&dir, name, Some(module.kind))?;
        std::fs::write(
            &path,
            pbps_load::render_module(name, module, &Default::default()),
        )
        .with_context(|| format!("cannot write `{}`", path.display()))?;
        written.insert(path);
    }

    // What a forced pull did not write, it removes. Leaving it would be worse
    // than deleting it: a declaration for an object that is gone plans its
    // recreation, and a view that became a procedure would leave
    // `dbo.x.view.yml` beside `dbo.x.procedure.yml` — two files declaring one
    // name, which `validate` refuses and every later load fails on. `--force`
    // already means "replace my declarations with this database"; this is the
    // rest of that sentence.
    let mut removed = Vec::new();
    // Same reading as the guard above, for the same reason in the other
    // direction: a failed listing that came back empty would silently skip the
    // cleanup and leave exactly the two-files-one-name state this block exists
    // to prevent.
    let present = pbps_load::schema_files(&dir)
        .with_context(|| format!("cannot list `{}`", dir.display()))?;
    for path in present {
        if written.contains(&path) {
            continue;
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("cannot remove the stale `{}`", path.display()))?;
        removed.push(path);
    }
    if !removed.is_empty() {
        eprintln!(
            "removed {} declaration file(s) for objects this database does not have:",
            removed.len()
        );
        for p in &removed {
            eprintln!("  {}", p.display());
        }
    }

    write_ids(project, &res.ids)?;

    println!(
        "Pulled {} table(s) and {} module(s) into `{}` and minted `{}`.",
        pulled.schema.tables.len(),
        pulled.schema.modules.len(),
        dir.display(),
        project.ids_file().display()
    );
    if !pulled.warnings.is_empty() {
        println!(
            "{} thing(s) could not be expressed and were left out; see the warnings above.",
            pulled.warnings.len()
        );
    }
    println!("Next: commit these files, then `pbps plan` should report no changes.");
    Ok(())
}

/// The dialect implementation this project is configured for.
fn dialect(project: &Project) -> anyhow::Result<Box<dyn Dialect>> {
    match project.config.dialect {
        DialectName::Mssql => Ok(Box::new(pbps_mssql::Mssql)),
        DialectName::Postgres => bail!(
            "the postgres dialect is not implemented yet (it arrives in Phase 4); this project's pbps.yml selects it"
        ),
    }
}

/// Loads the declarations, returning every error in one pass.
///
/// Silent, because `validate` has to decide how to render these — as miette
/// diagnostics or as JSON findings — and a helper that had already printed them
/// would leave it choosing between saying nothing and saying it twice.
pub(crate) fn load_quiet(
    project: &Project,
) -> Result<pbps_load::Loaded, Vec<pbps_load::LoadError>> {
    let dir = project.schema_dir();
    if !dir.is_dir() {
        return Err(vec![pbps_load::LoadError::Io {
            path: dir.clone(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no declarations directory"),
        }]);
    }
    pbps_load::load_schema_dir(&dir)
}

/// Loads the declarations, printing every error in one pass.
fn load(project: &Project) -> anyhow::Result<pbps_load::Loaded> {
    load_quiet(project).map_err(|errs| {
        for e in &errs {
            print_load_error(e);
        }
        anyhow::anyhow!("the declarations have {} problem(s)", errs.len())
    })
}

/// Renders one load error the way a person should see it.
///
/// The loader's own diagnostics already carry the source excerpt and the caret,
/// so they are rendered rather than reduced to their message — the JSON view
/// takes the reduced form instead, because a caret is not something a consumer
/// can act on.
fn print_load_error(e: &pbps_load::LoadError) {
    eprintln!("{:?}", miette::Report::msg(format!("{e}")));
}

/// One load error as a typed finding.
fn load_finding(e: &pbps_load::LoadError) -> output::Finding {
    let f = output::Finding::error(e.id(), e.to_string());
    match e.path() {
        Some(p) => f.at(p, e.line()),
        None => f,
    }
}

/// Reads the identity file, or `None` when the project has none yet.
///
/// The distinction matters to `validate`, which must not claim to have checked a
/// file that is not there. Everything else wants [`read_ids`], for which "absent"
/// and "empty" are the same thing.
fn read_ids_opt(project: &Project) -> anyhow::Result<Option<IdsFile>> {
    let path = project.ids_file();
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read identity file `{}`", path.display()))?;
    let ids: IdsFile = serde_json::from_str(&text)
        .with_context(|| format!("identity file `{}` is malformed", path.display()))?;
    // The path belongs on this one too: with a custom `ids_file` in pbps.yml, an
    // error naming only two uids leaves the user hunting for the file.
    ids.validate()
        .with_context(|| format!("identity file `{}` is inconsistent", path.display()))?;
    Ok(Some(ids))
}

fn read_ids(project: &Project) -> anyhow::Result<IdsFile> {
    Ok(read_ids_opt(project)?.unwrap_or_default())
}

fn write_ids(project: &Project, ids: &IdsFile) -> anyhow::Result<()> {
    let path = project.ids_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // A trailing newline, so the file never shows "\ No newline at end of file" in
    // a diff.
    let json = format!("{}\n", serde_json::to_string_pretty(ids)?);
    std::fs::write(&path, json)
        .with_context(|| format!("cannot write identity file `{}`", path.display()))?;
    Ok(())
}

fn context() -> Context {
    Context {
        operator: operator(),
        today: today(),
    }
}

/// Everything `validate` checks, as findings.
///
/// Extracted so `doctor` can run the same checks rather than a second, drifting
/// copy of them (SPEC §14.1). A readiness command that disagreed with `validate`
/// about whether the declarations are valid would be worse than one that never
/// looked.
pub fn validate_findings(
    project: &Project,
    dialect: &dyn Dialect,
) -> (Vec<output::Finding>, ValidateData) {
    // Both halves run before either is allowed to fail. A bad merge produces a
    // broken declaration *and* a scrambled identity file together, and the rule
    // everywhere else in this tool is to report every problem in one pass rather
    // than fix-one-run-again.
    let loaded = load_quiet(project);
    // Identity consistency is validate's job too (SPEC §5.3): two branches each
    // adding a same-named column merge cleanly at the line level — two uids, two
    // lines — so no git conflict flags it, and only a check can.
    let ids = read_ids_opt(project);

    let mut findings = Vec::new();
    if let Err(errs) = &loaded {
        findings.extend(errs.iter().map(load_finding));
    }

    // Three layers, all reported in the same pass: the loader checks shape, the
    // dialect checks what the engine will refuse (a nullable PK column, an
    // IDENTITY on nvarchar), and the identity file checks below stand alone.
    if let Ok(l) = &loaded {
        for (name, table) in &l.schema.tables {
            for e in dialect.validate_table(name, table) {
                findings.push(output::Finding::error(
                    "dialect.rejected",
                    format!("{name}: {e}"),
                ));
            }
        }
        for (name, module) in &l.schema.modules {
            for e in dialect.validate_module(name, module) {
                findings.push(output::Finding::error(
                    "dialect.rejected",
                    format!("{name}: {e}"),
                ));
            }
        }
        // Two problems only the whole schema can see: a module named after a
        // table, and a trigger on a table nobody declares. Both would otherwise
        // surface as an engine error at apply time, on a database that is
        // already half-changed.
        for problem in pbps_model::module::check_names(&l.schema) {
            findings.push(output::Finding::error("schema.name-collision", problem));
        }
        // And a third: a `depends_on:` naming a module nobody declared. It is
        // silently a no-op in the ordering, so nothing else would ever say so.
        for problem in pbps_model::module::check_dependencies(&l.schema, &l.hints.module_deps) {
            findings.push(output::Finding::error("schema.unknown-dependency", problem));
        }
    }
    if let Err(e) = &ids {
        findings.push(
            output::Finding::error("identity.inconsistent", format!("{e:#}"))
                .at(project.ids_file(), None),
        );
    }

    let data = ValidateData {
        dialect: dialect.name(),
        tables: loaded.as_ref().map(|l| l.schema.tables.len()).unwrap_or(0),
        columns: loaded
            .as_ref()
            .map(|l| l.schema.tables.values().map(|t| t.columns.len()).sum())
            .unwrap_or(0),
        modules: loaded.as_ref().map(|l| l.schema.modules.len()).unwrap_or(0),
        identity: match &ids {
            Ok(Some(i)) => Some(IdentityCounts {
                tables: i.tables.len(),
                columns: i.columns.len(),
                tombstones: i.tombstones.len(),
            }),
            _ => None,
        },
    };
    (findings, data)
}

fn cmd_validate(project: &Project, format: OutputFormat) -> anyhow::Result<()> {
    // `postgres` is an accepted `DialectName` with no implementation yet, so
    // this is a reachable failure on a perfectly valid project — and it escaped
    // before the JSON branch, leaving stdout empty.
    let dialect = match dialect(project) {
        Ok(d) => d,
        Err(e) => {
            if format == OutputFormat::Json {
                let report = output::Report::plain(
                    "validate",
                    vec![
                        output::Finding::error("project.unsupported-dialect", format!("{e:#}"))
                            .at(project.config_file(), None),
                    ],
                )
                .unanswerable();
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
            return Err(e);
        }
    };
    let (findings, data) = validate_findings(project, dialect.as_ref());
    let report = output::Report::new("validate", findings, Some(data));

    if format == OutputFormat::Json {
        return report.emit_json();
    }

    // The human view keeps the loader's own diagnostics, which quote the source
    // and point a caret at the value. Reducing them to the finding's one-line
    // message here would throw away the part that makes them worth having.
    let load_errors: Vec<&output::Finding> = report
        .findings
        .iter()
        .filter(|f| f.id.starts_with("load."))
        .collect();
    if !load_errors.is_empty() {
        // Re-loading to render is cheap and keeps one collection point for the
        // checks; the alternative is `validate_findings` returning the raw
        // errors as well and every other caller ignoring them.
        if let Err(errs) = load_quiet(project) {
            for e in &errs {
                print_load_error(e);
            }
        }
    }
    let unlocated: Vec<output::Finding> = report
        .findings
        .iter()
        .filter(|f| !f.id.starts_with("load."))
        .cloned()
        .collect();
    eprint!("{}", output::human(&unlocated));

    if let Some(d) = &report.data {
        if load_errors.is_empty() && unlocated.is_empty() {
            println!(
                "Declarations are valid for {}: {} table(s), {} column(s), {} module(s).",
                d.dialect, d.tables, d.columns, d.modules
            );
        }
        match &d.identity {
            Some(i) => println!(
                "Identity file is consistent: {} table uid(s), {} column uid(s), {} tombstone(s).",
                i.tables, i.columns, i.tombstones
            ),
            None if !report
                .findings
                .iter()
                .any(|f| f.id == "identity.inconsistent") =>
            {
                println!(
                    "No identity file yet at `{}`; run `pbps plan` to create it.",
                    project.ids_file().display()
                )
            }
            None => {}
        }
    }

    report.outcome()
}

/// What `validate` counted, for `--format json`.
///
/// Present even when the run failed: a consumer showing "3 of 40 tables are
/// broken" needs the 40, and a payload that vanished on failure would make the
/// only interesting case the one with no context.
#[derive(serde::Serialize)]
pub struct ValidateData {
    pub dialect: &'static str,
    pub tables: usize,
    pub columns: usize,
    pub modules: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentityCounts>,
}

#[derive(serde::Serialize)]
pub struct IdentityCounts {
    pub tables: usize,
    pub columns: usize,
    pub tombstones: usize,
}

/// Canonicalizes every declaration file.
///
/// This rewrites each file in full, so ordinary YAML comments are lost —
/// explanatory prose belongs in a `description` field (SPEC §4.2, "the tool owns
/// the file format").
fn cmd_fmt(project: &Project, check: bool, format: OutputFormat) -> anyhow::Result<()> {
    let dir = project.schema_dir();
    let json = format == OutputFormat::Json;
    let files = output::or_unanswerable(
        "fmt",
        json,
        "load.io",
        pbps_load::schema_files(&dir).with_context(|| format!("cannot list `{}`", dir.display())),
    )?;

    // Stripping a redundant `renamed_from` is fmt's job, not plan's: plan writes
    // only the ids file and never the user's YAML (SPEC §6.2). Redundancy is
    // judged against the ids file, so it is read here — an annotation whose fact
    // is not absorbed yet must survive the rewrite.
    //
    // A broken identity file must not stop fmt, though. Canonicalizing YAML does
    // not depend on identity, and a conflicted ids file is precisely the moment a
    // user reaches for fmt; treating it as "nothing absorbed" keeps every
    // annotation, which is the safe direction. `validate` and `plan` still refuse
    // to run on it, so the corruption is not swallowed.
    let mut findings: Vec<output::Finding> = Vec::new();
    let ids = match read_ids(project) {
        Ok(ids) => ids,
        Err(e) => {
            findings.push(
                output::Finding::warning(
                    "identity.unreadable",
                    format!("{e:#}; every `renamed_from` will be kept until it is fixed"),
                )
                .at(project.ids_file(), None)
                .remedy("pbps validate"),
            );
            IdsFile::default()
        }
    };

    let mut changed: Vec<(PathBuf, Vec<Intent>)> = Vec::new();
    for path in &files {
        let original = output::or_unanswerable(
            "fmt",
            json,
            "load.io",
            std::fs::read_to_string(path)
                .with_context(|| format!("cannot read `{}`", path.display())),
        )?;
        let loaded = pbps_load::load_file_str(path, &original).map_err(|errs| {
            // A file that does not parse cannot be canonicalized, and guessing
            // at what it meant would be the tool rewriting something it did not
            // understand. This is a tool failure rather than a finding: `fmt`
            // did not get to answer its own question.
            if format == OutputFormat::Human {
                for e in &errs {
                    print_load_error(e);
                }
            } else {
                // Unanswerable: `fmt` did not get to decide whether the file is
                // canonical, because it could not read it. The findings are the
                // parse errors, but the routing is "the tool could not run".
                let report = output::Report::plain("fmt", errs.iter().map(load_finding).collect())
                    .unanswerable();
                if let Ok(text) = serde_json::to_string_pretty(&report) {
                    println!("{text}");
                }
            }
            anyhow::anyhow!("`{}` does not parse", path.display())
        })?;

        let (rendered, absorbed): (String, Vec<Intent>) = match loaded {
            pbps_load::LoadedFile::Module(m) => (
                // A module has no one-shot annotations to absorb: it carries no
                // identity, so there is no rename intent to record (ADR-0002).
                pbps_load::render_module(&m.name, &m.module, &m.depends_on),
                Vec::new(),
            ),
            pbps_load::LoadedFile::Table(t) => {
                let (pending, absorbed): (Vec<Intent>, Vec<Intent>) = t
                    .intents
                    .iter()
                    .cloned()
                    .partition(|i| !pbps_diff::intent_is_absorbed(i, &ids));
                (
                    pbps_load::render(&t.name, &t.table, &pending, t.strategy.as_ref()),
                    absorbed,
                )
            }
        };
        if rendered == original {
            continue;
        }
        changed.push((path.clone(), absorbed));
        if !check {
            // The rewrite is what `fmt` is for, so failing it is the command
            // not finishing — and the path is the whole of the remedy, which a
            // consumer only gets if the envelope names it. Same wrapper as the
            // listing and the read above; this write was simply missed when
            // those two were done.
            output::or_unanswerable(
                "fmt",
                json,
                "load.io",
                std::fs::write(path, &rendered)
                    .with_context(|| format!("cannot write `{}`", path.display())),
            )?;
        }
    }

    // In check mode an unformatted file is the finding; in write mode it has
    // just been fixed, so it is a note. The same list either way — a consumer
    // asking "what did fmt touch" gets one answer whichever mode ran.
    for (p, _) in &changed {
        findings.push(if check {
            output::Finding::error("fmt.not-canonical", "not in canonical form")
                .at(p, None)
                .remedy("pbps fmt")
        } else {
            output::Finding::note("fmt.rewritten", "rewritten in canonical form").at(p, None)
        });
    }
    // Deleting a line the user wrote must never be silent. `intent_is_absorbed`
    // cannot tell "this rename happened" from "this rename never applied": a
    // `renamed_from` naming a column that never existed also looks absorbed,
    // because the ids file records no history to distinguish them. Naming what
    // went is the only thing that lets the user catch the typo.
    if !check {
        for (p, absorbed) in &changed {
            for i in absorbed {
                findings.push(
                    output::Finding::note(
                        "fmt.annotation-dropped",
                        format!("dropped a redundant annotation: {}", report::intent(i)),
                    )
                    .at(p, None),
                );
            }
        }
    }

    let report = output::Report::new(
        "fmt",
        findings,
        Some(FmtData {
            checked: files.len(),
            changed: changed.len(),
            mode: if check { "check" } else { "write" },
        }),
    );
    if format == OutputFormat::Json {
        return report.emit_json();
    }

    eprint!("{}", output::human(&report.findings));
    if changed.is_empty() {
        println!("All {} file(s) are already in canonical form.", files.len());
        return report.outcome();
    }
    if check {
        return Err(Found::new(format!(
            "{} file(s) are not in canonical form; run `pbps fmt`",
            changed.len()
        ))
        .into());
    }
    for (p, _absorbed) in &changed {
        println!("rewrote {}", p.display());
    }
    report.outcome()
}

/// What `fmt` looked at, for `--format json`.
#[derive(serde::Serialize)]
struct FmtData {
    checked: usize,
    changed: usize,
    /// `check` or `write`. The findings mean different things in each, and a
    /// consumer must not have to infer which ran from their severity.
    mode: &'static str,
}

/// Records one intent: re-resolves identity with it, then writes the identity
/// file back.
fn cmd_intent(project: &Project, intent: Intent) -> anyhow::Result<()> {
    let loaded = load(project)?;
    let ids = read_ids(project)?;
    let mut intents = loaded.intents;
    intents.push(intent);

    match pbps_diff::resolve(&loaded.schema, &ids, &intents, &context()) {
        Ok(res) => {
            if res.ids == ids {
                println!(
                    "The identity file is unchanged; this intent may have taken effect already."
                );
                return Ok(());
            }
            write_ids(project, &res.ids)?;
            println!("updated {}", project.ids_file().display());
            Ok(())
        }
        Err(blockers) => {
            eprintln!("{}", report::blockers(&blockers));
            Err(Found::new("identity could not be resolved").into())
        }
    }
}

/// Resolves identity, asking the user when there is one and a question to ask.
///
/// The prompt of SPEC §6.3 is a convenience wrapper over §6.1: what it produces
/// is exactly the intents `pbps rename` and `pbps drop` produce, resolved by the
/// same call. So a declined or impossible prompt has nothing to undo — it falls
/// through to §6.4's copy-pastable commands, which is also the behaviour with no
/// terminal at all.
///
/// # Why this loops
///
/// One blocker is not one question. A table that lost two columns and gained two
/// reaches here as a single `AmbiguousColumns` holding all four names, and
/// answering it names one pair — the other pair is still ambiguous. Asking once
/// per blocker would therefore make a two-column rename impossible to complete
/// interactively *and* throw away the answer already given, which is the worst
/// of both. So each round of answers is folded in and identity re-resolved, and
/// whatever is still ambiguous is asked again.
fn resolve_with_intent(
    project: &Project,
    loaded: &pbps_load::Loaded,
    ids: &IdsFile,
    may_prompt: bool,
    quiet: bool,
) -> anyhow::Result<Result<pbps_diff::Resolution, Vec<pbps_diff::Blocker>>> {
    let mut intents = loaded.intents.clone();
    let original = match pbps_diff::resolve(&loaded.schema, ids, &intents, &context()) {
        Ok(r) => return Ok(Ok(r)),
        Err(b) => b,
    };
    let mut blockers = original.clone();

    if may_prompt && prompt::interactive() {
        // The copy-pastable commands are *not* printed above the prompt. They
        // are the no-TTY answer (§6.4), and offering to do the thing while
        // telling the user to go and type it is one instruction too many.
        while let Some(answers) = prompt::ask(&blockers) {
            let before = intents.len();
            intents.extend(answers);
            if intents.len() == before {
                // A round that asked and learned nothing. Nothing here produces
                // that today, but looping forever on a blocker with no choices
                // would be a hang rather than a bug report.
                break;
            }
            // Re-resolved from scratch rather than patched: the answers are
            // ordinary intents, and running them through the same call is what
            // makes the prompt a wrapper rather than a second implementation of
            // identity resolution.
            match pbps_diff::resolve(&loaded.schema, ids, &intents, &context()) {
                Ok(r) => {
                    // Written now, so the answers survive whatever the rest of
                    // this plan does. They are the user's decisions, and losing
                    // them to a later failure would mean asking again.
                    // Silently: the caller compares against the pre-prompt
                    // mapping and prints one "updated" line, and two messages
                    // about one file would read as two files.
                    write_ids(project, &r.ids)?;
                    return Ok(Ok(r));
                }
                Err(again) => blockers = again,
            }
        }
        // Declined, or answered with something that is not an option. Falling
        // through to the no-TTY output rather than exiting quietly: the user
        // still needs the commands, and they may well have stopped because they
        // wanted to think about it somewhere other than a prompt.
        //
        // The commands printed are for the *original* blockers, not whatever is
        // left after a partial round. Nothing was recorded, so the user is back
        // where they started — and a list covering only the questions they had
        // not reached yet would silently omit the ones they had.
        eprintln!("\nNothing was recorded.");
        blockers = original;
    }
    // Silent when the caller is going to render these itself — as JSON, where a
    // prose report on stderr beside a JSON document on stdout would be two
    // descriptions of one failure.
    if !quiet {
        eprintln!("{}", report::blockers(&blockers));
    }
    Ok(Err(blockers))
}

/// What `plan` produced, for `--format json`.
///
/// The changes are counted rather than listed: the typed change set is what
/// `--out` writes, and `explain` is what renders it. Two spellings of the same
/// list, one of them abbreviated, is how a consumer comes to read the wrong one.
#[derive(serde::Serialize)]
struct PlanData {
    baseline: String,
    changes: usize,
    tables: usize,
    /// Counted apart from tables: a module change's `Change::table()` is the
    /// module's own name, so folding them together called a one-view plan
    /// "1 table" (ADR-0002).
    modules: usize,
    risks: Vec<&'static str>,
}

/// What an offline `plan` was asked to do.
///
/// Grouped rather than passed one by one: the list grew past the point where a
/// call site reads as anything but a row of booleans, and `cmd_plan(.., true,
/// false, ..)` is how the wrong flag gets wired to the wrong behaviour.
struct PlanOptions<'a> {
    check: bool,
    out: Option<&'a std::path::Path>,
    sql: Option<&'a std::path::Path>,
    dev: Option<&'a str>,
    /// False when there is no terminal, when `--no-input` was given, and always
    /// under `--check` — the read-only CI check must not be able to ask a
    /// question, or a pipeline hangs on one nobody can see.
    may_prompt: bool,
    format: OutputFormat,
}

fn cmd_plan(
    project: &Project,
    source: &baseline::Source,
    opts: PlanOptions<'_>,
) -> anyhow::Result<()> {
    let PlanOptions {
        check,
        out,
        sql,
        dev,
        may_prompt,
        format,
    } = opts;
    // Decided first. Everything below can fail, and a failure that escaped
    // before this was decided printed prose to stderr and nothing to stdout —
    // so a consumer asking for JSON got "pbps produced no output" instead of the
    // typed `load.*` findings it was owed.
    let json = format == OutputFormat::Json;

    let loaded = match load_quiet(project) {
        Ok(l) => l,
        Err(errs) => {
            // Unanswerable rather than a finding: `plan`'s question is "what
            // changes", and with declarations it cannot read it did not answer
            // that. The parse errors are still the findings — the user needs
            // them either way — and the exit code is 1 in both formats, as it
            // was before this branch existed. `validate` is the command whose
            // question *is* "are these valid", and there they are a finding.
            if json {
                let report = output::Report::plain("plan", errs.iter().map(load_finding).collect())
                    .unanswerable();
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                for e in &errs {
                    print_load_error(e);
                }
            }
            bail!("the declarations have {} problem(s)", errs.len());
        }
    };
    // The identity file is read here for the same reason the declarations were
    // above: `?` on it escaped before serialization, so a malformed or
    // inconsistent ids file left stdout empty. Wrapping only `load` fixed half
    // of one problem.
    let ids = match read_ids(project) {
        Ok(i) => i,
        Err(e) => {
            if json {
                let report = output::Report::plain(
                    "plan",
                    vec![
                        output::Finding::error("identity.unreadable", format!("{e:#}"))
                            .at(project.ids_file(), None),
                    ],
                )
                .unanswerable();
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
            return Err(e);
        }
    };
    let dialect = output::or_unanswerable(
        "plan",
        json,
        "project.unsupported-dialect",
        dialect(project),
    )?;

    let mut findings: Vec<output::Finding> = Vec::new();

    let res = match resolve_with_intent(project, &loaded, &ids, may_prompt, json)? {
        Ok(r) => r,
        Err(blockers) => {
            // A finding, not a tool failure: pbps did its job and is asking a
            // question only a human can answer (SPEC §6.1). CI must be able to
            // tell that apart from "the tool broke".
            if json {
                let report = output::Report::new(
                    "plan",
                    blockers.iter().map(report::blocker_finding).collect(),
                    None::<PlanData>,
                );
                return report.emit_json();
            }
            return Err(Found::new(
                "some changes could not be decided automatically; express the intent with the commands above and retry",
            )
            .into());
        }
    };

    if res.ids != ids {
        if check {
            let message = format!(
                "the identity file is out of date; run `pbps plan` locally and commit `{}` along with your changes",
                project.ids_file().display()
            );
            if json {
                let report = output::Report::new(
                    "plan",
                    vec![
                        output::Finding::error("identity.stale", message)
                            .at(project.ids_file(), None)
                            .remedy("pbps plan"),
                    ],
                    None::<PlanData>,
                );
                return report.emit_json();
            }
            return Err(Found::new(message).into());
        }
        // The identity file is the artifact `plan` exists to maintain — it is
        // what the MR reviews and what every later comparison matches by uid
        // — so failing to write it is as much a failure of the command as
        // failing to write `--out`. Those two were wrapped a commit earlier;
        // this one, the more important of the three, was not.
        output::or_unanswerable(
            "plan",
            json,
            "identity.unwritable",
            write_ids(project, &res.ids),
        )?;
        if !json {
            println!("updated {}", project.ids_file().display());
        }
    }

    // plan never rewrites the user's YAML (SPEC §6.2), so without this line
    // nothing tells the author that the annotations they just had absorbed are now
    // redundant — and CI's `pbps fmt --check` fails on exactly that, with no local
    // signal that anything was left undone.
    if loaded
        .intents
        .iter()
        .any(|i| pbps_diff::intent_is_absorbed(i, &res.ids))
    {
        findings.push(
            output::Finding::note(
                "fmt.redundant-annotation",
                "the `renamed_from` annotation(s) just absorbed into the identity file are now redundant",
            )
            .remedy("pbps fmt"),
        );
        if !json {
            println!("run `pbps fmt` to strip the now-redundant `renamed_from` annotation(s)");
        }
    }

    // A `--base` that is missing, malformed or of an unsupported version is a
    // failure of the *input*, not of the plan: the command never got as far as
    // comparing anything, so it is unanswerable rather than a finding.
    let base = output::or_unanswerable(
        "plan",
        json,
        "baseline.unreadable",
        baseline::load(project, source),
    )?;
    if base.is_empty_fallback {
        let message = format!(
            "the baseline is empty ({}). Everything will be listed as newly created, which is not a real plan against an existing database.",
            base.description
        );
        if json {
            findings.push(output::Finding::warning("baseline.empty", message));
        } else {
            eprintln!("warning: {message}");
        }
    }

    // Drop ordering is computed over the *baseline's* modules, so it needs the
    // annotations that travelled with that revision. A module this revision
    // removes declares no `depends_on:` any more, and the edge it recorded is
    // one the identifier scan could not find — so without this the drops fall
    // back to name order and can remove a dependency before its dependent.
    // Declared hints win where both sides have an entry: the current revision is
    // the one being planned. (`plan --db` cannot do the same; its baseline is
    // the ledger, which records the state, not the annotations beside it.)
    let mut hints = loaded.hints.clone();
    for (name, deps) in &base.hints.module_deps {
        hints
            .module_deps
            .entry(name.clone())
            .or_insert_with(|| deps.clone());
    }

    let cs = pbps_diff::diff(
        Side {
            schema: &base.schema,
            ids: &base.ids,
        },
        Side {
            schema: &loaded.schema,
            ids: &res.ids,
        },
        dialect.as_ref(),
        &hints,
    )
    .map_err(|errs| {
        // Structured, not wrapped through `or_unanswerable`: the differ hands
        // back one error per change it cannot express, and collapsing them into
        // a single message would throw away the column name that is the whole
        // remedy. Each becomes its own finding, carrying the id a future
        // `policies:` block can re-weight.
        if json {
            let report = output::Report::plain(
                "plan",
                errs.iter()
                    .map(|e| output::Finding::error("change.unexpressible", e.to_string()))
                    .collect(),
            )
            .unanswerable();
            let _ = report.emit_json();
        } else {
            for e in &errs {
                eprintln!("  {e}");
            }
        }
        anyhow::anyhow!("{} change(s) cannot be expressed", errs.len())
    })?;

    if !json {
        println!("Baseline: {}", base.description);
        print!("{}", report::plan(&cs));
    }

    // ADR-0003 decision 3: whether ONLINE exists is an edition question, and an
    // offline plan has no edition to ask. Saying so is the honest form of a
    // preview — the alternative is a plan.sql that reads as verified and turns
    // out to be Enterprise-only at the deployment gate.
    if cs.changes.iter().any(|c| c.strategy.online) && json {
        findings.push(output::Finding::note(
            "strategy.online-unverified",
            "`strategy: online` is emitted unverified: online index operations are Enterprise-only, \
             and only `pbps plan --db` can read the target's edition",
        ));
    }
    if cs.changes.iter().any(|c| c.strategy.online) && !json {
        println!(
            "\n  `strategy: online` is emitted here unverified: online index operations are \n  \
             Enterprise-only, and only `pbps plan --db` can read the target's edition."
        );
    }

    if let Some(path) = out {
        // Written as a `SavedPlan` marked `Preview`, not as a bare change set:
        // §7.3's "a plan computed offline is never accepted by apply" has to be
        // a property of the artifact. A file that merely lacked a checksum
        // would invite someone to add one.
        let mut plan = pbps_model::SavedPlan::new(
            pbps_model::PlanOrigin::Preview,
            dialect.name(),
            now(),
            pbps_model::PlanBaseline {
                description: base.description.clone(),
                checksum: pbps_model::state_checksum(&base.schema, &base.ids),
            },
            cs.clone(),
            res.ids.clone(),
        );
        plan.git_sha = db::git_sha(project.root());
        // The artifact is the deliverable, so failing to write it is a failure
        // of the whole command — but it is still the *command* that could not
        // finish, not a finding about the declarations. A read-only or missing
        // parent directory is the ordinary way this happens in CI.
        output::or_unanswerable("plan", json, "plan.unwritable", write_plan(path, &plan))?;
        if !json {
            println!(
                "\nwrote {} (a preview; `apply` will refuse it)",
                path.display()
            );
        }
    }

    if let Some(path) = sql {
        // Rendering can fail for the same reason `explain` can — a typed change
        // the emitter refuses — and writing for the same reason `--out` can.
        // Both leave the command unable to produce what it was asked for.
        let script = output::or_unanswerable(
            "plan",
            json,
            "plan.unwritable",
            render_sql(&cs, dialect.as_ref(), &base.description),
        )?;
        output::or_unanswerable(
            "plan",
            json,
            "plan.unwritable",
            std::fs::write(path, script)
                .with_context(|| format!("cannot write `{}`", path.display())),
        )?;
        if !json {
            println!("wrote {}", path.display());
        }
    }

    // The dev database is optional and is asked last: everything above is what
    // a plan produces with no engine in the room, and it must be identical
    // whether or not one is available (SPEC §9.3).
    if check && dev.is_some() {
        bail!("--check is the CI file check; it changes nothing and connects to nothing");
    }
    // The refusal above is for the flag; the `dev:` block in pbps.yml has to be
    // skipped rather than refused, or a project that configures one could never
    // run `plan --check` at all. Either way --check must not start a container
    // or open a connection: it is the read-only file check CI runs.
    // A rehearsal that could not be *set up* — no docker, a dev database that
    // does not answer — is unanswerable, distinct from one that ran and found
    // the plan does not converge (a finding, below). Confusing the two would
    // tell CI the plan is wrong when the rehearsal never happened.
    let dev_spec = if check {
        None
    } else {
        output::or_unanswerable(
            "plan",
            json,
            "rehearsal.unavailable",
            dev::spec(project, dev),
        )?
    };
    if let Some(spec) = dev_spec {
        let rehearsal = output::or_unanswerable(
            "plan",
            json,
            "rehearsal.unavailable",
            dev::rehearse(
                project,
                &spec,
                &base.schema,
                &base.ids,
                &base.hints,
                &loaded.schema,
                &res.ids,
                &statements(&cs, dialect.as_ref())?,
                dialect.as_ref(),
                &loaded.hints,
            ),
        )?;
        // Run in both formats. Skipping it in JSON mode made an output-format
        // choice silently disable a validation the project had asked for — a
        // plan that does not converge would have come back `result: "ok"`.
        if json {
            for d in &rehearsal.structural {
                findings.push(output::Finding::error(
                    "rehearsal.does-not-converge",
                    format!("after applying the plan, the database still differs: {d}"),
                ));
            }
            for d in &rehearsal.spelling {
                findings.push(
                    output::Finding::warning(
                        "rehearsal.spelling",
                        format!(
                            "the engine stores this differently, costing one rebuilt constraint \
                             per apply until the declaration is written in its stored form: {d}"
                        ),
                    )
                    .remedy("rewrite the declaration in the form shown"),
                );
            }
        } else {
            print!("{}", report::rehearsal(&rehearsal));
            if !rehearsal.converged() {
                return Err(Found::new(
                    "the plan does not converge on the declarations (SPEC §11.5 invariant 3); the \n                     differences above are what would be left behind",
                )
                .into());
            }
        }
    }

    if json {
        let (tables, modules) = report::touched(&cs);
        let report = output::Report::new(
            "plan",
            findings,
            Some(PlanData {
                baseline: base.description.clone(),
                changes: cs.changes.len(),
                tables,
                modules,
                risks: cs.risks().iter().map(|r| r.as_str()).collect(),
            }),
        );
        // A non-converging rehearsal is an error finding, so `outcome` exits 2
        // here exactly as `Found` does in the human path.
        return report.emit_json();
    }
    Ok(())
}

/// Writes a plan file. A trailing newline, so it never shows
/// "\ No newline at end of file" when a reviewer diffs two of them.
fn write_plan(path: &std::path::Path, plan: &pbps_model::SavedPlan) -> anyhow::Result<()> {
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(plan)?))
        .with_context(|| format!("cannot write `{}`", path.display()))
}

/// Renders the change set as one SQL script.
///
/// An offline plan is a preview (SPEC §7.3): the header says so, so a script
/// that escapes into a chat or a ticket still carries its own warning label.
fn render_sql(
    cs: &pbps_model::ChangeSet,
    dialect: &dyn Dialect,
    baseline: &str,
) -> anyhow::Result<String> {
    let statements = statements(cs, dialect)?;
    Ok(format!(
        "-- Generated by pbps against baseline: {baseline}\n-- A preview, not an applyable plan. Never hand-edit this file.\n\n{}",
        pbps_dialect::render_script(&statements, dialect.batch_separator())
    ))
}

/// Emits every change of a plan, in plan order.
///
/// The whole plan is emitted before anything runs. A change the dialect cannot
/// express must stop the apply at statement zero, not halfway through — which is
/// the same reason `plan` renders the SQL rather than the executor doing it one
/// change at a time.
fn statements(
    cs: &pbps_model::ChangeSet,
    dialect: &dyn Dialect,
) -> anyhow::Result<Vec<pbps_dialect::Statement>> {
    let mut statements = Vec::new();
    for p in &cs.changes {
        statements.extend(
            dialect
                .emit(&p.change, p.strategy)
                .map_err(|e| anyhow::anyhow!("cannot render a change as SQL: {e}"))?,
        );
    }
    Ok(statements)
}

/// The operator. An audit asks "who did this", and git's configuration is the
/// closest thing to the truth available.
fn operator() -> String {
    std::process::Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Today's date in UTC, as `YYYY-MM-DD`.
///
/// No date library is pulled in: this is the only thing needed from one, and this
/// tool gets audited, where a shorter dependency tree is worth more. The
/// algorithm is Howard Hinnant's civil_from_days.
fn today() -> String {
    let (y, m, d) = civil_from_days(unix_seconds().div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Now in UTC, as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Used to stamp plans and drift reports — always by the *client's* clock,
/// which is why the ledger's `applied_at` is set by the server instead: two CI
/// runners disagreeing about the time would make an environment's history
/// unorderable, and only the server has one clock for everyone.
fn now() -> String {
    let secs = unix_seconds();
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        // Leap day.
        assert_eq!(civil_from_days(20_513), (2026, 3, 1));
        assert_eq!(civil_from_days(20_512), (2026, 2, 28));
    }

    #[test]
    fn today_has_the_expected_shape() {
        let t = today();
        assert_eq!(t.len(), 10, "{t}");
        assert!(t.starts_with("20"), "{t}");
    }

    /// The timestamp goes into plan files and drift reports that other tools
    /// parse, so the shape is part of the contract.
    #[test]
    fn now_is_an_iso_instant_in_utc() {
        let n = now();
        assert_eq!(n.len(), 20, "{n}");
        assert!(n.ends_with('Z'), "{n}");
        assert_eq!(&n[..10], today(), "{n}");
        assert_eq!(&n[10..11], "T", "{n}");
        // Every field in range; a naive modulo elsewhere would show up as an
        // hour of 24 exactly once a day.
        let (h, m, s) = (&n[11..13], &n[14..16], &n[17..19]);
        assert!(h.parse::<u32>().unwrap() < 24, "{n}");
        assert!(m.parse::<u32>().unwrap() < 60, "{n}");
        assert!(s.parse::<u32>().unwrap() < 60, "{n}");
    }
}
