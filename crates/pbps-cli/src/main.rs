//! `pbps` — declarative database schema version control.

mod baseline;
mod db;
mod deploy;
mod hooks;
mod report;
mod status;

/// Drift was found. Not a failure of the tool, so it must not look like one.
///
/// A scheduled drift-watch pipeline (SPEC §10) needs to tell "the database
/// moved" from "the tool could not reach it": the first pages the schema owner,
/// the second pages whoever runs CI. One exit code for both would send every
/// alert to the wrong person half the time.
#[derive(Debug, thiserror::Error)]
#[error("drift found")]
pub struct DriftFound;

/// The exit code for [`DriftFound`].
const EXIT_DRIFT: i32 = 2;

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
struct Cli {
    /// Project directory containing pbps.yml. Defaults to searching upwards from
    /// the current directory.
    #[arg(long, global = true)]
    project: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
    },

    /// Check that the declarations are valid, without comparing to a baseline
    Validate,

    /// Rewrite the declarations in canonical form
    Fmt {
        /// Only check; exit non-zero if any file needs rewriting, and change nothing
        #[arg(long)]
        check: bool,
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
    },

    /// Check the live database against its recorded state
    Verify {
        #[command(flatten)]
        target: TargetArgs,

        /// text (default) or json
        #[arg(long, default_value = "text")]
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
        #[arg(long, default_value = "text")]
        format: OutputFormat,
    },

    /// Release a lock left behind by a process that died mid-apply
    Unlock {
        #[command(flatten)]
        target: TargetArgs,
    },
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

