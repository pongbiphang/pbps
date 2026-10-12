//! Is a partitioned table's series running out, and are its rows landing in
//! the DEFAULT partition (#1789, DEC-1789.2)?
//!
//! Two questions about each RANGE-partitioned parent the declarations hold,
//! asked of the database and never answered from the declarations alone:
//!
//! - **The runway.** How far the partitions reach past the newest partition
//!   that holds a row ([`crate::partition_runway`] is the arithmetic). The
//!   newest occupied partition is found by an `EXISTS` through the parent,
//!   one range at a time from the top, so a series with a long empty tail
//!   costs one empty read per partition and a table with rows in its newest
//!   partition costs one.
//! - **The DEFAULT's rows.** Whether the DEFAULT holds a row inside a range
//!   the declarations give a partition and the database does not have yet.
//!   Creating that partition moves those rows (DEC-1547.1), and only as far
//!   as `pbps move-rows` allowed (DEC-1788.1), which is better learned here
//!   than from the apply that refuses.
//!
//! # Why this reads rows when the rest of `doctor` reads none
//!
//! A plan reads no data (SPEC §7.2), so neither question can be a plan's. The
//! rows are not what is asked for, though: `EXISTS` returns a flag, never a
//! value, and every read is `READ ONLY`. What it costs is a scan: a range
//! predicate on the parent is pruned to the one partition a range names, but
//! the DEFAULT is never pruned, so its question runs under a statement
//! timeout and an unanswered one is reported as such.
//!
//! # Each question is a transaction of its own
//!
//! An error aborts a PostgreSQL transaction, and everything after it would
//! fail with "current transaction is aborted" and say nothing about its own
//! question. A denied `SELECT` on the parent is the likeliest error here, and
//! the runway and the DEFAULT's rows must each report their own.
//!
//! # What is not reported, and what is
//!
//! - A parent the database does not have (not deployed yet), a parent whose
//!   key is not one column of a type the generator steps, or one that is not
//!   RANGE: the question does not apply, and nothing is said.
//! - A series that reaches the horizon, a table with no row in any range
//!   partition, a series with no upper end: nothing to report.
//! - Anything that could not be read, a bound this reader does not know, a
//!   horizon this key cannot take: its own `…-unknown` finding. Absent, empty
//!   and unreadable are three different things.

use pbps_db::doctor::{
    DeclaredRange, DefaultRows, ParentRunway, PartitionedParent, Runway, RunwayAsk, RunwayReport,
    ShortRunway,
};
use pbps_db::{Conn, DbError, Param};
use pbps_model::{BoundDatum, IdsFile, ObjectName, PartitionBound};

use super::identity::{Identity, Table};
use crate::emit::{qualified, value_literal};
use crate::partition_runway::{self as runway, Horizon, Range, Verdict};
use crate::partition_series::Key;

/// A row of the DEFAULT, by `tableoid`, bound as `$1`: the DEFAULT's oid.
///
/// Through the parent rather than from the DEFAULT, so that the privilege
/// asked for is the parent's `SELECT`, which every other read of the table
/// asks, and a partition's own is not. The leaves of the DEFAULT's tree, not
/// the DEFAULT alone: a row's `tableoid` is the leaf's, and a DEFAULT that is
/// itself partitioned would otherwise hold rows this never sees. Measured on
/// 18.6 with a login holding `SELECT` on the parent alone, `pg_partition_tree`
/// answers for the DEFAULT all the same.
const IN_THE_DEFAULT: &str = "r.tableoid IN (SELECT relid::oid FROM \
     pg_catalog.pg_partition_tree($1::bigint::oid) WHERE isleaf)";

/// How long a statement waits for a lock the engine holds against it. A
/// rebuild holds `ACCESS EXCLUSIVE` on the parent for as long as it runs, and
/// `doctor` is the command someone runs to find out what is going on, not one
/// to queue behind it.
const LOCK_TIMEOUT: &str = "5s";

/// How long one question may run. The DEFAULT is read in full when it holds
/// nothing of the asked range, so its size sets the cost.
const STATEMENT_TIMEOUT: &str = "30s";

/// Why a question could not be answered.
enum Failure {
    Db(DbError),
    Said(String),
}

