//! Read recorded states without adopting, applying or rewriting identity intent.

use std::collections::{BTreeMap, BTreeSet};

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
    // As output::Location does: serde refuses non-UTF-8 PathBuf values.
    files: Vec<String>,
    removed: Vec<String>,
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

fn comparison_schemas(
    from: &StateSnapshot,
    to: &StateSnapshot,
) -> (pbps_model::Schema, pbps_model::Schema) {
    let before = from.declared.overlay(&from.schema);
    let after = to.declared.overlay(&to.schema);
    let covered = |snapshot: &StateSnapshot, overlaid: &pbps_model::Schema| {
        let mut texts = pbps_model::Declared::from_schema(overlaid);
        // Binding pins carry resolution evidence, not SQL spelling.
        texts.bindings.clone_from(&snapshot.declared.bindings);
        texts == snapshot.declared
    };
    // Baselines, snapshots and older/partial records can lack authored SQL.
    // Never compare that catalog spelling against an authored one. Reuse the
    // model's traversal so a new SQL slot cannot escape this coverage check.
    if covered(from, &before) && covered(to, &after) {
        (before, after)
    } else {
        (from.schema.clone(), to.schema.clone())
    }
}

// Fill only absent name mappings from the other record. Recorded identities,
// including tombstones and a UID now held by a different name, win over names.
fn historical_ids(
    schema: &pbps_model::Schema,
    recorded: &pbps_model::IdsFile,
    other: &pbps_model::IdsFile,
) -> pbps_model::IdsFile {
    fn missing_by_name<T: Clone + Ord>(
        own: &mut BTreeMap<pbps_model::Uid, T>,
        other: &BTreeMap<pbps_model::Uid, T>,
        taken: &mut BTreeSet<pbps_model::Uid>,
    ) {
        let mut names: BTreeSet<_> = own.values().cloned().collect();
        for (uid, name) in other {
            if !names.contains(name) && taken.insert(uid.clone()) {
                own.insert(uid.clone(), name.clone());
                names.insert(name.clone());
            }
        }
    }
    let mut ids = recorded.clone();
    let mut taken = ids
        .tables
        .keys()
        .chain(ids.columns.keys())
        .chain(ids.roles.keys())
        .chain(ids.types.keys())
        .chain(ids.tombstones.keys())
        .cloned()
        .collect();
    missing_by_name(&mut ids.tables, &other.tables, &mut taken);
    missing_by_name(&mut ids.columns, &other.columns, &mut taken);
    missing_by_name(&mut ids.roles, &other.roles, &mut taken);
    missing_by_name(&mut ids.types, &other.types, &mut taken);
    let observed = pbps_diff::observed_ids_reserving(schema, &ids, other);
    // Complete live names without replacing this snapshot's version or intent.
    // Spell every field so new identity metadata requires a preservation choice.
    ids.moves.retain(|uid, _| observed.tables.contains_key(uid));
    ids.conversions
        .retain(|uid, _| observed.columns.contains_key(uid));
    pbps_model::IdsFile {
        version: ids.version,
        tables: observed.tables,
        columns: observed.columns,
        roles: observed.roles,
        types: observed.types,
        tombstones: ids.tombstones,
        moves: ids.moves,
        rebuilds: ids.rebuilds,
        conversions: ids.conversions,
    }
}

