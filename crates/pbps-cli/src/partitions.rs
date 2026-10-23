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

    // The project's rules must be runnable before they can be asked anything.
    // The evaluator skips a rule it cannot run, so with a bad `policies:` block
    // (`validate` reports it as `policy.invalid`) the finding sets below
    // lose what that rule would say, and an empty delta would read as "no rule
    // broken" when the rule was never asked. Absent, empty and unreadable are
    // different answers (DEC-1942.1). Refused beside the declaration problems, for
    // the same reason, and before the no-op answer below, which would
    // otherwise say "nothing to add" for a project `validate` refuses.
    crate::refuse_invalid_policies(project, "the generated partitions")?;

    if series.partitions.is_empty() {
        println!(
            "{parent} already reaches `{}`, which is at or past --until `{until}`; nothing to \
             add.",
            series.start,
            parent = parent.shown()
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

    // And the project's own rules, which `validate` runs and the checks above
    // do not (ADR-0008, #1904, DEC-1941.1). Only an error the tree did not already carry
    // refuses: a finding it had before is not this change's, and a rule left
    // at `warning` or `note` is advice that `validate` prints beside a valid
    // project. A rule whose pattern the existing partitions break must not
    // make the series impossible to extend. The clock is read here, once, for
    // both trees (DEC-1943.1).
    let broken = errors_the_series_adds(
        project,
        dialect.as_ref(),
        &loaded.schema,
        &after.schema,
        crate::unix_seconds(),
    );
    if !broken.is_empty() {
        let lines: Vec<String> = broken
            .iter()
            .map(|f| format!("{}: {}", f.id, f.message))
            .collect();
        bail!(
            "the {} generated partition(s) would break a rule `pbps.yml` sets to `error`, so \
             nothing was written:\n{}\nChange the rule in the `policies:` block (a suppression \
             with a reason, a different severity or pattern), or declare the partitions by hand.",
            series.partitions.len(),
            list(lines.iter().map(String::as_str))
        );
    }

    std::fs::write(&path, &text).with_context(|| format!("cannot write `{}`", path.display()))?;

    let count = series.partitions.len();
    println!(
        "Added {count} partition(s) to {parent} in `{}`, from `{}` to `{until}`:",
        path.display(),
        series.start,
        parent = parent.shown()
    );
    for (i, p) in series.partitions.iter().enumerate() {
        if count > LISTED && (LISTED / 2..count - LISTED / 2).contains(&i) {
            if i == LISTED / 2 {
                println!("  ...");
            }
            continue;
        }
        println!("  {}  [{}, {})", p.name.shown(), p.from, p.to);
    }
    if loaded.schema.tables.values().any(|t| {
        t.partition_of
            .as_ref()
            .is_some_and(|of| of.parent == parent && of.bound == PartitionBound::Default)
    }) {
        println!(
            "{parent} has a DEFAULT partition: a new range beside it is made as a plain table \
             and attached, and takes the DEFAULT's rows of its range only as far as a \
             `pbps move-rows` allows.",
            parent = parent.shown()
        );
    }
    println!(
        "Nothing is created yet. Run `pbps plan` to give them identities, then `pbps plan --db \
         <target>` to see what applying them does."
    );
    Ok(())
}

/// The error-severity policy findings `after` has and `before` did not, with
/// both trees asked at the one instant `now`.
///
/// A finding is compared by rule and message, so two evaluations are
/// comparable only if nothing but the tree differs between them. A suppression
/// carries an `until` date, and a clock read once per tree can straddle the
/// instant it expires: the finding is suppressed in `before`, live in `after`,
/// and the series is refused for an error it did not add (#1943, DEC-1943.1).
fn errors_the_series_adds(
    project: &Project,
    dialect: &dyn pbps_dialect::Dialect,
    before: &pbps_model::Schema,
    after: &pbps_model::Schema,
    now: i64,
) -> Vec<crate::output::Finding> {
    let errors = |schema| {
        crate::raw_declaration_policy_findings(project, dialect, schema, None, now)
            .into_iter()
            .filter(|f| f.severity == pbps_model::Severity::Error)
    };
    let had: std::collections::BTreeSet<(String, String)> =
        errors(before).map(|f| (f.id, f.message)).collect();
    errors(after)
        .filter(|f| !had.contains(&(f.id.clone(), f.message.clone())))
        .map(crate::declaration_finding)
        .collect()
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
        dir.display(),
        parent = parent.shown()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One second before the suppression below expires, and the second it does:
    /// `until: 2026-12-31` stops applying at 2026-12-31T00:00:00Z.
    const BEFORE_EXPIRY: i64 = 1_798_675_199;
    const AT_EXPIRY: i64 = BEFORE_EXPIRY + 1;
    /// Well clear of the date, either way.
    const LONG_BEFORE: i64 = BEFORE_EXPIRY - 30 * 86_400;
    const LONG_AFTER: i64 = AT_EXPIRY + 30 * 86_400;

    fn project() -> Project {
        let yaml = "dialect: postgres\n\
                    policies:\n  rules:\n    naming.table: {severity: error, pattern: \"^[a-z][a-z0-9_]*$\"}\n  \
                    suppress:\n    - {rule: naming.table, on: app.Legacy, reason: inherited, until: 2026-12-31}\n";
        Project {
            root: PathBuf::new(),
            config: pbps_config::Config::parse(yaml, Path::new("pbps.yml")).unwrap(),
        }
    }

    fn schema(names: &[&str]) -> pbps_model::Schema {
        let mut schema = pbps_model::Schema::default();
        for name in names {
            schema
                .tables
                .insert(TableName::new("app", *name), Table::default());
        }
        schema
    }

    fn adds(before: &[&str], after: &[&str], now: i64) -> Vec<String> {
        let dialect = pbps_pg::Postgres::new();
        errors_the_series_adds(&project(), &dialect, &schema(before), &schema(after), now)
            .into_iter()
            .map(|f| f.message)
            .collect()
    }

    /// The window of #1943, closed: the tree already carries `app.Legacy`,
    /// which a suppression excuses until 2026-12-31, and the series adds a
    /// well-named partition. Asked at the one instant, the same set comes back
    /// for both trees on either side of the expiry, so the delta is empty
    /// whenever that instant falls.
    #[test]
    fn a_suppression_expiring_between_two_reads_does_not_count_as_added() {
        let (before, after) = (["Legacy"], ["Legacy", "ev_2027_01"]);
        for now in [LONG_BEFORE, BEFORE_EXPIRY, AT_EXPIRY, LONG_AFTER] {
            assert_eq!(adds(&before, &after, now), Vec::<String>::new(), "{now}");
        }
    }

    /// The hazard exists and the instant is honoured: the shared function
    /// excuses `app.Legacy` before the date and names it from the date on, so
    /// two reads that straddle it are not two answers about the same tree.
    /// The negative cases are the instants clear of the date, which agree.
    #[test]
    fn the_policy_findings_depend_on_the_instant_they_are_asked_at() {
        let dialect = pbps_pg::Postgres::new();
        let at = |now| {
            crate::declaration_policy_findings(
                &project(),
                &dialect,
                &schema(&["Legacy"]),
                None,
                now,
            )
            .len()
        };
        assert_eq!(at(LONG_BEFORE), 0);
        assert_eq!(at(BEFORE_EXPIRY), 0);
        assert_eq!(at(AT_EXPIRY), 1);
        assert_eq!(at(LONG_AFTER), 1);
    }

    /// The control that keeps the test above honest: a partition that breaks
    /// the rule is still named, with the suppression live and with it expired.
    #[test]
    fn an_error_the_series_does_add_is_named_at_any_instant() {
        let (before, after) = (["Legacy"], ["Legacy", "Ev_2027_01"]);
        for now in [LONG_BEFORE, BEFORE_EXPIRY, AT_EXPIRY, LONG_AFTER] {
            let added = adds(&before, &after, now);
            assert_eq!(added.len(), 1, "{now}: {added:?}");
            assert!(added[0].contains("Ev_2027_01"), "{now}: {added:?}");
        }
    }

    #[test]
    fn declaration_keys_stay_raw_until_the_series_has_compared_them() {
        let raw = "Bad\n\u{1b}[31m\u{9b}\\\u{202e}名前";
        let literal = raw.replace('\n', r"\n");
        let dialect = pbps_pg::Postgres::new();
        let project = project();
        let before = schema(&[raw]);
        let keys =
            crate::raw_declaration_policy_findings(&project, &dialect, &before, None, LONG_AFTER);
        let key = format!("app.{raw}: table `{raw}` does not match `^[a-z][a-z0-9_]*$`");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].message, key);
        assert_eq!(
            keys[0].subject.as_deref(),
            Some(format!("app.{raw}").as_str())
        );
        let presented =
            crate::declaration_policy_findings(&project, &dialect, &before, None, LONG_AFTER);
        let shown = pbps_model::escape::Escaped(&key).to_string();
        assert_eq!(presented[0].message, shown);
        assert_eq!(
            serde_json::to_value(&presented[0]).unwrap()["message"],
            shown
        );
        assert!(
            errors_the_series_adds(&project, &dialect, &before, &before, LONG_AFTER).is_empty()
        );
        let after = schema(&[raw, &literal]);
        let added = errors_the_series_adds(&project, &dialect, &before, &after, LONG_AFTER);
        assert_eq!(added.len(), 1);
        let literal_key =
            format!("app.{literal}: table `{literal}` does not match `^[a-z][a-z0-9_]*$`");
        assert_eq!(
            added[0].message,
            pbps_model::escape::Escaped(&literal_key).to_string()
        );
        assert_ne!(added[0].message, shown);
        assert!(!added[0].message.chars().any(char::is_control));
    }
}
