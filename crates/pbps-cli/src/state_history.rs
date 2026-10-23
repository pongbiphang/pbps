//! Read recorded states without adopting, applying or rewriting identity intent.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::Context as _;
use pbps_config::Project;
use pbps_db::{LedgerEntry, LedgerError};
use pbps_model::{StateSnapshot, Text, Unreadable};

use crate::{adopt, db, engine, output};

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct Entry {
    id: i64,
    applied_at: String,
    // The model's versioned serialization, as DriftReport carries ChangeSet.
    #[schemars(with = "serde_json::Value")]
    snapshot: StateSnapshot,
}

impl From<LedgerEntry> for Entry {
    fn from(value: LedgerEntry) -> Self {
        Self {
            id: value.id,
            applied_at: value.applied_at,
            snapshot: value.snapshot,
        }
    }
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct ShowData {
    environment: String,
    entry: Entry,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct DiffData {
    environment: String,
    from_id: i64,
    to_id: i64,
    #[schemars(with = "serde_json::Value")]
    changes: pbps_model::ChangeSet,
    unexpressible: Vec<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct ExportData {
    environment: String,
    entry_id: i64,
    state_version: u32,
    files: Vec<PathBuf>,
    removed: Vec<PathBuf>,
}

struct Refusal {
    id: &'static str,
    message: String,
}

impl Refusal {
    fn new(id: &'static str, message: impl Into<String>) -> Self {
        Self {
            id,
            message: message.into(),
        }
    }

    fn ledger(error: LedgerError, label: &str) -> Self {
        let label = Text::new(label).shown().to_string();
        match error {
            LedgerError::NotInitialized => Self::new(
                "state.uninitialized",
                format!(
                    "{label}: this database has no pbps ledger; no historical state can be read. \
                     Run pbps bootstrap, or pbps baseline --reason to adopt it."
                ),
            ),
            error
            @ (LedgerError::Db(_) | LedgerError::BadEntry { .. } | LedgerError::Locked(_)) => {
                Self::new(
                    "state.unreadable",
                    format!("{label}: the ledger could not be read: {error}"),
                )
            }
        }
    }
}

fn answer<T>(command: &'static str, json: bool, result: Result<T, Refusal>) -> anyhow::Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            output::or_unanswerable(command, json, error.id, Err(anyhow::anyhow!(error.message)))
        }
    }
}

async fn read_entry(
    conn: &mut pbps_db::Conn,
    id: Option<i64>,
    label: &str,
) -> Result<LedgerEntry, Refusal> {
    let stored = engine::entry(conn, id)
        .await
        .map_err(|e| Refusal::ledger(e, label))?;
    let shown_label = Text::new(label).shown().to_string();
    let Some(stored) = stored else {
        // Only ask whether there are any rows when a selected id was absent.
        // The timeline does not require unrelated snapshots to decode.
        let empty = id.is_none()
            || engine::timeline(conn, 1)
                .await
                .map_err(|e| Refusal::ledger(e, label))?
                .is_empty();
        return Err(match (empty, id) {
            (false, Some(id)) => Refusal::new(
                "state.entry-not-found",
                format!(
                    "{shown_label}: the ledger has no entry #{id}; use pbps state list to choose an existing entry."
                ),
            ),
            (true, _) | (false, None) => Refusal::new(
                "state.no-entries",
                format!(
                    "{shown_label}: the ledger exists but has no entries. \
                     Record a state with pbps snapshot or pbps baseline --reason."
                ),
            ),
        });
    };
    let id = stored.id;
    let entry = stored.read().map_err(|error| {
        let finding = match &error {
            Unreadable::UnsupportedVersion(_) => "state.entry-unsupported-version",
            Unreadable::Malformed(_) | Unreadable::Denied(_) => "state.unreadable",
        };
        Refusal::new(
            finding,
            format!(
                "{shown_label}: entry #{id}'s snapshot is unreadable: {}",
                Text::new(error.to_string()).shown()
            ),
        )
    })?;
    entry.snapshot.ids.clone().validated().map_err(|error| {
        let message = format!(
            "{shown_label}: entry #{id}'s snapshot has an inconsistent identity mapping: {error}"
        );
        Refusal::new("state.unreadable", message)
    })?;
    Ok(entry)
}

pub fn cmd_show(target: &db::Target, id: i64, json: bool) -> anyhow::Result<()> {
    const COMMAND: &str = "state show";
    let runtime = output::or_unanswerable(COMMAND, json, "db.runtime", db::runtime())?;
    let mut conn = output::or_unanswerable(
        COMMAND,
        json,
        "environment.unreachable",
        runtime.block_on(db::connect(target)),
    )?;
    let entry = answer(
        COMMAND,
        json,
        runtime.block_on(read_entry(&mut conn, Some(id), &target.label)),
    )?;
    let data = ShowData {
        environment: target.label.clone(),
        entry: entry.into(),
    };
    if json {
        output::Report::new(COMMAND, Vec::new(), Some(data)).emit_json()
    } else {
        // Walk values before formatting, so JSON/YAML quoting does not become
        // a first escape followed by a second terminal escape.
        print!("{}", render_value(&serde_json::to_value(&data)?, 0));
        Ok(())
    }
}

fn render_value(value: &serde_json::Value, indent: usize) -> String {
    let pad = " ".repeat(indent);
    match value {
        serde_json::Value::Object(fields) => fields
            .iter()
            .map(|(name, value)| {
                format!(
                    "{pad}{}:\n{}",
                    Text::new(name).shown(),
                    render_value(value, indent + 2)
                )
            })
            .collect(),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| format!("{pad}-\n{}", render_value(value, indent + 2)))
            .collect(),
        serde_json::Value::String(text) => format!("{pad}{}\n", Text::new(text).shown()),
        other @ (serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)) => format!("{pad}{other}\n"),
    }
}