const PUBLIC_EXECUTE_NOT_RECORDED: &str = "Snapshots do not record historical public_execute.";

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
    let (mut before, mut after) = comparison_schemas(&from.snapshot, &to.snapshot);
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
    // Match absent identities by name without changing either recorded UID.
    let before_ids = historical_ids(&before, &from.snapshot.ids, &to.snapshot.ids);
    let after_ids = historical_ids(&after, &to.snapshot.ids, &from.snapshot.ids);
    let mut diffed = pbps_diff::diff_partial(
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
    // This is declaration policy, not a fact retained by StateSnapshot.
    diffed
        .changes
        .changes
        .retain(|planned| !matches!(planned.change, pbps_model::Change::PublicExecution { .. }));
    let data = DiffData {
        environment: target.label.clone(),
        from_id: from.id,
        to_id: to.id,
        changes: diffed.changes,
        unexpressible: diffed.errors.iter().map(ToString::to_string).collect(),
    };
    let mut findings: Vec<_> = data
        .unexpressible
        .iter()
        .map(|message| output::Finding::error("change.unexpressible", message))
        .collect();
    if dialect.creates_public_executable_routines()
        && before
            .modules
            .keys()
            .chain(after.modules.keys())
            .any(|id| matches!(id, pbps_model::ModuleId::Routine(_)))
    {
        let message = format!(
            "{PUBLIC_EXECUTE_NOT_RECORDED} Historical diff omits routine PUBLIC execution policy changes."
        );
        findings.push(output::Finding::note("state.diff-public-execute", message));
    }
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
        }
        eprint!("{}", output::human(&findings));
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
    // Declarations do not carry the identity history, so the project's own
    // identity file says which rewrites were approved after this entry. An
    // absent file approved none; one that cannot be read cannot say, and a
    // later plan refusal would not undo what the export wrote.
    match crate::read_ids_opt(project) {
        Ok(Some(current)) => {
            if let Some(table) =
                pbps_diff::rebuild::rebuilt_since(&entry.snapshot.ids, &current).first()
            {
                return Err(Refusal::new(
                    "state.export-predates-rebuild",
                    format!(
                        "Entry #{} holds {} from before its approved rebuild; restoring it would \
                         need a reverse rebuild, which pbps does not plan. Nothing was written.",
                        entry.id,
                        table.shown()
                    ),
                ));
            }
        }
        Ok(None) => {}
        Err(e) => {
            return Err(Refusal::new(
                "state.export-ids-unreadable",
                format!(
                    "Entry #{} cannot be exported: the identity file cannot be read, so it is \
                     unknown whether a rebuild was approved since: {e:#}. Nothing was written.",
                    entry.id
                ),
            ));
        }
    }
    let schema = entry.snapshot.declared.overlay(&entry.snapshot.schema);
    let (stage, copies, files, removed, mut findings) = (|| -> anyhow::Result<_> {
        let stage = adopt::Stage::new("state-export")?;
        let staged = adopt::write_declarations_with_deps(
            stage.path(),
            &schema,
            &Default::default(),
            &entry.snapshot.module_deps,
        )?;
        let loaded = adopt::check_staged(stage.path(), dialect)?;
        let findings = adopt::refuse_policy_errors(project, &loaded, dialect)?;
        let copies: Vec<_> = staged
            .iter()
            .map(|source| {
                source
                    .strip_prefix(stage.path())
                    .map(|relative| (source.clone(), dir.join(relative)))
            })
            .collect::<Result<_, _>>()?;
        let files: BTreeSet<_> = copies.iter().map(|(_, path)| path.clone()).collect();
        let removed: Vec<_> = present
            .iter()
            .filter(|path| !files.contains(*path))
            .cloned()
            .collect();
        let root = crate::paths::absolute(&dir)?;
        // Force authorizes declaration replacement, never identity/config writes
        // through an alias or a generated filename that names a protected input.
        for path in files.iter().chain(&present) {
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
        Ok((stage, copies, files, removed, findings))
    })()
    .map_err(invalid)?;
    // Publish the validated bytes through the same restore boundary as pull.
    let cleanup = crate::publication::publish(&dir, &present, &copies).map_err(|error| {
        let message = format!("Entry #{} could not be published: {error:#}", entry.id);
        Refusal::new("state.export-invalid", message)
    })?;
    if let Some(note) = cleanup {
        findings.push(output::Finding::warning("state.export-backup", note));
    }
    drop(stage);
    if dialect.creates_public_executable_routines()
        && schema
            .modules
            .keys()
            .any(|id| matches!(id, pbps_model::ModuleId::Routine(_)))
    {
        let message = format!(
            concat!(
                "{} Export omits it and keeps the closed default; ",
                "review routine execution permissions before committing."
            ),
            PUBLIC_EXECUTE_NOT_RECORDED
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
            files: files
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            removed: removed
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        },
        findings,
    ))
}

