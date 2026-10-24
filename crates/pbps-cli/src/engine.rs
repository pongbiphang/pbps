//! The connected seam: one function per question a command asks a database,
//! answered by whichever engine the connection is to.
//!
//! # Why this is a `match` in the CLI and not a trait in `pbps-dialect`
//!
//! `Dialect` is pure — types, validation, emit, probes — and `pbps-db`
//! already depends on it for the transaction framing, so the trait cannot
//! reach a connection without a dependency cycle. The engines therefore keep
//! their connected work as free `async fn`s over `pbps_db::Conn`
//! (ARCHITECTURE, "Connection-bound work is free async fns in the dialect
//! crate, not trait methods"), and *this* file is where a command's question
//! is routed to one of them, by the driver the connection is over — the same
//! fact `pbps_db::Conn` itself dispatches drivers on. Every arm is spelled
//! out and every `match` is exhaustive, so a third engine is a compile error
//! in every function below until it has an answer for each (DECISIONS 417).
//!
//! # What an answer means
//!
//! The shapes come from `pbps-db` (`catalog`, `impact`, `doctor`, `ledger`),
//! filled by each engine from its own catalog. Where the two engines fill a
//! field differently, the field says so; where a question is one only one
//! engine can be asked — the SQL Server edition, the role names a plan needs
//! free — the other engine's arm answers by name: a fact ("this engine has one
//! edition") or a refusal ("this engine's roles are not this tool's to
//! create"), never an empty answer that reads as "nothing found". The functions
//! that can refuse return `anyhow::Result`; the rest keep the engine's own
//! error type, because callers match on it (`LedgerError::NotInitialized`).
//!
//! Nothing here names an engine except to route to it. The engine-specific
//! rendering a caller shows — a securable in `GRANT` spelling, an edition's
//! name — is rendered by the engine and travels as text.

use pbps_diff::order::PlanningPlan;
use std::collections::{BTreeMap, BTreeSet};

use pbps_db::catalog::{CatalogNames, Pulled, RowsError, Spellings};
use pbps_db::doctor::Ask;
use pbps_db::impact::{ImpactError, ImpactReport, RenameTarget};
use pbps_db::{Conn, DbError, Driver, LedgerEntry, LedgerError, LockInfo, TimelineEntry};
use pbps_model::{ChangeSet, IdsFile, ObservedRows, RowScope, Schema, StateSnapshot, TableName};

pub async fn compare_dependencies(
    conn: &mut Conn,
    declarations: &pbps_model::dependency::Dependencies,
) -> anyhow::Result<pbps_model::dependency::PlannedDependencies> {
    match conn.driver() {
        Driver::Mssql => Ok(pbps_mssql::dependencies::compare(conn, declarations).await?),
        Driver::Postgres if declarations.is_empty() => Ok(Default::default()),
        Driver::Postgres => {
            anyhow::bail!("PostgreSQL does not support cross-database dependencies")
        }
    }
}

/// Execute one staged DDL statement, including recovery owned by its engine.
pub async fn execute_staged_statement(
    conn: &mut Conn,
    statement: &pbps_dialect::Statement,
) -> Result<(), DbError> {
    match conn.driver() {
        // SQL Server's emitted DDL has no non-transactional build artifact.
        Driver::Mssql => conn.execute(&statement.sql).await,
        Driver::Postgres => pbps_pg::staged::execute(conn, statement).await,
    }
}

/// Binds and vets every recorded conversion's expression, before the
/// pre-flight evaluates it over the rows or the `ALTER` runs it (#2331,
/// DEC-2331.1). Nothing to vet on an engine that records none.
pub async fn vet_conversions(
    conn: &mut Conn,
    changes: &[pbps_model::PlannedChange],
) -> anyhow::Result<()> {
    if conn.driver() == Driver::Mssql {
        return Ok(());
    }
    for planned in changes {
        let pbps_model::Change::AlterColumnType {
            column,
            from,
            using: Some(using),
            ..
        } = &planned.change
        else {
            continue;
        };
        pbps_pg::input_path::vet_conversion(conn, column, from, using)
            .await
            .map_err(|why| anyhow::anyhow!("{why}"))?;
    }
    Ok(())
}

/// Recover validation after a staged step's caller has rolled its transaction back.
pub async fn recover_failed_staged_validation(
    conn: &mut Conn,
    statement: &pbps_dialect::Statement,
) -> Option<String> {
    let check = statement
        .scan_check
        .as_ref()
        .filter(|check| check.step == pbps_dialect::ScanCheckStep::Validate)?;
    match conn.driver() {
        // SQL Server does not emit a staged attach's temporary CHECK.
        Driver::Mssql => None,
        Driver::Postgres => Some(pbps_pg::staged::recover_failed_validation(conn, check).await),
    }
}

/// Catalog estimates are advisory. Failure to measure is an unavailable
/// answer, not a reason to refuse a valid plan (ADR-0012 §3, DECISIONS 430).
pub async fn operational_cost(
    conn: &mut Conn,
    cs: &ChangeSet,
    ids: &IdsFile,
) -> crate::cost::CostReport {
    use crate::cost::{ChangeCost, CostReport, Rows};
    let names = match pbps_model::plan_names::PlanNames::of(&cs.changes, ids) {
        Ok(names) => names,
        Err(error) => {
            return CostReport::Unavailable {
                engine: match conn.driver() {
                    Driver::Mssql => "sqlserver",
                    Driver::Postgres => "postgres",
                },
                reason: format!("operational_cost could not establish plan identities: {error}"),
            };
        }
    };
    match conn.driver() {
        Driver::Mssql => {
            use pbps_mssql::estimate as ms;
            let mut estimates = ms::planned_estimates(&names).into_iter().peekable();
            if estimates.peek().is_none() && !cs.changes.is_empty() {
                return CostReport::Unavailable {
                    engine: "sqlserver",
                    reason: "operational_cost currently measures SQL Server column type and nullability changes and clustered index changes; this plan contains none".to_owned(),
                };
            }
            let mut changes = Vec::with_capacity(cs.changes.len());
            for change_index in 0..cs.changes.len() {
                let Some((_, mut e)) = estimates.next_if(|(index, _)| *index == change_index)
                else {
                    changes.push(ChangeCost::Unavailable {
                        change_index,
                        reason: "operational_cost has no measurement for this SQL Server change"
                            .to_owned(),
                    });
                    continue;
                };
                if let Err(error) = ms::against(conn, &mut e).await {
                    changes.push(ChangeCost::Unavailable {
                        change_index,
                        reason: format!(
                            "operational_cost could not read SQL Server catalog context: {error}"
                        ),
                    });
                    continue;
                }
                changes.push(ChangeCost::Available {
                    change_index,
                    about: e.about,
                    table: e.table.raw().to_string(),
                    rewrite: e.rewrite.into(),
                    reads: e.reads.into(),
                    lock: e.lock.to_owned(),
                    blocks: e.blocks.to_owned(),
                    also_locks: Vec::new(),
                    rows: match e.rows {
                        Some(count) => Rows::Estimated { count },
                        None => Rows::Unknown {
                            reason: e.rows_unknown.unwrap_or_else(|| {
                                "no catalog row estimate is available".to_owned()
                            }),
                        },
                    },
                });
            }
            CostReport::Available {
                engine: "sqlserver",
                changes,
            }
        }
        Driver::Postgres => {
            use pbps_pg::estimate as pg;
            let mut estimates = pg::planned_estimates(&names).into_iter().peekable();
            let mut changes = Vec::with_capacity(cs.changes.len());
            for change_index in 0..cs.changes.len() {
                let Some((_, mut e)) = estimates.next_if(|(index, _)| *index == change_index)
                else {
                    changes.push(ChangeCost::Unavailable {
                        change_index,
                        reason: "operational_cost has no measurement for this PostgreSQL change"
                            .to_owned(),
                    });
                    continue;
                };
                if let Err(error) = pg::against(conn, &mut e).await {
                    changes.push(ChangeCost::Unavailable {
                        change_index,
                        reason: format!(
                            "operational_cost could not read PostgreSQL catalog context: {error}"
                        ),
                    });
                    continue;
                }
                changes.push(ChangeCost::Available {
                    change_index,
                    about: e.about,
                    table: e.table.raw().to_string(),
                    rewrite: e.rewrite.into(),
                    reads: e.reads.into(),
                    lock: e.lock.to_string(),
                    blocks: e.lock.blocks().to_owned(),
                    also_locks: e
                        .also_locks
                        .iter()
                        .map(|name| name.raw().to_string())
                        .collect(),
                    rows: match e.rows {
                        Some(pg::Rows::Estimated(count)) => Rows::Estimated { count },
                        Some(pg::Rows::NeverAnalyzed) => Rows::NeverAnalyzed,
                        None => Rows::Unknown {
                            reason: e.rows_unknown.unwrap_or_else(|| {
                                "no catalog row estimate is available".to_owned()
                            }),
                        },
                    },
                });
            }
            CostReport::Available {
                engine: "postgres",
                changes,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The ledger and the lock (SPEC §8.1)
// ---------------------------------------------------------------------------

pub async fn is_initialized(conn: &mut Conn) -> Result<bool, DbError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::is_initialized(conn).await,
        Driver::Postgres => pbps_pg::state::is_initialized(conn).await,
    }
}

/// The temporary CHECKs a staged attach left on its tables, which `baseline`
/// takes out before it takes the database as it stands (#1638, DEC-1638.1).
///
/// Only under the exact name a table's identity in the project's ids file
/// gives: never by a prefix. Not from the ledger: `baseline` is the remedy
/// for an entry this build cannot read (DECISIONS 203), and a run that dies
/// between the `ADD` and the `VALIDATE`'s checkpoint records nothing, so no
/// entry's kind or ids can be asked first. No declared constraint can hold
/// the name, the database cannot be read while a `NOT VALID` one stands, and
/// a validated one is pbps's own. SQL Server's staged plans write none.
pub async fn standing_scan_checks(
    conn: &mut Conn,
    ids: &pbps_model::IdsFile,
) -> anyhow::Result<Vec<pbps_dialect::ScanCheck>> {
    Ok(match conn.driver() {
        Driver::Mssql => Vec::new(),
        Driver::Postgres => pbps_pg::staged::standing_scan_checks(conn, &ids.tables).await?,
    })
}

/// Drops `checks`, inside the transaction `baseline` records in (#2127), and
/// names each for the record.
pub async fn drop_scan_checks(
    conn: &mut Conn,
    checks: &[pbps_dialect::ScanCheck],
) -> anyhow::Result<Vec<String>> {
    if checks.is_empty() {
        return Ok(Vec::new());
    }
    pbps_pg::staged::drop_scan_checks(conn, checks).await?;
    Ok(checks.iter().map(scan_check_removal_note).collect())
}

// This value is recorded inside opaque StateSnapshot.reason and participates in
// its original length/truncation check. Preserve the data; stdout shows it once.
fn scan_check_removal_note(check: &pbps_dialect::ScanCheck) -> String {
    format!(
        "the staged attach's temporary CHECK `{}` on `{}`",
        check.name.raw(),
        check.table.raw()
    )
}

pub async fn latest(conn: &mut Conn) -> Result<Option<LedgerEntry>, LedgerError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::latest(conn).await,
        Driver::Postgres => pbps_pg::state::latest(conn).await,
    }
}

pub async fn entry(
    conn: &mut Conn,
    id: Option<i64>,
) -> Result<Option<pbps_db::ledger::StoredEntry>, LedgerError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::entry(conn, id).await,
        Driver::Postgres => pbps_pg::state::entry(conn, id).await,
    }
}

pub async fn timeline(conn: &mut Conn, limit: u32) -> Result<Vec<TimelineEntry>, LedgerError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::timeline(conn, limit).await,
        Driver::Postgres => pbps_pg::state::timeline(conn, limit).await,
    }
}

/// The opaque evidence reader is reachable only through this connected seam.
pub async fn baseline_catalog(
    conn: &mut Conn,
) -> Result<Option<pbps_model::baseline_evidence::BaselineCatalog>, DbError> {
    match conn.driver() {
        Driver::Mssql => Ok(None),
        Driver::Postgres => pbps_pg::catalog::baseline_catalog(conn).await,
    }
}

pub async fn record(conn: &mut Conn, snapshot: &StateSnapshot) -> Result<i64, LedgerError> {
    // Snapshot, baseline and apply checkpoints share this write boundary.
    // Never persist an invalid projection that the next ledger read cannot use.
    snapshot
        .ids
        .validate()
        .map_err(|error| LedgerError::BadEntry {
            id: 0,
            message: format!("the snapshot's identity mapping is inconsistent: {error}"),
        })?;
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::record(conn, snapshot).await,
        Driver::Postgres => pbps_pg::state::record(conn, snapshot).await,
    }
}

pub async fn prune(conn: &mut Conn, keep: u32) -> Result<u64, LedgerError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::prune(conn, keep).await,
        Driver::Postgres => pbps_pg::state::prune(conn, keep).await,
    }
}

/// Takes the lock for `operator`, recording which process holds it (#1188).
///
/// The row names the person and also this process: its host, pid, CI job and
/// the application name its session carries. That is what `pbps unlock` and a
/// "locked by" refusal give an operator to check before overriding a lock
/// that, by design, never expires (DECISIONS 285, DEC-1188.1).
pub async fn lock(conn: &mut Conn, operator: &str) -> Result<(), LedgerError> {
    let application_name = match conn.driver() {
        Driver::Mssql => pbps_mssql::state::session_application_name(conn).await?,
        Driver::Postgres => pbps_pg::state::session_application_name(conn).await?,
    };
    let host = crate::host_name();
    let ci_job = crate::ci_job();
    let holder = pbps_db::LockHolder {
        operator,
        host: host.as_deref(),
        pid: std::process::id(),
        ci_job: ci_job.as_deref(),
        application_name: &application_name,
    }
    .render();
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::lock(conn, &holder).await,
        Driver::Postgres => pbps_pg::state::lock(conn, &holder).await,
    }
}

pub async fn unlock(conn: &mut Conn) -> Result<bool, DbError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::unlock(conn).await,
        Driver::Postgres => pbps_pg::state::unlock(conn).await,
    }
}

pub async fn lock_holder(conn: &mut Conn) -> Result<Option<LockInfo>, DbError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::lock_holder(conn).await,
        Driver::Postgres => pbps_pg::state::lock_holder(conn).await,
    }
}

/// Renders a failed deployment's error chain the way anything written
/// **durably** (the ledger) or **outward** (the `on_apply_attempt` hook) is
/// allowed to: every frame this tool itself composed, kept verbatim, and
/// every `DbError::Driver` frame — the one place a server's own sentence
/// enters this chain — replaced by its code alone.
///
/// Ready-phase review of PR #464 measured that `db.message()` is not always
/// safe the way `pbps-db`'s object identifiers are: a failed type conversion
/// names the value it could not convert
/// (`invalid input syntax for type integer: "…"` on 18.6, `Conversion failed
/// when converting the varchar value '…' to data type int.` on SQL Server
/// 2025 — measured on both engines, since `crates/pbps-db/src/mssql.rs`'s
/// `From<tiberius::error::Error>` builds the very same `DbError::Driver` this
/// reads), and `crates/pbps-pg/src/emit.rs`'s own comment on `as_stored`
/// confirms this tool's own column-retype apply reaches that path with a real
/// cell's value. Unlike the enrichment fields the seam itself dropped
/// (DECISIONS 454), `message()` cannot be dropped at the seam without
/// silencing the diagnosis issue #167 exists to give the operator — so the
/// split is here, between what stays on the operator's own terminal and what
/// this tool writes down or sends elsewhere (DECISIONS 455).
///
/// A second ready-phase round on that fix found this function redacting more
/// than the server ever said. `DbError::Driver` used to be built by any
/// caller with a message and an optional code — including several `pbps-pg`
/// guards refusing something *this tool* decided on its own (an unsafe data
/// trigger, a catalog read outside the transaction it needs), never a server.
/// Redacting those the same way as a real driver frame hid the one thing that
/// told an operator which trigger or rule to fix. [`DbError::Refused`]
/// (DECISIONS 455) is the fix at the type: a tool-composed refusal is no
/// longer representable as `Driver`, so it falls to the `_` arm below and
/// passes through unchanged — this function needs no case for it.
/// Wrapping diagnostics use `DbError::Context` (DECISIONS 456): its source
/// remains a separate chain frame, so the same redaction preserves the
/// context without accepting any part of the server's sentence as safe.
///
/// The same round measured that "SQLSTATE" is a PostgreSQL word.
/// `tiberius::Error::code()` returns SQL Server's own numeric message number
/// (`208`, `2627`, …), not a SQLSTATE, so naming every `Driver.code` a
/// SQLSTATE was simply wrong on that engine. The wording below is the engine's
/// own code, unnamed by convention, rather than a borrowed one.
///
/// Walking `error.chain()` rather than reading only the top frame is what
/// makes this work regardless of *where* the driver frame sits: most of this
/// codebase lets a `DbError` reach `anyhow::Error` through a bare `?`, which
/// puts it at the top with no frame of this tool's own text above it at all,
/// while a few call sites — `execute_statements`,
/// `apply_staged_under_lock`'s per-statement loop — wrap it in a `.context()`
/// frame first. **Both shapes must build their `anyhow::Error` with
/// `.context()`, never `anyhow::anyhow!("...{e}")`**: the macro's string
/// interpolation bakes the driver's `Display` into a new, sourceless string
/// before this function ever runs, which is exactly the shape that lost the
/// two calls above their downcast target until this fix.
///
/// `.chain().rev()`, not `.chain()`: `reason` is a bounded column
/// (`crate::engine::truncate_reason` keeps only the *first* `REASON_CHARS`
/// characters of what it is handed, never the last), so its content has to be
/// ordered by diagnostic value per character, highest first — not by the
/// order this tool happened to build the chain in. The redacted driver
/// marker is the highest: it is the one fact that identifies *why* the
/// server refused, and it exists nowhere else once `message()` is gone.
/// `stmt.sql` in `execute_statements`' own `.context()` sentence is the
/// lowest: it is this tool's own generated text, deterministic from the
/// checksum-pinned plan and the declarations already in git, so a partial
/// copy of it in the ledger tells a reader nothing they cannot read better
/// from the plan itself. Putting the marker last, where an un-reversed
/// `error.chain()` (outermost-first) leaves it, ordered the column by build
/// order instead — and a third ready-phase round measured the consequence:
/// a `CREATE VIEW` or a large reference-data block puts a `stmt.sql` past
/// `REASON_CHARS` on its own, so truncation cut before it ever reached the
/// marker, leaving a `Failed` row with a fragment of the emitted SQL and
/// neither the server's message nor its code. "Absent, empty and unreadable
/// are three different things," and a redaction result that reads as
/// *nothing was ever recorded* is the third one wearing the first one's
/// face. Reversing is the fix that cannot be quietly undone by a future
/// caller composing a longer chain: the priority order is structural, not a
/// truncation workaround this function has to remember to preserve.
///
/// The operator's own stderr is unaffected: `main.rs` prints `{e:#}`, which
/// walks the chain outermost-first — the natural reading order for a person,
/// who is never truncated — and shows every frame, driver sentence included;
/// that is issue #167's deliverable, and it is still what someone running an
/// apply against a database they already hold credentials for sees.
pub fn ledger_safe_reason(error: &anyhow::Error) -> String {
    error
        .chain()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|frame| {
            // `RowsError::Read` boxes its `#[source]` (`Box<DbError>`, kept
            // small beside every `Ok` it travels next to) — and a boxed
            // `#[source]` is not merely a `DbError` behind one more layer of
            // indirection: `thiserror` stores the trait object over `Box<T>`
            // itself, so `downcast_ref::<DbError>()` on that frame **fails**
            // (measured: a `Box<T>` field downcasts to `Box<T>`, never to
            // `T`, because `Box<T>`'s own blanket `Error` impl is what is
            // actually in the vtable). A first version of this fix missed
            // that and fell through to the frame's own `Display` — which
            // still renders the driver's message, since `Box`'s `Display`
            // forwards to what it holds — silently un-redacting exactly the
            // frame this match exists to catch. Trying both shapes is what a
            // boxed `#[source]` costs a redaction that has to survive being
            // downcast through it.
            // `LedgerError::Db` and `ImpactError::Query` are
            // `#[error(transparent)]`: not merely a `DbError` behind a
            // named wrapper, but thiserror's specific instruction to forward
            // `Display` to the wrapped value and — the part that matters
            // here — forward `source()` to *its* `source()`, skipping the
            // wrapped value itself. **Measured**: `error.chain()` on such a
            // value is one frame long, and that one frame downcasts to the
            // wrapper type (`LedgerError`), never to what it wraps
            // (`DbError`) — the opposite failure from the boxed case above,
            // for the opposite reason: there a real chain link downcast to
            // the wrong type, here `.chain()` never produces a second link
            // to downcast at all. Both still leave `frame.to_string()`
            // rendering the driver's raw message, since `Display` forwards
            // regardless of whether `source()` does.
            let as_db = frame
                .downcast_ref::<DbError>()
                .or_else(|| frame.downcast_ref::<Box<DbError>>().map(AsRef::as_ref))
                .or_else(|| match frame.downcast_ref::<LedgerError>() {
                    Some(LedgerError::Db(inner)) => Some(inner),
                    _ => None,
                })
                .or_else(|| match frame.downcast_ref::<ImpactError>() {
                    Some(ImpactError::Query(inner)) => Some(inner),
                    _ => None,
                });
            match as_db {
                Some(DbError::Driver { code, .. }) => match code {
                    Some(code) => {
                        format!("the driver reported code {code}; its message is not recorded here")
                    }
                    None => "the driver reported a failure with no code; its message is not \
                              recorded here"
                        .to_owned(),
                },
                Some(DbError::Context { message, .. }) => message.clone(),
                // Every other `DbError` variant — including `Refused`, this
                // tool's own composed refusal, which can never carry a server
                // value by construction — and every frame that is not a
                // `DbError` at all, is this tool's own composed text: a
                // malformed connection string, an `io::Error` naming a host
                // that refused a socket, a catalog row the introspection SQL
                // got wrong, or a `.context()` sentence this crate wrote.
                // None of it echoes a server-supplied value back.
                _ => frame.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(": ")
}

/// A failure reason cut to what the ledger's `reason` column holds.
///
/// Each engine counts differently — UTF-16 units on one, characters on the
/// other — because each column's width is measured the engine's way, so the
/// cut is the engine's too.
pub fn truncate_reason(driver: Driver, text: &str) -> String {
    match driver {
        Driver::Mssql => pbps_mssql::state::truncate_reason(text),
        Driver::Postgres => pbps_pg::state::truncate_reason(text),
    }
}

// ---------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------

/// Where a catalog read runs, which the command knows and no engine can
/// guess from the connection alone.
///
/// The two engines differ here in a way the seam had to learn from a live
/// run (DECISIONS 418): SQL Server reads its catalog the same way inside and
/// outside a transaction, and PostgreSQL's pull takes a transaction of its
/// own and refuses to be asked inside somebody else's (253) — while the
/// apply's read-back *must* run inside the transaction that built what it
/// reads (147). So the command says which it is, and a mismatch is refused by
/// the engine that can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// Outside any transaction. PostgreSQL takes its own `REPEATABLE READ
    /// READ ONLY` snapshot for it and refuses if one is open.
    Snapshot,
    /// Inside the transaction this command opened for a plan's statements, to
    /// record what they built before it commits. PostgreSQL reads under a
    /// savepoint and refuses if no transaction is open.
    InsideOwnTransaction,
}

/// A PostgreSQL closing catalog view is one statement, while declared rows
/// are read separately. Revalidate their managed projection before recording.
pub fn needs_readback_revalidation(driver: Driver, read: Read) -> bool {
    matches!(
        (driver, read),
        (Driver::Postgres, Read::InsideOwnTransaction)
    )
}

pub async fn introspect(conn: &mut Conn, read: Read) -> Result<Pulled, DbError> {
    match (conn.driver(), read) {
        (Driver::Mssql, _) => pbps_mssql::catalog::introspect(conn).await,
        (Driver::Postgres, Read::Snapshot) => pbps_pg::catalog::introspect(conn).await,
        (Driver::Postgres, Read::InsideOwnTransaction) => {
            pbps_pg::catalog::introspect_within_transaction(conn).await
        }
    }
}

/// The connected database's default collation, where the engine's model has
/// column collations (#1175); `None` on PostgreSQL.
pub async fn database_collation(conn: &mut Conn) -> Result<Option<String>, DbError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::catalog::database_collation(conn)
            .await
            .map(Some),
        Driver::Postgres => Ok(None),
    }
}

/// The schema pg_partman is installed in, if it is (#1533). SQL Server has
/// no such tool, and no partitioned table in this model.
pub async fn partman_schema(conn: &mut Conn) -> Result<Option<String>, DbError> {
    match conn.driver() {
        Driver::Mssql => Ok(None),
        Driver::Postgres => pbps_pg::partman::schema(conn).await,
    }
}

/// The parents pg_partman's catalog in `schema` names (#1533). Reached only
/// through [`partman_schema`], which answers `None` on SQL Server.
pub async fn partman_maintained(
    conn: &mut Conn,
    schema: &str,
) -> Result<Vec<pbps_pg::partman::Maintained>, DbError> {
    match conn.driver() {
        Driver::Mssql => Ok(Vec::new()),
        Driver::Postgres => pbps_pg::partman::maintained(conn, schema).await,
    }
}

/// Refuses a declared column collation the target does not have (#1175), by
/// name and before any statement runs. PostgreSQL declares none: `validate`
/// refuses the field there.
pub async fn refuse_unknown_collations(conn: &mut Conn, schema: &Schema) -> anyhow::Result<()> {
    let names: Vec<String> = schema
        .tables
        .values()
        .flat_map(|t| t.columns.values())
        .filter_map(|c| c.collation.as_ref().map(|c| c.as_str().to_owned()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let unknown = match conn.driver() {
        Driver::Mssql => pbps_mssql::catalog::unknown_collations(conn, &names).await?,
        Driver::Postgres => Vec::new(),
    };
    if !unknown.is_empty() {
        anyhow::bail!(
            "this server has no collation named {}; a `COLLATE` naming it would be refused \
             mid-apply",
            unknown
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// Refuses a foreign key to a declared history the database reads as one
/// name with it (#1625). Offline the check compares names as written
/// (`check_history_references`), since the collation is the server's
/// (DEC-1243.1); on a case-insensitive database a key to `dbo.t_history`
/// references the history `dbo.T_History`, and the engine refuses its
/// `ADD FOREIGN KEY` (13565) once the statements before it have run. The
/// database answers which spellings are one, as for every other name a plan
/// compares (`object_names_alike`). PostgreSQL has no system-versioned
/// table.
pub async fn refuse_history_references_alike(
    conn: &mut Conn,
    schema: &Schema,
    dialect: &dyn pbps_dialect::Dialect,
) -> anyhow::Result<()> {
    if conn.driver() != Driver::Mssql {
        return Ok(());
    }
    let histories: Vec<&TableName> = schema
        .tables
        .values()
        .filter_map(|t| t.system_time.as_ref()?.versioning.as_ref())
        .map(|v| &v.history)
        .collect();
    // A key to a history as written is the offline check's, already refused.
    let referenced: BTreeSet<&TableName> = schema
        .tables
        .values()
        .flat_map(|t| t.foreign_keys.values())
        .map(|fk| &fk.references_table)
        .filter(|r| !histories.contains(r))
        .collect();
    if histories.is_empty() || referenced.is_empty() {
        return Ok(());
    }
    let names: Vec<TableName> = histories
        .iter()
        .copied()
        .chain(referenced)
        .cloned()
        .collect();
    let alike = pbps_mssql::catalog::object_names_alike(conn, &names).await?;
    let problems = pbps_dialect::history_references_under(schema, dialect, |a, b| {
        a == b
            || alike
                .iter()
                .any(|(x, y)| (x == a && y == b) || (x == b && y == a))
    });
    if !problems.is_empty() {
        anyhow::bail!(
            "the declarations have a foreign key the engine refuses (13565):\n  {}",
            problems.join("\n  ")
        );
    }
    Ok(())
}

/// Refuses what this server cannot take of a system-versioned table, before
/// the first statement (#1502): SQL Server 2016 has no history retention and
/// no cascading key from a system-versioned table, and refuses either only
/// once the statements before it have run. A plan with neither asks nothing.
pub async fn refuse_unsupported_temporal(
    conn: &mut Conn,
    changes: &ChangeSet,
) -> anyhow::Result<()> {
    match temporal_refusal(&unsupported_temporal(conn, changes).await?) {
        Some(refusal) => Err(anyhow::anyhow!(refusal)),
        None => Ok(()),
    }
}

/// What [`refuse_unsupported_temporal`] refuses, each named: the answer to
/// the capability question, apart from a failure to ask it. `plan --db`
/// reports the answer as a finding and the failure as unanswerable (#1630).
pub async fn unsupported_temporal(
    conn: &mut Conn,
    changes: &ChangeSet,
) -> anyhow::Result<Vec<String>> {
    if conn.driver() != Driver::Mssql || !pbps_mssql::temporal::needs_the_question(changes) {
        return Ok(Vec::new());
    }
    if pbps_mssql::temporal::has_history_retention(conn).await? {
        return Ok(Vec::new());
    }
    let versioned = if pbps_mssql::temporal::needs_the_catalog(changes) {
        pbps_mssql::temporal::system_versioned_tables(conn).await?
    } else {
        Default::default()
    };
    Ok(pbps_mssql::temporal::refused_without_retention(
        changes, &versioned,
    ))
}

/// The refusal [`unsupported_temporal`]'s problems make, if there are any.
pub fn temporal_refusal(problems: &[String]) -> Option<String> {
    (!problems.is_empty()).then(|| {
        format!(
            "this server cannot take the plan's system-versioned tables as declared:\n  {}",
            problems.join("\n  ")
        )
    })
}

/// Ends a connected plan on what [`unsupported_temporal`] answered, if it
/// refused anything. The question was asked and answered, so a JSON plan
/// reports the refusal as a finding with exit 2, like the online edition's
/// (DECISIONS 485, #574), rather than as `plan.failed` with exit 1 (#1630).
/// A failure to ask never reaches here: it is `unsupported_temporal`'s error.
pub fn refuse_answered_temporal(
    problems: &[String],
    json: bool,
    findings: &mut Vec<crate::output::Finding>,
) -> anyhow::Result<()> {
    let Some(refusal) = temporal_refusal(problems) else {
        return Ok(());
    };
    if json {
        findings.push(
            crate::output::Finding::error("plan.temporal-unsupported", refusal).remedy(
                "leave `retention` out of these tables and use `no_action` on these keys, or \
                 deploy to SQL Server 2017 or later",
            ),
        );
        return crate::output::Report::plain("plan", findings.clone()).emit_json();
    }
    anyhow::bail!(refusal)
}

pub async fn read_rows(
    conn: &mut Conn,
    schema: &Schema,
    scopes: &BTreeMap<TableName, RowScope>,
    read: Read,
) -> Result<ObservedRows, RowsError> {
    match (conn.driver(), read) {
        (Driver::Mssql, _) => pbps_mssql::catalog::read_rows(conn, schema, scopes).await,
        (Driver::Postgres, Read::Snapshot) => {
            pbps_pg::catalog::read_rows(conn, schema, scopes).await
        }
        (Driver::Postgres, Read::InsideOwnTransaction) => {
            pbps_pg::catalog::read_rows_within_transaction(conn, schema, scopes).await
        }
    }
}

/// Every declared value the engine would not read back as written, and every
/// pair of declared keys it reads as one row (DECISIONS 101, 106).
pub async fn misspelt(
    conn: &mut Conn,
    schema: &Schema,
    at: &CatalogNames,
    type_routines_need_review: bool,
) -> Result<Spellings, RowsError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::catalog::misspelt(conn, schema, at).await,
        Driver::Postgres => {
            pbps_pg::catalog::misspelt(conn, schema, at, type_routines_need_review).await
        }
    }
}

/// How the database spells each declared schema name, or `None` where it has
/// no schema of that name (DECISIONS 142).
pub async fn schema_spellings(
    conn: &mut Conn,
    names: &BTreeSet<String>,
) -> Result<BTreeMap<String, Option<String>>, DbError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::catalog::schema_spellings(conn, names).await,
        Driver::Postgres => pbps_pg::catalog::schema_spellings(conn, names).await,
    }
}

/// Why the four role questions below refuse on PostgreSQL rather than answer.
///
/// A PostgreSQL role is the cluster's, so that dialect answers `false` to
/// `manages_roles` and the differ builds no `CreateRole`, `RenameRole` or
/// `DropRole` for it (DECISIONS 211). Each of these questions exists to clear
/// one of those three statements before it runs, so on that engine there is
/// never a plan to ask them for — and an arm that answered "none" would be an
/// empty answer to a question that was never asked, which is the shape this
/// tool refuses everywhere else.
fn roles_are_the_clusters(question: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{question} is a question about a role this tool creates, renames or drops, and a \
         PostgreSQL role is the cluster's: this dialect plans none of those statements \
         (DECISIONS 211), so a plan that asks this was not built for this engine"
    )
}