pub fn cmd_diff(
    project: &Project,
    target: &db::Target,
    from: i64,
    to: Option<i64>,
    json: bool,
) -> anyhow::Result<()> {
    const COMMAND: &str = "state diff";
    let dialect = output::or_unanswerable(
        COMMAND,
        json,
        "dialect.unavailable",
        crate::dialect(project),
    )?;
    let runtime = output::or_unanswerable(COMMAND, json, "db.runtime", db::runtime())?;
    let mut conn = output::or_unanswerable(
        COMMAND,
        json,
        "environment.unreachable",
        runtime.block_on(db::connect(target)),
    )?;
    let (from, to) = answer(
        COMMAND,
        json,
        runtime.block_on(async {
            let from = read_entry(&mut conn, Some(from), &target.label).await?;
            let to = read_entry(&mut conn, to, &target.label).await?;
            Ok((from, to))
        }),
    )?;
    let mut before = from.snapshot.declared.overlay(&from.snapshot.schema);
    let mut after = to.snapshot.declared.overlay(&to.snapshot.schema);
    pbps_dialect::name_read_sequences(&mut before, dialect.as_ref());
    pbps_dialect::name_read_sequences(&mut after, dialect.as_ref());
    let mut hints = pbps_model::Hints {
        module_deps: to.snapshot.module_deps.clone(),
        ..Default::default()
    };
    for (id, deps) in &from.snapshot.module_deps {
        if !after.modules.contains_key(id) {
            hints.module_deps.insert(id.clone(), deps.clone());
        }
    }
    // Baseline and forced snapshots record what was observed, which can hold
    // columns without identities and retain identities for missing columns.
    // Complete each side from its own schema while keeping recorded matches.
    let before_ids = pbps_diff::observed_ids(&before, &from.snapshot.ids);
    let after_ids = pbps_diff::observed_ids(&after, &to.snapshot.ids);
    let diffed = pbps_diff::diff_partial(
        pbps_diff::Side {
            schema: &before,
            ids: &before_ids,
        },
        pbps_diff::Side {
            schema: &after,
            ids: &after_ids,
        },
        dialect.as_ref(),
        &hints,
    );
    let data = DiffData {
        environment: target.label.clone(),
        from_id: from.id,
        to_id: to.id,
        changes: diffed.changes,
        unexpressible: diffed.errors.iter().map(ToString::to_string).collect(),
    };
    let findings: Vec<_> = data
        .unexpressible
        .iter()
        .map(|message| output::Finding::error("change.unexpressible", message))
        .collect();
    if json {
        let report = output::Report::new(COMMAND, findings, Some(&data));
        if !data.unexpressible.is_empty() {
            // Keep the partial report, but never advertise a complete answer.
            println!("{}", serde_json::to_string_pretty(&report.unanswerable())?);
        } else {
            return report.emit_json();
        }
    } else {
        println!(
            "Environment: {}\nEntry #{} -> entry #{}",
            Text::new(&target.label).shown(),
            data.from_id,
            data.to_id
        );
        if data.changes.is_empty() && data.unexpressible.is_empty() {
            println!("No changes.");
        } else {
            print!("{}", crate::report::changes(&data.changes));
            eprint!("{}", output::human(&findings));
        }
    }
    if data.unexpressible.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "the historical comparison contains differences pbps cannot express as changes"
        )
    }
}

