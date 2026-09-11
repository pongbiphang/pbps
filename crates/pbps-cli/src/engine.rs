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

/// Catalog estimates are advisory. Failure to measure is an unavailable
/// answer, not a reason to refuse a valid plan (ADR-0012 §3, DECISIONS 430).
pub async fn operational_cost(conn: &mut Conn, cs: &ChangeSet) -> crate::cost::CostReport {
    use crate::cost::{ChangeCost, CostReport, Reads, Rewrite, Rows};
    match conn.driver() {
        Driver::Mssql => CostReport::Unavailable {
            engine: "sqlserver",
            reason: "operational_cost is not implemented for SQL Server; its costs have not been measured (ADR-0012, issue #255)".to_owned(),
        },
        Driver::Postgres => {
            use pbps_pg::estimate as pg;
            let mut estimates = pg::planned_estimates(cs).into_iter().peekable();
            let mut changes = Vec::with_capacity(cs.changes.len());
            for (change_index, p) in cs.changes.iter().enumerate() {
                let Some((_, mut e)) = estimates.next_if(|(index, _)| *index == change_index) else {
                    changes.push(ChangeCost::Unavailable {
                        change_index,
                        reason: "operational_cost has no measurement for this PostgreSQL change".to_owned(),
                    });
                    continue;
                };
                let column = match &p.change {
                    pbps_model::Change::AlterColumnType { column, .. }
                    | pbps_model::Change::AlterColumnNullability { column, .. } => Some(column.name.as_str()),
                    pbps_model::Change::CreateTable { .. }
                    | pbps_model::Change::DropTable { .. }
                    | pbps_model::Change::RenameTable { .. }
                    | pbps_model::Change::AddColumn { .. }
                    | pbps_model::Change::DropColumn { .. }
                    | pbps_model::Change::RenameColumn { .. }
                    | pbps_model::Change::AlterColumnDefault { .. }
                    | pbps_model::Change::SetColumnDeprecated { .. }
                    | pbps_model::Change::SetPrimaryKey { .. }
                    | pbps_model::Change::AddUnique { .. }
                    | pbps_model::Change::DropUnique { .. }
                    | pbps_model::Change::AddForeignKey { .. }
                    | pbps_model::Change::DropForeignKey { .. }
                    | pbps_model::Change::AddCheck { .. }
                    | pbps_model::Change::DropCheck { .. }
                    | pbps_model::Change::AddIndex { .. }
                    | pbps_model::Change::DropIndex { .. }
                    | pbps_model::Change::InsertRow { .. }
                    | pbps_model::Change::UpdateRow { .. }
                    | pbps_model::Change::DeleteRow { .. }
                    | pbps_model::Change::SetDataMode { .. }
                    | pbps_model::Change::CreateModule { .. }
                    | pbps_model::Change::AlterModule { .. }
                    | pbps_model::Change::DropModule { .. }
                    | pbps_model::Change::CreateRole { .. }
                    | pbps_model::Change::DropRole { .. }
                    | pbps_model::Change::RenameRole { .. }
                    | pbps_model::Change::Grant { .. }
                    | pbps_model::Change::Revoke { .. } => None,
                };
                if let Err(error) = pg::against(conn, &mut e, column).await {
                    changes.push(ChangeCost::Unavailable {
                        change_index,
                        reason: format!("operational_cost could not read PostgreSQL catalog context: {error}"),
                    });
                    continue;
                }
                changes.push(ChangeCost::Available {
                    change_index,
                    about: e.about,
                    table: e.table.to_string(),
                    rewrite: match e.rewrite {
                        pg::Rewrite::Yes => Rewrite::Yes,
                        pg::Rewrite::No => Rewrite::No,
                        pg::Rewrite::Unknown(reason) => Rewrite::Unknown { reason },
                    },
                    reads: match e.reads {
                        pg::Reads::EveryRow => Reads::EveryRow,
                        pg::Reads::Nothing => Reads::Nothing,
                        pg::Reads::Unknown(reason) => Reads::Unknown { reason },
                    },
                    lock: e.lock.to_string(),
                    blocks: e.lock.blocks().to_owned(),
                    also_locks: e.also_locks.iter().map(ToString::to_string).collect(),
                    rows: match e.rows {
                        Some(pg::Rows::Estimated(count)) => Rows::Estimated { count },
                        Some(pg::Rows::NeverAnalyzed) => Rows::NeverAnalyzed,
                        None => Rows::Unknown {
                            reason: e.rows_unknown.unwrap_or_else(|| "no catalog row estimate is available".to_owned()),
                        },
                    },
                });
            }
            CostReport::Available { engine: "postgres", changes }
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

/// Predict existing table/column DROP dependencies in the caller's transaction.
/// The other engine's rename reader does not answer this question (issue 254).
pub async fn check_drop_blockers(
    conn: &mut Conn,
    changes: &ChangeSet,
) -> anyhow::Result<ConnectedCheck> {
    match conn.driver() {
        Driver::Mssql => Ok(ConnectedCheck {
            name: "drop_blockers",
            engine: "SQL Server",
            status: "unavailable",
            message: "The table/column drop dependency reader is not implemented for SQL Server"
                .to_owned(),
        }),
        Driver::Postgres => {
            let reports = pbps_pg::impact::drop_blockers(conn, changes).await?;
            let blocked: Vec<_> = reports
                .iter()
                .filter(|r| !r.blocking.is_empty())
                .map(|r| format!("{}: {}", r.target, r.blocking.join("; ")))
                .collect();
            if !blocked.is_empty() {
                anyhow::bail!(
                    "drop_blockers (PostgreSQL): {}.\n\
                    Remove these dependencies before the drop, through earlier declared changes \
                    or a separately reviewed deployment, then recompute the plan.",
                    blocked.join("\n")
                );
            }
            Ok(ConnectedCheck {
                name: "drop_blockers",
                engine: "PostgreSQL",
                status: "passed",
                message: format!(
                    "{} table/column drop(s) checked against current catalog dependencies",
                    reports.len()
                ),
            })
        }
    }
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
    pub why: &'static str,
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
/// is the declared one, and only SQL Server's own permission questions (see
/// `pbps_mssql::doctor::permissions`) need to resolve it against a pending
/// rename this environment may not have caught up to yet (issue #133).
pub async fn permissions(
    conn: &mut Conn,
    project_ids: &IdsFile,
    ask: &Ask<'_>,
) -> Result<Permissions, DbError> {
    match conn.driver() {
        Driver::Mssql => {
            let held = pbps_mssql::doctor::permissions(
                conn,
                ask.managed_schemas,
                ask.referenced,
                ask.granted,
                ask.data,
                project_ids,
            )
            .await?;
            Ok(Permissions {
                gaps: pbps_mssql::doctor::missing(&held)
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
        Driver::Postgres => {
            let held = pbps_pg::doctor::permissions(conn, ask).await?;
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

/// PostgreSQL rebuilds must keep their locks, checks and DDL in one transaction.
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

#[cfg(test)]
mod tests {
    use super::*;

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
            let mut conn = pg().await;
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