/// Refuses a name this plan creates that a PostgreSQL relation-namespace
/// entry outside the catalog inventory already holds (#951). SQL Server's
/// `sys.objects` walk also orders the plan, so it runs after every pass that
/// changes the plan: [`order_created_object_names`]. On PostgreSQL it first
/// orders a cross-schema rename after the rename that moves a carried
/// object's holder away (#1767, DEC-1957.2). What it read is returned, for
/// [`NameFacts::order_and_refuse`] again once the module passes, which may
/// plan the revision afresh in the differ's order, have run.
/// Type occupants are retained here and refused only by that settled replay.
#[allow(clippy::wildcard_enum_match_arm)] // Only type creators/renames add pg_type queries.
pub async fn refuse_created_name_occupants<S: Clone>(
    conn: &mut Conn,
    cs: &mut PlanningPlan<S>,
    label: &str,
    plan: Planned<'_>,
) -> anyhow::Result<Option<NameFacts>> {
    match conn.driver() {
        Driver::Postgres => {
            // The names it creates, and those its table renames take on
            // their way (#1785 review).
            let mut asked = crate::deploy::created_relation_names(cs);
            asked.extend(crate::deploy::rename_claims(cs));
            let mut owners = crate::deploy::transferred_tables(cs);
            let mut detach_releases = std::collections::BTreeMap::new();
            let mut detach_retained = std::collections::BTreeMap::new();
            for change in &cs.changes {
                if let pbps_model::Change::DetachPartition {
                    uid, parent, names, ..
                } = &change.change
                {
                    let Some(table) = plan.base.ids.tables.get(uid) else {
                        continue;
                    };
                    owners.push(table.clone());
                    let Some(parent) = plan
                        .desired
                        .ids
                        .tables
                        .iter()
                        .find(|(_, name)| *name == parent)
                        .and_then(|(id, _)| plan.base.ids.tables.get(id))
                    else {
                        continue;
                    };
                    let clones = pbps_pg::catalog::detach_clone_names(conn, table, parent).await?;
                    let slots = crate::deploy::detach_clone_slots(uid, names, &clones);
                    detach_releases.extend(slots.detach_releases);
                    detach_retained.extend(slots.detach_retained);
                }
            }
            let mut occupants =
                pbps_pg::catalog::relation_name_occupants(conn, &asked, &owners).await?;
            // What a cross-schema rename carries lands on names of its own,
            // which something else may hold there now (#1749), a table or a
            // view included (#1765).
            let carried = crate::deploy::carried_names(cs, &occupants);
            let landing: Vec<TableName> = carried
                .iter()
                .filter(|n| !occupants.iter().any(|o| &o.name == *n))
                .cloned()
                .collect();
            for found in pbps_pg::catalog::relation_name_occupants(conn, &landing, &[]).await? {
                if !occupants.contains(&found) {
                    occupants.push(found);
                }
            }
            let mut at_names = carried;
            at_names.extend(crate::deploy::rename_claims(cs));
            at_names.extend(cs.changes.iter().flat_map(|p| {
                crate::deploy::sequence_names_claimed(&p.change)
                    .into_iter()
                    .map(|(name, _)| name)
            }));
            at_names.extend(
                cs.changes
                    .iter()
                    .filter(|p| matches!(p.change, pbps_model::Change::DetachPartition { .. }))
                    .flat_map(|p| crate::deploy::relation_names_created(&p.change)),
            );
            let mut type_names = asked.clone();
            type_names.extend(cs.changes.iter().flat_map(|p| match &p.change {
                pbps_model::Change::CreateType { name, .. } => {
                    vec![TableName::new(name.schema(), name.name())]
                }
                pbps_model::Change::RenameType { from, to, .. } => vec![
                    TableName::new(to.schema(), to.name()),
                    TableName::new(to.schema(), from.name()),
                ],
                _ => Vec::new(),
            }));
            let type_occupants = pbps_pg::catalog::type_names_at(conn, &type_names)
                .await?
                .into_iter()
                .filter(|name| {
                    !plan.base.schema.tables.contains_key(name)
                        && !plan.base.schema.modules.iter().any(|(id, m)| {
                            m.kind == pbps_model::ModuleKind::View && id.object_name() == *name
                        })
                        && !plan
                            .base
                            .schema
                            .types
                            .keys()
                            .any(|t| t.schema() == name.schema && t.name() == name.name)
                })
                .collect();
            let facts = NameFacts {
                type_occupants,
                relations: pbps_pg::catalog::relations_at(conn, &at_names).await?,
                occupants,
                detach_releases,
                detach_retained,
            };
            facts.order_relations_and_refuse(cs, label, plan)?;
            Ok(Some(facts))
        }
        Driver::Mssql => Ok(None),
    }
}

/// What [`refuse_created_name_occupants`] read of a PostgreSQL target: the
/// name occupants the plan's names and moves meet, and the tables and views
/// at the carried names.
pub struct NameFacts {
    type_occupants: Vec<TableName>,
    occupants: Vec<pbps_pg::catalog::NameOccupant>,
    relations: Vec<(TableName, &'static str)>,
    detach_releases: std::collections::BTreeMap<pbps_dialect::slots::SlotKey, pbps_model::Uid>,
    detach_retained: std::collections::BTreeMap<pbps_dialect::slots::SlotKey, pbps_model::Uid>,
}

/// What the name passes read the plan's names through: its two sides and the
/// dialect (#1957).
#[derive(Clone, Copy)]
pub struct Planned<'a> {
    pub base: pbps_diff::Side<'a>,
    pub desired: pbps_diff::Side<'a>,
    pub dialect: &'a dyn pbps_dialect::Dialect,
}

impl NameFacts {
    /// Orders a cross-schema rename after what moves its carried objects'
    /// holders away, by the plan's name handovers (#1767, #1957,
    /// DEC-1957.2), then refuses what is left (#951, #1749, #1765).
    pub fn order_and_refuse<S: Clone>(
        &self,
        cs: &mut PlanningPlan<S>,
        label: &str,
        plan: Planned<'_>,
    ) -> anyhow::Result<()> {
        self.order_relations_and_refuse(cs, label, plan)?;
        let slots = self.slots();
        let names = pbps_model::plan_names::PlanNames::of(&cs.changes, plan.desired.ids)?;
        let owners = pbps_model::operation::PlanOwners::new(names, plan.base.ids, plan.desired.ids);
        let operations = pbps_model::operation::Operations::of_plan(
            &cs.changes,
            pbps_model::operation::ModuleEdits::Rebuilt,
        );
        let before = pbps_model::operation::Snapshot {
            schema: plan.base.schema,
            ids: plan.base.ids,
        };
        let effects =
            pbps_diff::order::slots::analyse(operations.as_slice(), &owners, before, plan.dialect);
        for violation in pbps_diff::order::slots::verify(
            operations.as_slice(),
            &effects,
            plan.dialect,
            Some(&slots),
            &owners,
            before,
        )
        .0
        {
            if let pbps_diff::order::slots::SlotViolation::Contested { slot, .. }
            | pbps_diff::order::slots::SlotViolation::IntermediateHeld { slot, .. } = violation
                && matches!(slot.namespace, pbps_dialect::slots::Namespace::Types(_))
            {
                anyhow::bail!(
                    "pg_type name `{}` is already occupied; rename the declaration or release its holder in the plan",
                    pbps_model::escape::Escaped(&slot.to_string())
                );
            }
        }
        Ok(())
    }

    // Catalog reads precede shared scheduling. Only the historical relation
    // pass runs there; Types claims are judged by order_and_refuse afterwards.
    fn order_relations_and_refuse<S: Clone>(
        &self,
        cs: &mut PlanningPlan<S>,
        label: &str,
        plan: Planned<'_>,
    ) -> anyhow::Result<()> {
        let slots = self.slots();
        crate::deploy::order_carried_moves(
            cs,
            &self.occupants,
            &self.relations,
            (plan.base, plan.desired),
            plan.dialect,
            &slots,
        );
        crate::deploy::refuse_uninventoried_occupants(
            cs,
            &self.occupants,
            &self.relations,
            label,
            &slots,
        )
    }

    /// The occupants as name slots, for the slot replay (#1964, DEC-1964.2).
    pub fn slots(&self) -> pbps_dialect::slots::CatalogFacts {
        let mut slots = crate::deploy::occupant_slots(&self.occupants);
        slots
            .occupants
            .extend(
                self.type_occupants
                    .iter()
                    .map(|name| pbps_dialect::slots::Occupant {
                        slot: pbps_dialect::slots::SlotKey {
                            namespace: pbps_dialect::slots::Namespace::Types(name.schema.clone()),
                            name: name.name.clone(),
                        },
                        kind: "type".to_owned(),
                        owner: None,
                        column: None,
                    }),
            );
        slots.detach_releases.clone_from(&self.detach_releases);
        slots.detach_retained.clone_from(&self.detach_retained);
        slots
    }
}

#[cfg(test)]
mod type_slot_tests {
    use super::*;

    #[test]
    fn a_type_move_refuses_an_undeclared_intermediate_catalog_holder_by_name() {
        use pbps_model::{Change, ChangeSet, IdsFile, ManagedType, PlannedChange, Schema};
        let from: pbps_model::TypeRef = "app.a".parse().unwrap();
        let to: pbps_model::TypeRef = "dst.b".parse().unwrap();
        let uid: pbps_model::Uid = "y_000001".parse().unwrap();
        let definition = ManagedType::Enum {
            values: vec!["ok".into()],
        };
        let base = Schema {
            types: [(from.clone(), definition.clone())].into(),
            ..Default::default()
        };
        let desired = Schema {
            types: [(to.clone(), definition.clone())].into(),
            ..Default::default()
        };
        let base_ids = IdsFile {
            types: [(uid.clone(), from.clone())].into(),
            ..Default::default()
        };
        let ids = IdsFile {
            types: [(uid.clone(), to.clone())].into(),
            ..Default::default()
        };
        let dialect = pbps_pg::Postgres::new();
        let plan = Planned {
            base: pbps_diff::Side {
                schema: &base,
                ids: &base_ids,
            },
            desired: pbps_diff::Side {
                schema: &desired,
                ids: &ids,
            },
            dialect: &dialect,
        };
        let mut cs = pbps_diff::order::propose(ChangeSet {
            changes: vec![PlannedChange::new(Change::RenameType {
                uid,
                from,
                to,
                definition: Box::new(definition),
            })],
        });
        let mut facts = NameFacts {
            type_occupants: vec![TableName::new("dst", "a")],
            occupants: vec![],
            relations: vec![],
            detach_releases: Default::default(),
            detach_retained: Default::default(),
        };
        let error = facts
            .order_and_refuse(&mut cs, "target", plan)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "pg_type name `dst.a (type)` is already occupied; rename the declaration or release its holder in the plan"
        );
        facts.type_occupants.clear();
        facts.order_and_refuse(&mut cs, "target", plan).unwrap();
    }
}

/// Refuses an `UNLOGGED` partition this plan creates under a parent that a
/// permanent table outside the declarations references (#1595,
/// DEC-1595.1), asked of the catalog in the caller's transaction. A failed
/// read is an error, never "no referencers": the engine accepts the
/// partition, so nothing later would catch what this missed.
pub async fn refuse_unlogged_partition_referencers(
    conn: &mut Conn,
    cs: &ChangeSet,
    ids: &pbps_model::IdsFile,
    staged: bool,
) -> anyhow::Result<()> {
    if conn.driver() != Driver::Postgres {
        return Ok(());
    }
    // Each parent under the name the catalog has it by: the partition names
    // it as declared, after a rename this plan may run first (#1613, #1690).
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, ids)?;
    let parents = crate::deploy::stored_unlogged_parents(&names);
    if parents.is_empty() {
        return Ok(());
    }
    let referencers = pbps_pg::catalog::permanent_referencers(conn, &parents)
        .await
        .map_err(|e| {
            anyhow::Error::new(e).context(
                "cannot read the foreign keys that reference a parent this plan adds an unlogged \
                 partition to",
            )
        })?;
    crate::deploy::refuse_permanent_referencers(cs, &names, &referencers, staged)
}

/// Refuses making a table unlogged while a publication names it (#1489,
/// DEC-1489.1), asked of the catalog in the caller's transaction. A failed
/// read is an error, never "unpublished".
pub async fn refuse_published_unlogged(
    conn: &mut Conn,
    cs: &ChangeSet,
    ids: &pbps_model::IdsFile,
) -> anyhow::Result<()> {
    if conn.driver() != Driver::Postgres {
        return Ok(());
    }
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, ids)?;
    let made_unlogged = crate::deploy::tables_made_unlogged(&names);
    if made_unlogged.is_empty() {
        return Ok(());
    }
    let stored: Vec<TableName> = made_unlogged.iter().map(|(_, s)| s.clone()).collect();
    let naming = pbps_pg::catalog::publications_naming(conn, &stored)
        .await
        .map_err(|e| {
            anyhow::Error::new(e)
                .context("cannot read the publications of a table this plan makes unlogged")
        })?;
    crate::deploy::refuse_published_unlogged(&made_unlogged, &naming)
}

/// Catalog reads precede the plan, while the caller names an object at its
/// statement boundary. Keep that translation identical for the edge query,
/// its visibility proof, and the preliminary computed-column presence read.
fn computed_catalog_table(
    names: &pbps_model::plan_names::PlanNames<'_>,
    table: &TableName,
    at: usize,
) -> TableName {
    names
        .stored(table, pbps_model::plan_names::At::Before(at))
        .unwrap_or_else(|| table.clone())
}

/// Orders a SQL Server plan's function drops after the computed columns that
/// call them, with what each is schema-bound to after it, and refuses what
/// the catalog's expression edges say the engine will not do (#1431,
/// DEC-1431.1). Reads the edges of the tables and modules the plan touches,
/// under the names the catalog has them by. PostgreSQL has no computed
/// columns, and its generated ones are `release_generated_inputs`'s.
///
/// Returns which drop waits for which by those edges, for the rename search
/// that runs after it ([`order_created_object_names`], #1680): empty where
/// no edge was read.
// The complement is every change that touches no object an expression edge
// can name: a computed column, a column it reads, its table, or a module.
#[allow(clippy::wildcard_enum_match_arm)]
pub async fn order_computed_by_edges<S: Clone>(
    conn: &mut Conn,
    cs: &mut PlanningPlan<S>,
    ids: &pbps_model::IdsFile,
) -> anyhow::Result<Vec<(pbps_model::Change, pbps_model::Change)>> {
    if conn.driver() != Driver::Mssql {
        return Ok(Vec::new());
    }
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, ids)?;
    let catalog = |at, table: &TableName| computed_catalog_table(&names, table, at);
    let mut objects: Vec<pbps_model::TableName> = Vec::new();
    for (at, p) in cs.changes.iter().enumerate() {
        let touched = match &p.change {
            pbps_model::Change::AddComputedColumn { table, .. }
            | pbps_model::Change::DropComputedColumn { table, .. }
            | pbps_model::Change::RenameColumn { table, .. } => Some(catalog(at, table)),
            pbps_model::Change::DropTable { name, .. } => Some(name.clone()),
            pbps_model::Change::DropColumn { column, .. }
            | pbps_model::Change::AlterColumnType { column, .. }
            | pbps_model::Change::AlterColumnNullability { column, .. } => {
                Some(catalog(at, &column.table))
            }
            pbps_model::Change::AlterModule { id, .. }
            | pbps_model::Change::DropModule { id, .. } => Some(id.object_name()),
            _ => None,
        };
        if let Some(object) = touched
            && !objects.contains(&object)
        {
            objects.push(object);
        }
    }
    if objects.is_empty() {
        return Ok(Vec::new());
    }
    // The edges refuse or move only these: a column a computed column of its
    // own table reads, a function one calls, a computed column a module is
    // bound to, and drops `release` orders, which takes two. A plan with none
    // of them, such as one adding a computed column or dropping one view,
    // needs no edge read and so no grant that makes one complete (#1643
    // review).
    let mut inputs: Vec<pbps_model::TableName> = Vec::new();
    let mut drops = 0;
    let mut decides = false;
    for (at, p) in cs.changes.iter().enumerate() {
        match &p.change {
            pbps_model::Change::RenameColumn { table, .. } => inputs.push(catalog(at, table)),
            pbps_model::Change::DropColumn { column, .. }
            | pbps_model::Change::AlterColumnType { column, .. }
            | pbps_model::Change::AlterColumnNullability { column, .. } => {
                inputs.push(catalog(at, &column.table));
            }
            pbps_model::Change::DropComputedColumn { .. } => decides = true,
            pbps_model::Change::AlterModule { module, .. } => {
                decides |= module.kind == pbps_model::ModuleKind::Function;
            }
            pbps_model::Change::DropModule { kind, .. } => {
                decides |= *kind == pbps_model::ModuleKind::Function;
                drops += 1;
            }
            pbps_model::Change::DropTable { .. } => drops += 1,
            _ => {}
        }
    }
    let modules_dropped = cs
        .changes
        .iter()
        .any(|p| matches!(p.change, pbps_model::Change::DropModule { .. }));
    decides |= modules_dropped && drops > 1;
    if !decides {
        decides = pbps_mssql::catalog::may_hold_computed_columns(conn, &inputs)
            .await
            .map_err(|e| {
                anyhow::Error::new(e)
                    .context("cannot read whether a changed column's table has computed columns")
            })?;
    }
    if !decides {
        return Ok(Vec::new());
    }
    // An empty read is "no edge" only where no referrer the pass decides by
    // can be hidden (#1462).
    let mut targets = pbps_mssql::catalog::ReferrerTargets::default();
    for (at, p) in cs.changes.iter().enumerate() {
        match &p.change {
            pbps_model::Change::AlterModule { id, module }
                if module.kind == pbps_model::ModuleKind::Function =>
            {
                targets.functions.push(id.object_name());
            }
            pbps_model::Change::DropModule {
                id,
                kind: pbps_model::ModuleKind::Function,
            } => targets.functions.push(id.object_name()),
            pbps_model::Change::DropComputedColumn { table, name, .. } => {
                targets
                    .computed_columns
                    .push((catalog(at, table), name.clone()));
            }
            _ => {}
        }
    }
    pbps_mssql::catalog::prove_referrers_visible(conn, &targets)
        .await
        .map_err(|e| {
            anyhow::Error::new(e).context(
                "cannot prove the catalog shows everything that calls what this plan changes",
            )
        })?;
    let edges = pbps_mssql::catalog::expression_edges(conn, &objects).await?;
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    // One group: whether two strings are one name is the collation's, not
    // the kind of object they name.
    let spellings: Vec<String> = crate::computed_order::spellings(cs, &edges)
        .into_iter()
        .collect();
    let grouped: Vec<(usize, String)> = spellings.iter().map(|s| (0, s.clone())).collect();
    let alike = crate::computed_order::Alike::from_pairs(
        pbps_mssql::catalog::column_names_alike(conn, &grouped)
            .await?
            .into_iter()
            .map(|(a, b)| (spellings[a].clone(), spellings[b].clone())),
    );
    crate::computed_order::order_by_edges(cs, ids, &edges, &alike)
        .map_err(|why| anyhow::anyhow!("computed_dependencies (SQL Server): {why}"))?;
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, ids)?;
    Ok(crate::computed_order::drop_precedence(
        cs, &names, &edges, &alike,
    ))
}

/// Refuses a SQL Server computed column this plan adds, new or again, whose
/// expression calls a function the plan creates, alters or drops, comparing
/// the names under the database's collation (#1459, DEC-1459.1). Only the
/// collation is asked, not `sys.sql_expression_dependencies`, which has no
/// edge for an expression not yet stored, so no grant beyond reading names.
pub async fn refuse_added_computed_calls(
    conn: &mut Conn,
    cs: &ChangeSet,
    declared: &pbps_model::Schema,
) -> anyhow::Result<()> {
    if conn.driver() != Driver::Mssql {
        return Ok(());
    }
    let dialect = pbps_mssql::Mssql;
    let spellings: Vec<String> =
        crate::computed_order::added_call_spellings(cs, declared, &dialect)
            .into_iter()
            .collect();
    if spellings.is_empty() {
        return Ok(());
    }
    let grouped: Vec<(usize, String)> = spellings.iter().map(|s| (0, s.clone())).collect();
    let alike = crate::computed_order::Alike::from_pairs(
        pbps_mssql::catalog::column_names_alike(conn, &grouped)
            .await?
            .into_iter()
            .map(|(a, b)| (spellings[a].clone(), spellings[b].clone())),
    );
    crate::computed_order::refuse_added_calls(cs, declared, &dialect, &alike)
        .map_err(|why| anyhow::anyhow!("computed_dependencies (SQL Server): {why}"))
}

/// Orders this plan's column and table renames on SQL Server from what the
/// catalog holds and how its collation compares names, and refuses a name a
/// change claims that another `sys.objects` entry holds when it runs (#1077,
/// #1366): it reads the names the plan
/// creates or moves things to, and the children of the tables it changes,
/// so the plan's own moves can be walked. The last pass to reorder the plan:
/// called after every other one, so what it settles is what is saved.
pub async fn order_created_object_names<S: Clone>(
    conn: &mut Conn,
    cs: &mut PlanningPlan<S>,
    ids: &pbps_model::IdsFile,
    label: &str,
    precedence: &[(pbps_model::Change, pbps_model::Change)],
) -> anyhow::Result<Option<pbps_dialect::slots::CatalogFacts>> {
    if conn.driver() != Driver::Mssql {
        return Ok(None);
    }
    // Column renames first: each one may move a generated default, which the
    // `sys.objects` walk below follows in the order they run (DEC-1366.3).
    let columns = crate::object_order::renamed_column_names(cs);
    if !columns.is_empty() {
        let tables: Vec<&TableName> = columns.iter().map(|(t, _)| t).collect();
        let grouped: Vec<(usize, String)> = columns
            .iter()
            .map(|(t, name)| {
                let group = tables.iter().position(|u| *u == t).unwrap_or_default();
                (group, name.raw().to_owned())
            })
            .collect();
        let alike: Vec<_> = pbps_mssql::catalog::column_names_alike(conn, &grouped)
            .await?
            .into_iter()
            .map(|(a, b)| (columns[a].clone(), columns[b].clone()))
            .collect();
        crate::object_order::order_column_renames(cs, &alike, label)?;
    }
    let index = pbps_model::plan_names::PlanNames::of(&cs.changes, ids)?;
    let (mut names, parents) = crate::deploy::object_reads(cs, &index);
    let mut occupants = pbps_mssql::catalog::object_name_occupants(conn, &names, &parents).await?;
    // Where a transfer carries each child of a moved table: a name the
    // first read could not know to ask until it found the child (#1366).
    let carried: Vec<TableName> = crate::deploy::carried_destinations(cs, &occupants)
        .into_iter()
        .filter(|n| !names.contains(n))
        .collect();
    if !carried.is_empty() {
        occupants.extend(pbps_mssql::catalog::object_name_occupants(conn, &carried, &[]).await?);
        names.extend(carried);
    }
    // An absent row is a free name only where nothing can be hidden
    // from this login (#1192), so only the names the read found no
    // row at need the proof. A name holds one object per schema, so
    // one this login sees leaves no room for a hidden one: a table
    // dropped and recreated at its own name is not asked. Nor are
    // `parents`, read for what the walk moves or removes with a
    // table, so a drop-only plan claims nothing to prove (review of
    // #1360).
    let mut schemas: Vec<String> = names
        .iter()
        .filter(|n| !occupants.iter().any(|o| &o.wanted == *n))
        .map(|n| n.schema.clone())
        .collect();
    schemas.sort();
    schemas.dedup();
    pbps_mssql::catalog::prove_schemas_visible(conn, &schemas)
        .await
        // As context, never interpolated: a driver frame has to reach
        // `ledger_safe_reason` intact to be redacted (DECISIONS 455).
        .map_err(|e| {
            anyhow::Error::new(e).context("cannot prove the names this plan creates are free")
        })?;
    // Which of the plan's own names are one under the database's
    // collation (#1215).
    let candidates = crate::deploy::alike_candidates(cs, &names, &occupants);
    let alike = pbps_mssql::catalog::object_names_alike(conn, &candidates).await?;
    crate::object_order::order_occupied_objects_under(cs, &occupants, &alike, label, precedence)?;
    Ok(Some(object_slots(&occupants, &candidates, &alike)))
}

/// What the `sys.objects` reads found, as name slots for the slot replay
/// (#1964, DEC-1964.2): the occupants, every name compared under the
/// collation, and the pairs it reports alike. Column and role names are
/// compared by their own reads, which this does not carry: a slot check on
/// one is skipped and reported on a connected plan too.
fn object_slots(
    occupants: &[pbps_mssql::catalog::NameOccupant],
    candidates: &[TableName],
    alike: &[(TableName, TableName)],
) -> pbps_dialect::slots::CatalogFacts {
    use pbps_dialect::slots::{CatalogFacts, Namespace, Occupant, SlotKey};
    let key = |t: &TableName| SlotKey {
        namespace: Namespace::Relations(t.schema.clone()),
        name: t.name.clone(),
    };
    CatalogFacts {
        occupants: occupants
            .iter()
            .map(|o| Occupant {
                slot: key(&o.name),
                kind: o.kind.clone(),
                owner: o.parent.clone(),
                column: o.parent_column.clone(),
            })
            .collect(),
        compared: candidates.iter().map(key).collect(),
        alike: alike.iter().map(|(a, b)| (key(a), key(b))).collect(),
        ..CatalogFacts::default()
    }
}

/// Proves the tables a catalog read did not return are absent, not hidden,
/// before a state records them as missing (#1600). On SQL Server `sys.tables`
/// is filtered by metadata visibility, so a table under an effective `DENY
/// VIEW DEFINITION`, or in a schema this login cannot view, returns no row
/// and reads as missing. Recorded as missing, its uid would leave the state
/// and the table the managed set, silently. The proof is DEC-1192.1's. A
/// PostgreSQL role reads every row of `pg_class`, so nothing there is hidden.
pub async fn prove_tables_absent(conn: &mut Conn, missing: &[TableName]) -> anyhow::Result<()> {
    if conn.driver() != Driver::Mssql || missing.is_empty() {
        return Ok(());
    }
    let mut schemas: Vec<String> = missing.iter().map(|t| t.schema.clone()).collect();
    schemas.sort();
    schemas.dedup();
    pbps_mssql::catalog::prove_schemas_visible(conn, &schemas)
        .await
        .map_err(|e| absence_unproven(missing, e))
}