pub fn cmd_export(
    project: &Project,
    target: &db::Target,
    id: i64,
    force: bool,
    json: bool,
) -> anyhow::Result<()> {
    const COMMAND: &str = "state export";
    let dialect = output::or_unanswerable(
        COMMAND,
        json,
        "dialect.unavailable",
        crate::dialect(project),
    )?;
    let runtime = output::or_unanswerable(COMMAND, json, "db.runtime", db::runtime())?;
    let mut conn = output::or_unanswerable(
        COMMAND,
        json,
        "environment.unreachable",
        runtime.block_on(db::connect(target)),
    )?;
    let entry = answer(
        COMMAND,
        json,
        runtime.block_on(read_entry(&mut conn, Some(id), &target.label)),
    )?;
    let (mut data, findings) = answer(
        COMMAND,
        json,
        export(project, &entry, force, dialect.as_ref()).map_err(|mut refusal| {
            refusal.message = format!("{}: {}", Text::new(&target.label).shown(), refusal.message);
            refusal
        }),
    )?;
    data.environment = target.label.clone();
    if json {
        output::Report::new(COMMAND, findings, Some(&data)).emit_json()
    } else {
        println!(
            "Exported entry #{} into {} declaration file(s).",
            data.entry_id,
            data.files.len()
        );
        print!("{}", output::human(&findings));
        Ok(())
    }
}

