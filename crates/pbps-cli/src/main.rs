//! `pbps` — declarative database schema version control.

mod baseline;
mod report;

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};

use pbps_config::Project;
use pbps_dialect::MinimalDialect;
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
        } => {
            let source = match base {
                Some(p) => baseline::Source::File(p),
                None if since == "HEAD" => baseline::default_source(&project),
                None => baseline::Source::Git { rev: since },
            };
            cmd_plan(&project, &source, check, out.as_deref())
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

fn read_ids(project: &Project) -> anyhow::Result<IdsFile> {
    let path = project.ids_file();
    if !path.exists() {
        return Ok(IdsFile::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read identity file `{}`", path.display()))?;
    let ids: IdsFile = serde_json::from_str(&text)
        .with_context(|| format!("identity file `{}` is malformed", path.display()))?;
    ids.validate()?;
    Ok(ids)
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
    let loaded = load(project)?;
    // Identity consistency is validate's job too (SPEC §5.3): two branches each
    // adding a same-named column merge cleanly at the line level — two uids, two
    // lines — so no git conflict flags it, and only a check can.
    let ids = read_ids(project)?;
    println!(
        "Declarations are valid: {} table(s), {} column(s).",
        loaded.schema.tables.len(),
        loaded
            .schema
            .tables
            .values()
            .map(|t| t.columns.len())
            .sum::<usize>()
    );
    if project.ids_file().exists() {
        println!(
            "Identity file is consistent: {} table uid(s), {} column uid(s), {} tombstone(s).",
            ids.tables.len(),
            ids.columns.len(),
            ids.tombstones.len()
        );
    }
    Ok(())
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
    let ids = read_ids(project)?;

    let mut changed = Vec::new();
    for path in &files {
        let original = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read `{}`", path.display()))?;
        let loaded = pbps_load::load_table_str(path, &original).map_err(|errs| {
            for e in &errs {
                eprintln!("{:?}", miette::Report::msg(format!("{e}")));
            }
            anyhow::anyhow!("`{}` does not parse", path.display())
        })?;

        let pending: Vec<_> = loaded
            .intents
            .iter()
            .filter(|i| !pbps_diff::intent_is_absorbed(i, &ids))
            .cloned()
            .collect();
        let rendered = pbps_load::render(&loaded.name, &loaded.table, &pending);
        if rendered == original {
            continue;
        }
        changed.push(path.clone());
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
        for p in &changed {
            eprintln!("  needs rewriting: {}", p.display());
        }
        bail!(
            "{} file(s) are not in canonical form; run `pbps fmt`",
            changed.len()
        );
    }
    for p in &changed {
        println!("rewrote {}", p.display());
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
) -> anyhow::Result<()> {
    let loaded = load(project)?;
    let ids = read_ids(project)?;

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
        &MinimalDialect,
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
    Ok(())
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
