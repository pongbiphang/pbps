//! Record a report about external work; never load or execute its SQL.

use std::{io::Read as _, path::PathBuf};

use anyhow::{Context as _, bail};
use pbps_config::Project;
use pbps_db::{LedgerEntry, LedgerError};
use pbps_model::{DataWorkIdentity, DataWorkOutcome, DataWorkRecord, Reason, Text};
use sha2::{Digest as _, Sha256};

use crate::{Found, OutputFormat, TargetArgs, db, engine, output};

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("identity").required(true).args(["script", "external_version"])))]
pub struct RecordDataWorkArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// Script to hash, relative to the project root; never executed
    #[arg(long)]
    script: Option<PathBuf>,
    /// External job/script version instead of a local script
    #[arg(long, value_parser = external_version_arg)]
    external_version: Option<String>,
    /// Checksum of a retained successful apply in this target ledger
    #[arg(long, value_parser = crate::plan_checksum_arg)]
    expand_checksum: String,
    /// completed or failed; recording either outcome successfully exits 0
    #[arg(long)]
    outcome: DataWorkOutcome,
    #[arg(long)]
    reason: Reason,
    /// At most 4096 UTF-8 bytes; preserved verbatim
    #[arg(long, value_parser = notes_arg)]
    notes: Option<String>,
    #[arg(long, default_value = "human")]
    pub format: OutputFormat,
}

fn external_version_arg(value: &str) -> Result<String, String> {
    if value.trim().is_empty() || value.len() > pbps_model::data_work::EXTERNAL_VERSION_BYTES {
        return Err("external version must be nonblank and at most 1024 UTF-8 bytes".into());
    }
    Ok(value.into())
}

fn notes_arg(value: &str) -> Result<String, String> {
    if value.len() > pbps_model::data_work::NOTES_BYTES {
        return Err("notes exceed 4096 UTF-8 bytes".into());
    }
    Ok(value.into())
}