/// Why [`prove_tables_absent`] refused. The source error is kept as one,
/// never interpolated: a driver frame has to reach `ledger_safe_reason`
/// intact to be redacted, and a failed bootstrap records this (DECISIONS 455,
/// #1605 review).
fn absence_unproven(missing: &[TableName], error: DbError) -> anyhow::Error {
    let names: Vec<String> = missing
        .iter()
        .map(|name| name.shown().to_string())
        .collect();
    anyhow::Error::new(error).context(format!(
        "cannot record {} as missing; record the state with a login that can see those \
         schemas, so a table hidden from this one is not taken for one that is gone",
        names.join(", ")
    ))
}

/// The roles a recording may take out of the state: those the catalog read
/// did not return and the database proves it lacks (#1606). On SQL Server
/// `sys.database_principals` is filtered by metadata visibility, so a role
/// hidden from this login reads as missing; recorded as missing, its uid
/// would leave the state and the next plan create a role that exists. A name
/// the database still resolves refuses the command by name instead
/// (DEC-1606.1).
///
/// PostgreSQL's roles are the cluster's, which a plan refuses rather than
/// creates (`refuse_missing_cluster_roles`), so a missing one is kept as
/// before: taking its uid out would change no plan.
pub async fn absent_roles(
    conn: &mut Conn,
    missing: &[pbps_model::RoleName],
) -> anyhow::Result<Vec<pbps_model::RoleName>> {
    if conn.driver() != Driver::Mssql || missing.is_empty() {
        return Ok(Vec::new());
    }
    let held = pbps_mssql::catalog::held_principals(conn, missing).await?;
    if !held.is_empty() {
        anyhow::bail!(
            "cannot record role(s) {} as missing: the database holds a principal by that name \
             this login cannot see; record the state with a login that can see it, so a role \
             hidden from this one is not taken for one that is gone",
            held.iter()
                .map(|name| name.shown().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(missing.to_vec())
}

/// Which requested names occur in an already captured table inventory.
pub async fn matching_table_names(
    conn: &mut Conn,
    wanted: &[TableName],
    observed: &[TableName],
) -> anyhow::Result<Vec<TableName>> {
    match conn.driver() {
        Driver::Mssql => {
            Ok(pbps_mssql::catalog::matching_table_names(conn, wanted, observed).await?)
        }
        Driver::Postgres => Ok(pbps_pg::catalog::matching_table_names(wanted, observed)),
    }
}

/// Among `names`, the pairs the database reads as one name (DECISIONS 123).
pub async fn names_alike(conn: &mut Conn, names: &[&str]) -> anyhow::Result<Vec<(String, String)>> {
    match conn.driver() {
        Driver::Mssql => Ok(pbps_mssql::catalog::names_alike(conn, names).await?),
        Driver::Postgres => Err(roles_are_the_clusters("comparing the declared role names")),
    }
}

/// The database principals holding any of `names`, except the ones this plan
/// vacates (DECISIONS 118, 119).
pub async fn principals_holding(
    conn: &mut Conn,
    names: &[&str],
    except: &[&str],
) -> anyhow::Result<Vec<(String, String, String)>> {
    match conn.driver() {
        Driver::Mssql => Ok(pbps_mssql::catalog::principals_holding(conn, names, except).await?),
        Driver::Postgres => Err(roles_are_the_clusters(
            "reading which principals hold a name",
        )),
    }
}

/// Every user-defined role's members, for the `DROP ROLE` that has to remove
/// them first (ADR-0005).
pub async fn role_members(
    conn: &mut Conn,
) -> anyhow::Result<BTreeMap<pbps_model::RoleName, Vec<pbps_model::RoleName>>> {
    match conn.driver() {
        Driver::Mssql => Ok(pbps_mssql::catalog::role_members(conn).await?),
        Driver::Postgres => Err(roles_are_the_clusters("reading the role memberships")),
    }
}

/// Every securable each user-defined role owns, which the engine will not
/// drop the role over.
pub async fn role_owned_securables(
    conn: &mut Conn,
) -> anyhow::Result<BTreeMap<pbps_model::RoleName, Vec<String>>> {
    match conn.driver() {
        Driver::Mssql => Ok(pbps_mssql::catalog::role_owned_securables(conn).await?),
        Driver::Postgres => Err(roles_are_the_clusters("reading what the roles own")),
    }
}

// ---------------------------------------------------------------------------
// The server: what it is and what it can do
// ---------------------------------------------------------------------------

/// Advisory target observations; never acquires or contacts a resolver.
pub async fn resolver_discovery(conn: &mut Conn) -> Result<pbps_db::resolver::Discovery, DbError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::resolver::discover(conn).await,
        Driver::Postgres => pbps_pg::resolver::discover(conn).await,
    }
}

/// The server's version, as text an operator recognises.
pub async fn server_version(conn: &mut Conn) -> Result<String, DbError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::doctor::server_version(conn).await,
        Driver::Postgres => Ok(pbps_pg::doctor::server_version(conn).await?.text),
    }
}

/// What the connected server can do, as far as a plan's statements care.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The edition this server runs, named as the operator would recognise
    /// it, where the engine *has* editions. `None` is "this engine has one
    /// edition" — a fact about the engine, not an edition that could not be
    /// read; a read that fails is the `Err` of [`capabilities`].
    pub edition: Option<String>,
    /// Whether `strategy: online` is accepted at all (ADR-0003).
    pub supports_online: bool,
    /// Whether the statements the emitter writes for a module are accepted:
    /// `CREATE OR ALTER` arrived in SQL Server 2016 SP1, and an older server
    /// rejects every module statement in a plan. `None` where the version
    /// that decides it was not read.
    pub supports_create_or_alter: Option<bool>,
}

/// Reads the server's capabilities. `version` is what [`server_version`]
/// answered, where it answered: on SQL Server the version and the edition
/// together decide `supports_create_or_alter` (Azure reports 12.0.x and
/// supports the syntax regardless), and a version that could not be read
/// leaves that one `None` rather than guessed.
pub async fn capabilities(conn: &mut Conn, version: Option<&str>) -> Result<Capabilities, DbError> {
    match conn.driver() {
        Driver::Mssql => {
            use pbps_mssql::edition::Edition;
            let edition = pbps_mssql::edition::edition(conn).await?;
            Ok(Capabilities {
                edition: Some(match &edition {
                    // Named as unrecognised rather than passed through: the
                    // tool is about to treat it as limited, and an operator
                    // reading their own edition string back without comment
                    // would not know that.
                    Edition::Unknown(raw) => format!("{raw} (unrecognised; treated as limited)"),
                    known @ (Edition::Full(_) | Edition::Limited(_)) => known.name().to_owned(),
                }),
                supports_online: edition.supports_online(),
                supports_create_or_alter: version
                    .map(|v| pbps_mssql::doctor::supports_create_or_alter(v, edition.name())),
            })
        }
        // One edition, and every release this tool speaks to builds an index
        // `CONCURRENTLY` and replaces a routine with `CREATE OR REPLACE`; the
        // emitter writes both. Nothing is read because there is nothing that
        // could answer differently.
        Driver::Postgres => Ok(Capabilities {
            edition: None,
            supports_online: true,
            supports_create_or_alter: Some(true),
        }),
    }
}

/// What the connected server says about the plan's `strategy: online` hints
/// and the row rewrites its edition implies (ADR-0003).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditionVerdict {
    /// The changes whose `strategy: online` this server cannot honour, named
    /// as the plan names them. Empty is "every one is honoured".
    pub refused_online: Vec<String>,
    /// What the server runs, for the refusal's message.
    pub runs: String,
    /// Additions that are metadata-only on another edition and rewrite every
    /// row on this one, each already worded for the operator.
    pub warnings: Vec<String>,
}

/// Asks the server about a plan's edition-dependent questions, the only place
/// they can be answered honestly: the edition is a connection-time fact, and
/// nothing binds a saved plan to an environment.
pub async fn edition_verdict(
    conn: &mut Conn,
    changes: &ChangeSet,
) -> Result<EditionVerdict, DbError> {
    match conn.driver() {
        Driver::Mssql => {
            let edition = pbps_mssql::edition::edition(conn).await?;
            Ok(EditionVerdict {
                refused_online: pbps_mssql::edition::online_not_supported(changes, &edition),
                runs: edition.name().to_owned(),
                warnings: pbps_mssql::edition::size_of_data_warnings(changes, &edition),
            })
        }
        // No editions, so no hint is refused for one and no addition is
        // metadata-only elsewhere and a rewrite here: whether a PostgreSQL
        // change rewrites the table is the type's question, and the pure
        // dialect answers it at plan time (ADR-0012, `pbps_pg::estimate`).
        Driver::Postgres => Ok(EditionVerdict {
            refused_online: Vec::new(),
            runs: "PostgreSQL".to_owned(),
            warnings: Vec::new(),
        }),
    }
}

/// A connected capability answer, separate from the saved, checksum-pinned plan.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct ConnectedCheck {
    pub name: &'static str,
    pub engine: &'static str,
    pub status: &'static str,
    pub message: String,
}

/// Predict existing table/column/key DROP dependencies in the caller's transaction.
/// The other engine's rename reader does not answer this question (issue 254).
pub async fn check_drop_blockers(
    conn: &mut Conn,
    changes: &ChangeSet,
    ids: &pbps_model::IdsFile,
    stored: &pbps_model::Schema,
    stored_ids: &pbps_model::IdsFile,
) -> anyhow::Result<ConnectedCheck> {
    match conn.driver() {
        Driver::Mssql => {
            let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
            let reports = pbps_mssql::impact::key_drop_blockers(conn, &names).await?;
            refuse_drop_reports("SQL Server", &reports)?;
            // Two kinds of question now, and the count says which: a key this
            // plan drops, and a column it retypes while a key stands on it
            // (DECISIONS 515). Classified by the change each report points at
            // rather than by reading its target text, which belongs to the
            // engine that wrote it.
            let retypes = reports
                .iter()
                .filter(|r| {
                    matches!(
                        changes.changes.get(r.change_index).map(|p| &p.change),
                        Some(pbps_model::Change::AlterColumnType { .. })
                    )
                })
                .count();
            Ok(ConnectedCheck {
                name: "drop_blockers",
                engine: "SQL Server",
                status: "unavailable",
                message: format!(
                    "{} unique-key drop(s) and {retypes} column retype(s) checked; the table/column drop dependency reader is not implemented for SQL Server",
                    reports.len() - retypes
                ),
            })
        }
        Driver::Postgres => {
            let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
            let sources =
                pbps_model::physical_sources::PhysicalSources::of(names, stored, stored_ids)?;
            let reports = pbps_pg::impact::drop_blockers(conn, &sources).await?;
            refuse_drop_reports("PostgreSQL", &reports)?;
            Ok(ConnectedCheck {
                name: "drop_blockers",
                engine: "PostgreSQL",
                status: "passed",
                message: format!(
                    "{} table/column/key/index drop(s) checked against current catalog dependencies",
                    reports.len()
                ),
            })
        }
    }
}

fn refuse_drop_reports(
    engine: &str,
    reports: &[pbps_db::impact::DropReport],
) -> anyhow::Result<()> {
    let blocked: Vec<_> = reports
        .iter()
        .filter(|r| !r.blocking.is_empty())
        .map(|r| format!("{}: {}", r.target, r.blocking.join("; ")))
        .collect();
    if !blocked.is_empty() {
        anyhow::bail!(
            "drop_blockers ({engine}): {}.\nRemove these dependencies before the change that needs them gone, through earlier declared changes or a separately reviewed deployment, then recompute the plan.",
            blocked.join("\n")
        );
    }
    Ok(())
}

/// Check the permissions this deployment will grant, not every permission in
/// its recorded snapshot. A removal must not be refused for a grant it no
/// longer issues. The server version is checked again on apply because a
/// saved plan can travel between environments (DECISIONS 429).
/// A declared grant whose grantee already owns its target (#261).
///
/// # Why this is a refusal and not a plan
///
/// The `GRANT` cannot change what the role holds. Measured on 18.6: an owner
/// already holds every privilege on its own object with a `NULL` ACL, and
/// granting one back only forces the engine to write the *whole* default set
/// down — `relacl` becomes exactly `acldefault(r, owner)`, and a second grant
/// changes nothing again. The pull then skips that entry, because the owner's
/// is the zero point rather than a grant (DECISIONS 371), so the role reads
/// back as holding nothing there and the differ emits the same `GRANT` on
/// every plan. The apply runs it and its closing read refuses, having not
/// achieved what it asked for.
///
/// Recording the owner's entry as grants instead is the deadlock 371 exists
/// to prevent: a declaration naming one permission would be short of the
/// other seven, and every plan would revoke what ownership provides. So the
/// declaration is refused, with the line to delete.
///
/// # Why it is scoped to the declaration
///
/// A managed role that owns objects and is granted nothing on them is an
/// ordinary, plannable project. Only the grant is impossible, and only that
/// is refused.
// The complement is intentionally every change that does not create a
// securable a grant can name.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn owned_targets(
    driver: Driver,
    changes: &ChangeSet,
    ids: &pbps_model::IdsFile,
    owners: &std::collections::BTreeMap<pbps_model::GrantTarget, pbps_model::RoleName>,
    session_role: &pbps_model::RoleName,
) -> anyhow::Result<ConnectedCheck> {
    // The map is empty on a reader that carries no owner, so the loop would
    // find nothing anyway — named here so that "this engine was not asked"
    // reads differently from "this engine was asked and said no".
    if driver != Driver::Postgres {
        return Ok(ConnectedCheck {
            name: "owned_targets",
            engine: "SQL Server",
            status: "not_applicable",
            message: "SQL Server grants to an object's owner like any other principal, \
                      and this reader carries no object owner; the PostgreSQL check for a \
                      grant no `REVOKE` could ever undo does not apply"
                .to_owned(),
        });
    }
    // Who will own each target when the `GRANT` runs, which is not always who
    // owns it now. A plan that creates a target — or replaces one, which this
    // model spells as a drop and a create — leaves it owned by the role that
    // ran the statements, **measured** on 18.6: a table created in somebody
    // else's schema is owned by its creator, not by the schema's owner.
    //
    // Both directions matter. Asking the baseline about a replaced object
    // refuses a valid plan, because the old object's owner is gone with it;
    // asking it about a created one finds nothing and lets through the grant
    // whose closing read then refuses the apply.
    let created: std::collections::BTreeSet<pbps_model::GrantTarget> = changes
        .changes
        .iter()
        .filter_map(|planned| match &planned.change {
            pbps_model::Change::CreateTable { name, .. } => {
                Some(pbps_model::GrantTarget::Object(name.clone()))
            }
            pbps_model::Change::CreateModule { id, .. } => match id {
                pbps_model::ModuleId::Routine(r) => {
                    Some(pbps_model::GrantTarget::Routine(r.clone()))
                }
                pbps_model::ModuleId::Named(n) => Some(pbps_model::GrantTarget::Object(n.clone())),
                // A trigger is not a securable a grant can name.
                pbps_model::ModuleId::Trigger { .. } => None,
            },
            // Every other change either leaves the target where it was or
            // takes it away, and a grant on something this plan drops is a
            // different finding.
            _ => None,
        })
        .collect();

    // A rename moves the target without changing who owns it, and the grant
    // beside it in the same plan names the object as the plan leaves it. The
    // owner map is keyed by what the read saw, so the question has to be
    // asked under the earlier name.
    //
    // Only tables: a module is replaced rather than renamed, and a role this
    // dialect does not manage is renamed in the cluster before the plan is
    // built, so the read already knows it under its new name (DECISIONS 377).
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;

    let mut impossible = Vec::new();
    for planned in &changes.changes {
        let pbps_model::Change::Grant {
            role,
            target,
            permissions,
        } = &planned.change
        else {
            continue;
        };
        // An owner neither the plan nor the read supplies is an object this
        // read did not cover. Absent, not empty — and not something this
        // check can refuse on.
        let sequence_created = if let pbps_model::GrantTarget::Sequence(c) = target {
            created.contains(&pbps_model::GrantTarget::Object(c.table.clone()))
                || changes.changes.iter().any(|p| {
                    if let pbps_model::Change::AddColumn { table, name, .. } = &p.change {
                        return table == &c.table && name == &c.name;
                    }
                    if let pbps_model::Change::AlterColumnNumbering {
                        column, from, to, ..
                    } = &p.change
                    {
                        return column == c
                            && !matches!(from, pbps_model::ValueSource::Sequence(_))
                            && matches!(to, pbps_model::ValueSource::Sequence(_));
                    }
                    false
                })
        } else {
            false
        };
        let owner = if created.contains(target) || sequence_created {
            (!session_role.raw().is_empty()).then_some(session_role)
        } else {
            owners.get(&stored_grant_target(&names, target))
        };
        if owner == Some(role) {
            let when = if created.contains(target) || sequence_created {
                format!(
                    "role `{role}` will own `{target}`, because this plan creates it and \
                         the role that runs the statements owns what they create (measured)",
                    role = role.shown(),
                    target = target.shown(),
                )
            } else {
                format!(
                    "role `{role}` owns `{target}`",
                    role = role.shown(),
                    target = target.shown(),
                )
            };
            impossible.push(format!(
                "{when}, so `{}` on it is a grant that cannot change what the role holds: an \
                 owner already holds every privilege on its own object, and this engine \
                 records the whole default set rather than the one permission (ADR-0010 §1, \
                 measured). The pull reads that entry as the zero point rather than a grant \
                 (DECISIONS 371), so nothing would ever satisfy this line — delete it from \
                 role `{role}`",
                permissions
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                role = role.shown(),
            ));
        }
    }
    if !impossible.is_empty() {
        anyhow::bail!("owned_targets (PostgreSQL): {}", impossible.join("\n"));
    }
    Ok(ConnectedCheck {
        name: "owned_targets",
        engine: "PostgreSQL",
        status: "passed",
        message: "no declared grant names a target its own grantee owns".to_owned(),
    })
}

/// Refuse a plan that revokes a grant no `REVOKE` from this connection could
/// take away (#251).
///
/// Asked of the plan rather than of the read, and that is the whole point.
/// The grants are ordinary ones: a declaration that *keeps* one is satisfied
/// by the database exactly as it stands, and dropping them from the pull
/// instead refused every connected command over the database — `baseline`
/// included — for a plan that emits no statement at all.
///
/// The other direction has nothing downstream to catch it. A `REVOKE` whose
/// grantor the engine does not select reports success with the entry still
/// standing, and the apply's read-back does not notice: measured, it exited 0
/// and recorded the narrowing as converged (#700). So this is the only place
/// that answer can be given, and it is given before a statement runs.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn unrevocable_grants(
    driver: Driver,
    changes: &ChangeSet,
    ids: &pbps_model::IdsFile,
    unrevocable: &[pbps_db::catalog::Unrevocable],
) -> anyhow::Result<ConnectedCheck> {
    if driver != Driver::Postgres {
        return Ok(ConnectedCheck {
            name: "unrevocable_grants",
            engine: "SQL Server",
            status: "not_applicable",
            message: "SQL Server's `REVOKE` names its grantor with `AS`, and this reader \
                      carries no unrevocable grant; the PostgreSQL check for a grant this \
                      connection could not take away does not apply"
                .to_owned(),
        });
    }
    // A rename moves the target without changing who granted what on it, and
    // the `Revoke` beside it in the same plan names the object as the plan
    // leaves it — while this list is keyed by what the read saw. The same
    // translation `owned_targets` makes, for the same reason: without it a
    // plan that renames and narrows in one step passes a check that would
    // refuse either step alone.
    //
    // Only tables, again: a module is replaced rather than renamed, and a
    // schema this dialect does not rename cannot move under a grant.
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;

    let mut impossible = Vec::new();
    for planned in &changes.changes {
        let pbps_model::Change::Revoke {
            role,
            target,
            permissions,
        } = &planned.change
        else {
            continue;
        };
        let read_as = stored_grant_target(&names, target);
        for found in unrevocable {
            if &found.role == role
                && found.target == read_as
                && permissions.contains(&found.permission)
            {
                impossible.push(format!("role {role}: {}", found.why, role = role.shown(),));
            }
        }
    }
    if !impossible.is_empty() {
        anyhow::bail!("unrevocable_grants (PostgreSQL): {}", impossible.join("\n"));
    }
    Ok(ConnectedCheck {
        name: "unrevocable_grants",
        engine: "PostgreSQL",
        status: "passed",
        message: "every permission this plan revokes is one a `REVOKE` from this connection \
                  would carry the grantor of"
            .to_owned(),
    })
}

/// Grants on modules and schemas retain their catalog identity; only an
/// object tracked as a table participates in the plan's table renames.
fn stored_grant_target(
    names: &pbps_model::plan_names::PlanNames<'_>,
    target: &pbps_model::GrantTarget,
) -> pbps_model::GrantTarget {
    if let pbps_model::GrantTarget::Sequence(column) = target
        && let Some(stored) = names.stored_column(column, pbps_model::plan_names::At::End)
    {
        return pbps_model::GrantTarget::Sequence(stored);
    }
    if let pbps_model::GrantTarget::Object(table) = target
        && let Some(stored) = names.stored(table, pbps_model::plan_names::At::End)
    {
        return pbps_model::GrantTarget::Object(stored);
    }
    target.clone()
}

/// Catalog addresses of changed columns come from identity, including a
/// source-named drop whose address is handed to another column later.
fn changed_column<'c>(
    names: &pbps_model::plan_names::PlanNames<'_>,
    change: &'c pbps_model::Change,
) -> Option<(
    TableName,
    pbps_model::ColumnName,
    &'c pbps_model::ColumnRef,
    &'static str,
)> {
    use pbps_model::plan_names::At;
    let (uid, column, what) = crate::row_security::column_edit(change)?;
    let (owner, name) = names.column_at(uid, At::Start)?;
    Some((
        names.table_at(owner, At::Start)?.clone(),
        name.clone(),
        column,
        what,
    ))
}

fn stored_column(
    names: &pbps_model::plan_names::PlanNames<'_>,
    column: &pbps_model::ColumnRef,
) -> (TableName, pbps_model::ColumnName) {
    let stored = names
        .stored_column(column, pbps_model::plan_names::At::End)
        .unwrap_or_else(|| column.clone());
    (stored.table, stored.name)
}

/// Where the plan changes the expression of the generated column `generated`
/// (catalog names) to one that does not read `base`: the change that releases
/// `base` from it (DEC-1316.1). `None` when the plan does not change it, or
/// when its new text may still read `base`.
///
/// `base` is the input's catalog name and `named` the column as the plan names
/// it, after a rename: the new text speaks the plan's names, so naming either
/// one may still read it, unless the plan gives the catalog name to another
/// column.
fn release_of(
    changes: &ChangeSet,
    names: &pbps_model::plan_names::PlanNames<'_>,
    table: &TableName,
    generated: &pbps_model::ColumnName,
    base: &pbps_model::ColumnName,
    named: &pbps_model::ColumnRef,
) -> Option<usize> {
    // Where the plan renames the input and gives its catalog name to another
    // column, that name in the new text reads the other one (review of
    // #1424).
    let base_is_another = base != &named.name
        && reused(
            changes,
            &pbps_model::ColumnRef::new(named.table.clone(), base),
        );
    changes.changes.iter().position(|p| {
        matches!(&p.change, pbps_model::Change::AlterColumnExpression { column, to, .. }
            if stored_column(names, column) == (table.clone(), generated.clone())
                && (base_is_another || !pbps_pg::generated::may_read(to, base.raw()))
                && !pbps_pg::generated::may_read(to, named.name.raw()))
    })
}

/// Orders the retype or drop of a column a live generated column reads after
/// the plan's change to that column's expression that stops reading it
/// (DEC-1316.1). The differ puts a drop in class 5 and a retype at rank -1 of
/// class 9, both ahead of the expression change, where the engine still sees
/// the generated column reading the input and refuses.
///
/// The order comes from edges that do not depend on the order (DEC-1391.1):
/// - each live reader's release (`dependences`, by catalog name) runs before
///   the retype or drop of its input;
/// - a retype runs before the default written for its new type, matched by
///   uid, and before any other generated column's expression change whose new
///   text may read the column (`may_read`): once it reads it, the engine
///   refuses the retype.
///
/// A change runs where the differ put it unless an edge holds it back; then
/// it runs right after the last change it waits for. Nothing moves earlier.
/// Required inserts and their effect consumers follow a released drop
/// (#2174); they no longer cause the old #1397 kept-order refusal.
/// Returns how many changes moved, or a refusal:
/// - a change the moved one passes needs what it holds back: a column's name
///   taken before its drop runs, or a row, key or module that may need its
///   new type before its retype runs;
/// - a drop of a column another generated column's new text may read, which
///   no order performs (#1391);
/// - changes that wait on one another, as two generated columns trading
///   retyped inputs do.
// The complement is every change that carries no generation expression.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn order_after_releases<S: Clone>(
    changes: &mut PlanningPlan<S>,
    before: pbps_diff::Side<'_>,
    after: pbps_diff::Side<'_>,
    dependences: &BTreeMap<TableName, Vec<pbps_pg::generated::Dependence>>,
    dialect: &dyn pbps_dialect::Dialect,
    declared: &pbps_model::ModuleDeps,
) -> Result<usize, String> {
    use pbps_model::Change;
    use pbps_pg::generated::may_read;
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, after.ids)
        .map_err(|e| e.to_string())?;
    // Inputs a row cannot be inserted without while they stand, by their
    // live table and name (#1397), each with the position from which on it
    // is: `None` from the start, or `Some(j)` past the plan's first change
    // at `j` that drops or rewrites a trigger on its table, for an input
    // only a trigger fills. The catalog read the trigger as it stands before
    // the plan; once such a change has run, the one that would fill it may
    // be gone when the row is written. A trigger rewritten after the row
    // still fills it (#2178).
    let trigger_change_at = |table: &TableName| {
        changes.changes.iter().position(|p| {
            matches!(&p.change,
                Change::DropModule { id: pbps_model::ModuleId::Trigger { on, .. }, .. }
                | Change::AlterModule { id: pbps_model::ModuleId::Trigger { on, .. }, .. }
                    if on.schema == table.schema && on.name == table.name)
        })
    };
    let required: BTreeMap<(TableName, pbps_model::ColumnName), Option<usize>> = dependences
        .iter()
        .flat_map(|(table, ds)| {
            let from = trigger_change_at(table);
            ds.iter().filter_map(move |d| {
                let at = if d.base_required {
                    None
                } else if d.filled_by_trigger {
                    Some(from?)
                } else {
                    return None;
                };
                Some(((table.clone(), d.base.clone()), at))
            })
        })
        .collect();
    use pbps_diff::order::{Refusal, Schedule, Source};
    use pbps_model::resolver::OrderReason;
    let n = changes.changes.len();
    // What each change waits for, by position, and where that edge comes
    // from: the catalog's live readers, the new text's names, or the
    // changes' own payloads.
    let mut waits: Vec<std::collections::BTreeMap<usize, Source>> =
        vec![std::collections::BTreeMap::new(); n];
    for (i, p) in changes.changes.iter().enumerate() {
        let Some((table, live, column, _)) = changed_column(&names, &p.change) else {
            continue;
        };
        let readers: Vec<&pbps_model::ColumnName> = dependences
            .get(&table)
            .into_iter()
            .flatten()
            .filter(|d| d.base == live)
            .map(|d| &d.generated)
            .collect();
        for generated in &readers {
            if let Some(r) = release_of(changes, &names, &table, generated, &live, column) {
                waits[i].insert(r, Source::Catalog);
            }
        }
        // A generated column the catalog does not see reading the column,
        // whose new text may. A live reader whose new text still names it is
        // not released, which `unreleased` refuses. The text speaks the
        // plan's names, so where the plan renames the column and gives its
        // catalog name to another, that name reads the other one.
        let live_is_another = live != column.name
            && reused(
                changes,
                &pbps_model::ColumnRef::new(column.table.clone(), live.clone()),
            );
        for (j, q) in changes.changes.iter().enumerate() {
            // A changed expression, or a generated column the plan adds,
            // which reads its input from the moment it exists (#1425). The
            // differ adds a generated column after every column alteration
            // (DEC-1168.1), so only a drop has to look at it: a retype runs
            // before it already.
            let (c, to, added) = match &q.change {
                Change::AlterColumnExpression { column: c, to, .. } => (c.clone(), to, false),
                Change::AddColumn {
                    table: t,
                    name,
                    column: added,
                    ..
                } => match &added.value_source {
                    pbps_model::ValueSource::Generated(generated)
                        if matches!(p.change, Change::DropColumn { .. }) =>
                    {
                        (
                            pbps_model::ColumnRef::new(t.clone(), name.clone()),
                            &generated.expression,
                            true,
                        )
                    }
                    pbps_model::ValueSource::None
                    | pbps_model::ValueSource::Default(_)
                    | pbps_model::ValueSource::Identity(_)
                    | pbps_model::ValueSource::Sequence(_)
                    | pbps_model::ValueSource::Generated(_) => continue,
                },
                _ => continue,
            };
            let (at, generated) = stored_column(&names, &c);
            if at != table
                || (!added && readers.contains(&&generated))
                || !((!live_is_another && may_read(to, live.raw()))
                    || may_read(to, column.name.raw()))
            {
                continue;
            }
            if matches!(p.change, Change::AlterColumnType { .. }) {
                waits[j].entry(i).or_insert(Source::Lexical);
            } else if !reused(changes, column) {
                let what = if added {
                    "the expression of the new"
                } else {
                    "the new expression of"
                };
                return Err(format!(
                    "{column} is dropped in this plan, and {what} {c} may read it: once {c} \
                     reads it, the drop is refused, and before that the expression names a \
                     column that is gone. Change that expression in a plan of its own first, or \
                     write it without {column}.",
                    column = column.shown(),
                    c = c.shown()
                ));
            }
        }
        if let Change::AlterColumnType { uid, .. } = &p.change {
            for (j, q) in changes.changes.iter().enumerate() {
                if matches!(&q.change,
                    Change::AlterColumnDefault { uid: u, to: Some(_), .. } if u == uid)
                {
                    waits[j].entry(i).or_insert(Source::Effect);
                }
            }
        }
    }
    // Settled over those edges: a change runs where the differ put it unless
    // an edge holds it back, then right after the last change it waits for
    // (DEC-1391.1, DEC-1956.2). What a held change must not pass is kept, not
    // moved along with it (DEC-1956.3). Required inserts are the exception:
    // order carries their effect consumers in this same settlement (#2174).
    let mut schedule = Schedule::new(n);
    for (i, ws) in waits.iter().enumerate() {
        for (&w, &source) in ws {
            schedule.edge(w, i, source, OrderReason::Structural);
        }
    }
    let mut carried = std::collections::BTreeSet::new();
    for (h, ws) in waits.iter().enumerate() {
        if ws.is_empty() {
            continue;
        }
        // The furthest change it waits for, through what that waits for:
        // settled, it lands right after it, and passes exactly the changes
        // the differ put between (#1956 review). Bounding the kept orders by
        // it states only the crossings that happen, not one per later change.
        let mut reach = h;
        let mut seen = std::collections::BTreeSet::from([h]);
        let mut stack: Vec<usize> = ws.keys().copied().collect();
        while let Some(w) = stack.pop() {
            if seen.insert(w) {
                reach = reach.max(w);
                stack.extend(waits[w].keys().copied());
            }
        }
        keep_ahead(
            &mut schedule,
            changes,
            &names,
            &required,
            &mut carried,
            h,
            reach,
        );
    }
    let schedule = pbps_diff::order::release_inputs::carry_inserts(
        changes, schedule, &carried, before, after, dialect, declared,
    )?;
    let settled = match schedule.settle(changes.clone()) {
        Ok(settled) => settled,
        Err(refusal) => {
            return Err(match refusal {
                Refusal::Kept(keep) => keep.refusal,
                Refusal::Cycle { waiting, .. } => {
                    let first = waiting[0];
                    let column = changed_column(&names, &changes.changes[first].change)
                        .map_or_else(
                            || {
                                pbps_model::escape::Escaped(
                                    &changes.changes[first].change.subject(),
                                )
                                .to_string()
                            },
                            |c| c.2.shown().to_string(),
                        );
                    let waiting: Vec<String> = waiting
                        .iter()
                        .map(|&h| {
                            let change = &changes.changes[h].change;
                            format!(
                                "`{}` {}",
                                pbps_model::escape::Escaped(&change.subject()),
                                crate::report::describe(change)
                            )
                        })
                        .collect();
                    format!(
                        "{column} cannot be retyped in this plan: these changes wait on one \
                         another, as two generated columns trading retyped inputs do: {}. \
                         Change those generated columns to expressions that read neither \
                         input in a plan of its own first, then this one.",
                        waiting.join(", ")
                    )
                }
                Refusal::Outside { .. } | Refusal::Mismatch { .. } | Refusal::Unmet(_) => {
                    refusal.to_string()
                }
            });
        }
    };
    *changes = settled.changes;
    Ok(pbps_diff::order::schedule::moved(&settled.sequence))
}

