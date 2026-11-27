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
//! The operations every engine must provide to plan and apply (the ledger and
//! the lock, the catalog read, staged execution) are the exception: they are
//! the methods of `pbps_db::engine::Engine`, chosen once by [`of`], and the
//! functions for them delegate (#2798).
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
pub use pbps_db::doctor::{Capabilities, Permissions};
use pbps_db::impact::{ImpactError, ImpactReport, RenameTarget};
use pbps_db::{Conn, DbError, Driver, LedgerEntry, LedgerError, LockInfo, TimelineEntry};
use pbps_model::{ChangeSet, IdsFile, ObservedRows, RowScope, Schema, StateSnapshot, TableName};

/// The engine a connection over `driver` is to, for the operations every
/// engine must provide (`pbps_db::engine::Engine`).
///
/// The one `match` for those operations; the functions below delegate through
/// it, so a third engine is a compile error here and nowhere else for them
/// (#2798). The provided methods (`doctor`'s questions and plan target
/// assertions) have a stated default there, so a third engine answers each in
/// its own `impl` and is not named by `doctor.rs` or `plan_assert.rs`.
/// Operations only some other engines have still match on the driver
/// themselves, until they move behind the trait too.
pub(crate) fn of(driver: Driver) -> &'static dyn pbps_db::engine::Engine {
    static MSSQL: pbps_mssql::engine::MssqlEngine = pbps_mssql::engine::MssqlEngine;
    static POSTGRES: pbps_pg::engine::PostgresEngine = pbps_pg::engine::PostgresEngine;
    static SQLITE: pbps_sqlite::engine::SqliteEngine = pbps_sqlite::engine::SqliteEngine;
    match driver {
        Driver::Mssql => &MSSQL,
        Driver::Postgres => &POSTGRES,
        Driver::Sqlite => &SQLITE,
    }
}

/// The owned-test support of the engine `driver` names, or the refusal that
/// names the engine where it has none.
///
/// The one selection for `pbps test`: a command asks once, before it
/// reconciles records, discovers a runtime or writes a record, and carries the
/// answer through its phases (#2798).
pub(crate) fn require_owned_test(driver: Driver) -> anyhow::Result<&'static OwnedTest> {
    owned_test::require(of(driver).name(), profile::profile(driver).owned_test)
}

/// The support record of an engine that has one, for a test that has no
/// manifest to admit it from.
#[cfg(test)]
pub(crate) fn owned_test_of(driver: Driver) -> &'static OwnedTest {
    require_owned_test(driver).expect("both engines own a test scratch")
}

#[cfg(test)]
mod of_tests {
    use super::*;

    #[test]
    fn of_answers_with_the_engine_of_the_driver_asked_for() {
        for driver in [Driver::Mssql, Driver::Postgres, Driver::Sqlite] {
            assert_eq!(of(driver).driver(), driver);
        }
    }

    #[test]
    fn the_engines_are_told_apart_by_name() {
        let names = [
            of(Driver::Mssql).name(),
            of(Driver::Postgres).name(),
            of(Driver::Sqlite).name(),
        ];
        assert_eq!(
            names.iter().copied().collect::<BTreeSet<_>>().len(),
            names.len(),
            "error messages name the engine, so two engines sharing a name would blur them"
        );
    }

    #[test]
    fn engine_dialect_matches_each_dialects_wire_spelling() {
        use pbps_dialect::Dialect as _;
        assert_eq!(of(Driver::Mssql).dialect(), pbps_mssql::Mssql.name());
        assert_eq!(
            of(Driver::Postgres).dialect(),
            pbps_pg::Postgres::default().name()
        );
        assert_eq!(
            of(Driver::Sqlite).dialect(),
            pbps_sqlite::dialect::Sqlite::default().name()
        );
        // Negative: no two engines share a name, or the check could not tell them apart.
        let names = [Driver::Mssql, Driver::Postgres, Driver::Sqlite].map(|d| of(d).dialect());
        assert_eq!(
            names
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            names.len()
        );
    }
}

// What each engine is where the answer is a value, not a connected operation:
// one `Profile` per engine, read by the functions below that need no connection
// (#2798).
mod profile;
pub(crate) use profile::CommandFeature;
// What `pbps test` needs of an engine that owns its scratch server, where the
// answer is a value; selected by `require_owned_test` (#2798).
mod owned_test;
pub(crate) use owned_test::OwnedTest;
// The engine's steps of a connected plan, reached at named positions (#2798).
pub(crate) mod apply_session;
pub(crate) use apply_session::ApplySession;
mod plan_session;
// The PostgreSQL bodies that live tests ask of a bare change set; apply and
// planning reach them through their sessions.
#[cfg(test)]
pub(crate) use plan_session::postgres::{
    account_for_module_dependents, check_drop_blockers, check_module_dependents_fixture,
};
pub(crate) use plan_session::{
    DeclaredTypes, DomainCode, ExtensionSeals, Frame, PlanSession, SealedFacts, ServerLimits,
    SupportReads, SupportRows, in_contract_order,
};

/// Execute one staged DDL statement, including recovery owned by its engine.
pub async fn execute_staged_statement(
    conn: &mut Conn,
    statement: &pbps_dialect::Statement,
) -> Result<(), DbError> {
    of(conn.driver())
        .execute_staged_statement(conn, statement)
        .await
}

/// Judges the table expressions a change introduces (DEC-2563.4).
pub async fn vet_table_expressions(
    conn: &mut Conn,
    admission: &pbps_dialect::routines::RoutineAdmission,
    within: pbps_db::TransactionOwner,
    moment: pbps_pg::table_expressions::Moment,
    change: &pbps_model::Change,
) -> Result<pbps_pg::table_expressions::Vetted, String> {
    pbps_pg::table_expressions::vet_change(conn, admission, within, moment, change).await
}