impl RecordDataWorkArgs {
    fn work(&self, project: &Project) -> anyhow::Result<DataWorkRecord> {
        let identity = match (&self.script, &self.external_version) {
            (Some(path), None) => {
                let spelling = path.to_str().context("script path must be UTF-8")?;
                let full = project.root().join(path);
                // A FIFO's read-only open waits for a writer; refuse its type
                // before opening, then recheck the opened object below.
                let metadata = std::fs::metadata(&full).with_context(|| {
                    format!("cannot read script `{}`", Text::new(spelling).shown())
                })?;
                if !metadata.is_file() {
                    bail!("script identity requires a regular file");
                }
                let mut file = std::fs::File::open(&full).with_context(|| {
                    format!("cannot read script `{}`", Text::new(spelling).shown())
                })?;
                if !file.metadata()?.is_file() {
                    bail!("script identity requires a regular file");
                }
                let mut hash = Sha256::new();
                let mut bytes = [0; 65536];
                loop {
                    let read = file.read(&mut bytes)?;
                    if read == 0 {
                        break;
                    }
                    hash.update(&bytes[..read]);
                }
                DataWorkIdentity::Script {
                    path: spelling.replace(std::path::MAIN_SEPARATOR, "/").into(),
                    sha256: format!("{:x}", hash.finalize()),
                }
            }
            (None, Some(version)) => DataWorkIdentity::ExternalVersion {
                version: version.clone().into(),
            },
            _ => bail!("supply exactly one of --script or --external-version"),
        };
        let work = DataWorkRecord {
            identity,
            expand_checksum: self.expand_checksum.clone(),
            outcome: self.outcome,
            notes: self.notes.clone().map(Into::into),
        };
        work.validate()?;
        Ok(work)
    }
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct RecordedDataWorkData {
    environment: String,
    entry_id: i64,
    self_reported: bool,
    data_work: DataWorkRecord,
}

async fn previous(conn: &mut pbps_db::Conn) -> anyhow::Result<LedgerEntry> {
    match engine::latest(conn).await {
        Ok(Some(entry)) => Ok(entry),
        Ok(None) => Err(Found::new(
            "the ledger has no entries; bootstrap or baseline before recording data work",
        )
        .into()),
        Err(LedgerError::NotInitialized) => Err(Found::new(
            "the ledger is absent; bootstrap or baseline before recording data work",
        )
        .into()),
        Err(error) => Err(error.into()),
    }
}

pub fn cmd_record(project: &Project, args: RecordDataWorkArgs) -> anyhow::Result<()> {
    const COMMAND: &str = "state record-data-work";
    let json = args.format == OutputFormat::Json;
    let target = output::or_unanswerable(
        COMMAND,
        json,
        "environment.unresolved",
        args.target.resolve(project),
    )?;
    let work = output::or_unanswerable(
        COMMAND,
        json,
        "state.data-work.identity",
        args.work(project),
    )?;
    let operator = crate::operator(project.root())?;
    let runtime = output::or_unanswerable(COMMAND, json, "runtime.unavailable", db::runtime())?;
    let result = runtime.block_on(async {
        let mut conn = db::connect(&target).await?;
        // A missing/empty ledger cannot supply a snapshot. Probe before lock,
        // whose legacy ensure_tables path is allowed to create bookkeeping.
        previous(&mut conn).await?;
        engine::lock(&mut conn, &operator).await?;
        let result = async {
            // The pre-lock read is not the state to copy: another writer may
            // have finished in between. Every writer uses this durable mutex.
            let current = previous(&mut conn).await?;
            if !engine::has_applied_plan(&mut conn, &work.expand_checksum).await? {
                return Err(Found::new(format!("no retained successful apply matches expand checksum {}; apply that plan here before recording data work", work.expand_checksum)).into());
            }
            let snapshot = current.snapshot.recording_data_work(
                work.clone(), operator, args.reason.clone(), db::git_sha(project.root()),
            ).map_err(|e| Found::new(e.to_string()))?;
            engine::record(&mut conn, &snapshot).await.map_err(anyhow::Error::from)
        }.await;
        let released = engine::unlock(&mut conn).await;
        match result {
            Ok(id) => {
                if !released.with_context(|| format!("entry #{id} was recorded, but its lock could not be released"))? {
                    bail!("entry #{id} was recorded, but its lock was already absent");
                }
                Ok(id)
            }
            Err(error) => {
                if let Err(release) = released {
                    return Err(anyhow::Error::new(release).context(format!("the recording was refused ({error:#}) and its lock could not be released")));
                }
                Err(error)
            }
        }
    });
    let id = match result {
        Ok(id) => id,
        Err(error) => {
            if error.downcast_ref::<Found>().is_none() {
                return output::or_unanswerable(
                    COMMAND,
                    json,
                    "state.data-work.unreadable",
                    Err(error),
                );
            }
            let report = output::Report::plain(
                COMMAND,
                vec![output::Finding::error(
                    "state.data-work.refused",
                    format!("{error:#}"),
                )],
            );
            if json {
                return report.emit_json();
            }
            eprint!("{}", output::human(&report.findings));
            return report.outcome();
        }
    };
    if json {
        output::Report::new(
            COMMAND,
            vec![],
            Some(RecordedDataWorkData {
                environment: target.label,
                entry_id: id,
                self_reported: true,
                data_work: work,
            }),
        )
        .emit_json()
    } else {
        println!(
            "Recorded self-reported data work as entry #{id} in `{}`: {}.",
            Text::new(target.label).shown(),
            work.outcome
        );
        match &work.identity {
            DataWorkIdentity::Script { path, sha256 } => {
                println!("Script: {} (SHA-256 {sha256})", path.shown())
            }
            DataWorkIdentity::ExternalVersion { version } => {
                println!("External version: {}", version.shown())
            }
        }
        println!("Expand checksum: {}", work.expand_checksum);
        println!("Reason: {}", Text::new(args.reason.as_ref()).shown());
        if let Some(notes) = work.notes {
            println!("Notes: {}", notes.shown());
        }
        println!("pbps did not execute or verify this work.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn data_work_arguments_refuse_invalid_audit_input_before_io() {
        let checksum = "a".repeat(64);
        let args = [
            "pbps",
            "state",
            "record-data-work",
            "--env",
            "prod",
            "--expand-checksum",
            &checksum,
            "--outcome",
            "completed",
            "--reason",
            "backfill",
            "--external-version",
            "v7",
        ];
        assert!(crate::Cli::try_parse_from(args).is_ok());
        assert!(crate::Cli::try_parse_from(&args[..11]).is_err());
        for (index, bad) in [(6, "bad"), (8, "running"), (10, " \n"), (12, " ")] {
            let mut bad_args = args;
            bad_args[index] = bad;
            assert!(
                crate::Cli::try_parse_from(bad_args).is_err(),
                "{bad_args:?}"
            );
        }
        for extra in [
            vec!["--script", "x.sql"],
            vec!["--notes", "x"],
            vec!["--environment", "prod"],
        ] {
            let mut more = args.to_vec();
            more.extend(extra.iter().copied());
            if extra[0] != "--notes" {
                assert!(crate::Cli::try_parse_from(more).is_err());
            } else {
                assert!(crate::Cli::try_parse_from(more).is_ok());
            }
        }
        assert!(notes_arg(&"é".repeat(2048)).is_ok());
        assert!(notes_arg(&"x".repeat(4097)).is_err());
        assert!(external_version_arg(&"é".repeat(512)).is_ok());
        assert!(external_version_arg(&"x".repeat(1025)).is_err());
    }
}
