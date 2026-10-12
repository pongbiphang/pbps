//! `pbps partitions extend`: declare the next range partitions of a
//! partitioned table (#1789, DEC-1789.1).
//!
//! A RANGE-partitioned table needs a partition to exist before a row arrives
//! for it, and keeping that series ahead of the clock is a chore someone does
//! by hand until the day they do not. This writes the declarations; it does
//! not create anything. The partitions it adds are plain entries in the
//! parent's file, so they take the whole ordinary road: `plan` gives them
//! identities, `plan --db` refuses a bound the engine would spell otherwise,
//! and `apply` creates them behind the gate.
//!
//! **A generator, not a clock.** Nothing here knows today's date. The end is
//! an argument, so the same tree and the same words give the same file on
//! every machine and in every CI run, and the diff in git is the decision.
//! What it does not do is just as deliberate: it writes no identity (`plan`
//! assigns them, as for any table added by hand, SPEC §6.2), it touches no
//! file but the parent's, and it never connects.
//!
//! Where the series goes and how it is spelled is the dialect's
//! ([`pbps_dialect::Dialect::range_series`]); this file is the part that
//! reads and writes the project.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use pbps_config::Project;
use pbps_model::{BoundDatum, PartitionBound, PartitionOf, Table, TableName};

/// How many of the new partitions are listed before the middle is elided.
const LISTED: usize = 8;

pub fn cmd_extend(
    project: &Project,
    table: &str,
    every: &str,
    until: &str,
    from: Option<&str>,
) -> anyhow::Result<()> {
    let parent: TableName = table
        .parse()
        .with_context(|| format!("`{table}` is not a table name; write it as `schema.table`"))?;
    let dialect = crate::dialect(project)?;
    let loaded = crate::load(project, dialect.as_ref())?;

    let series = dialect.range_series(&pbps_dialect::RangeSeriesRequest {
        schema: &loaded.schema,
        parent: &parent,
        every,
        until,
        from,
    })?;

    // What the engine cannot extend is said first, by name. Only then must the
    // tree be valid, so that what the declaration checks say afterwards is
    // about this change alone.
    let before = crate::declaration_problems(&loaded, dialect.as_ref());
    if !before.is_empty() {
        bail!(
            "the declarations already have {} problem(s); fix them before extending a series \
             (`pbps validate`):\n{}",
            before.len(),
            list(before.iter().map(|(_, problem)| problem.as_str()))
        );
    }

    if series.partitions.is_empty() {
        println!(
            "{parent} already reaches `{}`, which is at or past --until `{until}`; nothing to \
             add.",
            series.start
        );
        return Ok(());
    }

    let (path, original, file) = parents_file(project, &parent)?;

    // Only a file `fmt` would leave alone is rewritten, so that the diff holds
    // the new entries and nothing else: re-rendering a file written by hand
    // would reformat all of it under the same commit.
    let ids = crate::read_ids(project)?;
    let files = pbps_load::schema_files(&project.schema_dir())
        .with_context(|| format!("cannot list `{}`", project.schema_dir().display()))?;
    let declared = crate::fmt_declared(&files);
    let (canonical, _) = crate::canonical_table(&file, &ids, &declared);
    if canonical != original {
        bail!(
            "`{}` is not in canonical form, and rewriting it would change more than the new \
             partitions; run `pbps fmt` and commit that first",
            path.display()
        );
    }

    let mut extended = file.clone();
    for p in &series.partitions {
        extended.partitions.push((
            p.name.clone(),
            Table {
                partition_of: Some(PartitionOf {
                    parent: parent.clone(),
                    bound: PartitionBound::Range {
                        from: vec![BoundDatum::Value(p.from.clone())],
                        to: vec![BoundDatum::Value(p.to.clone())],
                    },
                    columns: Default::default(),
                }),
                ..Table::default()
            },
        ));
    }
    let (text, _) = crate::canonical_table(&extended, &ids, &declared);

    // Read back what is about to be written, as every command will: the file
    // must hold the parent as it was and each new partition as generated, and
    // be what `fmt` writes. Anything else is this command's own bug, and
    // writing it would hand the user a file the next command refuses.
    let reloaded = pbps_load::load_table_str(&path, &text).map_err(|errors| {
        anyhow::anyhow!(
            "the rewritten `{}` does not read back ({} error(s)); nothing was written",
            path.display(),
            errors.len()
        )
    })?;
    let mut expected = extended.partitions.clone();
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    let mut got = reloaded.partitions.clone();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    if reloaded.table != file.table
        || got != expected
        || crate::canonical_table(&reloaded, &ids, &declared).0 != text
    {
        bail!(
            "the rewritten `{}` does not read back as generated; nothing was written",
            path.display()
        );
    }

    // And the whole project with the change in it, through the checks every
    // command that reads declarations runs. Only then is the file written.
    let mut after = loaded.clone();
    for (name, partition) in reloaded.partitions {
        after.schema.tables.insert(name, partition);
    }
    let problems = crate::declaration_problems(&after, dialect.as_ref());
    if !problems.is_empty() {
        bail!(
            "the {} generated partition(s) would make the declarations invalid, so nothing was \
             written:\n{}",
            series.partitions.len(),
            list(problems.iter().map(|(_, problem)| problem.as_str()))
        );
    }

    std::fs::write(&path, &text).with_context(|| format!("cannot write `{}`", path.display()))?;

    let count = series.partitions.len();
    println!(
        "Added {count} partition(s) to {parent} in `{}`, from `{}` to `{until}`:",
        path.display(),
        series.start
    );
    for (i, p) in series.partitions.iter().enumerate() {
        if count > LISTED && (LISTED / 2..count - LISTED / 2).contains(&i) {
            if i == LISTED / 2 {
                println!("  ...");
            }
            continue;
        }
        println!("  {}  [{}, {})", p.name, p.from, p.to);
    }
    if loaded.schema.tables.values().any(|t| {
        t.partition_of
            .as_ref()
            .is_some_and(|of| of.parent == parent && of.bound == PartitionBound::Default)
    }) {
        println!(
            "{parent} has a DEFAULT partition: a new range beside it is made as a plain table \
             and attached, and takes the DEFAULT's rows of its range only as far as a \
             `pbps move-rows` allows."
        );
    }
    println!(
        "Nothing is created yet. Run `pbps plan` to give them identities, then `pbps plan --db \
         <target>` to see what applying them does."
    );
    Ok(())
}

/// The file that declares `parent`, as written, and what it loaded as.
fn parents_file(
    project: &Project,
    parent: &TableName,
) -> anyhow::Result<(PathBuf, String, pbps_load::LoadedTable)> {
    let dir = project.schema_dir();
    for path in
        pbps_load::schema_files(&dir).with_context(|| format!("cannot list `{}`", dir.display()))?
    {
        let text = read(&path)?;
        if let Ok(pbps_load::LoadedFile::Table(table)) = pbps_load::load_file_str(&path, &text)
            && table.name == *parent
        {
            return Ok((path, text, *table));
        }
    }
    bail!(
        "no file under `{}` declares `{parent}` as a table",
        dir.display()
    )
}

fn read(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("cannot read `{}`", path.display()))
}

fn list<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    lines
        .map(|line| format!("  - {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}