pub async fn vet_drop_requirements<S: ApplySession>(
    session: &S,
    conn: &mut Conn,
    admission: &pbps_dialect::routines::RoutineAdmission,
    names: &pbps_model::plan_names::PlanNames<'_>,
) -> anyhow::Result<()> {
    for (index, step) in names.changes().iter().enumerate() {
        let Some(guard) = &step.drop_guard else {
            continue;
        };
        // The UID survives renames and address reuse. Never fall back to an
        // address when the target has no stored evidence for this identity.
        let table = names
            .table_at(&guard.table_uid, pbps_model::plan_names::At::Start)
            .ok_or_else(|| {
                anyhow::anyhow!("the requirement's table identity has no stored target")
            })?;
        for requirement in guard.requirements.values() {
            let columns = session
                .requirement(conn, admission, table, &requirement.predicate)
                .await?;
            pbps_diff::drop_requirements::check_columns(guard, index, names, &columns)
                .map_err(anyhow::Error::msg)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The ledger and the lock (SPEC §8.1)
// ---------------------------------------------------------------------------

pub async fn is_initialized(conn: &mut Conn) -> Result<bool, DbError> {
    of(conn.driver()).is_initialized(conn).await
}

// This value is recorded inside opaque StateSnapshot.reason and participates in
// its original length/truncation check. Preserve the data; stdout shows it once.
pub(crate) fn scan_check_removal_note(check: &pbps_dialect::ScanCheck) -> String {
    format!(
        "the staged attach's temporary CHECK `{}` on `{}`",
        check.name.raw(),
        check.table.raw()
    )
}

pub async fn latest(conn: &mut Conn) -> Result<Option<LedgerEntry>, LedgerError> {
    of(conn.driver()).latest(conn).await
}

pub async fn deployed_revision(
    conn: &mut Conn,
    observed_id: i64,
) -> Result<Option<pbps_db::ledger::StateRevision>, LedgerError> {
    of(conn.driver()).deployed_revision(conn, observed_id).await
}

pub async fn has_applied_plan(conn: &mut Conn, checksum: &str) -> Result<bool, LedgerError> {
    of(conn.driver()).has_applied_plan(conn, checksum).await
}

pub async fn entry(
    conn: &mut Conn,
    id: Option<i64>,
) -> Result<Option<pbps_db::ledger::StoredEntry>, LedgerError> {
    of(conn.driver()).entry(conn, id).await
}

pub async fn timeline(conn: &mut Conn, limit: u32) -> Result<Vec<TimelineEntry>, LedgerError> {
    of(conn.driver()).timeline(conn, limit).await
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
    of(conn.driver()).record(conn, snapshot).await
}

pub async fn prune(conn: &mut Conn, keep: u32) -> Result<u64, LedgerError> {
    of(conn.driver()).prune(conn, keep).await
}

/// Takes the lock for `operator`, recording which process holds it (#1188).
///
/// The row names the person and also this process: its host, pid, CI job and
/// the application name its session carries. That is what `pbps unlock` and a
/// "locked by" refusal give an operator to check before overriding a lock
/// that, by design, never expires (DECISIONS 285, DEC-1188.1).
pub async fn lock(conn: &mut Conn, operator: &str) -> Result<(), LedgerError> {
    let engine = of(conn.driver());
    let application_name = engine.session_application_name(conn).await?;
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
    if let Some(cleared) = engine.lock(conn, &holder).await? {
        // The one line the run owes whoever reads its output: a lock was
        // there, its holder was proved gone, and this run removed it.
        eprintln!("{}", cleared.cleared_notice());
    }
    Ok(())
}

pub async fn unlock(conn: &mut Conn) -> Result<bool, DbError> {
    of(conn.driver()).unlock(conn).await
}

pub async fn lock_holder(conn: &mut Conn) -> Result<Option<LockInfo>, DbError> {
    of(conn.driver()).lock_holder(conn).await
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
            // Creation reports include several server verdicts; their operator
            // rendering must never become a durable ledger frame (DEC-2465.2).
            use pbps_pg::resolver::reconstruct::{ReconstructError, refusal::Refusal};
            if frame.is::<pbps_cli::resolver::server::CreationFailure>()
                || frame.is::<std::sync::Arc<pbps_cli::resolver::server::CreationFailure>>()
                || frame.is::<Refusal>()
                || frame.is::<std::sync::Arc<Refusal>>()
                || matches!(
                    frame.downcast_ref::<ReconstructError>(),
                    Some(ReconstructError::Refused(_))
                )
                || matches!(
                    frame.downcast_ref::<pbps_cli::resolver::server::ProduceError>(),
                    Some(pbps_cli::resolver::server::ProduceError::Run(
                        pbps_cli::resolver::server::Error::Creation(_)
                    ))
                )
                || matches!(
                    frame.downcast_ref::<pbps_cli::resolver::server::Error>(),
                    Some(pbps_cli::resolver::server::Error::Creation(_))
                )
            {
                return "scratch creation was refused; transient diagnostics are not recorded here"
                    .into();
            }
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
    (profile::profile(driver).truncate_reason)(text)
}

/// Refuses drafting a resolver's externals file from a target whose engine
/// has no binding adapter.
///
/// PostgreSQL has one; SQL Server does not yet (#619, #620).
pub(crate) fn refuse_externals_draft_unsupported(driver: Driver) -> anyhow::Result<()> {
    match profile::profile(driver).externals_draft_refusal {
        None => Ok(()),
        Some(refusal) => anyhow::bail!(refusal),
    }
}

/// Whether the rehearsal's scratch session has already compiled the namespace
/// of the base by the time the plan is lowered.
///
/// SQL Server's does, with the engine's trials; what is left of it is who may
/// do what. PostgreSQL's compiles the base's statements later, from the
/// lowering.
///
/// It is also what decides whether the scratch is handed a bootstrap of the
/// base: the trials are what take one, so a scratch without them needs none.
pub(crate) fn scratch_compiles_base_namespace(driver: Driver) -> bool {
    profile::profile(driver).scratch_compiles_base_namespace
}

/// How a target's refused or failed read of its engine lineage identity is
/// reported.
///
/// PostgreSQL's refusal is the deployment role's to fix and names the grant;
/// SQL Server's `family_guid` read has no such remedy and is unanswerable.
pub(crate) fn target_identity_unread(driver: Driver, error: DbError) -> crate::resolution::Refused {
    (profile::profile(driver).target_identity_unread)(error)
}

/// Whether the resolver's scratch may share the target's instance, which
/// omission never grants consent to; inapplicability stays distinct from
/// refusal.
///
/// SQL Server's scratch can land on the instance it rehearses; PostgreSQL's
/// scratch is a cluster of its own, so the question does not apply.
pub(crate) fn same_instance_consent(
    driver: Driver,
    profile: &pbps_config::resolver::ResolverProfile,
) -> pbps_model::resolver::SameInstance {
    use pbps_config::resolver::ResolverProfile;
    if !self::profile::profile(driver).scratch_may_share_the_instance {
        return pbps_model::resolver::SameInstance::NotApplicable;
    }
    let choice = match profile {
        ResolverProfile::Server { same_instance, .. } => same_instance.unwrap_or_default(),
        ResolverProfile::Docker { .. } => pbps_config::resolver::SameInstance::Refuse,
    };
    match choice {
        pbps_config::resolver::SameInstance::Refuse => pbps_model::resolver::SameInstance::Refused,
        pbps_config::resolver::SameInstance::Allow => pbps_model::resolver::SameInstance::Allowed,
    }
}

/// Reads what the target assertions are held against: the planning target's
/// own identity, which an assertion login must match.
///
/// An engine's error stops planning with the same finding for every engine,
/// and an engine without assertions is refused by name (#2798).
pub(crate) async fn read_assertion_target(
    planning: &mut Conn,
) -> Result<Box<dyn pbps_db::engine::AssertionTarget>, crate::plan_assert::Stop> {
    use crate::plan_assert::stop;
    use pbps_db::engine::TargetAssertions;
    let engine = of(planning.driver());
    match engine
        .read_assertion_target(planning)
        .await
        .map_err(|e| stop("the planning target", e))?
    {
        TargetAssertions::Supported(target) => Ok(target),
        TargetAssertions::Unsupported => Err(crate::plan_assert::unsupported(engine.name())),
    }
}

// ---------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------

// The enum moved to `pbps-db` with the trait whose methods take it; callers
// keep spelling it `engine::Read`.
pub use pbps_db::engine::Read;

/// A PostgreSQL closing catalog view is one statement, while declared rows
/// are read separately. Revalidate their managed projection before recording.
pub fn needs_readback_revalidation(driver: Driver, read: Read) -> bool {
    profile::profile(driver).revalidates_read_inside_own_transaction
        && matches!(read, Read::InsideOwnTransaction)
}

pub async fn introspect(conn: &mut Conn, read: Read) -> Result<Pulled, DbError> {
    of(conn.driver()).introspect(conn, read).await
}

/// One advisory per declared filegroup, for a plan made without a database
/// (#2813, DEC-2813.1). The name is accepted as a name: whether the target
/// holds it is a fact about a database, and `plan --db` or `bootstrap` refuses
/// one it does not, by name. PostgreSQL declares none (`validate` refuses it).
pub fn unchecked_filegroups(schema: &Schema) -> Vec<String> {
    let mut by_name: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (what, fg, _) in plan_session::mssql::declared_filegroups(schema) {
        by_name
            .entry(fg.as_str().to_ascii_lowercase())
            .or_default()
            .push(format!(
                "{what} (`{}`)",
                pbps_model::escape::Escaped(fg.as_str())
            ));
    }
    by_name
        .into_values()
        .map(|uses| {
            format!(
                "{}: the filegroup is not checked offline; pbps never creates one, so it must \
                 exist in the database, and a plan against the database (`plan --db`) refuses \
                 it by name when it does not",
                uses.join(", ")
            )
        })
        .collect()
}

/// The refusal the problems of the capability question make, if there are any
/// (`ApplySession::refuse_unsupported_temporal` and `plan --db` both end on it).
pub fn temporal_refusal(problems: &[String]) -> Option<String> {
    (!problems.is_empty()).then(|| {
        format!(
            "this server cannot take the plan's system-versioned tables as declared:\n  {}",
            problems.join("\n  ")
        )
    })
}

/// Ends a connected plan on what the capability question answered, if it
/// refused anything. The question was asked and answered, so a JSON plan
/// reports the refusal as a finding with exit 2, like the online edition's
/// (DECISIONS 485, #574), rather than as `plan.failed` with exit 1 (#1630).
/// A failure to ask never reaches here: it is the question's own error.
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
    of(conn.driver())
        .read_rows(conn, schema, scopes, read)
        .await
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
        )?
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
        )?;
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
        use pbps_model::TypeChange;
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
            changes: vec![PlannedChange::new(Change::Type(TypeChange::RenameType {
                uid,
                from,
                to,
                definition: Box::new(definition),
            }))],
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

/// Why the SQL Server absence proof refused. The source error is kept as one,
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

// ---------------------------------------------------------------------------
// The server: what it is and what it can do
// ---------------------------------------------------------------------------

/// Advisory target observations; never acquires or contacts a resolver.
pub async fn resolver_discovery(conn: &mut Conn) -> Result<pbps_db::resolver::Discovery, DbError> {
    of(conn.driver()).resolver_discovery(conn).await
}

/// The server's version, as text an operator recognises.
pub async fn server_version(conn: &mut Conn) -> Result<String, DbError> {
    of(conn.driver()).server_version(conn).await
}

/// Reads the server's capabilities. `version` is what [`server_version`]
/// answered, where it answered: on SQL Server the version and the edition
/// together decide `supports_create_or_alter` (Azure reports 12.0.x and
/// supports the syntax regardless), and a version that could not be read
/// leaves that one `None` rather than guessed.
pub async fn capabilities(conn: &mut Conn, version: Option<&str>) -> Result<Capabilities, DbError> {
    of(conn.driver()).capabilities(conn, version).await
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

/// A connected capability answer, separate from the saved, checksum-pinned plan.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct ConnectedCheck {
    pub name: &'static str,
    pub engine: &'static str,
    pub status: &'static str,
    pub message: String,
}

/// Whether the plan gives a domain a check or NOT NULL it did not have: the
/// change whose scan `check_domain_alterations` reports.
pub(in crate::engine) fn tightens_a_domain(changes: &ChangeSet) -> bool {
    changes.changes.iter().any(|p| {
        matches!(
            p.change,
            pbps_model::Change::TypeValue(pbps_model::TypeValueChange::AlterDomain {
                half: pbps_model::DomainHalf::Tighten,
                ..
            })
        )
    })
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
        matches!(&p.change, pbps_model::Change::Column(pbps_model::ColumnChange::AlterColumnExpression { column, to, .. })
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
    use pbps_model::ColumnChange;
    use pbps_model::ModuleChange;
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
                Change::Module(ModuleChange::DropModule { id: pbps_model::ModuleId::Trigger { on, .. }, .. })
                | Change::Module(ModuleChange::AlterModule { id: pbps_model::ModuleId::Trigger { on, .. }, .. })
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
                Change::Column(ColumnChange::AlterColumnExpression { column: c, to, .. }) => {
                    (c.clone(), to, false)
                }
                Change::Column(ColumnChange::AddColumn {
                    table: t,
                    name,
                    column: added,
                    ..
                }) => match &added.value_source {
                    pbps_model::ValueSource::Generated(generated)
                        if matches!(p.change, Change::Column(ColumnChange::DropColumn { .. })) =>
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
            if matches!(
                p.change,
                Change::Column(ColumnChange::AlterColumnType { .. })
            ) {
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
        if let Change::Column(ColumnChange::AlterColumnType { uid, .. }) = &p.change {
            for (j, q) in changes.changes.iter().enumerate() {
                if matches!(&q.change,
                    Change::Column(ColumnChange::AlterColumnDefault { uid: u, to: Some(_), .. }) if u == uid)
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
        matches!(&p.change, pbps_model::Change::Column(pbps_model::ColumnChange::AddColumn { table, name, .. })
            if *table == column.table && *name == column.name)
            || matches!(&p.change, pbps_model::Change::Column(pbps_model::ColumnChange::RenameColumn { table, to, .. })
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
    if matches!(
        change,
        pbps_model::Change::Column(pbps_model::ColumnChange::DropColumn { .. })
    ) && let Some(&from) = required.get(&(table, live))
    {
        for (i, _) in later().filter(|(i, c)| {
            matches!(c, pbps_model::Change::Row(pbps_model::RowChange::InsertRow { table, .. }) if *table == column.table)
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
        matches!(c, pbps_model::Change::Column(pbps_model::ColumnChange::AddColumn { table, name, .. })
            if *table == column.table && *name == column.name)
            || matches!(c, pbps_model::Change::Column(pbps_model::ColumnChange::RenameColumn { table, to, .. })
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
    if matches!(
        change,
        pbps_model::Change::Column(pbps_model::ColumnChange::AlterColumnType { .. })
    ) && let Some((i, past)) = later().find(|(_, c)| after_the_alterations(c))
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
        pbps_model::Change::Row(_)
            | pbps_model::Change::Constraint(pbps_model::ConstraintChange::SetPrimaryKey {
                to: Some(_),
                ..
            })
            | pbps_model::Change::Constraint(pbps_model::ConstraintChange::AddUnique { .. })
            | pbps_model::Change::Constraint(pbps_model::ConstraintChange::AddForeignKey { .. })
            | pbps_model::Change::Constraint(pbps_model::ConstraintChange::AddCheck { .. })
            | pbps_model::Change::Index(pbps_model::IndexChange::AddIndex { .. })
            | pbps_model::Change::Module(pbps_model::ModuleChange::CreateModule { .. })
            | pbps_model::Change::Module(pbps_model::ModuleChange::AlterModule { .. })
            | pbps_model::Change::TypeValue(pbps_model::TypeValueChange::AddEnumValue { .. })
            | pbps_model::Change::TypeValue(pbps_model::TypeValueChange::RenameEnumValue { .. })
            | pbps_model::Change::TypeValue(pbps_model::TypeValueChange::EnumRebuild { .. })
            | pbps_model::Change::Type(_)
            | pbps_model::Change::Role(pbps_model::RoleChange::CreateRole { .. })
            | pbps_model::Change::Permission(pbps_model::GrantChange::Grant { .. })
            | pbps_model::Change::Permission(pbps_model::GrantChange::PublicExecution { .. })
    )
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
            if let pbps_model::Change::Column(pbps_model::ColumnChange::DropColumn { .. }) =
                &p.change
            {
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

/// Policy recipients need not be declared roles: an existing cluster role may
/// receive a policy without any managed grants. Ask the connected role reader,
/// shared by plan, bootstrap and saved-plan apply, rather than the managed scope.
async fn refuse_missing_policy_roles(conn: &mut Conn, changes: &ChangeSet) -> anyhow::Result<()> {
    let policies: Vec<_> = changes
        .changes
        .iter()
        .filter_map(|step| {
            if let pbps_model::Change::Policy(pbps_model::PolicyChange::CreatePolicy {
                table,
                name,
                policy,
                ..
            }) = &step.change
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
/// question, so it reports none. An engine that answers nothing is an `Err`,
/// which `doctor` reports as unknown, never as a clean ledger.
pub async fn ledger_problems(conn: &mut Conn) -> Result<Vec<String>, DbError> {
    of(conn.driver()).ledger_problems(conn).await
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
    of(conn.driver()).permissions(conn, project_ids, ask).await
}

/// Whether each partitioned parent's partitions reach far enough past its
/// rows, and whether its DEFAULT holds rows of a range not created yet
/// (#1789, DEC-1789.2). Reads existence, never a value, and writes nothing.
///
/// SQL Server has no RANGE partitions to run out of, so the question does not
/// apply and the report is empty; an engine that answers nothing is an `Err`
/// that `doctor` shows as a warning, never as an empty (clean) report.
pub async fn partition_runway(
    conn: &mut Conn,
    ask: &pbps_db::doctor::RunwayAsk<'_>,
    project_ids: &IdsFile,
) -> Result<pbps_db::doctor::RunwayReport, DbError> {
    of(conn.driver())
        .partition_runway(conn, ask, project_ids)
        .await
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
    of(driver)
        .lock_read_gap(&mut conn)
        .await
        .unwrap_or(LockReadGap::Unexplained)
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
    let facts = profile::profile(driver);
    let noun = facts.lock_principal_noun;
    let principal = crate::report::placeholder(facts.lock_principal_placeholder);
    let how = format!(
        "run the remedy as a principal allowed to grant it, with {principal} (quotes \
         included) replaced by this {noun}'s name quoted as an identifier, then rerun doctor"
    );
    let next = "if the lock is still unreadable after that, doctor names the next grant";
    (facts.lock_remedy)(gap, &how, next, &principal)
}

/// What to check when `doctor` could not read the account's permissions, in
/// this engine's words. Advice, not a statement: which read failed is not
/// carried this far, so no single grant is known to be the repair (#822).
pub fn permission_read_advice(driver: Driver) -> String {
    (profile::profile(driver).permission_read_advice)()
}

/// What `doctor` calls the permission subject (`"account"` or `"process"`).
pub const fn permission_subject(driver: Driver) -> &'static str {
    profile::profile(driver).permission_subject
}

/// What `doctor` advises checking when an environment cannot be reached.
pub const fn unreachable_remedy(driver: Driver) -> &'static str {
    profile::profile(driver).unreachable_remedy
}

/// What `doctor` labels the engine's version line (`"server"` or `"library"`).
pub const fn version_label(driver: Driver) -> &'static str {
    profile::profile(driver).version_label
}

/// Whether this engine has a target resolver / owned scratch server (`Profile::owned_test`).
pub const fn has_resolver(driver: Driver) -> bool {
    profile::profile(driver).owned_test.is_some()
}

pub fn unsupported_command_feature(
    driver: Driver,
    feature: CommandFeature,
) -> Option<&'static str> {
    (profile::profile(driver).unsupported_command)(feature)
}

pub fn require_command_feature(driver: Driver, feature: CommandFeature) -> anyhow::Result<()> {
    if let Some(why) = unsupported_command_feature(driver, feature) {
        anyhow::bail!("{why}");
    }
    Ok(())
}

pub const fn preflight_warning_id(driver: Driver) -> &'static str {
    profile::profile(driver).preflight_warning_id
}

pub const fn checks_empty_plan_under_lock(driver: Driver) -> bool {
    profile::profile(driver).checks_empty_plan_under_lock
}

pub const fn connection_is_path(driver: Driver) -> bool {
    profile::profile(driver).connection_is_path
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
    profile::profile(driver).online_depends_on_the_edition
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
    (profile::profile(driver).capabilities_unknown_message)(cause)
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
    (profile::profile(driver).impact_notes)(report)
}

/// Cluster-owned identities move before pbps plans their database grants.
/// SQL Server moves its database roles through the typed statements instead.
pub struct ExternalRoleRenames {
    pub renames: BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
    pub check: ConnectedCheck,
}

/// A replacement may be explicit or synthesized as a drop and create.
/// Ordinary drops intentionally discard the object; they are not rebuilds.
// The complement is intentionally every change that does not write a module.
#[allow(clippy::wildcard_enum_match_arm)]
fn rebuilt_modules(
    changes: &ChangeSet,
) -> BTreeMap<pbps_model::ModuleId, (pbps_model::ModuleKind, pbps_model::ModuleKind)> {
    use pbps_model::Change;
    use pbps_model::ModuleChange;
    let dropped: BTreeMap<_, _> = changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::Module(ModuleChange::DropModule { id, kind }) => Some((id, *kind)),
            _ => None,
        })
        .collect();
    changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::Module(ModuleChange::AlterModule { id, module }) => {
                Some((id.clone(), (module.kind, module.kind)))
            }
            Change::Module(ModuleChange::CreateModule { id, module }) => dropped
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

/// The connection's immutable planning capability, before any expansion.
/// Offline callers retain their ordinary unknown-capability dialect.
#[cfg(test)]
pub async fn planning_dialect(conn: &mut Conn) -> anyhow::Result<pbps_pg::Postgres> {
    Ok(pbps_pg::Postgres::new()
        .with_server_version(pbps_pg::roles::server_version_num(conn).await?))
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

/// What the saved plan must still remove before each module's drop, given the
/// dependents the engine just read: the one refusal both engines share, said in
/// the engine's name.
fn refuse_unaccounted_dependents(
    engine: &str,
    changes: &ChangeSet,
    found: &BTreeMap<pbps_model::ModuleId, Vec<pbps_pg::modules::Dependent>>,
    names: pbps_model::plan_names::PlanNames<'_>,
    stored: &pbps_model::Schema,
    stored_ids: &pbps_model::IdsFile,
) -> anyhow::Result<()> {
    // Match the planner's final relation first; each dependency lookup then
    // spells both ends at the statement's boundary (DEC-1875.1).
    let partitions = crate::dependents::standing_partitions(stored, changes).at(
        &names,
        pbps_model::plan_names::At::Start,
        pbps_model::plan_names::At::End,
    );
    let sources = pbps_model::physical_sources::PhysicalSources::of(names, stored, stored_ids)?;
    let left = crate::dependents::unaccounted(changes, found, &partitions, &sources);
    if !left.is_empty() {
        anyhow::bail!(
            "module_dependents ({engine}): the database now holds dependents this plan does not \
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

/// Artifact readers route by the engine named in the saved file, without a
/// connection or resolver. Connected apply separately checks that engine
/// against its actual target before accepting the artifact.
pub fn validate_plan_analysis(plan: &pbps_model::SavedPlan) -> anyhow::Result<()> {
    plan.validate_analysis()?;
    if let pbps_model::resolver::PlanAnalysis::Resolved(evidence) = &plan.analysis {
        evidence
            .qualification()
            .runtime
            .validate_dependencies(&plan.dialect, plan.dependencies.keys())?;
        match plan.dialect.as_str() {
            "postgres" => pbps_pg::resolver::validate_evidence(evidence)?,
            "mssql" => pbps_mssql::resolver::validate_evidence(evidence)?,
            engine => anyhow::bail!(
                "this build cannot enforce resolver evidence for {}",
                pbps_model::escape::Escaped(engine)
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "engine/mssql_live_tests.rs"]
mod mssql_live_tests;

#[cfg(test)]
mod tests {
    fn order_after_releases(
        changes: &mut ChangeSet,
        dependences: &BTreeMap<TableName, Vec<pbps_pg::generated::Dependence>>,
    ) -> Result<usize, String> {
        let mut ids = crate::plan_fixture::ids(changes);
        if !changes.changes.iter().any(|p| {
            matches!(
                p.change,
                pbps_model::Change::Row(pbps_model::RowChange::InsertRow { .. })
            )
        }) {
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
            if let pbps_model::Change::Module(pbps_model::ModuleChange::AlterModule { id, module }) =
                &p.change
                && module.kind == pbps_model::ModuleKind::Trigger
            {
                // A declared trigger stands at both ends even when the catalog
                // says an earlier rebuild stops filling the omitted input.
                before.modules.insert(id.clone(), (**module).clone());
                after.modules.insert(id.clone(), (**module).clone());
            }
            if let pbps_model::Change::Column(pbps_model::ColumnChange::DropColumn {
                uid,
                column,
            }) = &p.change
            {
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
        use pbps_model::TableChange;
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
            changes: vec![PlannedChange::new(Change::Table(
                TableChange::CreateTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.t".parse().unwrap(),
                    table: Box::new(table),
                    beside_default: None,
                    moves_at_most: None,
                },
            ))],
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
        use pbps_model::ColumnChange;
        use pbps_model::RowChange;
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
                Change::Column(ColumnChange::DropColumn {
                    uid: a,
                    column: table.column("a"),
                }),
                Change::Row(RowChange::InsertRow {
                    table: table.clone(),
                    key_column: "id".into(),
                    identity_key: false,
                    key: pbps_model::RowKey::from("1"),
                    row: pbps_model::Row::default(),
                    defaults: Default::default(),
                    types: Default::default(),
                }),
                Change::Column(ColumnChange::AlterColumnExpression {
                    uid: g,
                    column: table.column("g"),
                    from: "a * 2".into(),
                    to: "b * 2".into(),
                }),
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
                )
                .expect("read the analysis baseline");
                pbps_diff::order::verify(
                    operations.as_slice(),
                    effects.effects().expect("read every step"),
                    &pbps_diff::order::Baseline::of(base, &owners).expect("read the test baseline"),
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
        use pbps_model::ColumnChange;
        use pbps_model::ModuleChange;
        use pbps_model::RowChange;
        use pbps_model::{Change, ColumnRef, PlannedChange};
        let t: TableName = "app.t".parse().unwrap();
        let col = |name: &str| ColumnRef::new(t.clone(), name);
        let uid = |u: &str| -> pbps_model::Uid { u.parse().unwrap() };
        let drop = |name: &str| {
            Change::Column(ColumnChange::DropColumn {
                uid: uid("c_aaaaaa"),
                column: col(name),
            })
        };
        let retype = |name: &str| {
            Change::Column(ColumnChange::AlterColumnType {
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
            })
        };
        let recompute = |to: &str| {
            Change::Column(ColumnChange::AlterColumnExpression {
                uid: uid("c_dddddd"),
                column: col("g"),
                from: "(a * 2)".into(),
                to: to.into(),
            })
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
                    Change::Column(ColumnChange::DropColumn { .. }) => "drop",
                    Change::Column(ColumnChange::AlterColumnType { .. }) => "retype",
                    Change::Column(ColumnChange::AlterColumnExpression { .. }) => "expression",
                    Change::Column(ColumnChange::AddColumn { .. }) => "add",
                    Change::Column(ColumnChange::AlterColumnDefault { .. }) => "default",
                    Change::Column(ColumnChange::RenameColumn { .. }) => "rename",
                    Change::Row(RowChange::UpdateRow { .. }) => "update",
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
        let set_default = |u: &str, name: &str| {
            Change::Column(ColumnChange::AlterColumnDefault {
                uid: uid(u),
                column: col(name),
                from: None,
                to: Some("5000000000".into()),
            })
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
            Change::Column(ColumnChange::AlterColumnDefault { column, .. }) if column.name == "a".into()));
        // Released, the check has nothing left to refuse; unreleased, it
        // does.
        let mut cs = plan(vec![retype("a"), recompute("b * 2")]);
        assert_eq!(unreleased(&cs, &reads_a).len(), 1);
        order_after_releases(&mut cs, &reads_a).unwrap();
        assert!(unreleased(&cs, &reads_a).is_empty());
        // The input renamed to `x` in the same plan, and the new text naming
        // `x`: it still reads the column, whatever the catalog calls it.
        let rename = |from: &str, to: &str| {
            Change::Column(ColumnChange::RenameColumn {
                uid: uid(if from == "a" { "c_bbbbbb" } else { "c_cccccc" }),
                table: t.clone(),
                from: from.into(),
                to: to.into(),
                table_was: None,
            })
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
        let start_reading = Change::Column(ColumnChange::AlterColumnExpression {
            uid: uid("c_gggggg"),
            column: col("h"),
            from: "b".into(),
            to: "a + 1".into(),
        });
        let mut cs = plan(vec![retype("a"), start_reading, recompute("b * 2")]);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(2));
        assert!(
            matches!(&cs.changes[0].change, Change::Column(ColumnChange::AlterColumnExpression { column, .. }) if column.name == "g".into())
                && matches!(
                    cs.changes[1].change,
                    Change::Column(ColumnChange::AlterColumnType { .. })
                )
                && matches!(&cs.changes[2].change, Change::Column(ColumnChange::AlterColumnExpression { column, .. }) if column.name == "h".into()),
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
        let swap = |g: &str, to: &str| {
            Change::Column(ColumnChange::AlterColumnExpression {
                uid: uid(if g == "g" { "c_gggggg" } else { "c_hhhhhh" }),
                column: col(g),
                from: String::new(),
                to: to.into(),
            })
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
        let update = |column: &str| {
            Change::Row(RowChange::UpdateRow {
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
            })
        };
        let mut cs = plan(vec![retype("a"), update("a"), recompute("b * 2")]);
        let refusal = order_after_releases(&mut cs, &reads_a).unwrap_err();
        assert!(refusal.contains("may need the new type"), "{refusal}");
        // A drop moved past a row inserted into its table, as it is when the
        // release follows a function create: a `NOT NULL` input with no
        // default refuses the row while it stands. The row follows the
        // release and drop through an edge now (#2174).
        let insert = |table: &TableName| {
            Change::Row(RowChange::InsertRow {
                table: table.clone(),
                key_column: "id".into(),
                identity_key: false,
                key: pbps_model::RowKey::from("1"),
                row: pbps_model::Row::default(),
                defaults: BTreeMap::new(),
                types: BTreeMap::new(),
            })
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
            Change::Column(ColumnChange::AlterColumnExpression { .. })
        ));
        assert!(matches!(
            cs.changes[1].change,
            Change::Column(ColumnChange::DropColumn { .. })
        ));
        assert!(matches!(
            cs.changes[2].change,
            Change::Row(RowChange::InsertRow { .. })
        ));
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
        let drop_trigger = |table: &str| {
            Change::Module(ModuleChange::DropModule {
                id: trigger_on(table),
                kind: pbps_model::ModuleKind::Trigger,
            })
        };
        let mut cs = plan(vec![drop("a"), insert(&t), recompute("b * 2")]);
        assert_eq!(order_after_releases(&mut cs, &filled_a), Ok(1));
        let alter_trigger = || {
            Change::Module(ModuleChange::AlterModule {
                id: trigger_on("app.t"),
                module: Box::new(pbps_model::Module {
                    kind: pbps_model::ModuleKind::Trigger,
                    description: None,
                    definition: "BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.f()"
                        .into(),
                    options: Default::default(),
                }),
            })
        };
        for change in [drop_trigger("app.t"), alter_trigger()] {
            let mut cs = plan(vec![change, drop("a"), insert(&t), recompute("b * 2")]);
            assert_eq!(order_after_releases(&mut cs, &filled_a), Ok(2));
            assert!(matches!(
                cs.changes[2].change,
                Change::Column(ColumnChange::DropColumn { .. })
            ));
            assert!(matches!(
                cs.changes[3].change,
                Change::Row(RowChange::InsertRow { .. })
            ));
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
            Change::Column(ColumnChange::AddColumn {
                uid: uid("c_eeeeee"),
                table: t.clone(),
                name: "a".into(),
                column: Box::new(pbps_model::Column::new("text".parse().unwrap())),
            }),
            recompute("b * 2"),
        ]);
        let refusal = order_after_releases(&mut cs, &reads_a).unwrap_err();
        assert!(refusal.contains("in a plan of its own first"), "{refusal}");
        // A new reader sorted ahead of the retype it has to follow: held
        // back until the retype has run, which itself waits for its release
        // (DEC-1391.1).
        let reader = || {
            Change::Column(ColumnChange::AlterColumnExpression {
                uid: uid("c_gggggg"),
                column: col("h"),
                from: "b".into(),
                to: "a + 1".into(),
            })
        };
        let mut cs = plan(vec![reader(), retype("a"), recompute("b * 2")]);
        assert_eq!(order_after_releases(&mut cs, &reads_a), Ok(2));
        assert!(
            matches!(&cs.changes[0].change, Change::Column(ColumnChange::AlterColumnExpression { column, .. }) if column.name == "g".into())
                && matches!(
                    cs.changes[1].change,
                    Change::Column(ColumnChange::AlterColumnType { .. })
                )
                && matches!(&cs.changes[2].change, Change::Column(ColumnChange::AlterColumnExpression { column, .. }) if column.name == "h".into()),
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
            Change::Column(ColumnChange::AddColumn {
                uid: uid("c_eeeeee"),
                table: t.clone(),
                name: "a".into(),
                column: Box::new(pbps_model::Column::new("text".parse().unwrap())),
            }),
            reader(),
        ]);
        assert_eq!(order_after_releases(&mut cs, &BTreeMap::new()), Ok(0));
        // #1425: a generated column the plan adds reads its input from the
        // moment it exists, so one reading a dropped column has no order
        // either; the differ would run the drop first and the `ADD COLUMN`
        // would fail. Unless the name goes to a new column it reads instead.
        let generated_h = |expression: &str| {
            Change::Column(ColumnChange::AddColumn {
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
            })
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
            Change::Column(ColumnChange::AddColumn {
                uid: uid("c_eeeeee"),
                table: t.clone(),
                name: "a".into(),
                column: Box::new(pbps_model::Column::new("integer".parse().unwrap())),
            }),
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
            Change::Column(ColumnChange::AddColumn {
                uid: uid("c_eeeeee"),
                table: t.clone(),
                name: "a".into(),
                column: Box::new(pbps_model::Column::new("integer".parse().unwrap())),
            }),
            retype("x"),
            retype("b"),
            swap("g", "b * 2"),
            swap("h", "a + 1"),
        ]);
        order_after_releases(&mut cs, &reads_a_and_b).unwrap();
        let at = |what: &dyn Fn(&Change) -> bool| {
            cs.changes.iter().position(|p| what(&p.change)).unwrap()
        };
        let retyped = |name: &'static str| move |c: &Change| matches!(c, Change::Column(ColumnChange::AlterColumnType { column, .. }) if column.name == name.into());
        let expression = |name: &'static str| move |c: &Change| matches!(c, Change::Column(ColumnChange::AlterColumnExpression { column, .. }) if column.name == name.into());
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
            Change::Column(ColumnChange::AddColumn {
                uid: uid("c_eeeeee"),
                table: t.clone(),
                name: "a".into(),
                column: Box::new(pbps_model::Column::new("integer".parse().unwrap())),
            }),
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
        use pbps_model::ColumnChange;
        use pbps_model::RowChange;
        use pbps_model::{Change, ColumnRef, PlannedChange};
        let t: TableName = "app.t".parse().unwrap();
        let uid = |u: &str| -> pbps_model::Uid { u.parse().unwrap() };
        let retype = PlannedChange::new(Change::Column(ColumnChange::AlterColumnType {
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
        }));
        let release = PlannedChange::new(Change::Column(ColumnChange::AlterColumnExpression {
            uid: uid("c_dddddd"),
            column: ColumnRef::new(t.clone(), "g"),
            from: "(a * 2)".into(),
            to: "b * 2".into(),
        }));
        let update = |key: usize| {
            PlannedChange::new(Change::Row(RowChange::UpdateRow {
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
            }))
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
            diagnostic: None,
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

    #[test]
    fn scratch_reports_keep_driver_text_and_internal_queries_out_of_the_ledger() {
        use pbps_pg::resolver::reconstruct::{
            ReconstructError,
            refusal::{Diagnosis, Pending, Refusal},
        };
        use std::sync::Arc;
        let report = Arc::new(Refusal {
            version: 180006,
            committed: vec![],
            stalled: true,
            overtakes: vec![],
            pending: vec![Pending {
                declaration: "view app.v".into(),
                subject: pbps_model::Change::Module(pbps_model::ModuleChange::CreateModule {
                    id: "app.v".parse().unwrap(),
                    module: Box::new(pbps_model::Module {
                        kind: pbps_model::ModuleKind::View,
                        description: None,
                        definition: "SELECT 1 AS n".into(),
                        options: Default::default(),
                    }),
                }),
                owner: None,
                blocked: vec![],
                why: vec![],
                recovery: None,
                last: Some(Diagnosis {
                    error: Arc::new(DbError::Driver {
                        message: "private-row-value".into(),
                        code: Some("42P01".into()),
                        diagnostic: Some(Box::new(pbps_db::DriverDiagnostic {
                            primary: "private-row-value".into(),
                            schema: None,
                            table: None,
                            column: None,
                            datatype: None,
                            constraint: None,
                            position: Some(pbps_db::ErrorPosition::Internal {
                                position: 1,
                                query: "private-query-text".into(),
                            }),
                        })),
                    }),
                    statement: 0,
                    generation: 0,
                    excerpt: None,
                    reference: None,
                    candidates: vec![],
                    declared: vec![],
                    created: vec![],
                    overtaken: vec![],
                }),
            }],
        });
        for error in [
            anyhow::Error::new(ReconstructError::Refused(report.clone())),
            anyhow::Error::new(pbps_cli::resolver::server::Error::Creation(Arc::new(
                pbps_cli::resolver::server::CreationFailure::new(report.clone()),
            ))),
            anyhow::Error::new(pbps_cli::resolver::server::ProduceError::Run(
                pbps_cli::resolver::server::Error::Creation(Arc::new(
                    pbps_cli::resolver::server::CreationFailure::new(report),
                )),
            )),
        ] {
            assert!(error.to_string().contains("private-row-value"));
            let reason = ledger_safe_reason(&error);
            assert!(reason.contains("42P01"), "{reason}");
            assert!(!reason.contains("private-row-value"), "{reason}");
            assert!(!reason.contains("private-query-text"), "{reason}");
        }
    }
    /// #1605 review: the absence proof's refusal keeps a driver failure as
    /// its source, so a failed bootstrap's ledger row is redacted still, and
    /// the operator reads both the refusal and its cause.
    #[test]
    fn an_unproven_absence_keeps_its_driver_frame_redactable() {
        let refused = absence_unproven(
            &["other.t".parse().unwrap()],
            DbError::Driver {
                diagnostic: None,
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
            diagnostic: None,
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
            diagnostic: None,
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
            diagnostic: None,
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
            diagnostic: None,
            message: "invalid input syntax for type integer: \"super-secret-abc\"".to_owned(),
            code: Some("22P02".to_owned()),
        };
        let wrapped = anyhow::Error::new(db).context(format!(
            "the database rejected this statement, and the whole plan was rolled back:\n{long_statement}"
        ));
        let rendered = ledger_safe_reason(&wrapped);

        for driver in [Driver::Postgres, Driver::Mssql, Driver::Sqlite] {
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
            diagnostic: None,
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
            diagnostic: None,
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
            diagnostic: None,
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
                    diagnostic: None,
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
                for driver in [Driver::Postgres, Driver::Mssql, Driver::Sqlite] {
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
        use pbps_model::ModuleChange;
        use pbps_model::{Change, Module, ModuleKind, PlannedChange};
        let id: pbps_model::ModuleId = "app.v".parse().unwrap();
        let mut cs = ChangeSet::default();
        cs.changes.push(PlannedChange::new(Change::Module(
            ModuleChange::DropModule {
                id: id.clone(),
                kind: ModuleKind::View,
            },
        )));
        assert!(rebuilt_modules(&cs).is_empty());
        cs.changes.push(PlannedChange::new(Change::Module(
            ModuleChange::CreateModule {
                id: id.clone(),
                module: Box::new(Module {
                    kind: ModuleKind::Function,
                    description: None,
                    definition: "SELECT 1".into(),
                    options: Default::default(),
                }),
            },
        )));
        assert_eq!(
            rebuilt_modules(&cs)[&id],
            (ModuleKind::View, ModuleKind::Function)
        );
        assert!(plan_session::mssql::require_transactional_rebuilds(&cs, true).is_ok());
        assert!(plan_session::postgres::require_transactional_rebuilds(&cs, true).is_err());
        assert!(plan_session::postgres::require_transactional_rebuilds(&cs, false).is_ok());
    }

    /// The `CREATE` and the revoke that closes the routine are two statements,
    /// and a staged run commits each on its own — so between them the routine
    /// is committed, visible, and executable by every principal in the
    /// cluster (DECISIONS 517). Refused on **either** driver: what makes this
    /// unsafe is the staging, not the engine, and the change only ever
    /// reaches a plan for an engine that has the default.
    #[test]
    fn a_staged_plan_that_closes_a_routine_to_public_is_refused_on_either_driver() {
        use pbps_model::GrantChange;
        use pbps_model::{Change, PlannedChange};
        let mut cs = ChangeSet::default();
        cs.changes.push(PlannedChange::new(Change::Permission(
            GrantChange::PublicExecution {
                routine: "app.f(integer)".parse().unwrap(),
                access: pbps_model::PublicAccess::Revoked,
                origin: pbps_model::RoutineOrigin::Created,
            },
        )));
        for require_transactional_rebuilds in [
            plan_session::postgres::require_transactional_rebuilds,
            plan_session::mssql::require_transactional_rebuilds,
        ] {
            let e = require_transactional_rebuilds(&cs, true)
                .expect_err("a staged run leaves the routine open")
                .to_string();
            assert!(e.contains("app.f(integer)"), "{e}");
            assert!(e.contains("public_execute: true"), "{e}");
            assert!(require_transactional_rebuilds(&cs, false).is_ok());
        }

        // The other decision's window leaves the routine closed rather than
        // open, which is not the exposure this rule is about.
        let mut kept = ChangeSet::default();
        kept.changes.push(PlannedChange::new(Change::Permission(
            GrantChange::PublicExecution {
                routine: "app.f(integer)".parse().unwrap(),
                access: pbps_model::PublicAccess::Kept,
                origin: pbps_model::RoutineOrigin::Created,
            },
        )));
        assert!(plan_session::postgres::require_transactional_rebuilds(&kept, true).is_ok());
    }

    /// The routine the refusal names is the plan's, and its name may hold a
    /// line break or a terminal sequence: it is shown escaped (DEC-2032.1,
    /// #2035). Negative: a Unicode name is shown as it is.
    #[test]
    fn the_staged_refusal_shows_the_routine_name_escaped() {
        use pbps_model::GrantChange;
        use pbps_model::{Change, PlannedChange};
        let closing = |name: &str| {
            let mut cs = ChangeSet::default();
            cs.changes.push(PlannedChange::new(Change::Permission(
                GrantChange::PublicExecution {
                    routine: format!("app.{name}(integer)").parse().unwrap(),
                    access: pbps_model::PublicAccess::Revoked,
                    origin: pbps_model::RoutineOrigin::Created,
                },
            )));
            plan_session::postgres::require_transactional_rebuilds(&cs, true)
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

            let verdict = plan_session::postgres::edition_verdict();
            assert!(verdict.refused_online.is_empty() && verdict.warnings.is_empty());

            let pulled = introspect(&mut conn, Read::Snapshot).await.expect("pull");
            assert!(pulled.unmanaged_modules.is_empty());

            let names: BTreeSet<String> = ["public".to_owned(), "no_such_schema_here".to_owned()]
                .into_iter()
                .collect();
            let spelled = pbps_pg::Postgres::new()
                .schema_spellings(&mut conn, &names)
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
            let session = pbps_pg::Postgres::new();
            let alike = session.alike_role_names(&mut conn, &["a", "A"]).await;
            let holding = session.principal_name_holders(&mut conn, &["a"], &[]).await;
            let members = session.role_drop_facts(&mut conn).await;
            let owned = session.role_owned_securables(&mut conn).await;
            for (what, message) in [
                ("names_alike", alike.err().map(|e| e.to_string())),
                ("principals_holding", holding.err().map(|e| e.to_string())),
                ("role_members", members.err().map(|e| format!("{e:#}"))),
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
            match pbps_pg::impact::rename_impact(&mut conn, &target).await {
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
                changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Column(
                    pbps_model::ColumnChange::AlterColumnType {
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
                ))],
            };
            let ids = IdsFile {
                tables: [("t_aaaaaa".parse().unwrap(), "public.t".parse().unwrap())].into(),
                columns: [("c_aaaaaa".parse().unwrap(), "public.t.v".parse().unwrap())].into(),
                ..Default::default()
            };
            let report = plan_session::postgres::operational_cost(&mut conn, &cs, &ids).await;
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

    use pbps_model::ColumnChange;
    use pbps_model::GrantChange;
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
            PlannedChange::new(Change::Column(ColumnChange::AlterColumnType {
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
            })),
            PlannedChange::new(Change::Column(ColumnChange::AlterColumnExpression {
                uid: generated,
                column: table.column("g"),
                from: "final_column * 2".into(),
                to: "0".into(),
            })),
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
            Change::Column(ColumnChange::AlterColumnExpression { .. })
        ));
    }

    #[test]
    fn sequence_revocability_evidence_follows_both_table_and_column_renames() {
        let (mut changes, ids) = crate::plan_fixture::chain();
        changes.changes.push(PlannedChange::new(Change::Permission(
            GrantChange::Revoke {
                role: "writer".into(),
                target: "sequence::app.final_table.final_column".parse().unwrap(),
                permissions: [Permission::Usage].into(),
            },
        )));
        let mut found = pbps_db::catalog::Unrevocable {
            role: "writer".into(),
            target: "sequence::app.original_table.original_column"
                .parse()
                .unwrap(),
            permission: Permission::Usage,
            why: "the exact sequence grantor cannot be selected".into(),
        };
        assert!(
            plan_session::postgres::unrevocable_grants(&changes, &ids, &[found.clone()]).is_err()
        );
        found.target = "sequence::app.original_table.other".parse().unwrap();
        assert!(
            plan_session::postgres::unrevocable_grants(&changes, &ids, &[found.clone()]).is_ok()
        );
        found.target = "sequence::app.original_table.original_column"
            .parse()
            .unwrap();
        found.permission = Permission::Select;
        assert!(plan_session::postgres::unrevocable_grants(&changes, &ids, &[found]).is_ok());
    }

    #[test]
    fn serial_revocability_evidence_follows_its_table_through_renames() {
        let (mut changes, ids) = crate::plan_fixture::chain();
        changes.changes.push(PlannedChange::new(Change::Permission(
            GrantChange::Revoke {
                role: "writer".into(),
                target: GrantTarget::Object(TableName::new("app", "final_table")),
                permissions: [Permission::SerialSequences].into(),
            },
        )));
        let mut found = pbps_db::catalog::Unrevocable {
            role: "writer".into(),
            target: GrantTarget::Object(TableName::new("app", "original_table")),
            permission: Permission::SerialSequences,
            why: "the sequence grantor cannot be selected".into(),
        };
        assert!(
            plan_session::postgres::unrevocable_grants(&changes, &ids, &[found.clone()]).is_err()
        );
        found.permission = Permission::Usage;
        assert!(
            plan_session::postgres::unrevocable_grants(&changes, &ids, &[found.clone()]).is_ok()
        );
        found.permission = Permission::SerialSequences;
        found.target = "app.unmanaged".parse().unwrap();
        assert!(plan_session::postgres::unrevocable_grants(&changes, &ids, &[found]).is_ok());
    }

    #[test]
    fn grant_checks_keep_catalog_ownership_and_grantors_through_a_rename_chain() {
        let (mut changes, ids) = crate::plan_fixture::chain();
        let target = GrantTarget::Object(TableName::new("app", "final_table"));
        let stored = GrantTarget::Object(TableName::new("app", "original_table"));
        changes
            .changes
            .push(PlannedChange::new(Change::Permission(GrantChange::Grant {
                role: "owner".into(),
                target: target.clone(),
                permissions: [Permission::Select].into(),
            })));
        let owners = [(stored.clone(), "owner".into())].into();
        assert!(
            plan_session::postgres::owned_targets(
                &changes,
                &ids,
                &owners,
                &pbps_model::RoleName::from("deployer")
            )
            .is_err()
        );
        assert!(
            plan_session::postgres::owned_targets(
                &changes,
                &ids,
                &BTreeMap::new(),
                &pbps_model::RoleName::from("deployer")
            )
            .is_ok()
        );
        changes.changes.push(PlannedChange::new(Change::Permission(
            GrantChange::Revoke {
                role: "reader".into(),
                target,
                permissions: [Permission::Select].into(),
            },
        )));
        let found = pbps_db::catalog::Unrevocable {
            role: "reader".into(),
            target: stored,
            permission: Permission::Select,
            why: "the session cannot revoke the stored grant".into(),
        };
        assert!(plan_session::postgres::unrevocable_grants(&changes, &ids, &[found]).is_err());
        assert!(plan_session::postgres::unrevocable_grants(&changes, &ids, &[]).is_ok());
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
        let mut policies =
            if let pbps_model::Change::Table(pbps_model::TableChange::DropTable { uid, .. }) =
                &step.change
            {
                if let Some(table) = names.table_at(uid, pbps_model::plan_names::At::Start) {
                    pbps_pg::modules::policies_on_table(conn, table).await?
                } else {
                    Vec::new()
                }
            } else if let pbps_model::Change::Table(pbps_model::TableChange::RenameTable {
                to,
                ..
            }) = &step.change
            {
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
                if matches!(
                    &step.change,
                    pbps_model::Change::Column(pbps_model::ColumnChange::DropColumn { .. })
                ) {
                    pbps_pg::modules::policies_on_column_removal(conn, &column).await?
                } else {
                    pbps_pg::modules::policies_on_column(conn, &column).await?
                }
            } else {
                // A newly created column has no old catalog bindings.
                Vec::new()
            };
        if matches!(
            &step.change,
            pbps_model::Change::Column(pbps_model::ColumnChange::RenameColumn { .. })
        ) {
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

/// What a plan relies on that a package provides, sealed together: the type
/// pins the saved plan carries, and the pins of the other provider-owned
/// objects (DEC-2447.13). `providers` is empty until a kind that uses it lands,
/// so every plan seals exactly what it did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prerequisites {
    pub types: Vec<pbps_model::TypePin>,
    pub providers: Vec<pbps_model::ProviderPin>,
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
            provider: None,
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
                "postgres",
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

/// Snapshot and baseline retain a source UID until its approved copy commits.
pub async fn installation_ids(conn: &mut Conn, ids: &IdsFile) -> anyhow::Result<IdsFile> {
    if ids.rebuilds.is_empty() {
        return Ok(ids.clone());
    }
    let previous = latest(conn).await?;
    Ok(pbps_diff::rebuild::installation(
        ids,
        previous.as_ref().map(|e| &e.snapshot),
    )?)
}

pub type RebuildVerification = pbps_pg::rebuild::Verification;

/// Fixture reads have no apply session, so retain their baseline replay behind
/// the engine seam rather than omitting physical defaults (#2798).
#[cfg(test)]
pub(crate) fn physical_domain_defaults(
    before: &Schema,
    ids: &IdsFile,
    changes: &ChangeSet,
) -> BTreeMap<pbps_model::Uid, pbps_model::ManagedType> {
    pbps_pg::type_refs::declared::completed(before, ids, changes)
}

#[cfg(test)]
mod resolver_name_tests {
    use pbps_cli::resolver::server::PlanningDialect;

    /// The library spells an engine in its refusals from a constant, because
    /// `engine::of` is the binary's. The two must not drift.
    fn named<D: PlanningDialect>() -> (&'static str, &'static str) {
        (D::NAME, super::of(D::DRIVER).name())
    }

    #[test]
    fn a_refusal_names_the_engine_as_the_engine_names_itself() {
        let all = [
            named::<pbps_pg::Postgres>(),
            named::<pbps_mssql::Mssql>(),
            named::<pbps_sqlite::dialect::Sqlite>(),
        ];
        for (refusal, engine) in all {
            assert_eq!(refusal, engine);
        }
        assert_eq!(
            all.iter()
                .map(|(n, _)| *n)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            all.len(),
            "two engines must not share a name"
        );
    }
}