fn export(
    project: &Project,
    entry: &LedgerEntry,
    force: bool,
    dialect: &dyn pbps_dialect::Dialect,
) -> Result<(ExportData, Vec<output::Finding>), Refusal> {
    if entry.snapshot.version != pbps_model::state::CURRENT_VERSION {
        return Err(Refusal::new(
            "state.export-version",
            format!(
                "Entry #{} has state version {}; state export requires version {}. \
                 Inspect it with state show and reconstruct reviewed declarations, or recover them from git.",
                entry.id,
                entry.snapshot.version,
                pbps_model::state::CURRENT_VERSION,
            ),
        ));
    }
    let dir = project.schema_dir();
    let invalid = |e: anyhow::Error| {
        Refusal::new(
            "state.export-invalid",
            format!(
                "Entry #{} cannot be exported as valid declarations: {e:#}. Nothing was written.",
                entry.id
            ),
        )
    };
    let present = (|| -> anyhow::Result<_> {
        match std::fs::metadata(&dir) {
            Ok(meta) if meta.is_dir() => pbps_load::schema_files(&dir).map_err(Into::into),
            Ok(_) => anyhow::bail!("`{}` is not a directory", dir.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink is present, not an empty destination.
                match std::fs::symlink_metadata(&dir) {
                    Ok(_) => anyhow::bail!("`{}` is a dangling link", dir.display()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                    Err(error) => Err(error.into()),
                }
            }
            Err(e) => Err(e.into()),
        }
    })()
    .map_err(invalid)?;
    if !force && !present.is_empty() {
        return Err(Refusal::new(
            "state.export-overwrite",
            "This project already has declarations; state export would overwrite them. \
             Re-run with --force to replace them.",
        ));
    }
    let schema = entry.snapshot.declared.overlay(&entry.snapshot.schema);
    let (stage, files, removed, mut findings) = (|| -> anyhow::Result<_> {
        let stage = adopt::Stage::new("state-export")?;
        let staged = adopt::write_declarations_with_deps(
            stage.path(),
            &schema,
            &Default::default(),
            &entry.snapshot.module_deps,
        )?;
        let loaded = adopt::check_staged(stage.path(), dialect)?;
        let findings = adopt::refuse_policy_errors(project, &loaded, dialect)?;
        let files: BTreeSet<_> = staged
            .iter()
            .map(|path| path.strip_prefix(stage.path()).map(|p| dir.join(p)))
            .collect::<Result<_, _>>()?;
        let removed: Vec<_> = present
            .iter()
            .filter(|path| !files.contains(*path))
            .cloned()
            .collect();
        let root = crate::paths::absolute(&dir)?;
        // Force authorizes declaration replacement, never identity/config writes
        // through an alias or a generated filename that names a protected input.
        for path in files.iter().chain(&removed) {
            anyhow::ensure!(
                crate::paths::absolute(path)?.starts_with(&root),
                "`{}` resolves outside the declaration directory",
                path.display()
            );
            for protected in [project.ids_file(), project.config_file()] {
                anyhow::ensure!(
                    !crate::paths::same_file(path, &protected)?,
                    "`{}` aliases protected project input `{}`",
                    path.display(),
                    protected.display()
                );
            }
        }
        Ok((stage, files, removed, findings))
    })()
    .map_err(invalid)?;
    // No project write precedes the staged load and validation above. Publish
    // those exact validated bytes; never rerender a different representation.
    (|| -> anyhow::Result<()> {
        std::fs::create_dir_all(&dir)?;
        // Remove old directory entries before publishing: a stale spelling can
        // alias an output on a case-insensitive filesystem. Replacing every
        // declaration entry also breaks old symlinks instead of following them.
        for path in &present {
            std::fs::remove_file(path)
                .with_context(|| format!("cannot remove old declaration `{}`", path.display()))?;
        }
        for path in &files {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let source = stage.path().join(path.strip_prefix(&dir)?);
            std::fs::copy(&source, path)
                .with_context(|| format!("cannot publish `{}`", path.display()))?;
        }
        Ok(())
    })()
    .map_err(|error| {
        let message = format!(
            "Entry #{} could not be fully published: {error:#}. \
             Inspect the declaration files before retrying.",
            entry.id
        );
        Refusal::new("state.export-invalid", message)
    })?;
    if dialect.creates_public_executable_routines()
        && schema
            .modules
            .keys()
            .any(|id| matches!(id, pbps_model::ModuleId::Routine(_)))
    {
        let message = concat!(
            "Snapshots do not record historical public_execute. ",
            "Export omits it and keeps the closed default; ",
            "review routine execution permissions before committing."
        );
        findings.push(output::Finding::note(
            "state.export-public-execute",
            message,
        ));
    }
    let message = concat!(
        "Export restores recorded structure and reference data, ",
        "not business rows, sequence counters or role memberships. ",
        "Historical identities and binding evidence were not written."
    );
    findings.push(output::Finding::note("state.export-scope", message));
    let message = concat!(
        "Review and commit the declarations, then run pbps plan --db (or --env) ",
        "and apply the checksum-pinned approved plan. ",
        "Record any required rename/drop intent with the existing commands; ",
        "schema.ids.json is unchanged."
    );
    findings.push(output::Finding::note("state.export-next", message));
    Ok((
        ExportData {
            environment: String::new(),
            entry_id: entry.id,
            state_version: entry.snapshot.version,
            files: files.into_iter().collect(),
            removed,
        },
        findings,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn fixture(name: &str) -> (adopt::Stage, Project, LedgerEntry) {
        let stage = adopt::Stage::new(name).unwrap();
        std::fs::write(stage.path().join("pbps.yml"), "dialect: mssql\n").unwrap();
        let project = Project::discover(stage.path()).unwrap();
        let name = "dbo.product".parse().unwrap();
        let mut table = pbps_model::Table::default();
        table.columns.insert(
            "code".into(),
            pbps_model::Column::new("int".parse().unwrap()),
        );
        let mut schema = pbps_model::Schema::default();
        schema.tables.insert(name, table);
        let entry = LedgerEntry {
            id: 7,
            applied_at: "2026-10-23T00:00:00".into(),
            snapshot: StateSnapshot::new(
                pbps_model::StateKind::Baseline,
                schema,
                Default::default(),
                "operator",
            ),
        };
        (stage, project, entry)
    }

    #[test]
    fn state_history_old_export_versions_are_named_and_write_nothing() {
        let (_stage, project, mut entry) = fixture("history-old");
        entry.snapshot.version -= 1;
        let error = export(
            &project,
            &entry,
            true,
            crate::dialect(&project).unwrap().as_ref(),
        )
        .err()
        .unwrap();
        assert_eq!(error.id, "state.export-version");
        assert!(
            error
                .message
                .contains(&format!("state version {}", entry.snapshot.version))
        );
        assert!(!project.schema_dir().exists());
    }

    #[test]
    fn state_history_overwrite_refusal_preserves_existing_declarations_and_ids() {
        let (_stage, project, entry) = fixture("history-overwrite");
        std::fs::create_dir(project.schema_dir()).unwrap();
        let path = project.schema_dir().join("custom.yml");
        std::fs::write(&path, "keep these exact bytes\n").unwrap();
        std::fs::write(project.ids_file(), "identity intent, unchanged\n").unwrap();
        let dialect = crate::dialect(&project).unwrap();
        let error = export(&project, &entry, false, dialect.as_ref())
            .err()
            .unwrap();
        assert_eq!(error.id, "state.export-overwrite");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "keep these exact bytes\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.ids_file()).unwrap(),
            "identity intent, unchanged\n"
        );
        // Force changes declaration files, but neither parses nor writes ids.
        let (data, _) = export(&project, &entry, true, dialect.as_ref())
            .ok()
            .unwrap();
        assert_eq!(data.removed, vec![path.clone()]);
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(project.ids_file()).unwrap(),
            "identity intent, unchanged\n"
        );
    }

    #[test]
    fn state_history_staged_validation_precedes_every_project_write() {
        let (_stage, project, mut entry) = fixture("history-staged");
        let table = entry.snapshot.schema.tables.values_mut().next().unwrap();
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec![],
            storage_parameters: Default::default(),
        });
        std::fs::create_dir(project.schema_dir()).unwrap();
        let file = project.schema_dir().join("keep.yml");
        std::fs::write(&file, "do not touch\n").unwrap();
        std::fs::write(project.ids_file(), "original identity bytes\n").unwrap();
        let error = export(
            &project,
            &entry,
            true,
            crate::dialect(&project).unwrap().as_ref(),
        )
        .err()
        .unwrap();
        assert_eq!(error.id, "state.export-invalid");
        assert!(error.message.contains("Nothing was written"));
        assert_eq!(std::fs::read_dir(project.schema_dir()).unwrap().count(), 1);
        assert_eq!(std::fs::read_to_string(file).unwrap(), "do not touch\n");
        assert_eq!(
            std::fs::read_to_string(project.ids_file()).unwrap(),
            "original identity bytes\n"
        );
    }

    #[test]
    fn state_history_force_cannot_overwrite_an_identity_file_alias() {
        let (stage, _, entry) = fixture("history-identity-alias");
        std::fs::write(
            stage.path().join("pbps.yml"),
            "dialect: mssql\nids_file: schema/dbo.product.yml\n",
        )
        .unwrap();
        let project = Project::discover(stage.path()).unwrap();
        std::fs::create_dir(project.schema_dir()).unwrap();
        std::fs::write(project.ids_file(), "identity bytes\n").unwrap();
        let error = export(
            &project,
            &entry,
            true,
            crate::dialect(&project).unwrap().as_ref(),
        )
        .err()
        .unwrap();
        assert_eq!(error.id, "state.export-invalid");
        assert!(error.message.contains("aliases protected project input"));
        assert_eq!(
            std::fs::read_to_string(project.ids_file()).unwrap(),
            "identity bytes\n"
        );
    }

    #[test]
    fn state_history_force_replaces_differently_cased_filenames_without_deleting_output() {
        let (_stage, project, entry) = fixture("history-cased-path");
        std::fs::create_dir(project.schema_dir()).unwrap();
        let old = project.schema_dir().join("dbo.Product.yml");
        std::fs::write(&old, "old declaration bytes\n").unwrap();
        let (data, _) = export(
            &project,
            &entry,
            true,
            crate::dialect(&project).unwrap().as_ref(),
        )
        .ok()
        .unwrap();
        assert_eq!(data.removed, vec![old]);
        assert_eq!(
            pbps_load::schema_files(&project.schema_dir()).unwrap(),
            data.files
        );
        let loaded = pbps_load::load_schema_dir(&project.schema_dir()).unwrap();
        assert_eq!(loaded.schema, entry.snapshot.schema);
    }

    #[cfg(unix)]
    #[test]
    fn state_history_force_replaces_output_symlinks_before_removing_their_stale_targets() {
        let (_stage, project, entry) = fixture("history-output-symlink");
        std::fs::create_dir(project.schema_dir()).unwrap();
        let old = project.schema_dir().join("custom.yml");
        let output = project.schema_dir().join("dbo.product.yml");
        std::fs::write(&old, "old declaration bytes\n").unwrap();
        std::os::unix::fs::symlink("custom.yml", &output).unwrap();
        let (data, _) = export(
            &project,
            &entry,
            true,
            crate::dialect(&project).unwrap().as_ref(),
        )
        .ok()
        .unwrap();
        assert_eq!(data.removed, vec![old.clone()]);
        assert!(!old.exists());
        assert!(!std::fs::symlink_metadata(&output).unwrap().is_symlink());
        let loaded = pbps_load::load_schema_dir(&project.schema_dir()).unwrap();
        assert_eq!(loaded.schema, entry.snapshot.schema);
        assert_eq!(
            pbps_load::schema_files(&project.schema_dir()).unwrap(),
            data.files
        );
    }

    #[test]
    fn state_history_human_values_are_shown_once_and_json_names_stay_raw() {
        let hostile = "line\n\u{1b}[31m\u{202e}literal\\n";
        let raw = serde_json::json!({hostile: [hostile, "plain"]});
        let rendered = render_value(&raw, 0);
        assert_eq!(
            rendered
                .matches(&Text::new(hostile).shown().to_string())
                .count(),
            2
        );
        assert!(!rendered.contains('\u{1b}') && !rendered.contains('\u{202e}'));
        assert_eq!(raw[hostile][0], hostile);
        assert!(rendered.contains("plain"));
    }

    #[test]
    fn state_history_ids_require_positive_signed_64_bit_values() {
        for command in ["show", "diff", "export"] {
            for invalid in ["0", "-1", "9223372036854775808", "word"] {
                assert!(crate::Cli::try_parse_from(["pbps", "state", command, invalid]).is_err());
            }
            for valid in ["1", "9223372036854775807"] {
                assert!(crate::Cli::try_parse_from(["pbps", "state", command, valid]).is_ok());
            }
        }
        assert!(crate::Cli::try_parse_from(["pbps", "state", "diff", "1", "0"]).is_err());
    }
}