/// How a machine-readable command should speak.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    Text,
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
        // Drift has already printed its own report; repeating "error: drift
        // found" underneath it would add nothing but noise.
        if e.downcast_ref::<DriftFound>().is_some() {
            std::process::exit(EXIT_DRIFT);
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
    let project = Project::discover(&start)?;

    match cli.command {
        Command::Plan {
            target,
            since,
            base,
            check,
            out,
            sql,
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
                let target = target.resolve(&project)?;
                return deploy::cmd_plan_db(&project, &target, out.as_deref(), sql.as_deref());
            }
            let source = match base {
                Some(p) => baseline::Source::File(p),
                None if since == "HEAD" => baseline::default_source(&project),
                None => baseline::Source::Git { rev: since },
            };
            cmd_plan(&project, &source, check, out.as_deref(), sql.as_deref())
        }
        Command::Apply {
            target,
            plan,
            allow,
        } => {
            let target = target.resolve(&project)?;
            deploy::cmd_apply(&project, &target, &plan, &allow.into_iter().collect())
        }
        Command::Validate => cmd_validate(&project),
        Command::Fmt { check } => cmd_fmt(&project, check),
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
            let target = target.resolve(&project)?;
            deploy::cmd_verify(&project, &target, format == OutputFormat::Json)
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
        let existing = if dir.is_dir() {
            pbps_load::schema_files(&dir).unwrap_or_default()
        } else {
            Vec::new()
        };
        if !existing.is_empty() || project.ids_file().exists() {
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

    // Modules are not managed yet (ADR-0002), but staying quiet about them
    // would tell the user the database is fully covered when it is not.
    if !pulled.unmanaged_modules.is_empty() {
        eprintln!(
            "note: {} object(s) exist in the database that pbps does not manage yet:",
            pulled.unmanaged_modules.len()
        );
        for m in &pulled.unmanaged_modules {
            eprintln!("  {} {}", m.kind, m.name);
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
    for (name, table) in &pulled.schema.tables {
        let path = dir.join(format!("{}.{}.yml", name.schema, name.name));
        std::fs::write(&path, pbps_load::render(name, table, &[], None))
            .with_context(|| format!("cannot write `{}`", path.display()))?;
    }
    write_ids(project, &res.ids)?;

    println!(
        "Pulled {} table(s) into `{}` and minted `{}`.",
        pulled.schema.tables.len(),
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

/// Loads the declarations, printing every error in one pass.
fn load(project: &Project) -> anyhow::Result<pbps_load::Loaded> {
    let dir = project.schema_dir();
    if !dir.is_dir() {
        bail!("no declarations directory at `{}`", dir.display());
    }
    pbps_load::load_schema_dir(&dir).map_err(|errs| {
        for e in &errs {
            eprintln!("{:?}", miette::Report::msg(format!("{e}")));
        }
        anyhow::anyhow!("the declarations have {} problem(s)", errs.len())
    })
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

fn cmd_validate(project: &Project) -> anyhow::Result<()> {
    // Identity consistency is validate's job too (SPEC §5.3): two branches each
    // adding a same-named column merge cleanly at the line level — two uids, two
    // lines — so no git conflict flags it, and only a check can.
    //
    // Both halves run before either is allowed to fail. A bad merge produces a
    // broken declaration *and* a scrambled identity file together, and the rule
    // everywhere else in this tool is to report every problem in one pass rather
    // than fix-one-run-again.
    let loaded = load(project);
    let ids = read_ids_opt(project);
    let dialect = dialect(project)?;

    // Three layers, all reported in the same pass: the loader checks shape, the
    // dialect checks what the engine will refuse (a nullable PK column, an
    // IDENTITY on nvarchar), and the identity file checks below stand alone.
    let mut dialect_problems = 0usize;
    if let Ok(l) = &loaded {
        for (name, table) in &l.schema.tables {
            for e in dialect.validate_table(name, table) {
                eprintln!("  {name}: {e}");
                dialect_problems += 1;
            }
        }
        if dialect_problems == 0 {
            println!(
                "Declarations are valid for {}: {} table(s), {} column(s).",
                dialect.name(),
                l.schema.tables.len(),
                l.schema
                    .tables
                    .values()
                    .map(|t| t.columns.len())
                    .sum::<usize>()
            );
        }
    }
    match &ids {
        Ok(Some(i)) => println!(
            "Identity file is consistent: {} table uid(s), {} column uid(s), {} tombstone(s).",
            i.tables.len(),
            i.columns.len(),
            i.tombstones.len()
        ),
        Ok(None) => println!(
            "No identity file yet at `{}`; run `pbps plan` to create it.",
            project.ids_file().display()
        ),
        Err(e) => eprintln!("  {e:#}"),
    }

    match (loaded.is_err() || dialect_problems > 0, ids.is_err()) {
        (false, false) => Ok(()),
        (true, false) => bail!("the declarations did not validate"),
        (false, true) => bail!("the identity file did not validate"),
        (true, true) => bail!("neither the declarations nor the identity file validated"),
    }
}

/// Canonicalizes every declaration file.
///
/// This rewrites each file in full, so ordinary YAML comments are lost —
/// explanatory prose belongs in a `description` field (SPEC §4.2, "the tool owns
/// the file format").
fn cmd_fmt(project: &Project, check: bool) -> anyhow::Result<()> {
    let dir = project.schema_dir();
    let files = pbps_load::schema_files(&dir)
        .with_context(|| format!("cannot list `{}`", dir.display()))?;

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
    let ids = match read_ids(project) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("warning: {e:#}");
            eprintln!("warning: every `renamed_from` will be kept; run `pbps validate` to fix it");
            IdsFile::default()
        }
    };

    let mut changed: Vec<(PathBuf, Vec<Intent>)> = Vec::new();
    for path in &files {
        let original = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read `{}`", path.display()))?;
        let loaded = pbps_load::load_table_str(path, &original).map_err(|errs| {
            for e in &errs {
                eprintln!("{:?}", miette::Report::msg(format!("{e}")));
            }
            anyhow::anyhow!("`{}` does not parse", path.display())
        })?;

        let (pending, absorbed): (Vec<Intent>, Vec<Intent>) = loaded
            .intents
            .iter()
            .cloned()
            .partition(|i| !pbps_diff::intent_is_absorbed(i, &ids));
        let rendered = pbps_load::render(
            &loaded.name,
            &loaded.table,
            &pending,
            loaded.strategy.as_ref(),
        );
        if rendered == original {
            continue;
        }
        changed.push((path.clone(), absorbed));
        if !check {
            std::fs::write(path, &rendered)
                .with_context(|| format!("cannot write `{}`", path.display()))?;
        }
    }

    if changed.is_empty() {
        println!("All {} file(s) are already in canonical form.", files.len());
        return Ok(());
    }

    if check {
        for (p, _) in &changed {
            eprintln!("  needs rewriting: {}", p.display());
        }
        bail!(
            "{} file(s) are not in canonical form; run `pbps fmt`",
            changed.len()
        );
    }
    for (p, absorbed) in &changed {
        println!("rewrote {}", p.display());
        // Deleting a line the user wrote must never be silent. `intent_is_absorbed`
        // cannot tell "this rename happened" from "this rename never applied": a
        // `renamed_from` naming a column that never existed also looks absorbed,
        // because the ids file records no history to distinguish them. Naming what
        // went is the only thing that lets the user catch the typo.
        for i in absorbed {
            println!("  dropped a redundant annotation: {}", report::intent(i));
        }
    }
    Ok(())
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
            bail!("identity could not be resolved")
        }
    }
}

fn cmd_plan(
    project: &Project,
    source: &baseline::Source,
    check: bool,
    out: Option<&std::path::Path>,
    sql: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let loaded = load(project)?;
    let ids = read_ids(project)?;
    let dialect = dialect(project)?;

    let res = match pbps_diff::resolve(&loaded.schema, &ids, &loaded.intents, &context()) {
        Ok(r) => r,
        Err(blockers) => {
            eprintln!("{}", report::blockers(&blockers));
            bail!(
                "some changes could not be decided automatically; express the intent with the commands above and retry"
            );
        }
    };

    if res.ids != ids {
        if check {
            bail!(
                "the identity file is out of date; run `pbps plan` locally and commit `{}` along with your changes",
                project.ids_file().display()
            );
        }
        write_ids(project, &res.ids)?;
        println!("updated {}", project.ids_file().display());
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
        println!("run `pbps fmt` to strip the now-redundant `renamed_from` annotation(s)");
    }

    let base = baseline::load(project, source)?;
    if base.is_empty_fallback {
        eprintln!(
            "warning: the baseline is empty ({}). Everything will be listed as newly created, which is not a real plan against an existing database.",
            base.description
        );
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
    )
    .map_err(|errs| {
        for e in &errs {
            eprintln!("  {e}");
        }
        anyhow::anyhow!("{} change(s) cannot be expressed", errs.len())
    })?;

    println!("Baseline: {}", base.description);
    print!("{}", report::plan(&cs));

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
        plan.git_sha = db::git_sha();
        write_plan(path, &plan)?;
        println!(
            "\nwrote {} (a preview; `apply` will refuse it)",
            path.display()
        );
    }

    if let Some(path) = sql {
        let script = render_sql(&cs, dialect.as_ref(), &base.description)?;
        std::fs::write(path, script)
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        println!("wrote {}", path.display());
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
                .emit(&p.change)
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
