//! `pbps` — declarative database schema version control.

mod baseline;
mod report;

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};

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

    /// Reverse-generate declarations from an existing database
    Pull {
        /// ADO.NET-style connection string, e.g.
        /// "Server=host,1433;Database=app;User Id=u;Password=p;TrustServerCertificate=true"
        #[arg(long)]
        db: String,

        /// Overwrite existing declarations and identity file
        #[arg(long)]
        force: bool,
    },
}

fn main() {
    if let Err(e) = run() {
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
            since,
            base,
            check,
            out,
            sql,
        } => {
            let source = match base {
                Some(p) => baseline::Source::File(p),
                None if since == "HEAD" => baseline::default_source(&project),
                None => baseline::Source::Git { rev: since },
            };
            cmd_plan(&project, &source, check, out.as_deref(), sql.as_deref())
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
        Command::Pull { db, force } => cmd_pull(&project, &db, force),
    }
}

/// Reverse-generates declarations from a live database — the adoption path.
///
/// `pull` designates one source-of-truth environment (SPEC §9.2): everything the
/// database has and the model can express becomes YAML, everything it cannot
/// express is printed as a warning, and a fresh identity file is minted so the
/// next `plan` starts from "no changes".
fn cmd_pull(project: &Project, db: &str, force: bool) -> anyhow::Result<()> {
    if project.config.dialect != DialectName::Mssql {
        bail!(
            "pull is only implemented for mssql (this project's pbps.yml selects `{}`)",
            project.config.dialect
        );
    }

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

    // The runtime lives for exactly this one command: the tool is a CLI, not a
    // server, and only the driver needs async at all.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let pulled = rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(db).await?;
        pbps_mssql::catalog::introspect(&mut conn).await
    })?;

    for w in &pulled.warnings {
        eprintln!("warning: {w}");
    }

    // Mint fresh identity for everything pulled. resolve with an empty baseline
    // can produce no blockers (nothing disappears from empty), so a failure here
    // is a bug, not a user problem.
    let res = pbps_diff::resolve(&pulled.schema, &IdsFile::default(), &[], &context())
        .map_err(|b| anyhow::anyhow!("pull could not mint identities: {} blocker(s)", b.len()))?;

    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create `{}`", dir.display()))?;
    for (name, table) in &pulled.schema.tables {
        let path = dir.join(format!("{}.{}.yml", name.schema, name.name));
        std::fs::write(&path, pbps_load::render(name, table, &[]))
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
        let rendered = pbps_load::render(&loaded.name, &loaded.table, &pending);
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
        std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&cs)?))
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        println!("\nwrote {}", path.display());
    }

    if let Some(path) = sql {
        let script = render_sql(&cs, dialect.as_ref(), &base.description)?;
        std::fs::write(path, script)
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
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
    let mut statements = Vec::new();
    for p in &cs.changes {
        statements.extend(
            dialect
                .emit(&p.change)
                .map_err(|e| anyhow::anyhow!("cannot render a change as SQL: {e}"))?,
        );
    }
    Ok(format!(
        "-- Generated by pbps against baseline: {baseline}\n-- A preview, not an applyable plan. Never hand-edit this file.\n\n{}",
        pbps_dialect::render_script(&statements, dialect.batch_separator())
    ))
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
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
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
}