impl From<DbError> for Failure {
    fn from(error: DbError) -> Self {
        Self::Db(error)
    }
}

impl Failure {
    fn said(message: impl Into<String>) -> Self {
        Self::Said(message.into())
    }

    fn reason(&self) -> String {
        match self {
            Self::Db(error) => error.to_string(),
            Self::Said(message) => message.clone(),
        }
    }
}

/// The recorded identities of this environment, which say what a declared
/// parent is called here when a rename is pending. An absent ledger records
/// none.
async fn recorded_ids(conn: &mut Conn) -> Result<IdsFile, DbError> {
    match crate::state::latest(conn).await {
        Ok(Some(entry)) => Ok(entry.snapshot.ids),
        Ok(None) | Err(pbps_db::ledger::LedgerError::NotInitialized) => Ok(IdsFile::default()),
        Err(pbps_db::ledger::LedgerError::Db(error)) => {
            Err(error.context("cannot read recorded identities for doctor"))
        }
        Err(error) => Err(DbError::BadRow(format!(
            "cannot resolve doctor identities from the recorded state: {error}"
        ))),
    }
}

/// Asks the two questions of each parent. An `Err` is a read that stopped
/// every parent's answer, the recorded identities; anything narrower is in the
/// report, beside the parent it is about.
pub async fn partition_runway(
    conn: &mut Conn,
    ask: &RunwayAsk<'_>,
    project_ids: &IdsFile,
) -> Result<RunwayReport, DbError> {
    if ask.parents.is_empty() {
        return Ok(RunwayReport::default());
    }
    let recorded = recorded_ids(conn).await?;
    let identities = Identity::new(project_ids, &recorded);
    let horizon = Horizon::parse(ask.horizon);
    let mut report = RunwayReport::default();
    for parent in ask.parents {
        let physical = match identities.table(&parent.name) {
            Table::Recorded(name) | Table::Unrecorded(name) => name,
            // Another recorded identity still has this name: the declared
            // parent is not in the database yet.
            Table::Future => continue,
        };
        let runway = runway_of(conn, parent, &physical, &horizon).await;
        let default_rows = default_rows_of(conn, parent, &physical).await;
        if runway.is_some() || default_rows.is_some() {
            report.parents.push(ParentRunway {
                parent: parent.name.clone(),
                runway,
                default_rows,
            });
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// What the catalog says about a parent
// ---------------------------------------------------------------------------

/// A parent as the database holds it.
struct Found {
    key: Key,
    /// The key column, quoted for a statement.
    column: String,
    /// The parent, quoted for a statement.
    parent: String,
    ranges: Vec<Catalogued>,
    default: Option<(i64, ObjectName)>,
}

struct Catalogued {
    name: ObjectName,
    from: BoundDatum,
    to: BoundDatum,
}

enum Lookup {
    /// The database has no such table.
    Absent,
    /// It does, and the question does not apply to it.
    NotApplicable,
    Found(Found),
}

/// A parent's key and its partitions, from the catalog. Run inside a
/// canonical read, where a `timestamptz` bound prints in UTC (DEC-1170.1).
async fn lookup(conn: &mut Conn, physical: &ObjectName) -> Result<Lookup, Failure> {
    let rows = conn
        .query_with(
            "SELECT c.oid::bigint AS oid,
                    pt.partstrat::text AS strategy,
                    pt.partnatts::int AS natts,
                    pg_catalog.quote_ident(a.attname) AS column_quoted,
                    pg_catalog.format_type(a.atttypid, NULL) AS column_type
               FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
               JOIN pg_catalog.pg_partitioned_table pt ON pt.partrelid = c.oid
               LEFT JOIN pg_catalog.pg_attribute a
                 ON a.attrelid = c.oid AND pt.partnatts = 1 AND a.attnum = pt.partattrs[0]
              WHERE n.nspname = $1 AND c.relname = $2",
            &[Param::Str(&physical.schema), Param::Str(&physical.name)],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(Lookup::Absent);
    };
    let oid = row
        .try_get::<i64>("oid")?
        .ok_or_else(|| Failure::said("the catalog returned no oid for the parent"))?;
    let strategy = row.try_get::<&str>("strategy")?.unwrap_or_default();
    let natts = row.try_get::<i32>("natts")?.unwrap_or_default();
    let (Some(column), Some(ty)) = (
        row.try_get::<&str>("column_quoted")?,
        row.try_get::<&str>("column_type")?,
    ) else {
        return Ok(Lookup::NotApplicable);
    };
    let key = ty
        .parse::<pbps_model::ColumnType>()
        .ok()
        .and_then(|ty| Key::of(&ty));
    let Some(key) = key.filter(|_| strategy == "r" && natts == 1) else {
        return Ok(Lookup::NotApplicable);
    };
    let column = column.to_owned();

    let rows = conn
        .query_with(
            "SELECT ch.oid::bigint AS oid,
                    cn.nspname::text AS schema_name,
                    ch.relname::text AS table_name,
                    pg_catalog.pg_get_expr(ch.relpartbound, ch.oid) AS bound
               FROM pg_catalog.pg_inherits i
               JOIN pg_catalog.pg_class ch ON ch.oid = i.inhrelid
               JOIN pg_catalog.pg_namespace cn ON cn.oid = ch.relnamespace
              WHERE i.inhparent = $1::bigint::oid
                AND NOT i.inhdetachpending
                AND ch.relispartition
              ORDER BY cn.nspname, ch.relname",
            &[Param::I64(oid)],
        )
        .await?;
    let mut ranges = Vec::new();
    let mut default = None;
    for row in &rows {
        let name = ObjectName::new(
            row.try_get::<&str>("schema_name")?.unwrap_or_default(),
            row.try_get::<&str>("table_name")?.unwrap_or_default(),
        );
        let text = row.try_get::<&str>("bound")?.unwrap_or_default();
        match crate::introspect::parse_bound(text) {
            Some(PartitionBound::Default) => {
                let oid = row.try_get::<i64>("oid")?.unwrap_or_default();
                default = Some((oid, name));
            }
            Some(PartitionBound::Range { from, to }) => match (from.as_slice(), to.as_slice()) {
                ([from], [to]) => ranges.push(Catalogued {
                    name,
                    from: from.clone(),
                    to: to.clone(),
                }),
                _ => {
                    return Err(Failure::said(format!(
                        "partition {name} of {physical} has the bound `{text}`, which does not \
                         have one value at each end, as the key has one column"
                    )));
                }
            },
            None => {
                return Err(Failure::said(format!(
                    "partition {name} of {physical} has the bound `{text}`, which this reader \
                     does not know"
                )));
            }
        }
    }
    let parent = qualified(physical).map_err(|e| Failure::said(e.to_string()))?;
    Ok(Lookup::Found(Found {
        key,
        column,
        parent,
        ranges,
        default,
    }))
}

/// Opens a read of one question: the canonical session, a lock a rebuild
/// cannot make `doctor` wait for, and a bound on how long it may run.
async fn open(conn: &mut Conn) -> Result<(), DbError> {
    crate::catalog::open_canonical_read(conn).await?;
    let limits = conn
        .query(&format!(
            "SELECT pg_catalog.set_config('lock_timeout', {}, true),
                    pg_catalog.set_config('statement_timeout', {}, true)",
            value_literal(LOCK_TIMEOUT),
            value_literal(STATEMENT_TIMEOUT)
        ))
        .await;
    match limits {
        Ok(_) => Ok(()),
        Err(error) => {
            let _ = conn.execute("ROLLBACK").await;
            Err(error)
        }
    }
}

/// Ends the read [`open`] began, on every path, and says why it failed in
/// words when it did. A transaction the engine aborted is ended all the same.
async fn finish<T>(conn: &mut Conn, outcome: Result<T, Failure>) -> Result<T, String> {
    match outcome {
        Ok(value) => crate::catalog::close_canonical_read(conn, Ok(value))
            .await
            .map_err(|error| error.to_string()),
        Err(failure) => {
            let reason = failure.reason();
            let _ = crate::catalog::close_canonical_read::<T>(
                conn,
                Err(DbError::Refused(reason.clone())),
            )
            .await;
            Err(reason)
        }
    }
}

/// Whether a `SELECT EXISTS (…)` answered yes. A NULL is no answer.
async fn held(conn: &mut Conn, sql: &str, params: &[Param<'_>]) -> Result<bool, Failure> {
    let rows = conn.query_with(sql, params).await?;
    let answer = match rows.first() {
        Some(row) => row.try_get::<bool>("held")?,
        None => None,
    };
    answer.ok_or_else(|| Failure::said("the engine returned no answer to the existence question"))
}

// ---------------------------------------------------------------------------
// The runway
// ---------------------------------------------------------------------------

async fn runway_of(
    conn: &mut Conn,
    parent: &PartitionedParent,
    physical: &ObjectName,
    horizon: &Result<Horizon, String>,
) -> Option<Runway> {
    if let Err(error) = open(conn).await {
        return Some(Runway::Unknown(error.to_string()));
    }
    let outcome = runway_in(conn, parent, physical, horizon).await;
    // One transaction per question: whatever the outcome, it ends here.
    match finish(conn, outcome).await {
        Ok(None) => None,
        Ok(Some(short)) => Some(Runway::Short(short)),
        Err(reason) => Some(Runway::Unknown(reason)),
    }
}

async fn runway_in(
    conn: &mut Conn,
    parent: &PartitionedParent,
    physical: &ObjectName,
    horizon: &Result<Horizon, String>,
) -> Result<Option<ShortRunway>, Failure> {
    let Lookup::Found(found) = lookup(conn, physical).await? else {
        return Ok(None);
    };
    if found.ranges.is_empty() {
        return Ok(None);
    }
    let mut ranges = Vec::new();
    for catalogued in &found.ranges {
        ranges.push(
            runway::range(found.key, &catalogued.from, &catalogued.to).map_err(|why| {
                Failure::said(format!(
                    "partition {} of {physical}: {why}",
                    catalogued.name
                ))
            })?,
        );
    }
    // A series with no upper end cannot run out; nothing is read.
    if runway::is_open_ended(&ranges) {
        return Ok(None);
    }
    let horizon = horizon.as_ref().map_err(|why| Failure::said(why.clone()))?;

    let mut occupied = None;
    for i in runway::newest_first(&ranges) {
        let sql = exists_in_range(&found, &ranges[i]);
        if held(conn, &sql, &[]).await? {
            occupied = Some(i);
            break;
        }
    }
    // No row in any range partition: nothing to be ahead of.
    let Some(occupied) = occupied else {
        return Ok(None);
    };
    // A declared range this reader cannot read is only a hint lost: whether
    // the declarations already reach the horizon.
    let declared: Vec<Range> = parent
        .ranges
        .iter()
        .filter_map(|r| runway::range(found.key, &r.from, &r.to).ok())
        .collect();
    match runway::assess(found.key, &ranges, &declared, &ranges[occupied], horizon)
        .map_err(Failure::Said)?
    {
        Verdict::Covered => Ok(None),
        Verdict::Short(short) => Ok(Some(ShortRunway {
            frontier: short.frontier,
            held_from: short.held_from,
            reach: short.reach,
            horizon: short.horizon,
            every: short.every,
            declared_covers: short.declared_covers,
            default: found.default.map(|(_, name)| name),
        })),
    }
}

/// `SELECT EXISTS` of a row of the parent inside one range. The literals are
/// the engine's own text for the bounds, quoted as text and read by the engine
/// as the key's type, and pruning leaves the one partition the range names.
fn exists_in_range(found: &Found, range: &Range) -> String {
    let mut conditions = Vec::new();
    if let Some(lo) = &range.lo {
        conditions.push(format!("r.{} >= {}", found.column, value_literal(&lo.text)));
    }
    if let Some(hi) = &range.hi {
        conditions.push(format!("r.{} < {}", found.column, value_literal(&hi.text)));
    }
    if conditions.is_empty() {
        conditions.push("TRUE".to_owned());
    }
    format!(
        "SELECT EXISTS (SELECT 1 FROM {} AS r WHERE {}) AS held",
        found.parent,
        conditions.join(" AND ")
    )
}

// ---------------------------------------------------------------------------
// The DEFAULT's rows
// ---------------------------------------------------------------------------

async fn default_rows_of(
    conn: &mut Conn,
    parent: &PartitionedParent,
    physical: &ObjectName,
) -> Option<DefaultRows> {
    if let Err(error) = open(conn).await {
        return Some(DefaultRows::Unknown(error.to_string()));
    }
    let outcome = default_rows_in(conn, parent, physical).await;
    match finish(conn, outcome).await {
        Ok(None) => None,
        Ok(Some(held)) => Some(held),
        Err(reason) => Some(DefaultRows::Unknown(reason)),
    }
}

async fn default_rows_in(
    conn: &mut Conn,
    parent: &PartitionedParent,
    physical: &ObjectName,
) -> Result<Option<DefaultRows>, Failure> {
    let Lookup::Found(found) = lookup(conn, physical).await? else {
        return Ok(None);
    };
    let Some((default_oid, default)) = found.default.clone() else {
        return Ok(None);
    };
    // A range the database already has cannot have a row in the DEFAULT, so
    // only the declared ranges it lacks are asked about. Compared as the
    // catalog prints a bound: one declared in another spelling is asked about
    // too, and the answer is the same, no.
    let missing: Vec<&DeclaredRange> = parent
        .ranges
        .iter()
        .filter(|declared| {
            !found
                .ranges
                .iter()
                .any(|have| have.from == declared.from && have.to == declared.to)
        })
        .collect();
    let asked = conditions(&found.column, &missing);
    if asked.is_empty() {
        return Ok(None);
    }
    let params = [Param::I64(default_oid)];
    if !held(conn, &exists_sql(&found, &asked), &params).await? {
        return Ok(None);
    }
    // Which of them, so that the remedy names a partition that holds rows. One
    // more read of the DEFAULT, after the cheap one has answered yes: if it
    // cannot be had (the statement timeout), the finding stands and says that
    // it is one of them.
    let holding = which_hold_rows(conn, &found, &asked, &params).await;
    Ok(Some(DefaultRows::Held {
        default,
        missing: missing.iter().map(|r| r.partition.clone()).collect(),
        holding,
    }))
}

/// One declared range as a predicate on the key column, with the range it is.
struct Asked<'a> {
    range: &'a DeclaredRange,
    condition: String,
}

/// The predicates of the declared ranges that can take a row.
fn conditions<'a>(column: &str, missing: &[&'a DeclaredRange]) -> Vec<Asked<'a>> {
    missing
        .iter()
        .filter_map(|range| {
            let mut parts = Vec::new();
            match &range.from {
                BoundDatum::MinValue => {}
                BoundDatum::Value(text) => {
                    parts.push(format!("r.{column} >= {}", value_literal(text)));
                }
                // A range that starts at MAXVALUE takes no row.
                BoundDatum::MaxValue => return None,
            }
            match &range.to {
                BoundDatum::MaxValue => {}
                BoundDatum::Value(text) => {
                    parts.push(format!("r.{column} < {}", value_literal(text)));
                }
                BoundDatum::MinValue => return None,
            }
            Some(Asked {
                range,
                condition: if parts.is_empty() {
                    "TRUE".to_owned()
                } else {
                    format!("({})", parts.join(" AND "))
                },
            })
        })
        .collect()
}