/// Whether the plan gives a dropped column's name to another column: a reader
/// of that name then reads the new one.
fn reused(changes: &ChangeSet, column: &pbps_model::ColumnRef) -> bool {
    changes.changes.iter().any(|p| {
        matches!(&p.change, pbps_model::Change::AddColumn { table, name, .. }
            if *table == column.table && *name == column.name)
            || matches!(&p.change, pbps_model::Change::RenameColumn { table, to, .. }
                if *table == column.table && *to == column.name)
    })
}

/// Keep the existing name-taker and retype refusal boundaries (DEC-1956.3).
/// `reach` bounds crossings of the release's waits, preserving their original
/// refusal priority. Required inserts instead receive drop-to-insert edges
/// and seed the effect closure that carries their consumers (DEC-2174.1).
fn keep_ahead(
    schedule: &mut pbps_diff::order::Schedule,
    changes: &ChangeSet,
    names: &pbps_model::plan_names::PlanNames<'_>,
    required: &BTreeMap<(TableName, pbps_model::ColumnName), Option<usize>>,
    carried: &mut std::collections::BTreeSet<usize>,
    held: usize,
    reach: usize,
) {
    let change = &changes.changes[held].change;
    let Some((table, live, column, _)) = changed_column(names, change) else {
        return;
    };
    let later = || {
        changes
            .changes
            .iter()
            .enumerate()
            .take(reach)
            .skip(held + 1)
            .map(|(i, p)| (i, &p.change))
    };
    // #2086 states why an omitted required input must be gone. Move every
    // affected insert with its effect consumers, rather than refusing the
    // crossing or moving the row away from its child rows and keys (#2174).
    if matches!(change, pbps_model::Change::DropColumn { .. })
        && let Some(&from) = required.get(&(table, live))
    {
        for (i, _) in later().filter(|(i, c)| {
            matches!(c, pbps_model::Change::InsertRow { table, .. } if *table == column.table)
                && from.is_none_or(|j| j < *i)
        }) {
            schedule.edge(
                held,
                i,
                pbps_diff::order::Source::Catalog,
                pbps_model::resolver::OrderReason::Structural,
            );
            carried.insert(i);
        }
    }
    // A drop holds its name until it runs: a change taking the name needs it
    // free, and the release needs the column still there.
    if let Some((i, taker)) = later().find(|(_, c)| {
        matches!(c, pbps_model::Change::AddColumn { table, name, .. }
            if *table == column.table && *name == column.name)
            || matches!(c, pbps_model::Change::RenameColumn { table, to, .. }
            if *table == column.table && *to == column.name)
    }) {
        schedule.keep(
            held,
            i,
            format!(
                "{column} is released from its generated column by an expression change in \
                 this plan, and `{}` takes its name before that change runs. Apply the \
                 expression change in a plan of its own first, then this one.",
                pbps_model::escape::Escaped(&taker.subject()),
                column = column.shown()
            ),
        );
    }
    // A retype never moves past what the differ puts after the column
    // alterations. A release that follows a function the plan creates
    // sits after the rows, the constraints and the modules
    // (`after_the_rebuilds`), and any of those may need the column's new
    // type: a row writing a value only it accepts, a function whose body
    // is checked against it. The retype cannot both follow the release and
    // precede them, so the plan is refused by name (DEC-1316.1).
    if matches!(change, pbps_model::Change::AlterColumnType { .. })
        && let Some((i, past)) = later().find(|(_, c)| after_the_alterations(c))
    {
        schedule.keep(
            held,
            i,
            format!(
                "{column} is retyped after the expression change that releases it, which \
                 follows `{}`: that change may need the new type. Apply the expression \
                 change in a plan of its own first, then this one.",
                pbps_model::escape::Escaped(&past.subject()),
                column = column.shown()
            ),
        );
    }
}

/// A change the differ puts after the column alterations: rows, keys and
/// constraints, modules, roles and grants, and the data mode. Any of them may
/// need a column's new type, so a released retype must not move past one. The
/// changes ahead of the alterations, a column drop moved after its own release
/// among them, need nothing of another column's type.
fn after_the_alterations(change: &pbps_model::Change) -> bool {
    matches!(
        change,
        pbps_model::Change::InsertRow { .. }
            | pbps_model::Change::UpdateRow { .. }
            | pbps_model::Change::DeleteRow { .. }
            | pbps_model::Change::SetPrimaryKey { to: Some(_), .. }
            | pbps_model::Change::AddUnique { .. }
            | pbps_model::Change::AddForeignKey { .. }
            | pbps_model::Change::AddCheck { .. }
            | pbps_model::Change::AddIndex { .. }
            | pbps_model::Change::CreateModule { .. }
            | pbps_model::Change::AlterModule { .. }
            | pbps_model::Change::AddEnumValue { .. }
            | pbps_model::Change::RenameEnumValue { .. }
            | pbps_model::Change::CreateType { .. }
            | pbps_model::Change::DropType { .. }
            | pbps_model::Change::RenameType { .. }
            | pbps_model::Change::CreateRole { .. }
            | pbps_model::Change::Grant { .. }
            | pbps_model::Change::PublicExecution { .. }
            | pbps_model::Change::SetDataMode { .. }
            | pbps_model::Change::SetEnsureKeys { .. }
    )
}

/// [`order_after_releases`], with the live edges of every table the plan
/// retypes or drops a column of. PostgreSQL only.
pub async fn release_generated_inputs<S: Clone>(
    conn: &mut Conn,
    changes: &mut PlanningPlan<S>,
    before: pbps_diff::Side<'_>,
    after: pbps_diff::Side<'_>,
    dialect: &dyn pbps_dialect::Dialect,
    declared: &pbps_model::ModuleDeps,
) -> anyhow::Result<usize> {
    if conn.driver() != Driver::Postgres {
        return Ok(0);
    }
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, after.ids)?;
    let mut dependences = BTreeMap::new();
    for p in &changes.changes {
        if let Some((table, ..)) = changed_column(&names, &p.change)
            && let std::collections::btree_map::Entry::Vacant(e) = dependences.entry(table)
        {
            let found = pbps_pg::generated::dependences(conn, e.key()).await?;
            e.insert(found);
        }
    }
    order_after_releases(changes, before, after, &dependences, dialect, declared)
        .map_err(|why| anyhow::anyhow!("generation_support (PostgreSQL): {why}"))
}

/// Every retype or drop of a column a live generated column reads that
/// nothing releases first: the same plan dropping the generated column, or an
/// earlier change to its expression whose text does not name the column
/// (DEC-1168.1, DEC-1316.1). `dependences` is each table's live edges.
fn unreleased(
    changes: &ChangeSet,
    names: &pbps_model::plan_names::PlanNames<'_>,
    dependences: &BTreeMap<TableName, Vec<pbps_pg::generated::Dependence>>,
) -> Vec<String> {
    // A drop names its column by the catalog's name already: only its table
    // is reversed. A rename into the freed name is another column's.
    let dropped: std::collections::BTreeSet<(TableName, pbps_model::ColumnName)> = changes
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::DropColumn { .. } = &p.change {
                changed_column(names, &p.change).map(|(table, name, ..)| (table, name))
            } else {
                None
            }
        })
        .collect();
    let mut problems = Vec::new();
    for (i, p) in changes.changes.iter().enumerate() {
        let Some((table, live, column, what)) = changed_column(names, &p.change) else {
            continue;
        };
        for d in dependences.get(&table).into_iter().flatten() {
            if d.base != live || dropped.contains(&(table.clone(), d.generated.clone())) {
                continue;
            }
            if release_of(changes, names, &table, &d.generated, &live, column)
                .is_some_and(|r| r < i)
            {
                continue;
            }
            problems.push(format!(
                "{column} is {what} by this plan, and the generated column `{}` is computed \
                 from it: the engine refuses to retype such a column and drops it only with \
                 CASCADE. Drop the generated column in the same plan, or change its expression \
                 to one that does not name `{}`.",
                d.generated.shown(),
                column.name.shown(),
                column = column.shown()
            ));
        }
    }
    problems
}

/// The logical replication slots that would decode the rows this plan moves
/// out of a DEFAULT partition (#1788), listed and never refused: a slot's
/// consumer may take a delete and an insert in its stride, and whether it
/// does is what the operator allowing the move answers. A publication is
/// another matter, refused by the pre-flight, since a subscriber is known to
/// lose the rows. Asked only of a plan that makes a partition beside a
/// DEFAULT; whether it moves any row is the apply's to count.
pub async fn logical_slots(conn: &mut Conn, changes: &ChangeSet) -> anyhow::Result<ConnectedCheck> {
    if conn.driver() == Driver::Mssql {
        return Ok(ConnectedCheck {
            name: "logical_slots",
            engine: "SQL Server",
            status: "not_applicable",
            message: "SQL Server has no partition this model moves rows into".to_owned(),
        });
    }
    let moving: Vec<String> = changes
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::CreateTable {
                name,
                beside_default: Some(default),
                ..
            } = &p.change
            {
                Some(format!(
                    "{default} into {name}",
                    default = default.shown(),
                    name = name.shown()
                ))
            } else {
                None
            }
        })
        .collect();
    if moving.is_empty() {
        return Ok(ConnectedCheck {
            name: "logical_slots",
            engine: "PostgreSQL",
            status: "not_applicable",
            message: "this plan makes no partition beside a DEFAULT, so it moves no rows"
                .to_owned(),
        });
    }
    let slots = pbps_pg::impact::logical_slots(conn).await?;
    Ok(slots_listed(&moving, &slots))
}

/// [`logical_slots`]' answer over what the catalog said. Measured on 16 and
/// 18 with `test_decoding`: a move decodes as the DEFAULT's delete and the
/// new partition's insert.
fn slots_listed(moving: &[String], slots: &[(String, String)]) -> ConnectedCheck {
    let message = if slots.is_empty() {
        format!(
            "no logical replication slot on this database would decode the rows moved from {}",
            moving.join(", ")
        )
    } else {
        format!(
            "logical replication slot(s) {} on this database will decode each row moved from {} \
             as a delete and an insert",
            slots
                .iter()
                .map(|(slot, plugin)| format!(
                    "{} ({})",
                    pbps_model::escape::Escaped(slot),
                    pbps_model::escape::Escaped(plugin)
                ))
                .collect::<Vec<_>>()
                .join(", "),
            moving.join(", ")
        )
    };
    ConnectedCheck {
        name: "logical_slots",
        engine: "PostgreSQL",
        status: if slots.is_empty() { "passed" } else { "listed" },
        message,
    }
}

/// What logical decoding misses of a plan's identity or `serial` column
/// added to a table already there (#1890, DEC-1890.1), listed and never
/// refused. The engine numbers every row already there in the table's
/// rewrite, and measured on 18.6 that rewrite reaches no stream: a
/// `test_decoding` slot shows an empty transaction, a `pgoutput` slot nothing,
/// and a subscriber keeps the new column empty for those rows, while a row
/// inserted afterwards arrives with its number. So the publications over the
/// table and the database's logical slots are named, for whoever answers for
/// their consumers. A table the plan creates is empty when made: it has no
/// `AddColumn`, and its rows stream as they come.
pub async fn numbered_column_fills(
    conn: &mut Conn,
    changes: &ChangeSet,
    ids: &pbps_model::IdsFile,
) -> anyhow::Result<ConnectedCheck> {
    use pbps_model::plan_names::At;
    if conn.driver() == Driver::Mssql {
        return Ok(ConnectedCheck {
            name: "numbered_column_fills",
            engine: "SQL Server",
            status: "not_applicable",
            message: "SQL Server has no logical decoding this model reads".to_owned(),
        });
    }
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
    let mut fills: Vec<(TableName, pbps_model::ColumnName)> = Vec::new();
    for (i, p) in changes.changes.iter().enumerate() {
        let pbps_model::Change::AddColumn {
            table,
            name,
            column,
            ..
        } = &p.change
        else {
            continue;
        };
        let numbered = match column.value_source {
            pbps_model::ValueSource::Identity(_) | pbps_model::ValueSource::Sequence(_) => true,
            pbps_model::ValueSource::None
            | pbps_model::ValueSource::Default(_)
            | pbps_model::ValueSource::Generated(_) => false,
        };
        // The table under the name the catalog has it by now, after a
        // rename this plan may run first; none for one the plan creates.
        if let (true, Some(live)) = (numbered, names.stored(table, At::Before(i))) {
            fills.push((live, name.clone()));
        }
    }
    if fills.is_empty() {
        return Ok(ConnectedCheck {
            name: "numbered_column_fills",
            engine: "PostgreSQL",
            status: "not_applicable",
            message: "this plan adds no identity or serial column to a table already there"
                .to_owned(),
        });
    }
    let mut publications = Vec::new();
    for (table, _) in &fills {
        if !publications
            .iter()
            .any(|(t, _): &(TableName, _)| t == table)
        {
            let found = pbps_pg::impact::publications_of(conn, table).await?;
            publications.push((table.clone(), found));
        }
    }
    let slots = pbps_pg::impact::logical_slots(conn).await?;
    Ok(fills_listed(&fills, &publications, &slots))
}