#[cfg(test)]
mod tests {
    /// A readable identity file in a layout the tool never writes, so a test
    /// can tell that the export left it byte for byte as it was.
    const KEPT_IDS: &str = "{\n    \"version\":   1\n}\n";
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
    fn state_history_publication_errors_restore_files_and_cleanup_notes_remain_success() {
        let (_stage, project, entry) = fixture("history-publication2351");
        std::fs::create_dir(project.schema_dir()).unwrap();
        let old = project.schema_dir().join("custom.yml");
        std::fs::write(&old, "keep these bytes").unwrap();
        std::fs::write(project.ids_file(), KEPT_IDS).unwrap();
        let dialect = crate::dialect(&project).unwrap();
        let error = crate::publication::tests::with_fault(&[("write", 0)], || {
            export(&project, &entry, true, dialect.as_ref())
        })
        .err()
        .unwrap();
        assert!(error.message.contains("the project is unchanged"));
        assert_eq!(std::fs::read(&old).unwrap(), b"keep these bytes");
        assert_eq!(
            std::fs::read(project.ids_file()).unwrap(),
            KEPT_IDS.as_bytes()
        );
        let (data, findings) = crate::publication::tests::with_fault(&[("cleanup", 0)], || {
            export(&project, &entry, true, dialect.as_ref())
        })
        .map_err(|e| e.message)
        .unwrap();
        assert_eq!(data.files.len(), 1);
        let note = findings
            .iter()
            .find(|f| f.id == "state.export-backup")
            .unwrap();
        assert!(note.message.contains("were published"));
        assert!(note.message.contains(".pbps-publication-backup"));
        let report = output::Report::new("state export", findings, Some(&data));
        assert!(report.outcome().is_ok());
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("state.export-backup") && json.contains("were published"));
        assert_eq!(
            std::fs::read(project.ids_file()).unwrap(),
            KEPT_IDS.as_bytes()
        );
    }

    #[test]
    fn state_history_replacement_identity_checks_grow_linearly_with_file_count() {
        for count in [32, 64] {
            let (_stage, project, mut entry) = fixture("history-scaling2352");
            let table = entry
                .snapshot
                .schema
                .tables
                .values()
                .next()
                .unwrap()
                .clone();
            entry.snapshot.schema.tables.clear();
            for i in 0..count {
                entry
                    .snapshot
                    .schema
                    .tables
                    .insert(format!("dbo.t{i:03}").parse().unwrap(), table.clone());
            }
            let dialect = crate::dialect(&project).unwrap();
            export(&project, &entry, true, dialect.as_ref())
                .map_err(|e| e.message)
                .unwrap();
            crate::paths::inspections::CALLS.with(|n| n.set(0));
            export(&project, &entry, true, dialect.as_ref())
                .map_err(|e| e.message)
                .unwrap();
            let calls = crate::paths::inspections::CALLS.with(|n| n.get());
            // Two protected inputs, for each old entry and each output. A scan
            // of all removal candidates for every output exceeds this bound.
            assert!(
                calls <= 4 * count,
                "{count} files made {calls} identity inspections"
            );
        }
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
    fn state_history_export_refuses_an_entry_from_before_an_approved_rebuild() {
        let (_stage, project, mut entry) = fixture("history-predates-rebuild");
        let dialect = crate::dialect(&project).unwrap();
        let table: pbps_model::TableName = "dbo.product".parse().unwrap();
        let (source, replacement) = (
            pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.product", 1),
            pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.product", 2),
        );
        // The files approved a rebuild of the table after this entry was recorded.
        let mut current = pbps_model::IdsFile::default();
        current.tables.insert(replacement.clone(), table.clone());
        current.tombstones.insert(
            source.clone(),
            pbps_model::Tombstone {
                was: "dbo.product".into(),
                dropped_at: "2026-10-24".into(),
                reason: "maintenance".into(),
                operator: "o".into(),
            },
        );
        current.rebuilds.insert(
            source.clone(),
            pbps_model::Rebuild {
                source: pbps_model::RebuildIdentities {
                    tables: [(source.clone(), table.clone())].into(),
                    columns: Default::default(),
                },
                replacement: pbps_model::RebuildIdentities {
                    tables: [(replacement.clone(), table.clone())].into(),
                    columns: Default::default(),
                },
                replacement_root: replacement.clone(),
                continues: [(replacement.clone(), source.clone())].into(),
                target_checksum: "1".repeat(64),
                max_rows: 5,
                allowed_at: "2026-10-24".into(),
                reason: "maintenance".parse().unwrap(),
                operator: "o".into(),
                types: Default::default(),
                conversions: Default::default(),
            },
        );
        current.validate().unwrap();
        std::fs::write(
            project.ids_file(),
            serde_json::to_string_pretty(&current).unwrap(),
        )
        .unwrap();
        // An entry that still holds the source is refused by name, writing nothing.
        entry.snapshot.ids.tables.insert(source, table);
        let error = export(&project, &entry, true, dialect.as_ref())
            .err()
            .unwrap();
        assert_eq!(error.id, "state.export-predates-rebuild");
        assert!(error.message.contains("dbo.product") && error.message.contains("reverse rebuild"));
        assert!(!project.schema_dir().exists());
        // One recorded after it holds the replacement and exports as usual.
        entry.snapshot.ids.tables.clear();
        entry
            .snapshot
            .ids
            .tables
            .insert(replacement, "dbo.product".parse().unwrap());
        assert!(export(&project, &entry, true, dialect.as_ref()).is_ok());
    }

    #[test]
    fn state_history_export_reads_the_identity_file_as_absent_unreadable_or_clean() {
        let dialect_of = |p: &crate::Project| crate::dialect(p).unwrap();
        // Absent: no rewrite was approved, so the export proceeds.
        let (_stage, project, entry) = fixture("history-ids-absent");
        assert!(!project.ids_file().exists());
        assert!(export(&project, &entry, true, dialect_of(&project).as_ref()).is_ok());
        // Readable and without a rebuild: proceeds.
        let (_stage, project, entry) = fixture("history-ids-clean");
        std::fs::write(
            project.ids_file(),
            serde_json::to_string_pretty(&pbps_model::IdsFile::default()).unwrap(),
        )
        .unwrap();
        assert!(export(&project, &entry, true, dialect_of(&project).as_ref()).is_ok());
        // Present but malformed, inconsistent or empty: refused by name before
        // anything is written, the file left as it was.
        let orphan = r#"{"version":1,"columns":{"c_aaaaaa":{"table":"dbo.gone","name":"id"}}}"#;
        for (label, bytes) in [
            ("malformed", "{ not json\n"),
            ("empty", ""),
            ("inconsistent", orphan),
        ] {
            let (_stage, project, entry) = fixture(&format!("history-ids-{label}"));
            std::fs::write(project.ids_file(), bytes).unwrap();
            let error = export(&project, &entry, true, dialect_of(&project).as_ref())
                .err()
                .unwrap_or_else(|| panic!("{label}: the export was allowed"));
            assert_eq!(error.id, "state.export-ids-unreadable", "{label}");
            assert!(
                error.message.contains("schema.ids.json") || error.message.contains("identity"),
                "{label}: {}",
                error.message
            );
            assert!(
                !project.schema_dir().exists(),
                "{label}: wrote declarations"
            );
            assert_eq!(std::fs::read_to_string(project.ids_file()).unwrap(), bytes);
        }
    }

    #[test]
    fn state_history_overwrite_refusal_preserves_existing_declarations_and_ids() {
        let (_stage, project, entry) = fixture("history-overwrite");
        std::fs::create_dir(project.schema_dir()).unwrap();
        let path = project.schema_dir().join("custom.yml");
        std::fs::write(&path, "keep these exact bytes\n").unwrap();
        std::fs::write(project.ids_file(), KEPT_IDS).unwrap();
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
            KEPT_IDS
        );
        // Force changes declaration files, but neither parses nor writes ids.
        let (data, _) = export(&project, &entry, true, dialect.as_ref())
            .ok()
            .unwrap();
        assert_eq!(
            serde_json::to_value(&data.removed).unwrap(),
            serde_json::json!([path])
        );
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(project.ids_file()).unwrap(),
            KEPT_IDS
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
        std::fs::write(project.ids_file(), KEPT_IDS).unwrap();
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
            KEPT_IDS
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
        std::fs::write(project.ids_file(), KEPT_IDS).unwrap();
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
            KEPT_IDS
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
        assert_eq!(
            serde_json::to_value(&data.removed).unwrap(),
            serde_json::json!([old])
        );
        assert_eq!(
            serde_json::to_value(pbps_load::schema_files(&project.schema_dir()).unwrap()).unwrap(),
            serde_json::to_value(&data.files).unwrap()
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
        assert_eq!(
            serde_json::to_value(&data.removed).unwrap(),
            serde_json::json!([old])
        );
        assert!(!old.exists());
        assert!(!std::fs::symlink_metadata(&output).unwrap().is_symlink());
        let loaded = pbps_load::load_schema_dir(&project.schema_dir()).unwrap();
        assert_eq!(loaded.schema, entry.snapshot.schema);
        assert_eq!(
            serde_json::to_value(pbps_load::schema_files(&project.schema_dir()).unwrap()).unwrap(),
            serde_json::to_value(&data.files).unwrap()
        );
    }

    #[test]
    fn state_history_unlisted_output_directories_refuse_before_removing_declarations() {
        for linked in [false, true] {
            if linked && !cfg!(unix) {
                continue;
            }
            let (_stage, project, entry) = fixture("history-output-directory");
            std::fs::create_dir(project.schema_dir()).unwrap();
            let old = project.schema_dir().join("custom.yml");
            let output = project.schema_dir().join("dbo.product.yml");
            let directory = if linked {
                project.schema_dir().join("directory")
            } else {
                output.clone()
            };
            std::fs::write(&old, "keep old declaration\n").unwrap();
            std::fs::write(project.ids_file(), KEPT_IDS).unwrap();
            std::fs::create_dir(&directory).unwrap();
            let notes = directory.join("notes.txt");
            std::fs::write(&notes, "keep non-declaration\n").unwrap();
            #[cfg(unix)]
            if linked {
                std::os::unix::fs::symlink("directory", &output).unwrap();
            }
            let before = (
                std::fs::read(&old).unwrap(),
                std::fs::read(&notes).unwrap(),
                std::fs::read(project.ids_file()).unwrap(),
                std::fs::read(project.config_file()).unwrap(),
            );
            let result = export(
                &project,
                &entry,
                true,
                crate::dialect(&project).unwrap().as_ref(),
            );
            assert!(
                old.is_file(),
                "unlisted output failure deleted a declaration"
            );
            assert_eq!(
                (
                    std::fs::read(&old).unwrap(),
                    std::fs::read(&notes).unwrap(),
                    std::fs::read(project.ids_file()).unwrap(),
                    std::fs::read(project.config_file()).unwrap(),
                ),
                before
            );
            assert!(directory.is_dir());
            assert_eq!(std::fs::read_dir(project.root()).unwrap().count(), 3);
            assert_eq!(
                std::fs::read_dir(project.schema_dir()).unwrap().count(),
                if linked { 3 } else { 2 }
            );
            let error = result.err().unwrap();
            assert_eq!(error.id, "state.export-invalid");
            assert!(
                error.message.contains("move any existing"),
                "{}",
                error.message
            );
            assert!(error.message.contains("the project is unchanged"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn state_history_dangling_output_parent_failure_restores_declarations() {
        let (_stage, project, mut entry) = fixture("history-output-parent");
        let table = entry
            .snapshot
            .schema
            .tables
            .remove(&"dbo.product".parse().unwrap())
            .unwrap();
        entry
            .snapshot
            .schema
            .tables
            .insert("sales.country".parse().unwrap(), table);
        entry
            .snapshot
            .schema
            .roles
            .insert("reader".into(), Default::default());
        std::fs::create_dir(project.schema_dir()).unwrap();
        let old = project.schema_dir().join("sales.country.yml");
        std::fs::write(&old, "old declaration bytes\n").unwrap();
        std::fs::write(project.ids_file(), KEPT_IDS).unwrap();
        let roles = project.schema_dir().join("roles");
        std::os::unix::fs::symlink("missing-roles", &roles).unwrap();
        let before = (
            std::fs::read(&old).unwrap(),
            std::fs::read(project.ids_file()).unwrap(),
            std::fs::read(project.config_file()).unwrap(),
            std::fs::read_link(&roles).unwrap(),
        );
        let dialect = crate::dialect(&project).unwrap();
        let error = export(&project, &entry, true, dialect.as_ref())
            .err()
            .unwrap();
        assert_eq!(error.id, "state.export-invalid");
        assert!(
            old.is_file(),
            "output-parent failure deleted the old declaration"
        );
        assert_eq!(
            (
                std::fs::read(&old).unwrap(),
                std::fs::read(project.ids_file()).unwrap(),
                std::fs::read(project.config_file()).unwrap(),
                std::fs::read_link(&roles).unwrap(),
            ),
            before
        );
        assert_eq!(std::fs::read_dir(project.root()).unwrap().count(), 3);
        assert_eq!(std::fs::read_dir(project.schema_dir()).unwrap().count(), 2);

        std::fs::remove_file(roles).unwrap();
        assert!(export(&project, &entry, true, dialect.as_ref()).is_ok());
        let loaded = pbps_load::load_schema_dir(&project.schema_dir()).unwrap();
        assert_eq!(loaded.schema, entry.snapshot.schema);
    }

    #[test]
    fn state_history_missing_uids_match_names_without_overriding_recorded_conflicts() {
        let (_stage, project, entry) = fixture("history-missing-ids");
        let schema = entry.snapshot.schema;
        let table = schema.tables.keys().next().unwrap().clone();
        let mut recorded = pbps_model::IdsFile::default();
        recorded
            .tables
            .insert("t_aaaaaa".parse().unwrap(), table.clone());
        recorded
            .columns
            .insert("c_aaaaaa".parse().unwrap(), table.column("code"));
        let original = recorded.clone();
        let empty = pbps_model::IdsFile::default();
        assert_eq!(historical_ids(&schema, &empty, &recorded), recorded);
        assert_eq!(historical_ids(&schema, &recorded, &empty), recorded);
        let mut conflicting = recorded.clone();
        conflicting.columns.clear();
        conflicting
            .columns
            .insert("c_bbbbbb".parse().unwrap(), table.column("code"));
        assert_eq!(
            historical_ids(&schema, &conflicting, &recorded),
            conflicting
        );
        assert_eq!(historical_ids(&schema, &recorded, &conflicting), recorded);
        let changes = pbps_diff::diff_partial(
            pbps_diff::Side {
                schema: &schema,
                ids: &recorded,
            },
            pbps_diff::Side {
                schema: &schema,
                ids: &conflicting,
            },
            crate::dialect(&project).unwrap().as_ref(),
            &Default::default(),
        )
        .changes;
        assert_eq!(changes.changes.len(), 2);
        assert!(
            changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::DropColumn { .. }))
        );
        assert!(
            changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::AddColumn { .. }))
        );
        // A UID held under another name, or retired, cannot be borrowed.
        let mut renamed = recorded.clone();
        renamed
            .columns
            .values_mut()
            .for_each(|name| *name = table.column("other"));
        assert_ne!(
            historical_ids(&schema, &renamed, &recorded).column_uid(&table.column("code")),
            recorded.column_uid(&table.column("code"))
        );
        let mut retired = recorded.clone();
        retired.columns.clear();
        retired.tombstones.insert(
            "c_aaaaaa".parse().unwrap(),
            pbps_model::Tombstone {
                was: "dbo.product.code".into(),
                dropped_at: "2026-10-23".into(),
                reason: "retired".into(),
                operator: "test".into(),
            },
        );
        assert_ne!(
            historical_ids(&schema, &retired, &recorded).column_uid(&table.column("code")),
            recorded.column_uid(&table.column("code"))
        );
        assert_eq!(
            recorded, original,
            "comparison mutated recorded identity intent"
        );
    }

    #[test]
    fn state_history_fallback_uids_cannot_reuse_either_records_conflicting_uid() {
        let (_stage, _project, entry) = fixture("history-reserved-ids");
        let mut schema = entry.snapshot.schema;
        let (table, columns) = schema.tables.iter_mut().next().unwrap();
        columns.columns.insert(
            "other".into(),
            pbps_model::Column::new("int".parse().unwrap()),
        );
        let table = table.clone();
        let collision = pbps_model::Uid::derived(
            pbps_model::UidKind::Column,
            &table.column("code").raw().to_string(),
            0,
        );
        let mut from = pbps_model::IdsFile::default();
        from.tables
            .insert("t_aaaaaa".parse().unwrap(), table.clone());
        from.columns
            .insert("c_aaaaaa".parse().unwrap(), table.column("other"));
        let mut to = from.clone();
        to.columns.clear();
        to.columns.insert(collision.clone(), table.column("other"));
        let before = historical_ids(&schema, &from, &to);
        let after = historical_ids(&schema, &to, &from);
        assert_ne!(before.column_uid(&table.column("code")), Some(&collision));
        assert_eq!(
            before.column_uid(&table.column("code")),
            after.column_uid(&table.column("code"))
        );
        assert_eq!(
            before.column_uid(&table.column("other")),
            from.column_uid(&table.column("other"))
        );
        assert_eq!(
            after.column_uid(&table.column("other")),
            to.column_uid(&table.column("other"))
        );
    }

    #[test]
    fn state_history_completion_keeps_selected_metadata_and_filters_unheld_live_ids() {
        let (_stage, _project, entry) = fixture("history-id-metadata");
        let mut schema = entry.snapshot.schema;
        let table = schema.tables.keys().next().unwrap().clone();
        schema.roles.insert("reader".into(), Default::default());
        schema.types.insert(
            "dbo.kind".parse().unwrap(),
            pbps_model::ManagedType::Enum {
                values: vec!["a".into()],
            },
        );
        let move_permission = pbps_model::Move {
            max_rows: 7,
            allowed_at: "2026-10-23".into(),
            reason: "selected snapshot permission".into(),
            operator: "selected".into(),
        };
        let conversion = pbps_model::Conversion {
            from: "int".into(),
            to: "bigint".into(),
            using: "code::bigint".into(),
            allowed_at: "2026-10-24".into(),
            operator: "selected".into(),
        };
        let mut recorded = pbps_model::IdsFile {
            version: pbps_model::ids::CURRENT_VERSION,
            tables: [
                ("t_aaaaaa".parse().unwrap(), table.clone()),
                ("t_bbbbbb".parse().unwrap(), "dbo.missing".parse().unwrap()),
            ]
            .into(),
            columns: [
                ("c_aaaaaa".parse().unwrap(), table.column("code")),
                ("c_bbbbbb".parse().unwrap(), table.column("missing")),
            ]
            .into(),
            roles: [
                ("r_aaaaaa".parse().unwrap(), "reader".into()),
                ("r_bbbbbb".parse().unwrap(), "missing".into()),
            ]
            .into(),
            types: [
                ("y_aaaaaa".parse().unwrap(), "dbo.kind".parse().unwrap()),
                ("y_bbbbbb".parse().unwrap(), "dbo.missing".parse().unwrap()),
            ]
            .into(),
            tombstones: [
                (
                    "c_cccccc".parse().unwrap(),
                    pbps_model::Tombstone {
                        was: "dbo.product.retired".into(),
                        dropped_at: "2026-10-22".into(),
                        reason: "selected snapshot retirement".into(),
                        operator: "selected".into(),
                    },
                ),
                // The rebuild's sources, retired when it was approved.
                (
                    "t_dddddd".parse().unwrap(),
                    pbps_model::Tombstone {
                        was: table.raw().to_string(),
                        dropped_at: "2026-10-24".into(),
                        reason: "selected snapshot rebuild".into(),
                        operator: "selected".into(),
                    },
                ),
                (
                    "c_dddddd".parse().unwrap(),
                    pbps_model::Tombstone {
                        was: table.column("code").raw().to_string(),
                        dropped_at: "2026-10-24".into(),
                        reason: "selected snapshot rebuild".into(),
                        operator: "selected".into(),
                    },
                ),
            ]
            .into(),
            moves: [
                ("t_aaaaaa".parse().unwrap(), move_permission.clone()),
                ("t_bbbbbb".parse().unwrap(), move_permission),
            ]
            .into(),
            rebuilds: [(
                "t_dddddd".parse().unwrap(),
                pbps_model::Rebuild {
                    source: pbps_model::RebuildIdentities {
                        tables: [("t_dddddd".parse().unwrap(), table.clone())].into(),
                        columns: [("c_dddddd".parse().unwrap(), table.column("code"))].into(),
                    },
                    replacement: pbps_model::RebuildIdentities {
                        tables: [("t_aaaaaa".parse().unwrap(), table.clone())].into(),
                        columns: [("c_aaaaaa".parse().unwrap(), table.column("code"))].into(),
                    },
                    replacement_root: "t_aaaaaa".parse().unwrap(),
                    continues: [
                        ("t_aaaaaa".parse().unwrap(), "t_dddddd".parse().unwrap()),
                        ("c_aaaaaa".parse().unwrap(), "c_dddddd".parse().unwrap()),
                    ]
                    .into(),
                    target_checksum: "1".repeat(64),
                    max_rows: 17,
                    allowed_at: "2026-10-24".into(),
                    reason: "selected snapshot rebuild".parse().unwrap(),
                    operator: "selected".into(),
                    types: Default::default(),
                    conversions: Default::default(),
                },
            )]
            .into(),
            conversions: ["c_aaaaaa", "c_bbbbbb"]
                .map(|uid| (uid.parse().unwrap(), conversion.clone()))
                .into(),
        };
        recorded.clone().validated().unwrap();
        let mut other = recorded.clone();
        other
            .tombstones
            .values_mut()
            .for_each(|entry| entry.operator = "other".into());
        other
            .moves
            .values_mut()
            .for_each(|entry| entry.max_rows = 99);
        other
            .rebuilds
            .values_mut()
            .for_each(|entry| entry.operator = "other".into());
        other
            .conversions
            .values_mut()
            .for_each(|entry| entry.using = "other".into());
        let selected = recorded.clone();
        recorded.tables.remove(&"t_bbbbbb".parse().unwrap());
        recorded.columns.remove(&"c_bbbbbb".parse().unwrap());
        recorded.roles.remove(&"r_bbbbbb".parse().unwrap());
        recorded.types.remove(&"y_bbbbbb".parse().unwrap());
        recorded.moves.remove(&"t_bbbbbb".parse().unwrap());
        recorded.conversions.remove(&"c_bbbbbb".parse().unwrap());
        assert_eq!(historical_ids(&schema, &selected, &other), recorded);
        assert_eq!(
            historical_ids(&schema, &selected, &Default::default()),
            recorded
        );
        let completed = historical_ids(&schema, &Default::default(), &other);
        assert_eq!(completed.tables, recorded.tables);
        assert_eq!(completed.columns, recorded.columns);
        assert_eq!(completed.roles, recorded.roles);
        assert_eq!(completed.types, recorded.types);
        assert!(
            completed.moves.is_empty(),
            "borrowed names do not grant move permission"
        );
        assert!(
            completed.tombstones.is_empty(),
            "the other record cannot retire identities here"
        );
        assert!(
            completed.rebuilds.is_empty(),
            "borrowed names do not grant rebuild permission"
        );
        assert!(
            completed.conversions.is_empty(),
            "borrowed names do not grant a conversion"
        );
        completed.validated().unwrap();
        selected.validated().unwrap();
        other.validated().unwrap();
    }

    #[test]
    fn state_history_completion_never_derives_a_uid_either_record_holds_or_retired() {
        let (_stage, _project, entry) = fixture("history-rebuild-reservations");
        let mut schema = entry.snapshot.schema;
        let table = schema.tables.keys().next().unwrap().clone();
        schema.tables.get_mut(&table).unwrap().columns.insert(
            "source".into(),
            pbps_model::Column::new("int".parse().unwrap()),
        );
        let added: pbps_model::TableName = "dbo.added".parse().unwrap();
        schema
            .tables
            .insert(added.clone(), schema.tables[&table].clone());
        let table_collision =
            pbps_model::Uid::derived(pbps_model::UidKind::Table, &added.raw().to_string(), 0);
        let column_collision = pbps_model::Uid::derived(
            pbps_model::UidKind::Column,
            &table.column("code").raw().to_string(),
            0,
        );
        let stone = |was: String| pbps_model::Tombstone {
            was,
            dropped_at: "2026-10-24".into(),
            reason: "retired by the rebuild".into(),
            operator: "selected".into(),
        };
        // The rebuild's sources are retired in the selected record, so their
        // uids are as taken as any live one.
        let selected = pbps_model::IdsFile {
            version: pbps_model::ids::CURRENT_VERSION,
            tables: [("t_aaaaaa".parse().unwrap(), table.clone())].into(),
            columns: [("c_aaaaaa".parse().unwrap(), table.column("source"))].into(),
            roles: BTreeMap::new(),
            types: BTreeMap::new(),
            tombstones: [
                (table_collision.clone(), stone(table.raw().to_string())),
                (
                    column_collision.clone(),
                    stone(table.column("source").raw().to_string()),
                ),
            ]
            .into(),
            moves: BTreeMap::new(),
            conversions: BTreeMap::new(),
            rebuilds: [(
                table_collision.clone(),
                pbps_model::Rebuild {
                    source: pbps_model::RebuildIdentities {
                        tables: [(table_collision.clone(), table.clone())].into(),
                        columns: [(column_collision.clone(), table.column("source"))].into(),
                    },
                    replacement: pbps_model::RebuildIdentities {
                        tables: [("t_aaaaaa".parse().unwrap(), table.clone())].into(),
                        columns: [("c_aaaaaa".parse().unwrap(), table.column("source"))].into(),
                    },
                    replacement_root: "t_aaaaaa".parse().unwrap(),
                    continues: [
                        ("t_aaaaaa".parse().unwrap(), table_collision.clone()),
                        ("c_aaaaaa".parse().unwrap(), column_collision.clone()),
                    ]
                    .into(),
                    target_checksum: "1".repeat(64),
                    max_rows: 7,
                    allowed_at: "2026-10-24".into(),
                    reason: "reserve past identities".parse().unwrap(),
                    operator: "selected".into(),
                    types: Default::default(),
                    conversions: Default::default(),
                },
            )]
            .into(),
        };
        selected.clone().validated().unwrap();
        let original = selected.clone();
        // The other record names the same uids as live ones; they stay retired here.
        let mut other = selected.clone();
        other.rebuilds.clear();
        other.tombstones.clear();
        other.tables.insert(table_collision.clone(), added.clone());
        other
            .columns
            .insert(column_collision.clone(), table.column("code"));
        other.clone().validated().unwrap();
        let completed = historical_ids(&schema, &selected, &other);
        assert_eq!(completed.rebuilds, selected.rebuilds);
        assert_eq!(completed.tombstones, selected.tombstones);
        assert_ne!(completed.table_uid(&added), Some(&table_collision));
        assert_ne!(
            completed.column_uid(&table.column("code")),
            Some(&column_collision)
        );
        completed.validated().unwrap();
        // Derivation also avoids uids retired only on the other side, and a
        // borrowed name never brings the other side's lineage with it.
        let empty = pbps_model::IdsFile::default();
        let completed = historical_ids(&schema, &empty, &selected);
        assert_ne!(completed.table_uid(&added), Some(&table_collision));
        assert_ne!(
            completed.column_uid(&table.column("code")),
            Some(&column_collision)
        );
        assert!(completed.rebuilds.is_empty());
        completed.validated().unwrap();
        assert_eq!(selected, original);
    }

    #[test]
    fn state_history_uses_read_backs_when_either_sql_record_is_partial() {
        let (_stage, _project, entry) = fixture("history-partial-sql");
        let mut from = entry.snapshot;
        let table = from.schema.tables.values_mut().next().unwrap();
        table.columns.get_mut("code").unwrap().value_source =
            pbps_model::ValueSource::Default("7::integer".into());
        table.columns.insert(
            "other".into(),
            pbps_model::Column {
                value_source: pbps_model::ValueSource::Default("8::integer".into()),
                ..pbps_model::Column::new("int".parse().unwrap())
            },
        );
        let mut authored = from.schema.clone();
        for column in authored
            .tables
            .values_mut()
            .next()
            .unwrap()
            .columns
            .values_mut()
        {
            let pbps_model::ValueSource::Default(text) = &mut column.value_source else {
                panic!("fixture default");
            };
            *text = text.replace("::integer", "");
        }
        from.declared = pbps_model::Declared::from_schema(&authored);
        let (before, after) = comparison_schemas(&from, &from);
        assert_eq!(before, authored);
        assert_eq!(after, authored);
        let mut to = from.clone();
        to.declared
            .expressions
            .defaults
            .values_mut()
            .next()
            .unwrap()
            .remove("other");
        assert!(!to.declared.is_empty());
        for partial in [to.declared.clone(), Default::default()] {
            to.declared = partial;
            let (before, after) = comparison_schemas(&from, &to);
            assert_eq!(before, from.schema);
            assert_eq!(after, to.schema);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn state_history_export_json_reports_non_utf8_project_and_stale_paths() {
        use std::os::unix::ffi::OsStrExt as _;
        let (stage, _, entry) = fixture("history-non-utf8");
        let root = stage
            .path()
            .join(std::ffi::OsStr::from_bytes(b"project-\xff"));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("pbps.yml"), "dialect: mssql\n").unwrap();
        let project = Project::discover(&root).unwrap();
        std::fs::create_dir(project.schema_dir()).unwrap();
        let old = project
            .schema_dir()
            .join(std::ffi::OsStr::from_bytes(b"stale-\xff.yml"));
        std::fs::write(&old, "old declaration bytes\n").unwrap();
        std::fs::write(project.ids_file(), KEPT_IDS).unwrap();
        let (data, findings) = export(
            &project,
            &entry,
            true,
            crate::dialect(&project).unwrap().as_ref(),
        )
        .ok()
        .unwrap();
        let report = output::Report::new("state export", findings, Some(data));
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(
            value["data"]["removed"],
            serde_json::json!([old.display().to_string()])
        );
        assert_eq!(
            value["data"]["files"],
            serde_json::json!([project
                .schema_dir()
                .join("dbo.product.yml")
                .display()
                .to_string()])
        );
        assert!(!old.exists());
        let loaded = pbps_load::load_schema_dir(&project.schema_dir()).unwrap();
        assert_eq!(loaded.schema, entry.snapshot.schema);
        assert_eq!(
            std::fs::read_to_string(project.ids_file()).unwrap(),
            KEPT_IDS
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