/// The DEFAULT's rows inside any of the declared ranges, as one statement: the
/// DEFAULT is read once, whatever the number of ranges.
///
/// Through the parent and filtered on the DEFAULT's `tableoid`, so that the
/// privilege asked for is the parent's `SELECT`, which is what every other
/// read of the table asks. A partition's own privilege is not asked.
fn exists_sql(found: &Found, asked: &[Asked<'_>]) -> String {
    format!(
        "SELECT EXISTS (SELECT 1 FROM {} AS r WHERE {IN_THE_DEFAULT} \
         AND r.{} IS NOT NULL AND ({})) AS held",
        found.parent,
        found.column,
        any_of(asked)
    )
}

fn any_of(asked: &[Asked<'_>]) -> String {
    asked
        .iter()
        .map(|a| a.condition.as_str())
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// The declared partitions whose range the DEFAULT holds a row of, in the
/// order asked; empty when that could not be read. The first range a row
/// satisfies is the one it counts for, and declared ranges do not overlap.
async fn which_hold_rows(
    conn: &mut Conn,
    found: &Found,
    asked: &[Asked<'_>],
    params: &[Param<'_>],
) -> Vec<ObjectName> {
    let cases: String = asked
        .iter()
        .enumerate()
        .map(|(i, a)| format!(" WHEN {} THEN {i}", a.condition))
        .collect();
    let sql = format!(
        "SELECT DISTINCT n::int AS n FROM (SELECT CASE{cases} END AS n FROM {} AS r \
         WHERE {IN_THE_DEFAULT} AND r.{} IS NOT NULL AND ({})) AS s \
         WHERE n IS NOT NULL ORDER BY n",
        found.parent,
        found.column,
        any_of(asked)
    );
    let Ok(rows) = conn.query_with(&sql, params).await else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| row.try_get::<i32>("n").ok().flatten())
        .filter_map(|n| usize::try_from(n).ok())
        .filter_map(|n| asked.get(n))
        .map(|a| a.range.partition.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found() -> Found {
        Found {
            key: Key::Date,
            column: "\"ts\"".to_owned(),
            parent: "\"app\".\"ev\"".to_owned(),
            ranges: Vec::new(),
            default: None,
        }
    }

    fn declared(name: &str, from: BoundDatum, to: BoundDatum) -> DeclaredRange {
        DeclaredRange {
            partition: ObjectName::new("app", name),
            from,
            to,
        }
    }

    fn value(text: &str) -> BoundDatum {
        BoundDatum::Value(text.to_owned())
    }

    /// A bound is the engine's own text, quoted as a string and compared with
    /// the key, so a quote in a declared value cannot end the statement.
    #[test]
    fn the_default_question_is_one_statement_over_every_missing_range() {
        let first = declared("a", value("2027-01-01"), value("2027-02-01"));
        let second = declared("b", value("2027-02-01"), BoundDatum::MaxValue);
        let asked = conditions("\"ts\"", &[&first, &second]);
        assert_eq!(
            exists_sql(&found(), &asked),
            format!(
                "SELECT EXISTS (SELECT 1 FROM \"app\".\"ev\" AS r WHERE {IN_THE_DEFAULT} \
                 AND r.\"ts\" IS NOT NULL AND ((r.\"ts\" >= E'2027-01-01' AND r.\"ts\" < \
                 E'2027-02-01') OR (r.\"ts\" >= E'2027-02-01'))) AS held"
            )
        );
        let hostile = declared("c", value("x'; DROP TABLE t; --"), value("y\\"));
        let asked = conditions("\"ts\"", &[&hostile]);
        let sql = exists_sql(&found(), &asked);
        assert!(sql.contains("E'x''; DROP TABLE t; --'"), "{sql}");
        assert!(sql.contains("E'y\\\\'"), "{sql}");
    }

    /// Negative: a range that cannot take a row asks nothing, and a question
    /// about no range is not a question.
    #[test]
    fn a_range_that_takes_no_row_is_not_asked_about() {
        let backwards = declared("a", BoundDatum::MaxValue, value("2027-01-01"));
        let empty = declared("b", value("2027-01-01"), BoundDatum::MinValue);
        assert!(conditions("\"ts\"", &[&backwards, &empty]).is_empty());
        assert!(conditions("\"ts\"", &[]).is_empty());
        let all = declared("c", BoundDatum::MinValue, BoundDatum::MaxValue);
        let asked = conditions("\"ts\"", &[&all]);
        assert_eq!(asked.len(), 1);
        assert!(exists_sql(&found(), &asked).contains("(TRUE)"));
    }

    #[test]
    fn the_runway_question_is_bounded_on_the_sides_the_range_has() {
        let int = |text: &str| runway::end(Key::Date, text).unwrap();
        let range = Range {
            lo: Some(int("2026-11-01")),
            hi: Some(int("2026-12-01")),
        };
        assert_eq!(
            exists_in_range(&found(), &range),
            "SELECT EXISTS (SELECT 1 FROM \"app\".\"ev\" AS r WHERE r.\"ts\" >= E'2026-11-01' \
             AND r.\"ts\" < E'2026-12-01') AS held"
        );
        let open_below = Range {
            lo: None,
            hi: Some(int("2026-12-01")),
        };
        assert!(!exists_in_range(&found(), &open_below).contains(">="));
    }
}