/// [`numbered_column_fills`]' answer over what the catalog said.
fn fills_listed(
    fills: &[(TableName, pbps_model::ColumnName)],
    publications: &[(TableName, Vec<(String, bool)>)],
    slots: &[(String, String)],
) -> ConnectedCheck {
    let columns = fills
        .iter()
        .map(|(table, column)| format!("{}.{}", table.shown(), column.shown()))
        .collect::<Vec<_>>()
        .join(", ");
    let mut streams = Vec::new();
    let mut clauses = Vec::new();
    for (table, found) in publications {
        let mut whole = Vec::new();
        for (name, lists_columns) in found {
            if *lists_columns {
                clauses.push(format!(
                    "publication {} over {} lists its columns, so it publishes none of the \
                     added column(s) until they are added to its list",
                    pbps_model::escape::Escaped(name),
                    table.shown()
                ));
            } else {
                whole.push(pbps_model::escape::Escaped(name).to_string());
            }
        }
        if !whole.is_empty() {
            streams.push(format!(
                "publication(s) {} over {}",
                whole.join(", "),
                table.shown()
            ));
        }
    }
    if !slots.is_empty() {
        streams.push(format!(
            "logical replication slot(s) {} on this database",
            slots
                .iter()
                .map(|(slot, plugin)| format!(
                    "{} ({})",
                    pbps_model::escape::Escaped(slot),
                    pbps_model::escape::Escaped(plugin)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !streams.is_empty() {
        clauses.insert(
            0,
            format!(
                "{} will not carry those numbers, and a consumer keeps them empty until it takes \
                 a new snapshot of the table",
                streams.join("; ")
            ),
        );
    }
    let (status, message) = if clauses.is_empty() {
        (
            "passed",
            format!(
                "no publication or logical replication slot would miss the numbers {columns} \
                 gives the rows already there"
            ),
        )
    } else {
        (
            "listed",
            format!(
                "{columns} numbers the rows already there in a table rewrite, which logical \
                 decoding does not stream: {}",
                clauses.join("; ")
            ),
        )
    };
    ConnectedCheck {
        name: "numbered_column_fills",
        engine: "PostgreSQL",
        status,
        message,
    }
}

/// What a plan over generated columns needs of this server and of the columns
/// already there, asked before anything is written (DEC-1168.1).
///
/// `SET EXPRESSION` is PostgreSQL 17's; on 16 a plan that changes an
/// expression is refused by name, never turned into a drop and re-add that
/// would take the column's dependents and its place with it. And a column a
/// live generated column is computed from cannot be retyped, nor dropped
/// without `CASCADE` — measured on 16, 17 and 18 — so a plan that retypes or
/// drops one is refused here rather than by the engine halfway through. Two
/// things release it: the same plan dropping the generated column, or an
/// earlier change in the plan to the generated column's expression whose text
/// does not name the input (DEC-1316.1).
pub async fn generation_support(
    conn: &mut Conn,
    changes: &ChangeSet,
    ids: &pbps_model::IdsFile,
) -> anyhow::Result<ConnectedCheck> {
    if conn.driver() == Driver::Mssql {
        return Ok(ConnectedCheck {
            name: "generation_support",
            engine: "SQL Server",
            status: "not_applicable",
            message: "this model holds generated columns for PostgreSQL only".to_owned(),
        });
    }
    let mut problems = Vec::new();
    let expressions: Vec<&pbps_model::ColumnRef> = changes
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::AlterColumnExpression { column, .. } = &p.change {
                Some(column)
            } else {
                None
            }
        })
        .collect();
    let version = pbps_pg::roles::server_version_num(conn).await?;
    if !expressions.is_empty() && version < pbps_pg::generated::SET_EXPRESSION_ARRIVED_IN {
        for column in &expressions {
            problems.push(format!(
                "{column}: changing a generated column's expression needs PostgreSQL 17 or \
                 later (`SET EXPRESSION`), and this server is {version}",
                column = column.shown()
            ));
        }
    }
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
    let mut dependences = BTreeMap::new();
    for p in &changes.changes {
        if let Some((table, ..)) = changed_column(&names, &p.change)
            && let std::collections::btree_map::Entry::Vacant(e) = dependences.entry(table)
        {
            let found = pbps_pg::generated::dependences(conn, e.key()).await?;
            e.insert(found);
        }
    }
    problems.extend(unreleased(changes, &names, &dependences));
    if !problems.is_empty() {
        anyhow::bail!("generation_support (PostgreSQL): {}", problems.join("\n"));
    }
    Ok(ConnectedCheck {
        name: "generation_support",
        engine: "PostgreSQL",
        status: "passed",
        message: format!(
            "PostgreSQL server_version_num {version}: every generated-column change this plan \
             makes has an in-place form here, and no column it retypes or drops is one a live \
             generated column still reads at that point"
        ),
    })
}

/// Policy recipients need not be declared roles: an existing cluster role may
/// receive a policy without any managed grants. Ask the connected role reader,
/// shared by plan, bootstrap and saved-plan apply, rather than the managed scope.
async fn refuse_missing_policy_roles(conn: &mut Conn, changes: &ChangeSet) -> anyhow::Result<()> {
    let policies: Vec<_> = changes
        .changes
        .iter()
        .filter_map(|step| {
            if let pbps_model::Change::CreatePolicy {
                table,
                name,
                policy,
                ..
            } = &step.change
                && let pbps_model::PolicyRoles::Roles(roles) = &policy.to
            {
                Some((table, name, roles))
            } else {
                // PUBLIC is a target in its own right, never a role to look up.
                None
            }
        })
        .collect();
    let recipients = policies
        .iter()
        .flat_map(|(_, _, roles)| roles.iter())
        .collect();
    let missing: BTreeSet<_> = pbps_pg::roles::missing_roles(conn, &recipients)
        .await?
        .into_iter()
        .collect();
    let problems: Vec<_> = policies
        .iter()
        .flat_map(|(table, policy, roles)| {
            roles.iter().filter(|role| missing.contains(*role)).map(|role| {
                format!(
                    "policy `{}` on {} names missing cluster role `{}`; create it first, then plan again",
                    pbps_model::escape::Escaped(policy),
                    table.shown(),
                    role.shown()
                )
            })
        })
        .collect();
    if !problems.is_empty() {
        anyhow::bail!("missing_roles (PostgreSQL): {}", problems.join("\n"));
    }
    Ok(())
}

pub async fn permission_support(
    conn: &mut Conn,
    changes: &ChangeSet,
    ids: &IdsFile,
) -> anyhow::Result<ConnectedCheck> {
    match conn.driver() {
        Driver::Mssql => Ok(ConnectedCheck {
            name: "permission_support",
            engine: "SQL Server",
            status: "not_applicable",
            message: "SQL Server permissions are validated by its dialect; the PostgreSQL \
                      permission-version check does not apply"
                .to_owned(),
        }),
        Driver::Postgres => {
            let tables = crate::row_security::standing_owners(changes, ids)?;
            let ownership = pbps_pg::doctor::table_ownership(conn, &tables)
                .await
                .map_err(|error| {
                    anyhow::Error::new(error).context(format!(
                        "cannot read effective ownership for row-security tables: {}",
                        tables
                            .iter()
                            .map(|table| table.shown().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
            for (table, present, owned) in ownership {
                if !present {
                    anyhow::bail!(
                        "{}: standing row-security table is absent; plan again",
                        table.shown()
                    );
                }
                if !owned {
                    anyhow::bail!(
                        "{}: the deployer must own the table to change row-security policies or flags",
                        table.shown()
                    );
                }
            }
            refuse_missing_policy_roles(conn, changes).await?;
            let version = pbps_pg::roles::server_version_num(conn).await?;
            let mut roles: BTreeMap<pbps_model::RoleName, pbps_model::Role> = BTreeMap::new();
            for planned in &changes.changes {
                if let pbps_model::Change::Grant {
                    role,
                    target,
                    permissions,
                } = &planned.change
                {
                    roles
                        .entry(role.clone())
                        .or_default()
                        .grants
                        .entry(target.clone())
                        .or_default()
                        .extend(permissions.iter().copied());
                }
            }
            let errors: Vec<String> = roles
                .iter()
                .flat_map(|(name, role)| {
                    pbps_pg::roles::unsupported_permissions(version, name, role)
                })
                .map(|error| error.to_string())
                .collect();
            if !errors.is_empty() {
                anyhow::bail!("permission_support (PostgreSQL): {}", errors.join("\n"));
            }
            Ok(ConnectedCheck {
                name: "permission_support",
                engine: "PostgreSQL",
                status: "passed",
                message: format!(
                    "PostgreSQL server_version_num {version} supports the permissions \
                                  this plan grants"
                ),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// `doctor`: what the connected account may do
// ---------------------------------------------------------------------------

/// How the ledger's tables differ from the recipe pbps creates them from, one
/// line per difference; empty when they match or are not there yet.
///
/// PostgreSQL's answer is `pbps_pg::state::ledger_problems` (issue #313):
/// there, a role with `CREATE` on `public` can make the ledger tables first
/// and attach code the deployment account would run. SQL Server's ledger
/// lives in `dbo`, and whether it needs the same check is not this issue's
/// question, so it reports none.
pub async fn ledger_problems(conn: &mut Conn) -> Result<Vec<String>, DbError> {
    match conn.driver() {
        Driver::Mssql => Ok(Vec::new()),
        Driver::Postgres => pbps_pg::state::ledger_problems(conn).await,
    }
}

/// A permission the deployment needs and the connected account does not hold,
/// with the securable already spelled the way this engine's `GRANT` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionGap {
    pub permission: &'static str,
    pub securable: String,
    pub why: String,
}

/// What `doctor` learns about the connected account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Permissions {
    pub gaps: Vec<PermissionGap>,
    /// What the account lacks that only some plans need, each with what it
    /// would allow: advice, never a gap (#1644).
    pub advised: Vec<PermissionGap>,
    /// The managed schemas the database does not have yet.
    pub absent_schemas: BTreeSet<String>,
}

/// Reads what the connected account holds and reports what it lacks.
///
/// The gaps are rendered by the engine because the securable's spelling is
/// the engine's: `OBJECT::[dbo].[t]` on one, `TABLE "app"."t"` on the other,
/// and the report offers each as the securable a `GRANT` names.
///
/// `project_ids` is the project's own identity mapping — every name in `ask`
/// is the declared one. Both engines resolve it against this environment's
/// recorded mapping before asking about a pending rename's current object.
pub async fn permissions(
    conn: &mut Conn,
    project_ids: &IdsFile,
    ask: &Ask<'_>,
) -> Result<Permissions, DbError> {
    match conn.driver() {
        Driver::Mssql => {
            let held = pbps_mssql::doctor::permissions(
                conn,
                ask.managed_tables,
                ask.managed_schemas,
                ask.referenced_columns,
                ask.granted,
                ask.data,
                ask.declared_keys,
                project_ids,
            )
            .await?;
            let render = |gaps: Vec<pbps_mssql::doctor::Gap>| -> Vec<PermissionGap> {
                gaps.into_iter()
                    .map(|g| PermissionGap {
                        permission: g.permission,
                        securable: g.securable(),
                        why: g.why.into(),
                    })
                    .collect()
            };
            Ok(Permissions {
                gaps: render(pbps_mssql::doctor::missing(&held)),
                advised: render(pbps_mssql::doctor::advised(&held)),
                absent_schemas: held.absent_schemas,
            })
        }
        Driver::Postgres => {
            let held = pbps_pg::doctor::permissions(conn, ask, project_ids).await?;
            Ok(Permissions {
                gaps: pbps_pg::doctor::missing(&held)
                    .into_iter()
                    .map(|g| PermissionGap {
                        permission: g.permission,
                        securable: g.securable(),
                        why: g.why,
                    })
                    .collect(),
                // PostgreSQL's dependency reads need nothing the required
                // list does not ask.
                advised: Vec::new(),
                absent_schemas: held.absent_schemas,
            })
        }
    }
}

/// Whether each partitioned parent's partitions reach far enough past its
/// rows, and whether its DEFAULT holds rows of a range not created yet
/// (#1789, DEC-1789.2). Reads existence, never a value, and writes nothing.
///
/// SQL Server has no RANGE partitions to run out of, so the question does not
/// apply and the report is empty.
pub async fn partition_runway(
    conn: &mut Conn,
    ask: &pbps_db::doctor::RunwayAsk<'_>,
    project_ids: &IdsFile,
) -> Result<pbps_db::doctor::RunwayReport, DbError> {
    match conn.driver() {
        Driver::Mssql => Ok(pbps_db::doctor::RunwayReport::default()),
        Driver::Postgres => pbps_pg::doctor::partition_runway(conn, ask, project_ids).await,
    }
}

/// What `doctor` tells the operator when its lock read was refused, in this
/// engine's words: why, and the one statement that repairs what a fresh
/// session could see (#822).
///
/// The remedies used to be SQL Server's alone: a PostgreSQL environment whose
/// lock read was denied was told to grant `SELECT` on `dbo.__pbps_lock`, a
/// table that does not exist there (#306). Then they were prose, which
/// neither `psql` nor `sqlcmd` can run, and asked for every grant at once —
/// including `SELECT` on a lock table a never-initialized database does not
/// have (#821, #822). Now the statement is chosen from what the catalog
/// shows, one grant at a time; after it, `doctor` run again names the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRemedy {
    /// Why the lock could not be read, as far as the catalog says, and how
    /// to run the statement. Goes in the finding's message.
    pub why: String,
    /// One complete statement, or `None` when nothing seen explains the
    /// failure and no grant is named.
    pub statement: Option<String>,
}

/// Asks a session of its own, with the same connection string, why the lock
/// could not be read (#822).
///
/// A fresh session rather than the one that failed: the failed read's own
/// recovery is deliberately quiet (`rewind_quietly`), so that session is not
/// proof of a usable one, and a question asked on it could answer for the
/// wrong state. Same identity, so the catalog answers for the role that
/// deploys. Every failure here, the connection included, is
/// [`LockReadGap::Unexplained`]: a question that could not be asked never
/// becomes a grant.
pub async fn lock_read_gap(driver: Driver, connection: &str) -> pbps_db::doctor::LockReadGap {
    use pbps_db::doctor::LockReadGap;
    let Ok(mut conn) = Conn::connect(driver, connection).await else {
        return LockReadGap::Unexplained;
    };
    let answer = match driver {
        Driver::Mssql => pbps_mssql::doctor::lock_read_gap(&mut conn).await,
        Driver::Postgres => pbps_pg::doctor::lock_read_gap(&mut conn).await,
    };
    answer.unwrap_or(LockReadGap::Unexplained)
}

/// The remedy for a lock read that failed, from what [`lock_read_gap`] saw.
/// `None` — nothing asked — is treated as nothing seen.
///
/// The principal is the whole quoted placeholder, `"<database role>"` or
/// `"<database user>"`: the message says to replace it, quotes included,
/// with the name quoted as an identifier. It is not SQL-escaped for the
/// operator, and it cannot be: pbps does not know which principal they will
/// grant to.
pub fn lock_remedy(driver: Driver, gap: Option<pbps_db::doctor::LockReadGap>) -> LockRemedy {
    use pbps_db::doctor::LockReadGap;
    let gap = gap.unwrap_or(LockReadGap::Unexplained);
    let (noun, principal) = match driver {
        Driver::Mssql => ("user", crate::report::placeholder("database user")),
        Driver::Postgres => ("role", crate::report::placeholder("database role")),
    };
    let how = format!(
        "run the remedy as a principal allowed to grant it, with {principal} (quotes \
         included) replaced by this {noun}'s name quoted as an identifier, then rerun doctor"
    );
    let next = "if the lock is still unreadable after that, doctor names the next grant";
    match (driver, gap) {
        (Driver::Postgres, LockReadGap::SchemaUsage) => LockRemedy {
            why: format!(
                "this role may not use schema {schema}, which holds the ledger; {how}; {next}",
                schema = pbps_pg::state::LEDGER_SCHEMA
            ),
            statement: Some(format!(
                "GRANT USAGE ON SCHEMA {} TO {principal};",
                pbps_pg::state::LEDGER_SCHEMA
            )),
        },
        (Driver::Postgres, LockReadGap::TableSelect) => LockRemedy {
            why: format!(
                "{} exists and this role may not read it; {how}",
                pbps_pg::state::LOCK_TABLE
            ),
            statement: Some(format!(
                "GRANT SELECT ON TABLE {} TO {principal};",
                pbps_pg::state::LOCK_TABLE
            )),
        },
        // Measured on 2025 RTM: without any permission on it the table is
        // invisible, and whether it lacks only `SELECT` cannot be told. The
        // object's own `VIEW DEFINITION` makes it visible and nothing else:
        // the database-wide grant would also show every other schema's
        // objects and module definitions, which the narrower grants of
        // DECISIONS 505 keep hidden (review of #1641).
        (Driver::Mssql, LockReadGap::Hidden) => LockRemedy {
            why: format!(
                "this user cannot see {table} at all, so what it lacks on it cannot be told \
                 yet; {how}; {next}; the statement fails if {table} does not exist, and then \
                 the database is not initialized as pbps expects",
                table = pbps_mssql::state::LOCK_TABLE
            ),
            statement: Some(format!(
                "GRANT VIEW DEFINITION ON OBJECT::{} TO {principal};",
                pbps_mssql::state::LOCK_TABLE
            )),
        },
        (Driver::Mssql, LockReadGap::TableSelect) => LockRemedy {
            why: format!(
                "{} exists and this user may not read it; {how}; a DENY through a role this \
                 user is in overrides the grant, and leaves the lock unreadable",
                pbps_mssql::state::LOCK_TABLE
            ),
            statement: Some(format!(
                "GRANT SELECT ON OBJECT::{} TO {principal};",
                pbps_mssql::state::LOCK_TABLE
            )),
        },
        (Driver::Postgres, _) => LockRemedy {
            why: format!(
                "nothing this role can see explains it: check that the connection names the \
                 role that deploys and that {} is intact, then rerun doctor",
                pbps_pg::state::LOCK_TABLE
            ),
            statement: None,
        },
        (Driver::Mssql, _) => LockRemedy {
            why: format!(
                "nothing this user can see explains it: check what the login is mapped to in \
                 this database and that {} is intact, then rerun doctor",
                pbps_mssql::state::LOCK_TABLE
            ),
            statement: None,
        },
    }
}

/// What to check when `doctor` could not read the account's permissions, in
/// this engine's words. Advice, not a statement: which read failed is not
/// carried this far, so no single grant is known to be the repair (#822).
pub fn permission_read_advice(driver: Driver) -> String {
    match driver {
        Driver::Mssql => "check that this user has VIEW DEFINITION in this database and what \
                          the login is mapped to, then rerun doctor"
            .to_owned(),
        // The read asks `has_*_privilege` and the system catalogs, which
        // every role may read unless someone revoked it; the other way it
        // fails is a connection that is not the role meant to deploy. But
        // once the project has recorded identities (tables, columns or a
        // tombstone), `pbps_pg::doctor::permissions` first reads the last
        // recorded state, and that read needs the ledger's own grants: a
        // role without them was sent to the catalogs, which were fine
        // (#820). Named conditionally, because which read failed is not
        // carried this far, and the table may not exist at all.
        Driver::Postgres => format!(
            "check that the connection names the role that deploys and that it can read the \
             system catalogs (`pg_catalog`); once this project has recorded identities, doctor \
             also reads its last recorded state, which needs USAGE on schema {} and, where that \
             table exists, SELECT on {}",
            pbps_pg::state::LEDGER_SCHEMA,
            pbps_pg::state::STATE_TABLE
        ),
    }
}

/// Whether `strategy: online` depends on something only a connection can read.
///
/// On SQL Server it does: online index operations are an Enterprise-edition
/// feature, so an offline plan emits `ONLINE = ON` without knowing whether the
/// target accepts it, and says so (ADR-0003). PostgreSQL has one edition and
/// builds every index this tool emits `CONCURRENTLY` on every release it speaks
/// to ([`capabilities`] reads nothing there for the same reason), so an
/// offline plan's online statements are already what the target will run.
/// Telling a PostgreSQL user their plan was unverified because of an edition
/// question their engine does not have was the defect (#1614).
pub const fn online_depends_on_the_edition(driver: Driver) -> bool {
    match driver {
        Driver::Mssql => true,
        Driver::Postgres => false,
    }
}

/// What `doctor` says when it could not read what this server can do, in this
/// engine's words. The cause is the read's own error and goes in as it is.
///
/// The sentence used to be SQL Server's alone: a PostgreSQL environment whose
/// version read failed was told that `CREATE OR ALTER` and online index
/// operations were undetermined, neither of which the version decides there
/// (#817). On PostgreSQL that read is the only one that can fail here
/// ([`capabilities`] reads nothing on that engine), and what the version
/// decides is checked again by [`permission_support`] and the generated-column
/// check whenever a plan is made against the server, so the message says that
/// rather than promising a later query will answer.
pub fn capabilities_unknown_message(driver: Driver, cause: &str) -> String {
    match driver {
        Driver::Mssql => format!(
            "this server's version or edition could not be read ({cause}), so whether it \
             accepts `CREATE OR ALTER` and online index operations is undetermined"
        ),
        Driver::Postgres => format!(
            "this PostgreSQL server's version could not be read ({cause}), so doctor cannot \
             report it; what the version decides (`MAINTAIN` grants, changing a generated \
             column's expression) is checked again when a plan is made against this server"
        ),
    }
}

// ---------------------------------------------------------------------------
// What a rename touches (SPEC §7.4)
// ---------------------------------------------------------------------------

/// The lines printed under a rename's impact report, in this engine's words.
///
/// What an empty list *means* is the engine's to say (`pbps_pg::impact::notes`
/// explains the carried list and the view alias it keeps). SQL Server fills
/// neither `carried` nor a note of its own beyond the one about what no
/// catalog can see, which it says only under an advisory list, as before.
pub fn impact_notes(driver: Driver, report: &ImpactReport) -> Vec<String> {
    match driver {
        Driver::Mssql => {
            if report.advisory.is_empty() {
                Vec::new()
            } else {
                vec![
                    "Nothing outside the database is visible here: applications and \
                     downstream consumers need a human's checklist."
                        .to_owned(),
                ]
            }
        }
        Driver::Postgres => pbps_pg::impact::notes(report),
    }
}

/// Every rename in a plan, as the objects they are renamed *from* — the name
/// the catalog still knows them by. Which changes count is each engine's
/// question: a module rename is a drop plus a create, and only SQL Server asks
/// the drop side through the rename-impact query.
pub fn rename_targets(
    driver: Driver,
    names: &pbps_model::plan_names::PlanNames<'_>,
) -> Vec<RenameTarget> {
    match driver {
        Driver::Mssql => pbps_mssql::impact::rename_targets(names),
        Driver::Postgres => pbps_pg::impact::rename_targets(names),
    }
}

pub async fn rename_impact(
    conn: &mut Conn,
    target: &RenameTarget,
) -> Result<ImpactReport, ImpactError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::impact::rename_impact(conn, target).await,
        Driver::Postgres => pbps_pg::impact::rename_impact(conn, target).await,
    }
}

/// Bootstrap can build database roles, but cluster roles must already exist
/// and still exist when its successful snapshot is recorded.
pub fn refuse_missing_cluster_roles(
    driver: Driver,
    missing: &[pbps_model::RoleName],
) -> anyhow::Result<()> {
    match driver {
        // Missing database roles are created by the SQL Server typed plan.
        Driver::Mssql => Ok(()),
        Driver::Postgres => {
            if !missing.is_empty() {
                anyhow::bail!(
                    "{}",
                    missing
                        .iter()
                        .map(|name| pbps_pg::roles::refuse_missing(name).to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            Ok(())
        }
    }
}

/// Cluster-owned identities move before pbps plans their database grants.
/// SQL Server moves its database roles through the typed statements instead.
pub struct ExternalRoleRenames {
    pub renames: BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
    pub check: ConnectedCheck,
}

pub async fn external_role_renames(
    conn: &mut Conn,
    recorded: &pbps_model::IdsFile,
    declared: &pbps_model::IdsFile,
) -> anyhow::Result<ExternalRoleRenames> {
    match conn.driver() {
        Driver::Mssql => Ok(ExternalRoleRenames {
            renames: BTreeMap::new(),
            check: ConnectedCheck {
                name: "rename_evidence",
                engine: "SQL Server",
                status: "not_applicable",
                message: "SQL Server renames database roles through the typed plan; external cluster-role rename evidence does not apply".into(),
            },
        }),
        Driver::Postgres => {
            let mut renames = BTreeMap::new();
            for (uid, from) in &recorded.roles {
                let Some(to) = declared.roles.get(uid).filter(|to| *to != from) else {
                    continue;
                };
                let evidence = pbps_pg::roles::rename_evidence(conn, from, to).await?;
                if let Some(error) = pbps_pg::roles::refuse_rename(from, to, evidence) {
                    anyhow::bail!("rename_evidence (PostgreSQL): {error}");
                }
                renames.insert(from.clone(), to.clone());
            }
            let check = ConnectedCheck {
                name: "rename_evidence",
                engine: "PostgreSQL",
                status: "passed",
                message: format!("{} external cluster-role rename(s) confirmed complete", renames.len()),
            };
            Ok(ExternalRoleRenames { renames, check })
        }
    }
}

/// Reconcile only incoming and renamed cluster roles against the same read
/// used for the baseline checksum. A second presence query could approve a
/// role that arrived after that read, while the saved baseline still lacks it.
pub fn reconcile_cluster_roles(
    driver: Driver,
    recorded: &pbps_model::IdsFile,
    queried: &pbps_model::IdsFile,
    renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
    live: &Schema,
    expected: &mut Schema,
) -> anyhow::Result<ConnectedCheck> {
    match driver {
        Driver::Mssql => Ok(ConnectedCheck {
            name: "missing_roles",
            engine: "SQL Server",
            status: "not_applicable",
            message: "SQL Server creates and renames database roles through the typed plan; the external cluster-role presence check does not apply".into(),
        }),
        Driver::Postgres => {
            let mut checked = 0;
            for to in queried.roles.values().filter(|name| {
                renames.values().any(|to| to == *name)
                    || !recorded.roles.values().any(|old| old == *name)
            }) {
                let Some(role) = live.roles.get(to) else {
                    anyhow::bail!("missing_roles (PostgreSQL): declared cluster role `{to}` is missing; create or rename it first, then plan again",
                        to = to.shown(),
                    );
                };
                expected.roles.insert(to.clone(), role.clone());
                checked += 1;
            }
            Ok(ConnectedCheck {
                name: "missing_roles",
                engine: "PostgreSQL",
                status: "passed",
                message: format!("{checked} incoming or renamed cluster role(s) present in the queried baseline"),
            })
        }
    }
}

/// A replacement may be explicit or synthesized as a drop and create.
/// Ordinary drops intentionally discard the object; they are not rebuilds.
// The complement is intentionally every change that does not write a module.
#[allow(clippy::wildcard_enum_match_arm)]
fn rebuilt_modules(
    changes: &ChangeSet,
) -> BTreeMap<pbps_model::ModuleId, (pbps_model::ModuleKind, pbps_model::ModuleKind)> {
    use pbps_model::Change;
    let dropped: BTreeMap<_, _> = changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::DropModule { id, kind } => Some((id, *kind)),
            _ => None,
        })
        .collect();
    changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::AlterModule { id, module } => Some((id.clone(), (module.kind, module.kind))),
            Change::CreateModule { id, module } => dropped
                .get(id)
                .map(|kind| (id.clone(), (*kind, module.kind))),
            _ => None,
        })
        .collect()
}

/// The routines a set of changes creates or replaces, with the kind each
/// will have: what [`refuse_unsafe_definers`] reads back.
pub fn written_routines(
    changes: &ChangeSet,
) -> Vec<(pbps_model::ModuleId, pbps_model::ModuleKind)> {
    use pbps_model::{ModuleAfter, ModuleKind};
    // Through `Change::module`, which is exhaustive: a change added later that
    // leaves a routine standing is counted without a wildcard to hide it.
    changes
        .changes
        .iter()
        .filter_map(|p| match p.change.module()? {
            (id, ModuleAfter::Standing(module))
                if matches!(module.kind, ModuleKind::Function | ModuleKind::Procedure) =>
            {
                Some((id.clone(), module.kind))
            }
            (_, ModuleAfter::Standing(_) | ModuleAfter::Gone) => None,
        })
        .collect()
}

/// What an absent routine means to [`refuse_unsafe_definers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absent {
    /// Every statement has run, so the routine must be there.
    Refused,
    /// A staged step before the one that creates it.
    NotYetCreated,
}

/// Refuses a `SECURITY DEFINER` routine this plan created or replaced that
/// does not set `search_path` with `pg_temp` last (#322, DEC-322.1).
///
/// Runs inside the transaction that wrote the routines, after the statements,
/// so that the refusal rolls them back. It reads the stored `proconfig`, which
/// is what the engine will use when the routine is called, rather than
/// guessing from the declaration's text.
pub async fn refuse_unsafe_definers(
    conn: &mut Conn,
    routines: &[(pbps_model::ModuleId, pbps_model::ModuleKind)],
    absent: Absent,
) -> anyhow::Result<()> {
    use pbps_pg::modules::DefinerPath;
    if conn.driver() != Driver::Postgres || routines.is_empty() {
        return Ok(());
    }
    let mut unsafe_ = Vec::new();
    for (id, kind) in routines {
        match pbps_pg::modules::definer_path(conn, id, *kind).await? {
            DefinerPath::Safe => {}
            DefinerPath::Absent if absent == Absent::NotYetCreated => {}
            DefinerPath::Absent => anyhow::bail!(
                "{id}: this plan writes the routine, and the catalog holds no routine by that \
                 identity after its statements ran",
                id = id.shown()
            ),
            DefinerPath::Unsafe(why) => unsafe_.push(format!("{id}: {why}", id = id.shown())),
        }
    }
    if !unsafe_.is_empty() {
        anyhow::bail!(
            "a SECURITY DEFINER routine this plan writes is not in PostgreSQL's safe form:\n  {}\n\
             Give each one `SET search_path = <its schemas>, pg_temp` in its declaration, with \
             pg_temp last, and plan again (DEC-322.1). Nothing has been applied; the \
             transaction was rolled back.",
            unsafe_.join("\n  ")
        );
    }
    Ok(())
}

/// PostgreSQL rebuilds must keep their locks, checks and DDL in one
/// transaction — and so must the revoke that closes a routine this plan
/// creates.
pub fn require_transactional_rebuilds(
    driver: Driver,
    changes: &ChangeSet,
    staged: bool,
) -> anyhow::Result<()> {
    if driver == Driver::Postgres && staged && !rebuilt_modules(changes).is_empty() {
        anyhow::bail!(
            "PostgreSQL module rebuilds require a transaction to preserve their catalog state \
             (ADR-0009 §3); remove --staged and plan again"
        );
    }
    // The `CREATE` and the revoke that closes the routine are two statements,
    // and a staged run commits each one on its own (DECISIONS 517). Between
    // those two commits the routine is visible to the whole cluster holding
    // this engine's default `EXECUTE` to `PUBLIC` — and the revoke sorts with
    // the grants, after every row the plan writes, so the window is the rest
    // of the plan rather than an instant. A `SECURITY DEFINER` routine open
    // for that long is the exposure this revoke exists to prevent, so the
    // plan is refused rather than run with a gap in it.
    let closes: Vec<String> = changes
        .changes
        .iter()
        .filter_map(|p| {
            // Only the half whose window is an over-exposure. `Kept`
            // writes a `GRANT` of its own, but the state between its
            // `CREATE` and that grant is a routine *less* reachable than the
            // declaration asks for — a caller sees a permission error, not
            // somebody else's privileges — so staging it is merely slow.
            if let pbps_model::Change::PublicExecution {
                routine,
                access: pbps_model::PublicAccess::Revoked,
                ..
            } = &p.change
            {
                Some(routine.shown().to_string())
            } else {
                None
            }
        })
        .collect();
    if staged && !closes.is_empty() {
        anyhow::bail!(
            "a staged run commits each statement on its own, so {} would stay executable by \
             every principal in the cluster between its `CREATE` and the revoke that closes it \
             (DECISIONS 517): {}.\nRemove --staged and plan again, or declare \
             `public_execute: true` on the routines that are meant to stay open",
            if closes.len() == 1 {
                "1 routine".to_owned()
            } else {
                format!("{} routines", closes.len())
            },
            closes.join(", ")
        );
    }
    Ok(())
}

/// Called inside the transaction before the DROP and after the CREATE.
/// SQL Server's CREATE OR ALTER preserves this state without a rebuild.
pub async fn check_module_rebuilds(
    conn: &mut Conn,
    changes: &ChangeSet,
    after: bool,
) -> anyhow::Result<ConnectedCheck> {
    match conn.driver() {
        Driver::Mssql => Ok(ConnectedCheck {
            name: "before_a_rebuild",
            engine: "SQL Server",
            status: "not_applicable",
            message: "SQL Server uses CREATE OR ALTER to retain module state; the PostgreSQL drop/recreate check does not apply".into(),
        }),
        Driver::Postgres => {
            let rebuilt = rebuilt_modules(changes);
            for (id, (before_kind, after_kind)) in &rebuilt {
                let found = pbps_pg::modules::before_a_rebuild(
                    conn,
                    id,
                    if after { *after_kind } else { *before_kind },
                    changes,
                )
                .await?;
                if let Some(reason) = found.refusal() {
                    anyhow::bail!("before_a_rebuild (PostgreSQL): {reason}");
                }
                if !after && let pbps_pg::modules::Serialized::Not(reason) = found.serialized {
                    eprintln!("warning: {id}: {reason}", id = id.shown());
                }
            }
            Ok(ConnectedCheck {
                name: "before_a_rebuild",
                engine: "PostgreSQL",
                status: "passed",
                message: format!("{} module rebuild(s) checked for catalog state a replacement cannot preserve", rebuilt.len()),
            })
        }
    }
}

/// Complete physical evidence for future table-root expansion; this grants no admission.
#[allow(
    dead_code,
    reason = "PR2c consumes these facts; PR4 alone grants admission"
)]
pub async fn catalog_dependents(
    conn: &mut Conn,
    root: &pbps_diff::order::catalog::graph::Root,
    source: pbps_diff::Side<'_>,
) -> anyhow::Result<pbps_diff::order::catalog::graph::Catalog> {
    anyhow::ensure!(
        conn.driver() == Driver::Postgres,
        "{root:?}: complete dependency evidence requires PostgreSQL; SQL Server rebuild is refused"
    );
    Ok(pbps_pg::modules::dependencies::collect(conn, root, source).await?)
}

/// Complete naming domains for a rebuild, never a permissive offline default.
#[allow(dead_code)] // PR2c.2b.2 connects this seam; admission stays refused.
pub async fn rebuild_names(
    conn: &mut Conn,
    schemas: &std::collections::BTreeSet<String>,
) -> anyhow::Result<pbps_diff::order::catalog::graph::Naming> {
    match conn.driver() {
        Driver::Postgres => Ok(pbps_pg::partition_index::collect_names(conn, schemas).await?),
        Driver::Mssql => anyhow::bail!(
            "rebuild namespace occupancy requires PostgreSQL; SQL Server rebuild is refused"
        ),
    }
}

/// Every dependent of every module this plan drops, read inside the caller's
/// transaction (`modules::dependents` refuses outside one). Empty on SQL
/// Server, whose `CREATE OR ALTER` rebuilds nothing.
async fn module_dependents(
    conn: &mut Conn,
    changes: &ChangeSet,
) -> anyhow::Result<BTreeMap<pbps_model::ModuleId, Vec<pbps_pg::modules::Dependent>>> {
    let mut found = BTreeMap::new();
    if conn.driver() != Driver::Postgres {
        return Ok(found);
    }
    for (id, kind) in crate::dependents::dropped_modules(changes) {
        let deps = pbps_pg::modules::dependents(conn, &id, kind).await?;
        found.insert(id, deps);
    }
    Ok(found)
}

/// The planner's half of #314: reads what depends on every module this plan
/// drops and puts each dependent on the right side of that drop, in the plan
/// the approver sees (`dependents::weave`, which the schedule runs over the
/// dependents just read, ahead of the passes that read no catalog,
/// DEC-1955.1). Called inside the planning transaction, before
/// `check_module_rebuilds`, so that a view the plan now drops and recreates is
/// held to the same bar as one the declarations edit.
///
/// It is where a connected plan is scheduled, on either engine: SQL Server
/// has no module dependents to read, and its plan goes through the schedule
/// with the members of the roles it drops, `role_members`, which the caller
/// read when there were any.
///
/// `rediff` plans the same revision again with the given unchanged modules
/// rebuilt (`pbps_diff::diff_rebuilding`): a module dependent the plan does
/// not touch has to be rebuilt around the module under it, and a rebuild is
/// its grants and its `PUBLIC` execute as well as its two statements, which
/// only the differ's own passes write.
// The eighth argument is the role members the caller read: the schedule
// takes every fact in one call (#1955).
#[allow(clippy::too_many_arguments)]
pub async fn account_for_module_dependents<
    S: pbps_diff::order::RouteState<D>,
    D: crate::emission::Engine,
>(
    conn: &mut Conn,
    mut changes: PlanningPlan<S>,
    declared: &pbps_model::Schema,
    sources: crate::dependents::Sources<'_>,
    deps: &pbps_model::ModuleDeps,
    role_members: Option<&crate::dependents::schedule::RoleMembers>,
    dialect: &D,
    rediff: &dyn Fn(&BTreeSet<pbps_model::ModuleId>) -> anyhow::Result<PlanningPlan<S>>,
) -> anyhow::Result<(PlanningPlan<S::Output>, ConnectedCheck)> {
    let declarations = crate::dependents::schedule::Declarations {
        schema: declared,
        sources,
        module_deps: deps,
    };
    // The members of the roles this plan drops, which the caller read when
    // there were any: every connected plan goes through the schedule once,
    // whichever facts its engine supplies (#1955 review).
    let read = match role_members {
        Some(members) => crate::dependents::schedule::Facts::default().with_role_members(members),
        None => crate::dependents::schedule::Facts::default(),
    };
    if conn.driver() != Driver::Postgres {
        let scheduled =
            crate::dependents::schedule::schedule(changes, &declarations, &read, dialect)?;
        return Ok((scheduled.changes, ConnectedCheck {
            name: "module_dependents",
            engine: "SQL Server",
            status: "not_applicable",
            message: "SQL Server uses CREATE OR ALTER, so a module change drops nothing that depends on it".into(),
        }));
    }
    // Version is observed once, before the pure expansion. Ordinary CLI
    // planning has no extra write-path schemas (DEC-1503.1).
    let pg = dialect.with_planning_version(pbps_pg::roles::server_version_num(conn).await?);
    let dialect = &pg;
    let mut found = module_dependents(conn, &changes).await?;
    let untouched = crate::dependents::untouched_module_dependents(&changes, &found, declared);
    if !untouched.is_empty() {
        changes = rediff(&untouched)?;
        found = module_dependents(conn, &changes).await?;
    }
    // Every reordering pass, in one place (#1955): `weave` over what the
    // catalog just said depends on the modules this plan drops, then the
    // passes that read nothing.
    let policies = policy_dependents(conn, &changes, sources.after, sources.before.schema).await?;
    // Only a plan that moves a partition while changing its parent's columns
    // reads stored column facts. The read stays in this transaction and call,
    // beside the dependents it is consulted with (DEC-2296.2).
    let tables = pbps_diff::order::boundary_catalog_tables(
        &changes,
        sources.before,
        pbps_diff::Side {
            schema: declared,
            ids: sources.after,
        },
    );
    let stored = if tables.is_empty() {
        None
    } else {
        Some(
            pbps_pg::modules::dependencies::references(
                conn,
                pbps_model::operation::Snapshot {
                    schema: sources.before.schema,
                    ids: sources.before.ids,
                },
                &tables,
            )
            .await?,
        )
    };
    let facts = read
        .with_module_dependents(&found)
        .with_policy_dependents(&policies);
    let facts = match &stored {
        Some(stored) => facts.with_membership_catalog(stored),
        None => facts,
    };
    let scheduled = crate::dependents::schedule::schedule(changes, &declarations, &facts, dialect)?;
    let changes = scheduled.changes;
    pbps_diff::order::row_security::check_releases(&changes, sources.after, &policies)?;
    let (added, split, moved) = (scheduled.added, scheduled.split, scheduled.moved);
    let partitions = crate::dependents::standing_partitions(declared, &changes);
    // On the final order: every pass above may move what an expression
    // names, or the expression (#1576).
    // Which name each unnamed key's index takes depends on what holds its
    // candidates now (#1645).
    let holders =
        pbps_pg::catalog::key_name_holders(conn, &crate::dependents::key_name_prefixes(&changes))
            .await
            .map_err(|e| {
                anyhow::anyhow!("module_dependents (PostgreSQL): reading what holds key names: {e}")
            })?;
    let mut later = Vec::new();
    for name in crate::dependents::names_a_later_relation(
        &changes,
        dialect,
        Some(&holders),
        sources.before.schema,
    ) {
        if !resolves_now(conn, &name).await? {
            later.push(name);
        }
    }
    crate::dependents::later_relation_refusal(&later)
        .map_err(|why| anyhow::anyhow!("module_dependents (PostgreSQL): {why}"))?;
    let left = crate::dependents::unaccounted(
        &changes,
        &found,
        &partitions,
        &sources
            .project(&changes.changes)
            .map_err(anyhow::Error::msg)?,
    );
    if !left.is_empty() {
        anyhow::bail!(
            "module_dependents (PostgreSQL): the plan still drops a module before what depends \
             on it; this is a bug in pbps, please report it:\n  {}",
            left.join("\n  ")
        );
    }
    let dependents: usize = found.values().map(Vec::len).sum();
    let check = ConnectedCheck {
        name: "module_dependents",
        engine: "PostgreSQL",
        status: "passed",
        message: format!(
            "{dependents} dependent(s) of {} dropped or rebuilt module(s) removed before the drop; {added} change(s) added to the plan for them; {split} part(s) split out of new tables; {moved} addition(s) moved after the function(s) they call",
            found.len()
        ),
    };
    Ok((changes, check))
}

/// A staged plan's table-local name facts, read beside its schema facts.
/// Identities map a renamed attaching table back to its current catalog name.
/// Preserve the project's dialect and ordinary planning's unknown server version.
pub async fn staged_attach_dialect<D: crate::emission::Engine>(
    conn: &mut Conn,
    dialect: &D,
    before: pbps_diff::Side<'_>,
    after: pbps_diff::Side<'_>,
) -> anyhow::Result<D> {
    Ok(match conn.driver() {
        Driver::Postgres => {
            let attaching = after
                .ids
                .tables
                .iter()
                .filter_map(|(uid, name)| {
                    let table = after.schema.tables.get(name)?;
                    table.partition_of.as_ref()?;
                    let live = before.ids.tables.get(uid)?;
                    let standing = before.schema.tables.get(live)?;
                    standing
                        .partition_of
                        .is_none()
                        .then(|| (uid.clone(), live.clone()))
                })
                .collect();
            dialect.with_attach_constraint_names(
                pbps_pg::staged::attach_constraint_names(conn, &attaching).await?,
            )
        }
        Driver::Mssql => dialect.clone(),
    })
}

/// The connection's immutable planning capability, before any expansion.
/// Offline callers retain their ordinary unknown-capability dialect.
#[cfg(test)]
pub async fn planning_dialect(conn: &mut Conn) -> anyhow::Result<pbps_pg::Postgres> {
    Ok(pbps_pg::Postgres::new()
        .with_server_version(pbps_pg::roles::server_version_num(conn).await?))
}

/// Preserve the concrete selected engine while observing the same capability.
pub async fn planning_dialect_for<D: crate::emission::Engine>(
    conn: &mut Conn,
    dialect: &D,
) -> anyhow::Result<D> {
    Ok(match conn.driver() {
        Driver::Postgres => {
            dialect.with_planning_version(pbps_pg::roles::server_version_num(conn).await?)
        }
        Driver::Mssql => dialect.clone(),
    })
}

/// Whether a literal [`crate::dependents::names_a_later_relation`] read as a
/// later relation already resolves to one on the target, searching its
/// schemas in order as the write path would (#1589).
pub(crate) async fn resolves_now(
    conn: &mut Conn,
    name: &crate::dependents::LaterName,
) -> anyhow::Result<bool> {
    for schema in &name.searched {
        if pbps_pg::modules::relation_exists(conn, schema, &name.name)
            .await
            .map_err(|e| anyhow::anyhow!("looking up {schema}.{} on the target: {e}", name.name))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The apply's half of #314: the saved plan must still remove, before each
/// module's drop, everything that depends on that module now. A dependent
/// created after planning is one the approver never saw; the plan is refused
/// rather than extended, and planning again shows it.
///
/// `stored` is the database as the plan's baseline found it, which says
/// which table is whose partition (DEC-1699.1).
pub async fn check_module_dependents<D: ?Sized>(
    conn: &mut Conn,
    plan: &(dyn pbps_diff::order::PlanView<D> + 'static),
    stored: &pbps_model::Schema,
    ids: &pbps_model::IdsFile,
    stored_ids: &pbps_model::IdsFile,
) -> anyhow::Result<()> {
    let changes = pbps_dialect::PlanPayload::changes(plan);
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
    refuse_policy_dependents(conn, plan, ids, stored).await?;
    check_module_dependency_facts(conn, changes, names, stored, stored_ids).await
}

#[cfg(test)]
pub async fn check_module_dependents_fixture(
    conn: &mut Conn,
    input: &PlanningPlan,
    stored: &pbps_model::Schema,
    ids: &pbps_model::IdsFile,
    stored_ids: &pbps_model::IdsFile,
) -> anyhow::Result<()> {
    // Adverse logical snapshots omit execution decisions deliberately. Observe
    // the same catalog checks without certifying or repairing their payload.
    let changes = input.changes();
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
    if conn.driver() == Driver::Postgres {
        let found = policy_dependents(conn, changes, ids, stored).await?;
        pbps_diff::order::row_security::check_releases(changes, ids, &found)?;
    }
    check_module_dependency_facts(conn, changes, names, stored, stored_ids).await
}

async fn check_module_dependency_facts(
    conn: &mut Conn,
    changes: &ChangeSet,
    names: pbps_model::plan_names::PlanNames<'_>,
    stored: &pbps_model::Schema,
    stored_ids: &pbps_model::IdsFile,
) -> anyhow::Result<()> {
    let found = module_dependents(conn, changes).await?;
    // Match the planner's final relation first; each dependency lookup then
    // spells both ends at the statement's boundary (DEC-1875.1).
    let partitions = crate::dependents::standing_partitions(stored, changes).at(
        &names,
        pbps_model::plan_names::At::Start,
        pbps_model::plan_names::At::End,
    );
    let sources = pbps_model::physical_sources::PhysicalSources::of(names, stored, stored_ids)?;
    let left = crate::dependents::unaccounted(changes, &found, &partitions, &sources);
    if !left.is_empty() {
        anyhow::bail!(
            "module_dependents (PostgreSQL): the database now holds dependents this plan does not \
             remove before dropping what they depend on, so the engine would refuse the drop:\n  {}\n\
             Plan again, so the plan accounts for them where the approver can see it.",
            left.join("\n  ")
        );
    }
    Ok(())
}

/// Holds only trigger identities authenticated in the caller's open transaction.
#[derive(Default)]
pub struct DataWriteGuard {
    postgres: Option<pbps_pg::data_triggers::Guard>,
}

pub async fn prepare_data_writes(
    conn: &mut Conn,
    changes: &ChangeSet,
    baseline: &StateSnapshot,
    planned_ids: &pbps_model::IdsFile,
) -> anyhow::Result<DataWriteGuard> {
    match conn.driver() {
        // SQL Server keeps its existing reference-data execution policy.
        Driver::Mssql => Ok(DataWriteGuard::default()),
        Driver::Postgres => {
            use pbps_dialect::{RowOperation, RowWrite};
            use pbps_model::Change;
            let mut writes = Vec::new();
            let mut dropped = pbps_pg::data_triggers::Dropped::default();
            // Asked once per row change and updated column: indexed once,
            // not scanned per question (#1870).
            let planned_names = planned_ids.index();
            let baseline_names = baseline.ids.index();
            // The plan's own name for a table, in the spelling the catalog
            // still holds: the guard runs before the renames (DECISIONS 445).
            let stored = |table: &pbps_model::TableName| {
                planned_names
                    .table_uid(table)
                    .or_else(|| baseline_names.table_uid(table))
                    .and_then(|uid| baseline.ids.tables.get(uid))
                    .cloned()
            };
            for planned in &changes.changes {
                if let Change::DropModule { id, .. } = &planned.change {
                    dropped.modules.insert(id.clone());
                }
                // Both are ordered ahead of every row change, so the
                // referential action they carry cannot write when the row
                // statement runs (DECISIONS 451).
                if let Change::DropForeignKey { table, name } = &planned.change
                    && let Some(old) = stored(table)
                {
                    dropped.foreign_keys.insert((old, name.clone()));
                }
                if let Change::DropTable { name, .. } = &planned.change {
                    dropped
                        .tables
                        .insert(stored(name).unwrap_or_else(|| name.clone()));
                }
                let (table, mut operation) =
                    if let Change::InsertRow { table, .. } = &planned.change {
                        (table, RowOperation::Insert)
                    } else if let Change::UpdateRow { table, columns, .. } = &planned.change {
                        (
                            table,
                            RowOperation::Update {
                                columns: columns.keys().cloned().collect(),
                            },
                        )
                    } else if let Change::DeleteRow { table, .. } = &planned.change {
                        (table, RowOperation::Delete)
                    } else {
                        continue;
                    };
                // The logical target has the plan's final spelling; locks are
                // acquired before any rename, through the stable table uid.
                // New tables receive no existing-trigger allowance.
                if let Some(old) = stored(table).as_ref() {
                    // Like the table, an updated column may still carry its
                    // pre-plan name while the guard authenticates triggers.
                    if let RowOperation::Update { columns } = &mut operation {
                        *columns = columns
                            .iter()
                            .map(|name| {
                                let column = pbps_model::ColumnRef {
                                    table: table.clone(),
                                    name: name.clone(),
                                };
                                planned_names
                                    .column_uid(&column)
                                    .or_else(|| baseline_names.column_uid(&column))
                                    .and_then(|uid| baseline.ids.columns.get(uid))
                                    .map_or_else(|| name.clone(), |old| old.name.clone())
                            })
                            .collect();
                    }
                    writes.push(RowWrite {
                        table: old.clone(),
                        operation,
                    });
                }
            }
            Ok(DataWriteGuard {
                postgres: Some(
                    pbps_pg::data_triggers::prepare(conn, &writes, &baseline.schema, &dropped)
                        .await?,
                ),
            })
        }
    }
}

/// Whether deferred work was flushed and its effects need a managed-state check.
pub async fn settle_data_writes(conn: &mut Conn) -> anyhow::Result<bool> {
    match conn.driver() {
        Driver::Postgres => {
            pbps_pg::data_triggers::settle(conn).await?;
            Ok(true)
        }
        Driver::Mssql => Ok(false),
    }
}

pub async fn check_data_write(
    conn: &mut Conn,
    statement: &pbps_dialect::Statement,
    guard: &DataWriteGuard,
) -> anyhow::Result<()> {
    let Some(write) = &statement.row_write else {
        return Ok(());
    };
    match conn.driver() {
        Driver::Mssql => Ok(()),
        Driver::Postgres => {
            let empty = pbps_pg::data_triggers::Guard::default();
            pbps_pg::data_triggers::check(conn, write, guard.postgres.as_ref().unwrap_or(&empty))
                .await?;
            Ok(())
        }
    }
}

/// Artifact readers route by the engine named in the saved file, without a
/// connection or resolver. Connected apply separately checks that engine
/// against its actual target before accepting the artifact.
pub fn validate_plan_analysis(plan: &pbps_model::SavedPlan) -> anyhow::Result<()> {
    plan.validate_analysis()?;
    if let pbps_model::resolver::PlanAnalysis::Resolved(evidence) = &plan.analysis {
        match plan.dialect.as_str() {
            "postgres" => pbps_pg::resolver::validate_evidence(evidence)?,
            engine => anyhow::bail!(
                "this build cannot enforce resolver evidence for {}",
                pbps_model::escape::Escaped(engine)
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    fn order_after_releases(
        changes: &mut ChangeSet,
        dependences: &BTreeMap<TableName, Vec<pbps_pg::generated::Dependence>>,
    ) -> Result<usize, String> {
        let mut ids = crate::plan_fixture::ids(changes);
        if !changes
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::InsertRow { .. }))
        {
            // Older name-only fixtures do not enter the carried-row graph.
            let schema = pbps_model::Schema::default();
            return crate::test_planning(changes, |plan| {
                super::order_after_releases(
                    plan,
                    pbps_diff::Side {
                        schema: &schema,
                        ids: &ids,
                    },
                    pbps_diff::Side {
                        schema: &schema,
                        ids: &ids,
                    },
                    dependences,
                    &pbps_pg::Postgres::new(),
                    &pbps_model::ModuleDeps::new(),
                )
            });
        }
        let mut base_ids = ids.clone();
        let mut before = pbps_model::Schema::default();
        for (table, dependences) in dependences {
            let shape = before.tables.entry(table.clone()).or_default();
            for name in ["id", "b"]
                .into_iter()
                .map(pbps_model::ColumnName::from)
                .chain(
                    dependences
                        .iter()
                        .flat_map(|d| [d.base.clone(), d.generated.clone()]),
                )
            {
                shape
                    .columns
                    .entry(name.clone())
                    .or_insert_with(|| pbps_model::Column::new("integer".parse().unwrap()));
                if !ids
                    .columns
                    .values()
                    .any(|c| c.table == *table && c.name == name)
                {
                    let address = table.column(&name).raw().to_string();
                    let uid = pbps_model::Uid::derived(pbps_model::UidKind::Column, &address, 0);
                    ids.columns.insert(uid.clone(), table.column(&name));
                    base_ids.columns.insert(uid, table.column(&name));
                }
            }
            for d in dependences {
                shape.columns.get_mut(&d.base).unwrap().nullable =
                    !d.base_required && !d.filled_by_trigger;
                let base = d.base.raw();
                shape.columns.get_mut(&d.generated).unwrap().value_source =
                    pbps_model::ValueSource::Generated(pbps_model::Generated {
                        expression: format!("{base} * 2"),
                        stored: true,
                    });
            }
        }
        let mut after = before.clone();
        for p in &changes.changes {
            if let pbps_model::Change::AlterModule { id, module } = &p.change
                && module.kind == pbps_model::ModuleKind::Trigger
            {
                // A declared trigger stands at both ends even when the catalog
                // says an earlier rebuild stops filling the omitted input.
                before.modules.insert(id.clone(), (**module).clone());
                after.modules.insert(id.clone(), (**module).clone());
            }
            if let pbps_model::Change::DropColumn { uid, column } = &p.change {
                base_ids.columns.retain(|_, c| c != column);
                base_ids.columns.insert(uid.clone(), column.clone());
                ids.columns.retain(|_, c| c != column);
                if let Some(table) = after.tables.get_mut(&column.table) {
                    table.columns.shift_remove(&column.name);
                }
            }
        }
        crate::test_planning(changes, |plan| {
            super::order_after_releases(
                plan,
                pbps_diff::Side {
                    schema: &before,
                    ids: &base_ids,
                },
                pbps_diff::Side {
                    schema: &after,
                    ids: &ids,
                },
                dependences,
                &pbps_pg::Postgres::new(),
                &pbps_model::ModuleDeps::new(),
            )
        })
    }
    fn unreleased(
        changes: &ChangeSet,
        dependences: &BTreeMap<TableName, Vec<pbps_pg::generated::Dependence>>,
    ) -> Vec<String> {
        let ids = crate::plan_fixture::ids(changes);
        let names = pbps_model::plan_names::PlanNames::of(&changes.changes, &ids).unwrap();
        super::unreleased(changes, &names, dependences)
    }

    use super::*;

    /// #1630: what a server without history retention refuses is an
    /// answered question. A connected JSON plan reports it as an error
    /// finding of its own id, with the remedy, so the envelope says
    /// `findings` and the command ends on exit 2, not as `plan.failed`. A
    /// plan with nothing refused makes no refusal. No 2016 server is in the
    /// live matrix, so the problems come from the pure function the
    /// connected check feeds.
    #[test]
    fn an_answered_temporal_refusal_is_a_finding_with_exit_two() {
        use pbps_model::{
            Change, PlannedChange, Retention, RetentionUnit, SystemTime, SystemVersioning, Table,
        };
        let table = Table {
            system_time: Some(SystemTime {
                start: "vf".into(),
                end: "vt".into(),
                hidden: false,
                versioning: Some(SystemVersioning {
                    history: "dbo.t_history".parse().unwrap(),
                    retention: Some(Retention {
                        count: 3,
                        unit: RetentionUnit::Day,
                    }),
                }),
            }),
            ..Default::default()
        };
        let cs = ChangeSet {
            changes: vec![PlannedChange::new(Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.t".parse().unwrap(),
                table: Box::new(table),
                beside_default: None,
                moves_at_most: None,
            })],
        };
        let problems = pbps_mssql::temporal::refused_without_retention(&cs, &Default::default());
        let refusal = temporal_refusal(&problems).expect("a finite retention is refused");
        assert!(
            refusal.contains("`dbo.t` declares a history retention"),
            "{refusal}"
        );

        // As JSON: the refusal joins the findings as an error of its own id,
        // with the remedy, and the command ends on `Found`, exit 2.
        let mut findings = vec![crate::output::Finding::warning("plan.edition", "w")];
        let exit = refuse_answered_temporal(&problems, true, &mut findings).unwrap_err();
        assert!(exit.downcast_ref::<crate::Found>().is_some(), "{exit:?}");
        let json = serde_json::to_value(crate::output::Report::plain("plan", findings)).unwrap();
        assert_eq!(json["result"], "findings", "{json}");
        let finding = &json["findings"][1];
        assert_eq!(finding["id"], "plan.temporal-unsupported", "{json}");
        assert_eq!(finding["severity"], "error", "{json}");
        assert!(
            finding["message"].as_str().unwrap().contains("`dbo.t`"),
            "{json}"
        );
        assert!(
            finding["remedy"]
                .as_str()
                .unwrap()
                .contains("leave `retention` out"),
            "{json}"
        );
        // Human output: the refusal itself, an ordinary error.
        let exit = refuse_answered_temporal(&problems, false, &mut Vec::new()).unwrap_err();
        assert!(exit.downcast_ref::<crate::Found>().is_none(), "{exit:?}");
        assert_eq!(exit.to_string(), refusal);

        // Negative: nothing refused, no refusal, and the findings untouched.
        assert!(temporal_refusal(&[]).is_none());
        let mut kept = vec![crate::output::Finding::warning("plan.edition", "w")];
        refuse_answered_temporal(&[], true, &mut kept).unwrap();
        assert_eq!(kept.len(), 1);
    }

    /// #822: when the follow-up question cannot be asked — here, nothing
    /// answers the connection — the answer is "unexplained", which names no
    /// grant, on either engine. A failure to ask is never read as a gap.
    #[test]
    fn a_lock_question_that_cannot_be_asked_names_no_grant() {
        use pbps_db::doctor::LockReadGap;
        for (driver, connection) in [
            (
                Driver::Postgres,
                "host=127.0.0.1 port=1 user=u dbname=d sslmode=disable connect_timeout=1",
            ),
            (
                Driver::Mssql,
                "Server=127.0.0.1,1;User Id=u;Password=p;TrustServerCertificate=true;\
                 Connect Timeout=1",
            ),
        ] {
            let gap = block_on(lock_read_gap(driver, connection));
            assert_eq!(gap, LockReadGap::Unexplained, "{driver:?}");
            assert_eq!(lock_remedy(driver, Some(gap)).statement, None, "{driver:?}");
        }
    }

    /// A retype or drop of a column a live generated column reads goes after
    /// the plan's change to that expression which stops reading it, and only
    /// then (DEC-1316.1). An omitted disappearing default is required absent,
    /// even when the engine could fill it before the drop. A nullable input
    /// without a default has no such requirement and keeps its original order.
    #[test]
    fn crossed_inserts_follow_their_actual_default_requirements() {
        use pbps_model::{
            Change, Column, Generated, IdsFile, PlannedChange, Schema, Table, UidKind, ValueSource,
        };
        let table: TableName = "app.t".parse().unwrap();
        for (nullable, defaulted) in [(true, false), (false, true), (true, true)] {
            let mut shape = Table::default();
            for name in ["id", "a", "b", "g"] {
                shape
                    .columns
                    .insert(name.into(), Column::new("integer".parse().unwrap()));
            }
            let input = shape.columns.get_mut("a").unwrap();
            input.nullable = nullable;
            if defaulted {
                input.value_source = ValueSource::Default("7".into());
            }
            shape.columns.get_mut("g").unwrap().value_source = ValueSource::Generated(Generated {
                expression: "a * 2".into(),
                stored: true,
            });
            let mut before = Schema::default();
            before.tables.insert(table.clone(), shape);
            let mut base_ids = IdsFile::default();
            base_ids
                .tables
                .insert("t_aaaaaa".parse().unwrap(), table.clone());
            for name in ["id", "a", "b", "g"] {
                base_ids.columns.insert(
                    pbps_model::Uid::derived(UidKind::Column, name, 0),
                    table.column(name),
                );
            }
            let a = pbps_model::Uid::derived(UidKind::Column, "a", 0);
            let g = pbps_model::Uid::derived(UidKind::Column, "g", 0);
            let mut ids = base_ids.clone();
            ids.columns.remove(&a);
            let mut after = before.clone();
            let shape = after.tables.get_mut(&table).unwrap();
            shape.columns.shift_remove("a");
            shape.columns.get_mut("g").unwrap().value_source = ValueSource::Generated(Generated {
                expression: "b * 2".into(),
                stored: true,
            });
            let changes = vec![
                Change::DropColumn {
                    uid: a,
                    column: table.column("a"),
                },
                Change::InsertRow {
                    table: table.clone(),
                    key_column: "id".into(),
                    identity_key: false,
                    key: pbps_model::RowKey::from("1"),
                    row: pbps_model::Row::default(),
                    defaults: Default::default(),
                    types: Default::default(),
                },
                Change::AlterColumnExpression {
                    uid: g,
                    column: table.column("g"),
                    from: "a * 2".into(),
                    to: "b * 2".into(),
                },
            ];
            let needs_default_gone = defaulted;
            let expected = if needs_default_gone {
                vec![changes[2].clone(), changes[0].clone(), changes[1].clone()]
            } else {
                vec![changes[1].clone(), changes[2].clone(), changes[0].clone()]
            };
            let mut cs = ChangeSet {
                changes: changes.into_iter().map(PlannedChange::new).collect(),
            };
            let found = BTreeMap::from([(
                table.clone(),
                vec![pbps_pg::generated::Dependence {
                    base: "a".into(),
                    generated: "g".into(),
                    base_required: false,
                    filled_by_trigger: false,
                }],
            )]);
            let moved = crate::test_planning(&mut cs, |plan| {
                super::order_after_releases(
                    plan,
                    pbps_diff::Side {
                        schema: &before,
                        ids: &base_ids,
                    },
                    pbps_diff::Side {
                        schema: &after,
                        ids: &ids,
                    },
                    &found,
                    &pbps_pg::Postgres::new(),
                    &pbps_model::ModuleDeps::new(),
                )
            })
            .unwrap();
            assert_eq!(moved, if needs_default_gone { 2 } else { 1 });
            assert_eq!(
                cs.changes
                    .iter()
                    .map(|p| p.change.clone())
                    .collect::<Vec<_>>(),
                expected
            );
            use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
            let violations = |changes: &ChangeSet| {
                let names = pbps_model::plan_names::PlanNames::of(&changes.changes, &ids).unwrap();
                let owners = PlanOwners::new(names, &base_ids, &ids);
                let operations = Operations::of_plan(&changes.changes, ModuleEdits::InPlace);
                let base = Snapshot {
                    schema: &before,
                    ids: &base_ids,
                };
                let end = Snapshot {
                    schema: &after,
                    ids: &ids,
                };
                let effects = pbps_diff::order::analyse(
                    &operations,
                    &owners,
                    base,
                    end,
                    &pbps_pg::Postgres::new(),
                );
                pbps_diff::order::verify(
                    operations.as_slice(),
                    &effects.effects,
                    &pbps_diff::order::Baseline::of(base, &owners),
                    &pbps_model::ModuleDeps::new(),
                )
            };
            assert!(violations(&cs).is_empty(), "{:?}", violations(&cs));
        }
    }

    // A test rendering: any other kind of change is a failure.
    #[allow(clippy::wildcard_enum_match_arm)]
    #[test]
    fn a_released_input_is_retyped_or_dropped_after_its_release() {
        use pbps_model::{Change, ColumnRef, PlannedChange};
        let t: TableName = "app.t".parse().unwrap();
        let col = |name: &str| ColumnRef::new(t.clone(), name);
        let uid = |u: &str| -> pbps_model::Uid { u.parse().unwrap() };
        let drop = |name: &str| Change::DropColumn {
            uid: uid("c_aaaaaa"),
            column: col(name),
        };
        let retype = |name: &str| Change::AlterColumnType {
            uid: uid(if matches!(name, "a" | "x") {
                "c_bbbbbb"
            } else {
                "c_ffffff"
            }),
            column: col(name),
            from: "integer".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            through_external_partitions: false,
            from_collation: None,
            to_collation: None,
            using: None,
        };
        let recompute = |to: &str| Change::AlterColumnExpression {
            uid: uid("c_dddddd"),
            column: col("g"),
            from: "(a * 2)".into(),
            to: to.into(),
        };
        let plan = |c: Vec<Change>| ChangeSet {
            changes: c.into_iter().map(PlannedChange::new).collect(),
        };
        let reads_a = BTreeMap::from([(
            t.clone(),
            vec![pbps_pg::generated::Dependence {
                base: "a".into(),
                generated: "g".into(),
                base_required: false,
                filled_by_trigger: false,
            }],
        )]);
        let kinds = |cs: &ChangeSet| -> Vec<&'static str> {
            cs.changes
                .iter()
                .map(|p| match &p.change {
                    Change::DropColumn { .. } => "drop",
                    Change::AlterColumnType { .. } => "retype",
                    Change::AlterColumnExpression { .. } => "expression",
                    Change::AddColumn { .. } => "add",
                    Change::AlterColumnDefault { .. } => "default",
                    Change::RenameColumn { .. } => "rename",
                    Change::UpdateRow { .. } => "update",
                    other => panic!("unexpected {other:?}"),
                })
                .collect()
        };

        for first in [drop("a"), retype("a")] {
            let mut cs = plan(vec![first, recompute("b * 2")]);
            assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(1));
            assert_eq!(kinds(&cs)[0], "expression", "{:?}", kinds(&cs));
        }
        // A retype takes the default written for its new type along, in
        // order; a default of another column stays.
        let set_default = |u: &str, name: &str| Change::AlterColumnDefault {
            uid: uid(u),
            column: col(name),
            from: None,
            to: Some("5000000000".into()),
        };
        let mut cs = plan(vec![
            retype("a"),
            set_default("c_bbbbbb", "a"),
            set_default("c_ffffff", "b"),
            recompute("b * 2"),
        ]);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(2));
        assert_eq!(
            kinds(&cs),
            ["default", "expression", "retype", "default"],
            "{:?}",
            cs.changes
        );
        // Settled again under the same facts, the plan does not change
        // (#1956).
        let settled = cs.clone();
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(0));
        assert_eq!(cs, settled);
        assert!(matches!(&cs.changes[3].change,
            Change::AlterColumnDefault { column, .. } if column.name == "a".into()));
        // Released, the check has nothing left to refuse; unreleased, it
        // does.
        let mut cs = plan(vec![retype("a"), recompute("b * 2")]);
        assert_eq!(unreleased(&cs, &reads_a).len(), 1);
        order_after_releases(&mut cs, &reads_a).unwrap();
        assert!(unreleased(&cs, &reads_a).is_empty());
        // The input renamed to `x` in the same plan, and the new text naming
        // `x`: it still reads the column, whatever the catalog calls it.
        let rename = |from: &str, to: &str| Change::RenameColumn {
            uid: uid(if from == "a" { "c_bbbbbb" } else { "c_cccccc" }),
            table: t.clone(),
            from: from.into(),
            to: to.into(),
            table_was: None,
        };
        let mut cs = plan(vec![rename("a", "x"), retype("x"), recompute("x * 2")]);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(0));
        assert_eq!(unreleased(&cs, &reads_a).len(), 1);
        // The generated column dropped while another column is renamed into
        // its name: the drop still names the catalog's `g`, which releases
        // its input.
        let cs = plan(vec![drop("g"), rename("c", "g"), retype("a")]);
        assert!(
            unreleased(&cs, &reads_a).is_empty(),
            "{:?}",
            unreleased(&cs, &reads_a)
        );
        // Another generated column starting to read `a`, sorted ahead of the
        // release: it follows the retype, or the retype is refused once it
        // reads `a`.
        let start_reading = Change::AlterColumnExpression {
            uid: uid("c_gggggg"),
            column: col("h"),
            from: "b".into(),
            to: "a + 1".into(),
        };
        let mut cs = plan(vec![retype("a"), start_reading, recompute("b * 2")]);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(2));
        assert!(
            matches!(&cs.changes[0].change, Change::AlterColumnExpression { column, .. } if column.name == "g".into())
                && matches!(cs.changes[1].change, Change::AlterColumnType { .. })
                && matches!(&cs.changes[2].change, Change::AlterColumnExpression { column, .. } if column.name == "h".into()),
            "{:?}",
            cs.changes
        );
        // Two generated columns swapping retyped inputs: `g` releases `a` and
        // starts reading `b`, `h` releases `b` and starts reading `a`. Each
        // must follow one retype and precede the other; no order exists, and
        // the pass refuses rather than loop.
        let reads_both = BTreeMap::from([(
            t.clone(),
            vec![
                pbps_pg::generated::Dependence {
                    generated: "g".into(),
                    base: "a".into(),
                    base_required: false,
                    filled_by_trigger: false,
                },
                pbps_pg::generated::Dependence {
                    generated: "h".into(),
                    base: "b".into(),
                    base_required: false,
                    filled_by_trigger: false,
                },
            ],
        )]);
        let swap = |g: &str, to: &str| Change::AlterColumnExpression {
            uid: uid(if g == "g" { "c_gggggg" } else { "c_hhhhhh" }),
            column: col(g),
            from: String::new(),
            to: to.into(),
        };
        let mut cs = plan(vec![
            retype("a"),
            retype("b"),
            swap("g", "b * 2"),
            swap("h", "a * 2"),
        ]);
        let refusal = order_after_releases(&mut cs, &reads_both).unwrap_err();
        assert!(
            refusal.contains("cannot be retyped in this plan"),
            "{refusal}"
        );
        // A chain is no cycle: `g` releases `a` and starts reading `b`, which
        // nothing generated reads. `a`'s retype follows `g`, `b`'s precedes it.
        let mut cs = plan(vec![retype("a"), retype("b"), swap("g", "b * 2")]);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(1));
        // A row writing `a` that runs before the release, as it does when the
        // release follows a function create: the retype cannot both follow the
        // release and precede the row, so the plan is refused by name.
        let update = |column: &str| Change::UpdateRow {
            table: t.clone(),
            key_column: "id".into(),
            key: pbps_model::RowKey::from("1"),
            columns: BTreeMap::from([(
                column.into(),
                (
                    pbps_model::Cell::Value(pbps_model::Value::Int(1)),
                    pbps_model::Cell::Value(pbps_model::Value::Int(2)),
                ),
            )]),
            unchanged: BTreeMap::new(),
            types: BTreeMap::new(),
            after_types: BTreeMap::new(),
            key_type: None,
        };
        let mut cs = plan(vec![retype("a"), update("a"), recompute("b * 2")]);
        let refusal = order_after_releases(&mut cs, &reads_a).unwrap_err();
        assert!(refusal.contains("may need the new type"), "{refusal}");
        // A drop moved past a row inserted into its table, as it is when the
        // release follows a function create: a `NOT NULL` input with no
        // default refuses the row while it stands. The row follows the
        // release and drop through an edge now (#2174).
        let insert = |table: &TableName| Change::InsertRow {
            table: table.clone(),
            key_column: "id".into(),
            identity_key: false,
            key: pbps_model::RowKey::from("1"),
            row: pbps_model::Row::default(),
            defaults: BTreeMap::new(),
            types: BTreeMap::new(),
        };
        let requires_a = BTreeMap::from([(
            t.clone(),
            vec![pbps_pg::generated::Dependence {
                base: "a".into(),
                generated: "g".into(),
                base_required: true,
                filled_by_trigger: false,
            }],
        )]);
        let mut cs = plan(vec![drop("a"), insert(&t), recompute("b * 2")]);
        assert_eq!(order_after_releases(&mut cs, &requires_a), Ok(2));
        assert!(matches!(
            cs.changes[0].change,
            Change::AlterColumnExpression { .. }
        ));
        assert!(matches!(cs.changes[1].change, Change::DropColumn { .. }));
        assert!(matches!(cs.changes[2].change, Change::InsertRow { .. }));
        // Negatives: a nullable input without a default lets the row in,
        // another table is no concern of the column, and an insert after the
        // release is never passed.
        let mut cs = plan(vec![drop("a"), insert(&t), recompute("b * 2")]);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(1));
        let other: TableName = "app.other".parse().unwrap();
        let mut cs = plan(vec![drop("a"), insert(&other), recompute("b * 2")]);
        assert_eq!(order_after_releases(&mut cs, &requires_a), Ok(1));
        let mut cs = plan(vec![drop("a"), recompute("b * 2"), insert(&t)]);
        assert_eq!(order_after_releases(&mut cs, &requires_a), Ok(1));
        // An input a trigger on the table may fill passes the row, unless
        // the plan drops or rewrites a trigger there: the one that would
        // fill it may be gone when the row is written, so it counts as
        // required (#2178). A trigger on another table changes nothing.
        let filled_a = BTreeMap::from([(
            t.clone(),
            vec![pbps_pg::generated::Dependence {
                base: "a".into(),
                generated: "g".into(),
                base_required: false,
                filled_by_trigger: true,
            }],
        )]);
        let trigger_on = |table: &str| pbps_model::ModuleId::Trigger {
            on: table.parse().unwrap(),
            name: "fill".into(),
        };
        let drop_trigger = |table: &str| Change::DropModule {
            id: trigger_on(table),
            kind: pbps_model::ModuleKind::Trigger,
        };
        let mut cs = plan(vec![drop("a"), insert(&t), recompute("b * 2")]);
        assert_eq!(order_after_releases(&mut cs, &filled_a), Ok(1));
        let alter_trigger = || Change::AlterModule {
            id: trigger_on("app.t"),
            module: Box::new(pbps_model::Module {
                kind: pbps_model::ModuleKind::Trigger,
                description: None,
                definition: "BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.f()".into(),
            }),
        };
        for change in [drop_trigger("app.t"), alter_trigger()] {
            let mut cs = plan(vec![change, drop("a"), insert(&t), recompute("b * 2")]);
            assert_eq!(order_after_releases(&mut cs, &filled_a), Ok(2));
            assert!(matches!(cs.changes[2].change, Change::DropColumn { .. }));
            assert!(matches!(cs.changes[3].change, Change::InsertRow { .. }));
        }
        // A trigger rewritten after the row still stands when the row is
        // written, as the differ puts a module change after the rows.
        let mut cs = plan(vec![
            drop("a"),
            insert(&t),
            alter_trigger(),
            recompute("b * 2"),
        ]);
        assert_eq!(order_after_releases(&mut cs, &filled_a), Ok(1));
        let mut cs = plan(vec![
            drop_trigger("app.other"),
            drop("a"),
            insert(&t),
            recompute("b * 2"),
        ]);
        assert_eq!(order_after_releases(&mut cs, &filled_a), Ok(1));
        // So does any row in between, of another column as much, and a
        // module; a moved drop of another input does not hold it back.
        let mut cs = plan(vec![retype("a"), update("b"), recompute("b * 2")]);
        assert!(order_after_releases(&mut cs, &reads_a).is_err());
        let mut cs = plan(vec![
            retype("a"),
            set_default("c_ffffff", "b"),
            drop("c"),
            recompute("b * 2"),
        ]);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(1));
        // The new text may still read `a`: nothing moves, and the check
        // refuses it.
        let mut cs = plan(vec![drop("a"), recompute("a + b")]);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(0));
        assert_eq!(kinds(&cs), ["drop", "expression"]);
        // A column no generated column reads stays where it was.
        let mut cs = plan(vec![drop("c"), recompute("b * 2")]);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(0));
        // A new column taking the name between the drop and the release.
        let mut cs = plan(vec![
            drop("a"),
            Change::AddColumn {
                uid: uid("c_eeeeee"),
                table: t.clone(),
                name: "a".into(),
                column: Box::new(pbps_model::Column::new("text".parse().unwrap())),
            },
            recompute("b * 2"),
        ]);
        let refusal = order_after_releases(&mut cs, &reads_a).unwrap_err();
        assert!(refusal.contains("in a plan of its own first"), "{refusal}");
        // A new reader sorted ahead of the retype it has to follow: held
        // back until the retype has run, which itself waits for its release
        // (DEC-1391.1).
        let reader = || Change::AlterColumnExpression {
            uid: uid("c_gggggg"),
            column: col("h"),
            from: "b".into(),
            to: "a + 1".into(),
        };
        let mut cs = plan(vec![reader(), retype("a"), recompute("b * 2")]);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(2));
        assert!(
            matches!(&cs.changes[0].change, Change::AlterColumnExpression { column, .. } if column.name == "g".into())
                && matches!(cs.changes[1].change, Change::AlterColumnType { .. })
                && matches!(&cs.changes[2].change, Change::AlterColumnExpression { column, .. } if column.name == "h".into()),
            "{:?}",
            cs.changes
        );
        // #1391: a new reader of a dropped input has no order: once it reads
        // the column the drop is refused, and before that its text names a
        // column that is gone. Refused by name, whatever the order.
        for changes in [
            vec![drop("a"), reader(), recompute("b * 2")],
            vec![reader(), drop("a"), recompute("b * 2")],
        ] {
            let mut cs = plan(changes);
            let refusal = order_after_releases(&mut cs, &reads_a).unwrap_err();
            assert!(refusal.contains("new expression of app.t.h"), "{refusal}");
        }
        // Unless the plan gives the name to a new column, which is what the
        // new text then reads.
        let mut cs = plan(vec![
            drop("a"),
            Change::AddColumn {
                uid: uid("c_eeeeee"),
                table: t.clone(),
                name: "a".into(),
                column: Box::new(pbps_model::Column::new("text".parse().unwrap())),
            },
            reader(),
        ]);
        assert_eq!(order_after_releases(&mut cs, &BTreeMap::new()), Ok(0));
        // #1425: a generated column the plan adds reads its input from the
        // moment it exists, so one reading a dropped column has no order
        // either; the differ would run the drop first and the `ADD COLUMN`
        // would fail. Unless the name goes to a new column it reads instead.
        let generated_h = |expression: &str| Change::AddColumn {
            uid: uid("c_hhhhhh"),
            table: t.clone(),
            name: "h".into(),
            column: Box::new(pbps_model::Column {
                value_source: pbps_model::ValueSource::Generated(pbps_model::Generated {
                    expression: expression.into(),
                    stored: true,
                }),
                ..pbps_model::Column::new("integer".parse().unwrap())
            }),
        };
        let mut cs = plan(vec![drop("a"), generated_h("a + 1")]);
        let refusal = order_after_releases(&mut cs, &BTreeMap::new()).unwrap_err();
        assert!(
            refusal.contains("the expression of the new app.t.h"),
            "{refusal}"
        );
        let mut cs = plan(vec![drop("a"), generated_h("b + 1")]);
        assert_eq!(order_after_releases(&mut cs, &BTreeMap::new()), Ok(0));
        let mut cs = plan(vec![
            drop("a"),
            Change::AddColumn {
                uid: uid("c_eeeeee"),
                table: t.clone(),
                name: "a".into(),
                column: Box::new(pbps_model::Column::new("integer".parse().unwrap())),
            },
            generated_h("a + 1"),
        ]);
        assert_eq!(order_after_releases(&mut cs, &BTreeMap::new()), Ok(0));
        // `a` renamed to `x` and retyped, and a new column taking the name
        // `a`: `h`'s new text naming `a` reads the new column, not `x`, so it
        // need not follow `x`'s retype. `g` moves from `a` to `b` and `h` from
        // `b` to the new `a`, with `b` retyped too: `h`, `b`'s retype, `g`,
        // then `x`'s retype (review of #1424).
        let reads_a_and_b = BTreeMap::from([(
            t.clone(),
            vec![
                pbps_pg::generated::Dependence {
                    generated: "g".into(),
                    base: "a".into(),
                    base_required: false,
                    filled_by_trigger: false,
                },
                pbps_pg::generated::Dependence {
                    generated: "h".into(),
                    base: "b".into(),
                    base_required: false,
                    filled_by_trigger: false,
                },
            ],
        )]);
        let mut cs = plan(vec![
            rename("a", "x"),
            Change::AddColumn {
                uid: uid("c_eeeeee"),
                table: t.clone(),
                name: "a".into(),
                column: Box::new(pbps_model::Column::new("integer".parse().unwrap())),
            },
            retype("x"),
            retype("b"),
            swap("g", "b * 2"),
            swap("h", "a + 1"),
        ]);
        order_after_releases(&mut cs, &reads_a_and_b).unwrap();
        let at = |what: &dyn Fn(&Change) -> bool| {
            cs.changes.iter().position(|p| what(&p.change)).unwrap()
        };
        let retyped = |name: &'static str| move |c: &Change| matches!(c, Change::AlterColumnType { column, .. } if column.name == name.into());
        let expression = |name: &'static str| move |c: &Change| matches!(c, Change::AlterColumnExpression { column, .. } if column.name == name.into());
        assert!(
            at(&expression("h")) < at(&retyped("b"))
                && at(&retyped("b")) < at(&expression("g"))
                && at(&expression("g")) < at(&retyped("x")),
            "{:?}",
            cs.changes
        );
        // The same reuse when the released reader is the one moving to the
        // new column's name: `g` moves from the renamed `x` to the new `a`,
        // which releases `x`.
        let mut cs = plan(vec![
            rename("a", "x"),
            Change::AddColumn {
                uid: uid("c_eeeeee"),
                table: t.clone(),
                name: "a".into(),
                column: Box::new(pbps_model::Column::new("integer".parse().unwrap())),
            },
            retype("x"),
            recompute("a * 2"),
        ]);
        assert_eq!(unreleased(&cs, &reads_a).len(), 1);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(1));
        assert!(unreleased(&cs, &reads_a).is_empty(), "{:?}", cs.changes);
    }

    /// #1956 review: the kept orders a released retype states are the
    /// crossings that happen, not one per later change. Released before the
    /// rows, it passes none and keeps none; released after them, it keeps
    /// only the first, the one its refusal names.
    #[test]
    fn a_released_retype_keeps_only_the_changes_it_would_pass() {
        use pbps_model::{Change, ColumnRef, PlannedChange};
        let t: TableName = "app.t".parse().unwrap();
        let uid = |u: &str| -> pbps_model::Uid { u.parse().unwrap() };
        let retype = PlannedChange::new(Change::AlterColumnType {
            uid: uid("c_bbbbbb"),
            column: ColumnRef::new(t.clone(), "a"),
            from: "integer".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            through_external_partitions: false,
            from_collation: None,
            to_collation: None,
            using: None,
        });
        let release = PlannedChange::new(Change::AlterColumnExpression {
            uid: uid("c_dddddd"),
            column: ColumnRef::new(t.clone(), "g"),
            from: "(a * 2)".into(),
            to: "b * 2".into(),
        });
        let update = |key: usize| {
            PlannedChange::new(Change::UpdateRow {
                table: t.clone(),
                key_column: "id".into(),
                key: pbps_model::RowKey::from(key.to_string().as_str()),
                columns: BTreeMap::from([(
                    "b".into(),
                    (
                        pbps_model::Cell::Value(pbps_model::Value::Int(1)),
                        pbps_model::Cell::Value(pbps_model::Value::Int(2)),
                    ),
                )]),
                unchanged: BTreeMap::new(),
                types: BTreeMap::new(),
                after_types: BTreeMap::new(),
                key_type: None,
            })
        };
        let rows = 200;
        let kept = |changes: Vec<PlannedChange>, held: usize, reach: usize| {
            let cs = ChangeSet { changes };
            let ids = crate::plan_fixture::ids(&cs);
            let names = pbps_model::plan_names::PlanNames::of(&cs.changes, &ids).unwrap();
            let mut schedule = pbps_diff::order::Schedule::new(cs.changes.len());
            keep_ahead(
                &mut schedule,
                &cs,
                &names,
                &BTreeMap::new(),
                &mut std::collections::BTreeSet::new(),
                held,
                reach,
            );
            schedule.kept_orders().to_vec()
        };
        // Released first: the retype waits for nothing after it.
        let mut early = vec![release.clone(), retype.clone()];
        early.extend((0..rows).map(update));
        assert_eq!(kept(early, 1, 1), []);
        // Released after every row: one kept order, the first row's.
        let mut late = vec![retype];
        late.extend((0..rows).map(update));
        late.push(release);
        let late = kept(late, 0, rows + 1);
        assert_eq!(late.len(), 1, "{late:?}");
        assert_eq!((late[0].before, late[0].after), (0, 1));
        assert!(
            late[0].refusal.contains("may need the new type"),
            "{late:?}"
        );
    }

    /// The one frame the ready-phase review named: a `DbError::Driver`'s
    /// `message` is exactly what a driver's `db.message()` can carry — a
    /// value nobody declared to this tool, measured on both engines
    /// (DECISIONS 455) — and this is the only frame `ledger_safe_reason` may
    /// not repeat verbatim. The code is PostgreSQL's SQLSTATE here, but the
    /// marker names it neutrally: SQL Server's own `Driver` frame carries a
    /// numeric message number instead, not a SQLSTATE (see the MSSQL test
    /// below), and this function has one wording for both.
    #[test]
    fn a_driver_frame_is_redacted_to_its_code_and_nothing_else() {
        let db = anyhow::Error::new(DbError::Driver {
            message: "invalid input syntax for type integer: \"super-secret-abc\"".to_owned(),
            code: Some("22P02".to_owned()),
        });
        let rendered = ledger_safe_reason(&db);
        assert!(
            !rendered.contains("super-secret-abc"),
            "the driver's own text must not survive: {rendered}"
        );
        assert!(
            rendered.contains("22P02"),
            "the code must survive: {rendered}"
        );
        assert!(
            !rendered.contains("SQLSTATE"),
            "SQLSTATE is a PostgreSQL word; SQL Server's own code is not one \
             (this same wording renders both): {rendered}"
        );
    }

    /// #1605 review: the absence proof's refusal keeps a driver failure as
    /// its source, so a failed bootstrap's ledger row is redacted still, and
    /// the operator reads both the refusal and its cause.
    #[test]
    fn an_unproven_absence_keeps_its_driver_frame_redactable() {
        let refused = absence_unproven(
            &["other.t".parse().unwrap()],
            DbError::Driver {
                message: "server says super-secret-abc".to_owned(),
                code: Some("229".to_owned()),
            },
        );
        let recorded = ledger_safe_reason(&refused);
        assert!(!recorded.contains("super-secret-abc"), "{recorded}");
        assert!(
            recorded.contains("cannot record other.t as missing"),
            "{recorded}"
        );
        assert!(recorded.contains("229"), "{recorded}");
        // Negative: what the operator reads still names the cause.
        assert!(format!("{refused:#}").contains("super-secret-abc"));
    }

    /// A driver failure with no code at all (a broken protocol, not a
    /// refused statement) is still redacted — the marker just says so, rather
    /// than rendering an empty code or falling back to the message it exists
    /// to withhold.
    #[test]
    fn a_driver_frame_with_no_code_is_redacted_without_inventing_one() {
        let db = anyhow::Error::new(DbError::Driver {
            message: "super-secret-protocol-detail".to_owned(),
            code: None,
        });
        let rendered = ledger_safe_reason(&db);
        assert!(
            !rendered.contains("super-secret-protocol-detail"),
            "{rendered}"
        );
        assert_eq!(
            rendered,
            "the driver reported a failure with no code; its message is not recorded here"
        );
    }

    /// SQL Server's own `Driver` frame: `tiberius::Error::code()` is a numeric
    /// message number (`208`, "cannot drop the object because..."; `2627`,
    /// a constraint violation), never a SQLSTATE — ready-phase review found
    /// this function calling every engine's code a SQLSTATE regardless, which
    /// is simply wrong on this one. The marker must still redact the message
    /// and keep the code, without calling it something it is not.
    #[test]
    fn a_mssql_driver_frame_is_redacted_without_being_called_a_sqlstate() {
        let db = anyhow::Error::new(DbError::Driver {
            message: "Conversion failed when converting the varchar value \
                      'super-secret-abc' to data type int."
                .to_owned(),
            code: Some("245".to_owned()),
        });
        let rendered = ledger_safe_reason(&db);
        assert!(
            !rendered.contains("super-secret-abc"),
            "the driver's own text must not survive: {rendered}"
        );
        assert!(
            rendered.contains("245"),
            "the code must survive: {rendered}"
        );
        assert!(
            !rendered.contains("SQLSTATE"),
            "245 is a SQL Server message number, not a SQLSTATE: {rendered}"
        );
    }

    /// Every other `DbError` variant is this tool's own composed text, not a
    /// value the server echoed back, and passes through unchanged — the
    /// redaction is narrow, not "any `DbError` at all".
    #[test]
    fn a_non_driver_dberror_frame_passes_through_unchanged() {
        let bad_row = anyhow::Error::new(DbError::BadRow(
            "column `n` was read as an unsigned byte".to_owned(),
        ));
        assert_eq!(
            ledger_safe_reason(&bad_row),
            "unexpected row shape: column `n` was read as an unsigned byte"
        );
    }

    /// A tool-composed refusal — an unsafe data trigger, a catalog read
    /// outside the transaction it needs — names its own rule or trigger in
    /// its message, never a server value, and `DbError::Refused` makes that
    /// unrepresentable as `Driver` by construction (DECISIONS 455). Ready-phase
    /// review found this function redacting exactly this shape before the
    /// split existed; this pins that it cannot happen again even if a new
    /// call site is added, since `Refused` falls to the same `_` arm as any
    /// other non-`Driver` frame rather than a case naming it specially.
    #[test]
    fn a_refused_frame_names_its_own_rule_and_is_never_redacted() {
        let refused = anyhow::Error::new(DbError::Refused(
            "unsafe data trigger `audit.enforce_price` on `sales.orders`: this trigger is not \
             part of the recorded baseline"
                .to_owned(),
        ));
        assert_eq!(
            ledger_safe_reason(&refused),
            "unsafe data trigger `audit.enforce_price` on `sales.orders`: this trigger is not \
             part of the recorded baseline"
        );
    }

    /// The shape `execute_statements` and `apply_staged_under_lock` build
    /// today: this tool's own `.context()` sentence naming the emitted
    /// statement (safe — it is the plan's own SQL, not server-echoed data) on
    /// top of the driver's frame underneath it. Both survive, but only one of
    /// them keeps its own text.
    #[test]
    fn a_context_wrapped_driver_frame_keeps_the_context_and_redacts_the_source() {
        let db = DbError::Driver {
            message: "invalid input syntax for type integer: \"super-secret-abc\"".to_owned(),
            code: Some("22P02".to_owned()),
        };
        let wrapped = anyhow::Error::new(db).context(
            "the database rejected this statement, and the whole plan was rolled back:\n\
             ALTER TABLE t ALTER COLUMN n TYPE integer",
        );
        let rendered = ledger_safe_reason(&wrapped);
        assert!(
            rendered.contains("the database rejected this statement"),
            "this tool's own framing must survive: {rendered}"
        );
        assert!(
            rendered.contains("ALTER TABLE t ALTER COLUMN n TYPE integer"),
            "the plan's own emitted SQL is not server-echoed data, and must survive: {rendered}"
        );
        assert!(!rendered.contains("super-secret-abc"), "{rendered}");
        assert!(rendered.contains("22P02"), "{rendered}");
    }

    /// The boundary a ready-phase round found: a `.context()` sentence long
    /// enough on its own to fill the ledger's `reason` column — a `CREATE
    /// VIEW` or a large reference-data block, not a contrived string — used
    /// to push the redacted driver marker past where `truncate_reason` cuts,
    /// leaving a `Failed` row with a fragment of the emitted SQL and neither
    /// the server's message nor its code. `ledger_safe_reason` now orders the
    /// marker first for exactly this reason, so this test builds a context
    /// frame longer than *both* engines' column widths and checks the code
    /// still survives `truncate_reason` for each.
    #[test]
    fn a_context_frame_longer_than_the_column_does_not_crowd_out_the_code() {
        assert_eq!(
            pbps_pg::state::REASON_CHARS,
            pbps_mssql::state::REASON_UTF16_UNITS,
            "this test's one oversized context frame must outgrow both engines' columns"
        );
        let long_statement = "x".repeat(pbps_pg::state::REASON_CHARS * 2);
        let db = DbError::Driver {
            message: "invalid input syntax for type integer: \"super-secret-abc\"".to_owned(),
            code: Some("22P02".to_owned()),
        };
        let wrapped = anyhow::Error::new(db).context(format!(
            "the database rejected this statement, and the whole plan was rolled back:\n{long_statement}"
        ));
        let rendered = ledger_safe_reason(&wrapped);

        for driver in [Driver::Postgres, Driver::Mssql] {
            let cut = truncate_reason(driver, &rendered);
            assert!(
                cut.contains("22P02"),
                "{driver:?}: the code is the highest-value fact in a bounded column \
                 and must survive truncation even behind an oversized context frame: {cut}"
            );
            assert!(
                !cut.contains("super-secret-abc"),
                "{driver:?}: redaction must still hold after reordering: {cut}"
            );
        }
    }

    /// A ready-phase round on the truncation fix found this shape: a wrapper
    /// whose own `Display` interpolates `{source}` — `RowsError::Read`, for
    /// an apply whose managed-row read hits a data-bearing server error —
    /// reproduces the driver's full, unredacted text as part of *its own*
    /// rendered frame, before `error.chain()` ever reaches the `DbError`
    /// separately to redact it. Redacting that later frame changes nothing;
    /// the leak already happened one frame up. Fixed by dropping `{source}`
    /// from `RowsError::Read`'s format string (`crates/pbps-db/src/
    /// catalog.rs`) rather than special-casing `RowsError` here: `#[source]`
    /// alone still lets `.chain()` walk into the `DbError`, so this function
    /// needs no new arm, the same way it needed none for `DbError::Refused`.
    #[test]
    fn a_wrapper_that_names_its_source_does_not_repeat_the_drivers_text() {
        let db = DbError::Driver {
            message: "invalid input syntax for type integer: \"super-secret-xyz\"".to_owned(),
            code: Some("22P02".to_owned()),
        };
        let wrapped = anyhow::Error::new(RowsError::Read {
            table: TableName::new("app", "t"),
            source: Box::new(db),
        });
        let rendered = ledger_safe_reason(&wrapped);
        assert!(
            !rendered.contains("super-secret-xyz"),
            "a wrapper frame's own Display must not smuggle the driver's text \
             past the redaction that runs on the frame beneath it: {rendered}"
        );
        assert!(
            rendered.contains("22P02"),
            "the code must still survive, from the DbError frame beneath the \
             wrapper: {rendered}"
        );
        assert!(
            rendered.contains("reading its rows back failed"),
            "the wrapper's own text, which names no server value, must still \
             survive: {rendered}"
        );
    }

    /// A ready-phase round on the previous fix found a third shape:
    /// `LedgerError::Db` and `ImpactError::Query` are `#[error(transparent)]`,
    /// which is not a boxed `#[source]` either — thiserror forwards
    /// `Display` to the wrapped `DbError` but forwards `source()` to *its*
    /// `source()`, so `error.chain()` never produces a second link to
    /// downcast at all: the one frame it does produce downcasts to
    /// `LedgerError`, never to `DbError`, and its `Display` still renders
    /// the driver's raw message regardless. `ledger_safe_reason` now also
    /// tries `downcast_ref::<LedgerError>()` (and `ImpactError`) and unwraps
    /// their transparent `DbError` directly, rather than relying on `.chain()`
    /// to have produced it as its own link.
    #[test]
    fn a_transparent_wrapper_does_not_repeat_the_drivers_text() {
        let db = DbError::Driver {
            message: "invalid input syntax for type integer: \"super-secret-def\"".to_owned(),
            code: Some("22P02".to_owned()),
        };
        let wrapped = anyhow::Error::new(LedgerError::Db(db));
        let rendered = ledger_safe_reason(&wrapped);
        assert!(
            !rendered.contains("super-secret-def"),
            "a transparent wrapper's forwarded Display must not smuggle the \
             driver's text past redaction: {rendered}"
        );
        assert!(
            rendered.contains("22P02"),
            "the code must still survive, from the DbError a transparent \
             wrapper hides from .chain(): {rendered}"
        );

        let db2 = DbError::Driver {
            message: "invalid input syntax for type integer: \"super-secret-ghi\"".to_owned(),
            code: Some("22P03".to_owned()),
        };
        let wrapped2 = anyhow::Error::new(ImpactError::Query(db2));
        let rendered2 = ledger_safe_reason(&wrapped2);
        assert!(!rendered2.contains("super-secret-ghi"), "{rendered2}");
        assert!(rendered2.contains("22P03"), "{rendered2}");
    }

    #[test]
    fn nested_database_context_survives_redaction_through_every_wrapper() {
        for code in [Some("42501"), Some("1088"), None] {
            for wrapper in 0..4 {
                let db = DbError::Driver {
                    message: "undeclared-secret-488".into(),
                    code: code.map(str::to_owned),
                }
                .context("the data-trigger guard cannot lock `public.c`")
                .context("The deployment role needs INSERT, UPDATE, DELETE or TRUNCATE");
                let error = match wrapper {
                    0 => anyhow::Error::new(db),
                    1 => anyhow::Error::new(RowsError::Read {
                        table: TableName::new("app", "p"),
                        source: Box::new(db),
                    }),
                    2 => anyhow::Error::new(LedgerError::Db(db)),
                    _ => anyhow::Error::new(ImpactError::Query(db)),
                }
                .context("x".repeat(pbps_pg::state::REASON_CHARS * 2));
                assert!(format!("{error:#}").contains("undeclared-secret-488"));
                let rendered = ledger_safe_reason(&error);
                for text in ["public.c", "INSERT, UPDATE, DELETE or TRUNCATE"] {
                    assert_eq!(rendered.matches(text).count(), 1, "{rendered}");
                }
                assert_eq!(
                    rendered.matches("its message is not recorded here").count(),
                    1
                );
                assert!(!rendered.contains("undeclared-secret-488"), "{rendered}");
                for driver in [Driver::Postgres, Driver::Mssql] {
                    let cut = truncate_reason(driver, &rendered);
                    assert!(cut.contains(code.unwrap_or("no code")), "{cut}");
                    assert!(cut.contains("public.c"), "{cut}");
                    assert!(cut.contains("INSERT, UPDATE, DELETE or TRUNCATE"), "{cut}");
                }
            }
        }
        let error =
            anyhow::Error::new(DbError::Refused("own refusal".into()).context("own context"));
        assert_eq!(ledger_safe_reason(&error), "own refusal: own context");
    }

    #[test]
    fn replacement_checks_include_paired_drops_but_not_ordinary_drops() {
        use pbps_model::{Change, Module, ModuleKind, PlannedChange};
        let id: pbps_model::ModuleId = "app.v".parse().unwrap();
        let mut cs = ChangeSet::default();
        cs.changes.push(PlannedChange::new(Change::DropModule {
            id: id.clone(),
            kind: ModuleKind::View,
        }));
        assert!(rebuilt_modules(&cs).is_empty());
        cs.changes.push(PlannedChange::new(Change::CreateModule {
            id: id.clone(),
            module: Box::new(Module {
                kind: ModuleKind::Function,
                description: None,
                definition: "SELECT 1".into(),
            }),
        }));
        assert_eq!(
            rebuilt_modules(&cs)[&id],
            (ModuleKind::View, ModuleKind::Function)
        );
        assert!(require_transactional_rebuilds(Driver::Mssql, &cs, true).is_ok());
        assert!(require_transactional_rebuilds(Driver::Postgres, &cs, true).is_err());
        assert!(require_transactional_rebuilds(Driver::Postgres, &cs, false).is_ok());
    }

    /// The `CREATE` and the revoke that closes the routine are two statements,
    /// and a staged run commits each on its own — so between them the routine
    /// is committed, visible, and executable by every principal in the
    /// cluster (DECISIONS 517). Refused on **either** driver: what makes this
    /// unsafe is the staging, not the engine, and the change only ever
    /// reaches a plan for an engine that has the default.
    #[test]
    fn a_staged_plan_that_closes_a_routine_to_public_is_refused_on_either_driver() {
        use pbps_model::{Change, PlannedChange};
        let mut cs = ChangeSet::default();
        cs.changes.push(PlannedChange::new(Change::PublicExecution {
            routine: "app.f(integer)".parse().unwrap(),
            access: pbps_model::PublicAccess::Revoked,
            origin: pbps_model::RoutineOrigin::Created,
        }));
        for driver in [Driver::Postgres, Driver::Mssql] {
            let e = require_transactional_rebuilds(driver, &cs, true)
                .expect_err("a staged run leaves the routine open")
                .to_string();
            assert!(e.contains("app.f(integer)"), "{e}");
            assert!(e.contains("public_execute: true"), "{e}");
            assert!(require_transactional_rebuilds(driver, &cs, false).is_ok());
        }

        // The other decision's window leaves the routine closed rather than
        // open, which is not the exposure this rule is about.
        let mut kept = ChangeSet::default();
        kept.changes
            .push(PlannedChange::new(Change::PublicExecution {
                routine: "app.f(integer)".parse().unwrap(),
                access: pbps_model::PublicAccess::Kept,
                origin: pbps_model::RoutineOrigin::Created,
            }));
        assert!(require_transactional_rebuilds(Driver::Postgres, &kept, true).is_ok());
    }

    /// The routine the refusal names is the plan's, and its name may hold a
    /// line break or a terminal sequence: it is shown escaped (DEC-2032.1,
    /// #2035). Negative: a Unicode name is shown as it is.
    #[test]
    fn the_staged_refusal_shows_the_routine_name_escaped() {
        use pbps_model::{Change, PlannedChange};
        let closing = |name: &str| {
            let mut cs = ChangeSet::default();
            cs.changes.push(PlannedChange::new(Change::PublicExecution {
                routine: format!("app.{name}(integer)").parse().unwrap(),
                access: pbps_model::PublicAccess::Revoked,
                origin: pbps_model::RoutineOrigin::Created,
            }));
            require_transactional_rebuilds(Driver::Postgres, &cs, true)
                .expect_err("a staged run leaves the routine open")
                .to_string()
        };
        let e = closing("f\n\u{1b}[31mred\r\u{202e}");
        assert!(!e.chars().any(|c| c.is_control() && c != '\n'), "{e:?}");
        assert!(
            e.contains("app.f\\n\\u{1b}[31mred\\r\\u{202e}(integer)"),
            "{e}"
        );
        assert_eq!(e.lines().count(), 2, "the name started no line: {e}");
        assert!(closing("名前_ünï").contains("app.名前_ünï(integer)"));
    }

    /// A runtime for the live tests below, built by hand because the
    /// workspace `tokio` has no `macros` feature.
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(f)
    }

    async fn pg() -> Conn {
        let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
        Conn::connect(Driver::Postgres, &url)
            .await
            .expect("connect to the live PostgreSQL")
    }

    /// The seam routes by the connection, and the PostgreSQL arms answer the
    /// engine-only questions as facts about the engine rather than as empty
    /// answers: one edition, nothing refused for one, and a pull that keeps
    /// no inventory of unmanageable modules (the field's own documentation
    /// says where those go instead).
    #[test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    fn the_postgres_arms_answer_engine_only_questions_by_name() {
        block_on(async {
            let database = crate::test_pg::TestDb::create("engine").await;
            let mut conn = database.connect().await;
            assert_eq!(conn.driver(), Driver::Postgres);

            let version = server_version(&mut conn).await.expect("version");
            assert!(version.starts_with("1"), "{version}");
            let caps = capabilities(&mut conn, Some(&version)).await.expect("caps");
            assert_eq!(
                caps,
                Capabilities {
                    edition: None,
                    supports_online: true,
                    supports_create_or_alter: Some(true),
                }
            );

            let verdict = edition_verdict(&mut conn, &ChangeSet::default())
                .await
                .expect("verdict");
            assert!(verdict.refused_online.is_empty() && verdict.warnings.is_empty());

            let pulled = introspect(&mut conn, Read::Snapshot).await.expect("pull");
            assert!(pulled.unmanaged_modules.is_empty());

            let names: BTreeSet<String> = ["public".to_owned(), "no_such_schema_here".to_owned()]
                .into_iter()
                .collect();
            let spelled = schema_spellings(&mut conn, &names)
                .await
                .expect("spellings");
            assert_eq!(spelled["public"], Some("public".to_owned()));
            assert_eq!(spelled["no_such_schema_here"], None);

            assert_eq!(truncate_reason(conn.driver(), "abc"), "abc");
            drop(conn);
            database.drop().await;
        });
    }

    /// The role questions refuse on PostgreSQL, by name, rather than answer
    /// "none": every one exists to clear a statement that dialect never
    /// plans, so an answer would be one to a question nobody asked.
    #[test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    fn the_role_questions_refuse_on_postgres_rather_than_answer_none() {
        block_on(async {
            let mut conn = pg().await;
            let alike = names_alike(&mut conn, &["a", "A"]).await;
            let holding = principals_holding(&mut conn, &["a"], &[]).await;
            let members = role_members(&mut conn).await;
            let owned = role_owned_securables(&mut conn).await;
            for (what, message) in [
                ("names_alike", alike.err().map(|e| e.to_string())),
                ("principals_holding", holding.err().map(|e| e.to_string())),
                ("role_members", members.err().map(|e| e.to_string())),
                ("role_owned_securables", owned.err().map(|e| e.to_string())),
            ] {
                let message = message.unwrap_or_else(|| panic!("{what} answered on PostgreSQL"));
                assert!(message.contains("DECISIONS 211"), "{what}: {message}");
            }
        });
    }

    /// A module is never a rename target on PostgreSQL, and handed one anyway
    /// the engine refuses rather than reporting "nothing depends on it".
    #[test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    fn a_module_handed_to_the_postgres_rename_impact_is_refused_not_empty() {
        block_on(async {
            let mut conn = pg().await;
            let target = RenameTarget::Module("public.v".parse().unwrap());
            match rename_impact(&mut conn, &target).await {
                Err(ImpactError::Name(e)) => assert!(e.to_string().contains("module"), "{e}"),
                Err(other) => panic!("refused for the wrong reason: {other}"),
                Ok(report) => panic!("answered: {report:?}"),
            }
        });
    }

    #[test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
    fn a_failed_cost_query_is_unavailable_not_a_static_guess() {
        block_on(async {
            let mut conn = pg().await;
            conn.execute("BEGIN").await.unwrap();
            assert!(conn.execute("SELECT 1 / 0").await.is_err());
            let cs = ChangeSet {
                changes: vec![pbps_model::PlannedChange::new(
                    pbps_model::Change::AlterColumnType {
                        uid: "c_aaaaaa".parse().unwrap(),
                        column: "public.t.v".parse().unwrap(),
                        from: "integer".parse().unwrap(),
                        to: "bigint".parse().unwrap(),
                        from_nullable: true,
                        to_nullable: true,
                        through_external_partitions: false,
                        from_collation: None,
                        to_collation: None,
                        using: None,
                    },
                )],
            };
            let ids = IdsFile {
                tables: [("t_aaaaaa".parse().unwrap(), "public.t".parse().unwrap())].into(),
                columns: [("c_aaaaaa".parse().unwrap(), "public.t.v".parse().unwrap())].into(),
                ..Default::default()
            };
            let report = operational_cost(&mut conn, &cs, &ids).await;
            conn.execute("ROLLBACK").await.unwrap();
            let value = serde_json::to_value(&report).unwrap();
            assert_eq!(value["status"], "available");
            let change = &value["changes"][0];
            assert_eq!(change["status"], "unavailable");
            assert!(
                change["reason"]
                    .as_str()
                    .unwrap()
                    .contains("could not read PostgreSQL catalog")
            );
            assert!(change.get("rewrite").is_none());
            let human = crate::cost::render(&report);
            assert!(human.contains("unavailable"));
            assert!(!human.contains("Rewrite: yes"));
        });
    }

    /// A logical slot on the database is listed with its plugin, by the
    /// moves it would decode, and never refused; none is a pass that still
    /// names the move (#1788).
    #[test]
    fn logical_slots_are_listed_not_refused() {
        let moving = vec!["app.ev_rest into app.ev_2025".to_owned()];
        let listed = slots_listed(
            &moving,
            &[
                ("cdc".to_owned(), "pgoutput".to_owned()),
                ("debezium".to_owned(), "decoderbufs".to_owned()),
            ],
        );
        assert_eq!(listed.status, "listed");
        assert_eq!(
            listed.message,
            "logical replication slot(s) cdc (pgoutput), debezium (decoderbufs) on this \
             database will decode each row moved from app.ev_rest into app.ev_2025 as a delete \
             and an insert"
        );
        // Negative: no slot is not a listing.
        let none = slots_listed(&moving, &[]);
        assert_eq!(none.status, "passed");
        assert!(
            none.message.contains("no logical replication slot")
                && none.message.contains("app.ev_rest into app.ev_2025"),
            "{}",
            none.message
        );
    }

    #[test]
    fn numbered_column_fill_messages_are_shown_once_in_every_format() {
        const RAW: &str = "name\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";
        const SHOWN: &str = r"name\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";
        let table = TableName::new("app", RAW);
        let fills = [(table.clone(), RAW.into())];
        for listed in [false, true] {
            let publications = if listed {
                vec![(
                    table.clone(),
                    vec![("whole".into(), false), ("cols".into(), true)],
                )]
            } else {
                Vec::new()
            };
            let check = fills_listed(&fills, &publications, &[]);
            assert_eq!(check.status, if listed { "listed" } else { "passed" });
            let expected = |name: &str| {
                if listed {
                    format!(
                        "app.{name}.{name} numbers the rows already there in a table rewrite, \
                         which logical decoding does not stream: publication(s) whole over \
                         app.{name} will not carry those numbers, and a consumer keeps them \
                         empty until it takes a new snapshot of the table; publication cols over \
                         app.{name} lists its columns, so it publishes none of the added \
                         column(s) until they are added to its list"
                    )
                } else {
                    format!(
                        "no publication or logical replication slot would miss the numbers \
                         app.{name}.{name} gives the rows already there"
                    )
                }
            };
            assert_eq!(check.message, expected(SHOWN));
            assert_eq!(
                serde_json::to_value(&check).unwrap()["message"],
                expected(SHOWN)
            );
            // An escaped-looking identifier is a different column, not an alias.
            let literal = fills_listed(&[(table.clone(), SHOWN.into())], &publications, &[]);
            assert_ne!(literal.message, check.message);
        }
    }

    /// A numbered column added to a table already there names each
    /// publication over the table and each logical slot, and refuses
    /// nothing; with neither it passes, still naming the column (#1890).
    #[test]
    fn a_numbered_column_fill_lists_the_streams_that_miss_it() {
        let t: TableName = "app.t".parse().unwrap();
        let u: TableName = "app.u".parse().unwrap();
        let fills = vec![(t.clone(), "n".into()), (u.clone(), "m".into())];
        let listed = fills_listed(
            &fills,
            &[
                (
                    t.clone(),
                    vec![
                        ("cdc".into(), false),
                        ("cols".into(), true),
                        ("every".into(), false),
                    ],
                ),
                (u.clone(), Vec::new()),
            ],
            &[("debezium".to_owned(), "pgoutput".to_owned())],
        );
        assert_eq!(listed.status, "listed");
        assert_eq!(
            listed.message,
            "app.t.n, app.u.m numbers the rows already there in a table rewrite, which logical \
             decoding does not stream: publication(s) cdc, every over app.t; logical \
             replication slot(s) debezium (pgoutput) on this database will not carry those \
             numbers, and a consumer keeps them empty until it takes a new snapshot of the table; \
             publication cols over app.t lists its columns, so it publishes none of the added \
             column(s) until they are added to its list"
        );
        let columns_only = fills_listed(
            &fills[..1],
            &[(t.clone(), vec![("cols".into(), true)])],
            &[],
        );
        assert_eq!(columns_only.status, "listed");
        assert!(columns_only.message.contains(
            "publication cols over app.t lists its columns, so it publishes none of the added \
             column(s) until they are added to its list"
        ));
        assert!(!columns_only.message.contains("new snapshot"));
        let slots_only = fills_listed(
            &fills[..1],
            &[(t.clone(), Vec::new())],
            &[("s".to_owned(), "test_decoding".to_owned())],
        );
        assert_eq!(slots_only.status, "listed");
        // Negative: no publication and no slot is a pass, not a listing.
        let none = fills_listed(&fills[..1], &[(t, Vec::new())], &[]);
        assert_eq!(none.status, "passed");
        assert_eq!(
            none.message,
            "no publication or logical replication slot would miss the numbers app.t.n gives \
             the rows already there"
        );
    }
}

