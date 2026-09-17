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

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::catalog::{CatalogNames, Pulled, RowsError, Spellings};
use pbps_db::doctor::Ask;
use pbps_db::impact::{ImpactError, ImpactReport, RenameTarget};
use pbps_db::{Conn, DbError, Driver, LedgerEntry, LedgerError, LockInfo, TimelineEntry};
use pbps_model::{ChangeSet, IdsFile, ObservedRows, RowScope, Schema, StateSnapshot, TableName};

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

/// Catalog estimates are advisory. Failure to measure is an unavailable
/// answer, not a reason to refuse a valid plan (ADR-0012 §3, DECISIONS 430).
pub async fn operational_cost(conn: &mut Conn, cs: &ChangeSet) -> crate::cost::CostReport {
    use crate::cost::{ChangeCost, CostReport, Rows};
    match conn.driver() {
        Driver::Mssql => {
            use pbps_mssql::estimate as ms;
            let mut estimates = ms::planned_estimates(cs).into_iter().peekable();
            if estimates.peek().is_none() && !cs.changes.is_empty() {
                return CostReport::Unavailable {
                    engine: "sqlserver",
                    reason: "operational_cost currently measures SQL Server column type and nullability changes; this plan contains neither".to_owned(),
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
                    table: e.table.to_string(),
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
            let mut estimates = pg::planned_estimates(cs).into_iter().peekable();
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
                    table: e.table.to_string(),
                    rewrite: e.rewrite.into(),
                    reads: e.reads.into(),
                    lock: e.lock.to_string(),
                    blocks: e.lock.blocks().to_owned(),
                    also_locks: e.also_locks.iter().map(ToString::to_string).collect(),
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

pub async fn latest(conn: &mut Conn) -> Result<Option<LedgerEntry>, LedgerError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::latest(conn).await,
        Driver::Postgres => pbps_pg::state::latest(conn).await,
    }
}

pub async fn timeline(conn: &mut Conn, limit: u32) -> Result<Vec<TimelineEntry>, LedgerError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::timeline(conn, limit).await,
        Driver::Postgres => pbps_pg::state::timeline(conn, limit).await,
    }
}

pub async fn record(conn: &mut Conn, snapshot: &StateSnapshot) -> Result<i64, LedgerError> {
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

pub async fn lock(conn: &mut Conn, holder: &str) -> Result<(), LedgerError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::state::lock(conn, holder).await,
        Driver::Postgres => pbps_pg::state::lock(conn, holder).await,
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
) -> Result<Spellings, RowsError> {
    match conn.driver() {
        Driver::Mssql => pbps_mssql::catalog::misspelt(conn, schema, at).await,
        Driver::Postgres => pbps_pg::catalog::misspelt(conn, schema, at).await,
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
pub async fn role_members(conn: &mut Conn) -> anyhow::Result<BTreeMap<String, Vec<String>>> {
    match conn.driver() {
        Driver::Mssql => Ok(pbps_mssql::catalog::role_members(conn).await?),
        Driver::Postgres => Err(roles_are_the_clusters("reading the role memberships")),
    }
}

/// Every securable each user-defined role owns, which the engine will not
/// drop the role over.
pub async fn role_owned_securables(
    conn: &mut Conn,
) -> anyhow::Result<BTreeMap<String, Vec<String>>> {
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
) -> anyhow::Result<ConnectedCheck> {
    match conn.driver() {
        Driver::Mssql => {
            let reports = pbps_mssql::impact::key_drop_blockers(conn, changes).await?;
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
            let reports = pbps_pg::impact::drop_blockers(conn, changes).await?;
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
pub async fn permission_support(
    conn: &mut Conn,
    changes: &ChangeSet,
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
            let version = pbps_pg::roles::server_version_num(conn).await?;
            let mut roles: BTreeMap<String, pbps_model::Role> = BTreeMap::new();
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
            Ok(Permissions {
                gaps: pbps_mssql::doctor::missing(&held)
                    .into_iter()
                    .map(|g| PermissionGap {
                        permission: g.permission,
                        securable: g.securable(),
                        why: g.why.into(),
                    })
                    .collect(),
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
                absent_schemas: held.absent_schemas,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// What a rename touches (SPEC §7.4)
// ---------------------------------------------------------------------------

/// Every rename in a plan, as the objects they are renamed *from* — the name
/// the catalog still knows them by. Which changes count is each engine's
/// question: a module rename is a drop plus a create, and only SQL Server asks
/// the drop side through the rename-impact query.
pub fn rename_targets(driver: Driver, changes: &ChangeSet) -> Vec<RenameTarget> {
    match driver {
        Driver::Mssql => pbps_mssql::impact::rename_targets(changes),
        Driver::Postgres => pbps_pg::impact::rename_targets(changes),
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
pub fn refuse_missing_cluster_roles(driver: Driver, missing: &[String]) -> anyhow::Result<()> {
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
    pub renames: BTreeMap<String, String>,
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
    renames: &BTreeMap<String, String>,
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
                    anyhow::bail!("missing_roles (PostgreSQL): declared cluster role `{to}` is missing; create or rename it first, then plan again");
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
            if let pbps_model::Change::RevokePublicExecute { routine, .. } = &p.change {
                Some(routine.to_string())
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
                    eprintln!("warning: {id}: {reason}");
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
            // The plan's own name for a table, in the spelling the catalog
            // still holds: the guard runs before the renames (DECISIONS 445).
            let stored = |table: &pbps_model::TableName| {
                planned_ids
                    .table_uid(table)
                    .or_else(|| baseline.ids.table_uid(table))
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
                                planned_ids
                                    .column_uid(&column)
                                    .or_else(|| baseline.ids.column_uid(&column))
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

#[cfg(test)]
mod tests {
    use super::*;

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
        cs.changes
            .push(PlannedChange::new(Change::RevokePublicExecute {
                routine: "app.f(integer)".parse().unwrap(),
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
                    },
                )],
            };
            let report = operational_cost(&mut conn, &cs).await;
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
}