#[cfg(test)]
mod plan_name_tests {
    use super::*;
    fn order_after_releases(
        changes: &mut ChangeSet,
        ids: &pbps_model::IdsFile,
        dependences: &BTreeMap<TableName, Vec<pbps_pg::generated::Dependence>>,
    ) -> Result<usize, String> {
        crate::test_planning(changes, |plan| {
            // These name-only fixtures never enter the carried-row graph.
            let schema = pbps_model::Schema::default();
            super::order_after_releases(
                plan,
                pbps_diff::Side {
                    schema: &schema,
                    ids,
                },
                pbps_diff::Side {
                    schema: &schema,
                    ids,
                },
                dependences,
                &pbps_pg::Postgres::new(),
                &pbps_model::ModuleDeps::new(),
            )
        })
    }

    use pbps_model::{Change, GrantTarget, Permission, PlannedChange};

    #[test]
    fn computed_catalog_reads_use_the_original_holder_at_each_statement() {
        let (changes, ids) = crate::plan_fixture::chain();
        let names = pbps_model::plan_names::PlanNames::of(&changes.changes, &ids).unwrap();
        let original = TableName::new("app", "original_table");
        for (at, name) in [
            (0, "original_table"),
            (1, "temporary_table"),
            (2, "final_table"),
        ] {
            assert_eq!(
                computed_catalog_table(&names, &TableName::new("app", name), at),
                original
            );
        }
        let unmanaged = TableName::new("app", "external_table");
        assert_eq!(computed_catalog_table(&names, &unmanaged, 2), unmanaged);
    }

    #[test]
    fn a_generated_readers_release_survives_table_and_column_rename_chains() {
        let (mut changes, mut ids) = crate::plan_fixture::chain();
        let table = TableName::new("app", "final_table");
        let generated: pbps_model::Uid = "c_bbbbbb".parse().unwrap();
        ids.columns.insert(generated.clone(), table.column("g"));
        changes.changes.extend([
            PlannedChange::new(Change::AlterColumnType {
                uid: "c_aaaaaa".parse().unwrap(),
                column: table.column("final_column"),
                from: "integer".parse().unwrap(),
                to: "bigint".parse().unwrap(),
                from_nullable: true,
                to_nullable: true,
                through_external_partitions: false,
                from_collation: None,
                to_collation: None,
                using: None,
            }),
            PlannedChange::new(Change::AlterColumnExpression {
                uid: generated,
                column: table.column("g"),
                from: "final_column * 2".into(),
                to: "0".into(),
            }),
        ]);
        let reads = BTreeMap::from([(
            TableName::new("app", "original_table"),
            vec![pbps_pg::generated::Dependence {
                base: "original_column".into(),
                generated: "g".into(),
                base_required: false,
                filled_by_trigger: false,
            }],
        )]);
        assert_eq!(
            order_after_releases(&mut changes.clone(), &ids, &BTreeMap::new()),
            Ok(0)
        );
        assert_eq!(order_after_releases(&mut changes, &ids, &reads), Ok(1));
        assert!(matches!(
            changes.changes[4].change,
            Change::AlterColumnExpression { .. }
        ));
    }

    #[test]
    fn sequence_revocability_evidence_follows_both_table_and_column_renames() {
        let (mut changes, ids) = crate::plan_fixture::chain();
        changes.changes.push(PlannedChange::new(Change::Revoke {
            role: "writer".into(),
            target: "sequence::app.final_table.final_column".parse().unwrap(),
            permissions: [Permission::Usage].into(),
        }));
        let mut found = pbps_db::catalog::Unrevocable {
            role: "writer".into(),
            target: "sequence::app.original_table.original_column"
                .parse()
                .unwrap(),
            permission: Permission::Usage,
            why: "the exact sequence grantor cannot be selected".into(),
        };
        assert!(unrevocable_grants(Driver::Postgres, &changes, &ids, &[found.clone()]).is_err());
        found.target = "sequence::app.original_table.other".parse().unwrap();
        assert!(unrevocable_grants(Driver::Postgres, &changes, &ids, &[found.clone()]).is_ok());
        found.target = "sequence::app.original_table.original_column"
            .parse()
            .unwrap();
        found.permission = Permission::Select;
        assert!(unrevocable_grants(Driver::Postgres, &changes, &ids, &[found]).is_ok());
    }

    #[test]
    fn serial_revocability_evidence_follows_its_table_through_renames() {
        let (mut changes, ids) = crate::plan_fixture::chain();
        changes.changes.push(PlannedChange::new(Change::Revoke {
            role: "writer".into(),
            target: GrantTarget::Object(TableName::new("app", "final_table")),
            permissions: [Permission::SerialSequences].into(),
        }));
        let mut found = pbps_db::catalog::Unrevocable {
            role: "writer".into(),
            target: GrantTarget::Object(TableName::new("app", "original_table")),
            permission: Permission::SerialSequences,
            why: "the sequence grantor cannot be selected".into(),
        };
        assert!(unrevocable_grants(Driver::Postgres, &changes, &ids, &[found.clone()]).is_err());
        found.permission = Permission::Usage;
        assert!(unrevocable_grants(Driver::Postgres, &changes, &ids, &[found.clone()]).is_ok());
        found.permission = Permission::SerialSequences;
        found.target = "app.unmanaged".parse().unwrap();
        assert!(unrevocable_grants(Driver::Postgres, &changes, &ids, &[found]).is_ok());
    }

    #[test]
    fn grant_checks_keep_catalog_ownership_and_grantors_through_a_rename_chain() {
        let (mut changes, ids) = crate::plan_fixture::chain();
        let target = GrantTarget::Object(TableName::new("app", "final_table"));
        let stored = GrantTarget::Object(TableName::new("app", "original_table"));
        changes.changes.push(PlannedChange::new(Change::Grant {
            role: "owner".into(),
            target: target.clone(),
            permissions: [Permission::Select].into(),
        }));
        let owners = [(stored.clone(), "owner".into())].into();
        assert!(
            owned_targets(
                Driver::Postgres,
                &changes,
                &ids,
                &owners,
                &pbps_model::RoleName::from("deployer")
            )
            .is_err()
        );
        assert!(
            owned_targets(
                Driver::Postgres,
                &changes,
                &ids,
                &BTreeMap::new(),
                &pbps_model::RoleName::from("deployer")
            )
            .is_ok()
        );
        changes.changes.push(PlannedChange::new(Change::Revoke {
            role: "reader".into(),
            target,
            permissions: [Permission::Select].into(),
        }));
        let found = pbps_db::catalog::Unrevocable {
            role: "reader".into(),
            target: stored,
            permission: Permission::Select,
            why: "the session cannot revoke the stored grant".into(),
        };
        assert!(unrevocable_grants(Driver::Postgres, &changes, &ids, &[found]).is_err());
        assert!(unrevocable_grants(Driver::Postgres, &changes, &ids, &[]).is_ok());
    }
}

/// Read old policy bindings after module rediff settles the edits. A UID may
/// be renamed and then edited; accumulate its facts instead of letting the
/// first edit hide a later one. No failed read becomes an empty result.
async fn policy_dependents(
    conn: &mut Conn,
    changes: &ChangeSet,
    ids: &pbps_model::IdsFile,
    managed: &pbps_model::Schema,
) -> anyhow::Result<crate::row_security::PolicyDependents> {
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
    let mut found = crate::row_security::PolicyDependents::new();
    for step in &changes.changes {
        let Some(uid) = crate::row_security::policy_target_uid(&step.change) else {
            continue;
        };
        let mut policies = if let pbps_model::Change::DropTable { uid, .. } = &step.change {
            if let Some(table) = names.table_at(uid, pbps_model::plan_names::At::Start) {
                pbps_pg::modules::policies_on_table(conn, table).await?
            } else {
                Vec::new()
            }
        } else if let pbps_model::Change::RenameTable { to, .. } = &step.change {
            if let Some(table) = names.table_at(uid, pbps_model::plan_names::At::Start) {
                pbps_pg::modules::policies_affected_by_table_rename(conn, table, to, managed)
                    .await?
            } else {
                Vec::new()
            }
        } else if let Some((owner, column)) =
            names.column_at(uid, pbps_model::plan_names::At::Start)
            && let Some(table) = names.table_at(owner, pbps_model::plan_names::At::Start)
        {
            let column = table.column(column);
            if matches!(&step.change, pbps_model::Change::DropColumn { .. }) {
                pbps_pg::modules::policies_on_column_removal(conn, &column).await?
            } else {
                pbps_pg::modules::policies_on_column(conn, &column).await?
            }
        } else {
            // A newly created column has no old catalog bindings.
            Vec::new()
        };
        if matches!(&step.change, pbps_model::Change::RenameColumn { .. }) {
            // A rename carries unmanaged policies cleanly; only a modeled
            // policy's changed deparse fails the typed closing guard. Drops
            // and retypes still need every physical catalog dependency.
            policies.retain(|policy| policy.is_declared_in(managed));
        }
        let all = found.entry(uid.clone()).or_default();
        for policy in policies {
            if !all.contains(&policy) {
                all.push(policy);
            }
        }
    }
    Ok(found)
}

/// Apply validates the approved order against fresh catalog bindings. It
/// never schedules a newly discovered dependent into the saved plan.
async fn refuse_policy_dependents<D: ?Sized>(
    conn: &mut Conn,
    plan: &(dyn pbps_diff::order::PlanView<D> + 'static),
    ids: &pbps_model::IdsFile,
    managed: &pbps_model::Schema,
) -> anyhow::Result<()> {
    if conn.driver() == Driver::Postgres {
        let changes = pbps_dialect::PlanPayload::changes(plan);
        let found = policy_dependents(conn, changes, ids, managed).await?;
        crate::row_security::check_policy_releases(plan, ids, &found)?;
    }
    Ok(())
}

/// Read prerequisites before probes can rely on a referenced type.
pub async fn seal_type_pins(
    conn: &mut Conn,
    schemas: &[&Schema],
    changes: &ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
    pin_routines: bool,
) -> anyhow::Result<Vec<pbps_model::TypePin>> {
    match conn.driver() {
        Driver::Postgres => {
            let pins = pbps_pg::type_refs::seal(conn, schemas, changes, trusted).await?;
            type_input_routines(conn, &pins, trusted, pin_routines, false).await?;
            Ok(pins)
        }
        Driver::Mssql if trusted.is_empty() => Ok(Vec::new()),
        Driver::Mssql => anyhow::bail!("trusted_roles is supported only on PostgreSQL"),
    }
}

/// Lifecycle pin collection must retain each reference's side-specific identity.
pub async fn seal_plan_type_pins(
    conn: &mut Conn,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    changes: &ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
    pin_routines: bool,
) -> anyhow::Result<Vec<pbps_model::TypePin>> {
    match conn.driver() {
        Driver::Postgres => {
            let pins =
                pbps_pg::type_refs::seal_sides(conn, base, desired, changes, trusted).await?;
            type_input_routines(conn, &pins, trusted, pin_routines, false).await?;
            Ok(pins)
        }
        Driver::Mssql if trusted.is_empty() => Ok(Vec::new()),
        Driver::Mssql => anyhow::bail!("trusted_roles is supported only on PostgreSQL"),
    }
}

/// Requirements already classified as external cannot borrow a lifecycle name.
fn require_type_pins(
    pins: &[pbps_model::TypePin],
    required: &BTreeSet<pbps_model::TypeRef>,
) -> anyhow::Result<()> {
    for reference in required {
        if !pins
            .iter()
            .any(|pin| pin.schema == reference.schema() && pin.name == reference.name())
        {
            anyhow::bail!(
                "plan has no type pin for `{reference}`; plan again against the database",
                reference = reference.shown(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod type_pin_identity_tests {
    use super::*;

    #[test]
    fn every_external_identity_requires_its_own_pin() {
        let required = ["app.mood".parse().unwrap()].into();
        assert!(require_type_pins(&[], &required).is_err());
        let pin = pbps_model::TypePin {
            schema: "app".into(),
            name: "mood".into(),
            oid: 1,
            kind: "enum".into(),
            digest: "ab".repeat(32),
        };
        require_type_pins(std::slice::from_ref(&pin), &required).unwrap();
        let other = pbps_model::TypePin {
            name: "other".into(),
            ..pin
        };
        assert!(require_type_pins(&[other], &required).is_err());
        require_type_pins(&[], &BTreeSet::new()).unwrap();
    }
}

/// Refuse changed type prerequisites and owner trust before replay proceeds.
pub async fn check_type_pins(
    conn: &mut Conn,
    plan: &pbps_model::SavedPlan,
    trusted: &BTreeSet<pbps_model::RoleName>,
    required: &BTreeSet<pbps_model::TypeRef>,
    within_transaction: bool,
) -> anyhow::Result<()> {
    if &plan.trusted_roles != trusted {
        anyhow::bail!("trusted_roles changed since planning; plan again against the database");
    }
    require_type_pins(&plan.type_pins, required)?;
    match conn.driver() {
        Driver::Postgres => {
            let within = if within_transaction {
                pbps_pg::pins::Within::CallersTransaction
            } else {
                pbps_pg::pins::Within::OwnTransaction
            };
            pbps_pg::type_refs::trusted_roles(conn, trusted).await?;
            pbps_pg::type_refs::check(
                conn,
                within,
                &plan.type_pins,
                &plan.declared_type_pins,
                &plan.changes,
            )
            .await?;
        }
        Driver::Mssql if plan.type_pins.is_empty() && trusted.is_empty() => {}
        Driver::Mssql => {
            anyhow::bail!("this SQL Server plan carries PostgreSQL type prerequisites")
        }
    }
    Ok(())
}

/// Read the potential path of every pinned type, including unchanged columns.
pub async fn type_input_routines(
    conn: &mut Conn,
    pins: &[pbps_model::TypePin],
    trusted: &BTreeSet<pbps_model::RoleName>,
    pin_routines: bool,
    inside: bool,
) -> anyhow::Result<BTreeSet<i64>> {
    match conn.driver() {
        Driver::Postgres => {
            let references = pins
                .iter()
                .map(|p| pbps_model::TypeRef::new(&p.schema, &p.name))
                .collect::<Result<Vec<_>, _>>()?;
            let within = if inside {
                pbps_pg::pins::Within::CallersTransaction
            } else {
                pbps_pg::pins::Within::OwnTransaction
            };
            Ok(
                pbps_pg::type_refs::input_routines(
                    conn,
                    within,
                    &references,
                    trusted,
                    pin_routines,
                )
                .await?,
            )
        }
        Driver::Mssql if !pin_routines && pins.is_empty() => Ok(BTreeSet::new()),
        Driver::Mssql => anyhow::bail!("pin_type_routines is supported only on PostgreSQL"),
    }
}

/// Keep bootstrap's prerequisite reads valid through execution and recording.
pub async fn recheck_type_prerequisites(
    conn: &mut Conn,
    expected: &[pbps_model::TypePin],
    changes: &ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
    inside: bool,
) -> anyhow::Result<()> {
    match conn.driver() {
        Driver::Postgres => {
            let within = if inside {
                pbps_pg::pins::Within::CallersTransaction
            } else {
                pbps_pg::pins::Within::OwnTransaction
            };
            pbps_pg::type_refs::check(conn, within, expected, &[], changes).await?;
            type_input_routines(conn, expected, trusted, false, inside).await?;
        }
        Driver::Mssql if expected.is_empty() => {}
        Driver::Mssql => anyhow::bail!("SQL Server does not support type references"),
    }
    Ok(())
}

/// Column references in a recorded or declared schema.
pub fn type_references(schema: &Schema) -> BTreeSet<pbps_model::TypeRef> {
    pbps_pg::type_refs::references(&[schema], &ChangeSet::default())
}

#[cfg(test)]
mod removal_note_tests {
    use super::*;

    #[test]
    fn scan_check_removal_notes_preserve_ledger_data_and_show_names_once() {
        for name in [
            "check\n\u{1b}[31m\u{9b}\\\u{202e}名前",
            r"check\n",
            "名前_ünï❤️",
        ] {
            let check = pbps_dialect::ScanCheck {
                table: TableName::new("app", name),
                name: name.into(),
                step: pbps_dialect::ScanCheckStep::Add,
            };
            let note = scan_check_removal_note(&check);
            let expected = format!("the staged attach's temporary CHECK `{name}` on `app.{name}`");
            assert_eq!(note, expected);
            let mut snapshot = pbps_model::StateSnapshot::new(
                pbps_model::StateKind::Baseline,
                Default::default(),
                Default::default(),
                "operator",
            );
            snapshot.reason = Some(format!("adopt (first removed {note})").into());
            assert_eq!(
                serde_json::to_value(&snapshot).unwrap()["reason"],
                format!("adopt (first removed {expected})")
            );
            let shown = pbps_model::escape::Escaped(name).to_string();
            assert_eq!(
                pbps_model::escape::Escaped(&note).to_string(),
                format!("the staged attach's temporary CHECK `{shown}` on `app.{shown}`")
            );
        }
    }
}
