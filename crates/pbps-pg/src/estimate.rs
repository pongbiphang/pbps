//! How expensive a change is, which is a different question from how dangerous
//! it is ([ADR-0012](../docs/ADR-0012-postgres-type-catalogue.md) §3–§4,
//! SPEC 14.1's P1 row).
//!
//! # Why this is not part of the risk class
//!
//! `TypeChangeRisk` answers "can this change fail or lose data at all", and it
//! answers it correctly. It says `integer -> bigint` is `Safe`, and it is:
//! nothing is lost and nothing can fail. **Measured**, that same change rewrites
//! a million-row table in 410ms against 0.662ms for one that does not, under a
//! lock that blocks readers — so the gate waves a total outage through with no
//! approval at all, and is right to by its own rule.
//!
//! The tempting fix is to call the rewrite `Narrowing`. ADR-0012 §3 rules it
//! out twice over: it lies about what the class means, teaching a reviewer that
//! the classes do not say what they say; and it breaks the class's own
//! criterion, which is deliberately data-independent, because a rewrite's cost
//! is entirely a function of how many rows there are. So cost lives here, on
//! its own axis, and **an estimate never loosens a gate and never reclassifies
//! anything**. There is nothing in this module a risk class can read.
//!
//! # Three facts, and what each is worth
//!
//! - **[`Rewrite`]** — whether the table is rebuilt. Exact, because it is the
//!   engine's own answer: `pg_class.relfilenode` either side of the statement.
//! - **[`Reads`]** — whether every row is read even so. `relfilenode` says
//!   nothing about this, which is the limit ADR-0012 states in as many words,
//!   and it matters: **measured**, `SET NOT NULL` rewrites nothing and reads all
//!   100,000 rows of the table, while `varchar(10) -> varchar(20)` rewrites
//!   nothing and reads **none** of them. Read as one fact those two are the same
//!   change, and one of them is free.
//! - **[`Lock`]** — measured rather than recalled, and in the engine's own
//!   words so an operator can match it against `pg_locks`.
//!
//! # `unknown` is an answer here
//!
//! ADR-0012 §4 measured three places where the same declared change costs
//! differently, and the conclusion it draws is the rule this module is built
//! on: **an estimate that guesses *cheap* is worse than one that admits it does
//! not know.** So a default expression this tool does not parse is
//! [`Rewrite::Unknown`] and never "free", a `timestamp` gaining a time zone is
//! unknown because the applying session's `TimeZone` decides it, and the three
//! shapes ADR-0012 records as unmeasured — a partitioned table, an inheritance
//! parent, and a type change on an indexed column — take the answer back to
//! unknown when [`against`] finds one, whatever the static half said.
//!
//! # Two halves, and one entry point
//!
//! [`estimates`] answers from the plan alone and is the whole public surface;
//! [`against`] fills in what only a connection knows. They are separate because
//! the first is a pure function and the second is the one that can be wrong
//! about which database it is looking at.
//!
//! A plan is what [`estimates`] takes, never a single change, and that is
//! deliberate: a table this plan renames is described by its new name and has
//! to be *measured* under the one the catalog still has, and only something
//! holding the whole plan can know the difference (DECISIONS 409).

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError};
use pbps_model::{Change, ChangeSet, ColumnType, Strategy, TableName};

/// Whether the statement rebuilds the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rewrite {
    /// The table is rebuilt: every row is written again, and the space the old
    /// copy holds is needed alongside it until the statement commits.
    Yes,
    /// A catalog change only.
    No,
    /// Not a function of the declaration alone. The reason is the answer.
    Unknown(String),
}

/// Whether the statement reads every row, which is a separate question from
/// whether it rewrites them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reads {
    EveryRow,
    Nothing,
    Unknown(String),
}

/// The lock the statement takes on the table, in the engine's own words.
///
/// Every one measured on 18.6 by reading `pg_locks` from inside the statement's
/// own transaction, or from a second session for the concurrent build, which
/// cannot be in one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    /// Blocks readers as well as writers. Most `ALTER TABLE` subcommands.
    AccessExclusive,
    /// Blocks writers, not readers. A non-concurrent index build.
    Share,
    /// Blocks writers, not readers, and is what a foreign key takes — on the
    /// **referenced** table as well as on the one the constraint is written on.
    ShareRowExclusive,
    /// Blocks neither readers nor writers, only other schema changes. A
    /// concurrent index build.
    ShareUpdateExclusive,
}

impl std::fmt::Display for Lock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Lock::AccessExclusive => "AccessExclusiveLock",
            Lock::Share => "ShareLock",
            Lock::ShareRowExclusive => "ShareRowExclusiveLock",
            Lock::ShareUpdateExclusive => "ShareUpdateExclusiveLock",
        })
    }
}

impl Lock {
    /// What the lock stops, for a reader who should not have to know the
    /// engine's lock table by heart.
    pub fn blocks(self) -> &'static str {
        match self {
            Lock::AccessExclusive => "reads and writes",
            Lock::Share => "writes",
            Lock::ShareRowExclusive => "writes",
            Lock::ShareUpdateExclusive => "neither reads nor writes, only other schema changes",
        }
    }
}

/// How many rows the table holds, as the catalog has it.
///
/// **Never analyzed is not empty.** Measured, `reltuples` is `-1` for a table
/// nobody has analyzed — a thousand rows in it and `relpages` still `0` — and
/// reading that as a row count says the change is free on the largest table in
/// the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rows {
    Estimated(i64),
    NeverAnalyzed,
}

/// What one change costs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estimate {
    /// The change, in the operator's words.
    pub about: String,
    /// The table the statement runs on, as the **plan** names it. This is the
    /// one the operator reads, and after a rename in the same plan it is the
    /// name the table will have by the time the statement runs.
    pub table: TableName,
    /// The same table as the **catalog** has it *now*, which is the only name
    /// [`against`] can ask about — a plan that renames the table describes one
    /// the database does not have yet, and the query would find nothing and
    /// report a rename as "no such table" (DECISIONS 409).
    ///
    /// Private, and the reason is the whole design: `Estimate` has no public
    /// constructor, so the only way to hold one is through [`estimates`], which
    /// is the only function that can see the rest of the plan.
    stored: TableName,
    /// Whether the catalog row is absent because this plan creates the table.
    /// Kept private for the same reason as `stored`: only the whole plan can
    /// supply this provenance.
    created: bool,
    pub rewrite: Rewrite,
    pub reads: Reads,
    pub lock: Lock,
    /// Other tables this statement locks — a foreign key takes its lock on the
    /// referenced table too, which is a table nobody reading the change would
    /// think to look at.
    pub also_locks: Vec<TableName>,
    /// Filled in by [`against`], which is the only half that can know it.
    pub rows: Option<Rows>,
    /// Why [`rows`](Self::rows) is absent after [`against`] has run.
    pub rows_unknown: Option<String>,
}

impl Estimate {
    fn new(about: String, table: TableName, rewrite: Rewrite, reads: Reads, lock: Lock) -> Self {
        Self {
            about,
            stored: table.clone(),
            created: false,
            table,
            rewrite,
            reads,
            lock,
            also_locks: Vec::new(),
            rows: None,
            rows_unknown: None,
        }
    }

    /// Takes both answers back to unknown, with the reason.
    fn unknown(&mut self, why: &str) {
        self.rewrite = Rewrite::Unknown(why.to_owned());
        self.reads = Reads::Unknown(why.to_owned());
    }

    /// Whether this change is free of the table's size — the one thing an
    /// operator wants at a glance, and the one an estimate must not claim
    /// without knowing.
    pub fn is_cheap(&self) -> bool {
        self.rewrite == Rewrite::No && self.reads == Reads::Nothing
    }
}

/// Whether changing `from` into `to` rebuilds the table, from the typed change
/// alone.
///
/// **Measured**: a matrix of every ordered pair of the catalogue's twenty-four
/// spellings on 18.6, 219 of them accepted by the engine. Ten pairs of distinct
/// types rewrite nothing, and the rule behind them is the one this function
/// implements — the target has to impose no new constraint on the bytes already
/// stored:
///
/// ```text
/// character varying(5)  -> character varying(10)   no rewrite
/// character varying(5)  -> character varying       no rewrite
/// character varying(5)  -> text                    no rewrite
/// text                  -> character varying       no rewrite
/// numeric(10,2)         -> numeric                 no rewrite
/// numeric(10,2)         -> numeric(12,2)           no rewrite
/// timestamp             -> timestamptz             the session's TimeZone decides
/// ```
///
/// and everything else rewrites, including the ones that look free:
/// `integer -> bigint`, `real -> double precision`, `character(5) ->
/// character(10)`, `jsonb -> json`, and `numeric(10,2) -> numeric(10,4)`, where
/// widening the *precision* is free and widening the *scale* is not. Nothing
/// about the declaration's shape suggests that asymmetry, which is the argument
/// for measuring the whole table rather than reasoning about it.
///
/// `character` is not in the free list and that is the padding: the stored value
/// is blank-padded to the declared width, so changing the width changes every
/// row.
fn rewrites(from: &ColumnType, to: &ColumnType) -> Rewrite {
    let (Ok(from), Ok(to)) = (crate::types::normalize(from), crate::types::normalize(to)) else {
        return Rewrite::Unknown("this dialect does not know one of the two types".to_owned());
    };
    if from == to {
        return Rewrite::No;
    }
    // The session's `TimeZone` decides this one — measured, no rewrite from a
    // `UTC` session and a rebuild from `America/New_York`. `emit` refuses the
    // change for a different reason (the stored instant is the session's too,
    // ADR-0012 §5), so this arm is unreachable through a plan; it is here
    // because a rewrite table with a silent hole in it is one somebody later
    // reads as a complete one.
    if crate::types::depends_on_the_session_time_zone(&from, &to) {
        return Rewrite::Unknown(
            "the applying session's TimeZone decides it: measured, no rewrite under `UTC` and a \
             full rebuild under `America/New_York`"
                .to_owned(),
        );
    }
    let unbounded_or_at_least = |a: &ColumnType, b: &ColumnType| match (arg(a, 0), arg(b, 0)) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(x), Some(y)) => x <= y,
    };
    let free = match (from.base.as_str(), to.base.as_str()) {
        // A varying string keeps its bytes; only the declared bound moves.
        ("character varying", "character varying") => unbounded_or_at_least(&from, &to),
        ("character varying", "text") => true,
        // `text` is a `character varying` with no bound, so the only free way
        // back is to one that has none either.
        ("text", "character varying") => arg(&to, 0).is_none(),
        // An unbounded `numeric` constrains nothing, so anything reaches it
        // free — measured, `numeric(10,2) -> numeric` rewrites nothing while
        // `numeric -> numeric(10,2)` rebuilds the table. Otherwise the scale is
        // what decides the stored form and has to stay put, and the precision
        // is a bound on top of it that may only widen: measured,
        // `numeric(10,2) -> numeric(12,2)` is free and
        // `numeric(10,2) -> numeric(10,4)` is a rebuild.
        ("numeric", "numeric") if to.args.is_empty() => true,
        ("numeric", "numeric") => {
            arg(&from, 1).unwrap_or(0) == arg(&to, 1).unwrap_or(0)
                && unbounded_or_at_least(&from, &to)
        }
        _ => false,
    };
    if free { Rewrite::No } else { Rewrite::Yes }
}

fn arg(ty: &ColumnType, i: usize) -> Option<i64> {
    match ty.args.get(i) {
        Some(pbps_model::TypeArg::Int(n)) => Some(*n),
        Some(pbps_model::TypeArg::Max | pbps_model::TypeArg::Ident(_)) | None => None,
    }
}

/// Every estimate a plan carries, each table named twice: as the plan names it
/// and as the catalog has it now.
///
/// The plan-level entry point, and the **only** one — `estimate`, the
/// single-change form, is not
/// public, because a caller mapping it over a change set would be building
/// exactly the estimate that cannot be measured. A plan may rename a table and
/// then alter one of its columns, and the `AlterColumnType` carries the
/// *declared*, post-rename table (`pbps_diff::schema_diff::order_key` gives a
/// column rename a class of its own after the table renames for the same
/// reason). Asked about that name, [`against`] finds no row and reports "this
/// database has no table by that name to measure" — a rename read as an absence,
/// which is the failure this repo has a rule about (DECISIONS 409).
///
/// Changes with no estimate are dropped rather than carried as `None`: a
/// module, a role, a grant and a row change are not about a table's stored
/// rows, and `estimate` says which.
pub fn estimates(changes: &ChangeSet, strategy: Strategy) -> Vec<Estimate> {
    // Built over the whole plan before anything is estimated, because the
    // order that puts a table rename ahead of what follows it is `order_key`'s
    // guarantee and not this function's to lean on.
    let mut stored: BTreeMap<&TableName, &TableName> = BTreeMap::new();
    let mut created: BTreeSet<&TableName> = BTreeSet::new();
    for p in &changes.changes {
        if let Change::RenameTable { from, to, .. } = &p.change {
            stored.insert(to, from);
        }
        if let Change::CreateTable { name, .. } = &p.change {
            created.insert(name);
        }
    }
    changes
        .changes
        .iter()
        .filter_map(|p| {
            let mut e = estimate(&p.change, strategy)?;
            if let Some(catalog) = stored.get(&e.table) {
                e.stored = (*catalog).clone();
            }
            e.created = created.contains(&e.table);
            Some(e)
        })
        .collect()
}

/// What this change costs, from the typed change alone — SPEC §7.2 is untouched
/// because not one row of data is read to answer it.
///
/// `None` where the change is not about a table's stored rows: a module, a
/// role, a grant, a data mode. A row change is `None` too, and deliberately: an
/// `INSERT` of one declared row costs what one row costs, and putting it beside
/// a rebuild of the whole table would bury the number that matters.
pub(crate) fn estimate(change: &Change, strategy: Strategy) -> Option<Estimate> {
    let e = |about: String, table: &TableName, rewrite, reads, lock| {
        Some(Estimate::new(about, table.clone(), rewrite, reads, lock))
    };
    match change {
        Change::AlterColumnType {
            column, from, to, ..
        } => {
            let rewrite = rewrites(from, to);
            // Measured, the two answers move together for this statement and
            // for no other: `text -> varchar(50)` rebuilds and reads every row,
            // and `varchar(10) -> varchar(20)` does neither — the rewrite *is*
            // the read. A change whose rewrite is unknown has an unknown read
            // for the same reason.
            let reads = match &rewrite {
                Rewrite::Yes => Reads::EveryRow,
                Rewrite::No => Reads::Nothing,
                Rewrite::Unknown(why) => Reads::Unknown(why.clone()),
            };
            e(
                format!("changing {column} to {to}"),
                &column.table,
                rewrite,
                reads,
                Lock::AccessExclusive,
            )
        }

        // The case ADR-0012's Limits name outright: nothing is rebuilt and
        // every row is read anyway. Measured, 100,000 of them.
        Change::AlterColumnNullability {
            column,
            to_nullable,
            ..
        } => e(
            format!(
                "making {column} {}",
                if *to_nullable { "nullable" } else { "NOT NULL" }
            ),
            &column.table,
            Rewrite::No,
            if *to_nullable {
                // Relaxing asks nothing of the rows already there.
                Reads::Nothing
            } else {
                Reads::EveryRow
            },
            Lock::AccessExclusive,
        ),

        Change::AddColumn {
            table,
            name,
            column,
            ..
        } => {
            let about = format!("adding the column {}", table.column(name));
            // Measured, `ADD COLUMN d integer DEFAULT 7` rewrites nothing and
            // reads nothing, and `ADD COLUMN d uuid DEFAULT gen_random_uuid()`
            // rebuilds the table. Both are one `AddColumn` carrying a default,
            // and only the expression tells them apart — which this tool does
            // not parse (SPEC §8.2). So a default that is not a literal is
            // `unknown`, never free: ADR-0012 §4 rules out the guess by name.
            match column.default.as_deref() {
                Some(default) if !crate::rows::is_constant(default.trim()) => {
                    let mut estimate = Estimate::new(
                        about,
                        table.clone(),
                        Rewrite::No,
                        Reads::Nothing,
                        Lock::AccessExclusive,
                    );
                    estimate.unknown(
                        "the new column's default is an expression this tool does not parse, and \
                         whether it rebuilds the table depends on the expression's volatility: \
                         measured, a literal default rebuilds nothing and \
                         `DEFAULT gen_random_uuid()` rebuilds every row",
                    );
                    Some(estimate)
                }
                _ => e(
                    about,
                    table,
                    Rewrite::No,
                    Reads::Nothing,
                    Lock::AccessExclusive,
                ),
            }
        }

        // Measured: no rewrite, nothing read — and **the space is not
        // reclaimed** (ADR-0012 §6). "The drop was cheap" and "the table got
        // smaller" are different claims, and an estimate that ran them together
        // would be wrong in the direction that surprises an operator.
        Change::DropColumn { column, .. } => e(
            format!("dropping {column}, which does not reclaim its space"),
            &column.table,
            Rewrite::No,
            Reads::Nothing,
            Lock::AccessExclusive,
        ),

        Change::AddCheck { table, name, .. } => e(
            format!("adding the check {name}"),
            table,
            Rewrite::No,
            Reads::EveryRow,
            Lock::AccessExclusive,
        ),

        Change::AddUnique { table, name, .. } => e(
            format!("adding the unique constraint {name}"),
            table,
            Rewrite::No,
            Reads::EveryRow,
            Lock::AccessExclusive,
        ),

        Change::SetPrimaryKey {
            table, to: Some(_), ..
        } => e(
            "adding the primary key".to_owned(),
            table,
            Rewrite::No,
            Reads::EveryRow,
            Lock::AccessExclusive,
        ),

        // The one that names a second table. Measured, the constraint takes
        // `ShareRowExclusiveLock` on the referenced table as well as on this
        // one — so a key nobody thought of as touching the parent blocks every
        // write to it for as long as the scan takes.
        Change::AddForeignKey {
            table,
            name,
            constraint,
        } => {
            let mut estimate = Estimate::new(
                format!("adding the foreign key {name}"),
                table.clone(),
                Rewrite::No,
                Reads::EveryRow,
                Lock::ShareRowExclusive,
            );
            if constraint.references_table != *table {
                estimate
                    .also_locks
                    .push(constraint.references_table.clone());
            }
            Some(estimate)
        }

        Change::AddIndex { table, name, index } => {
            // A concurrent build is the one statement here that lets writes
            // through — measured from a second session,
            // `ShareUpdateExclusiveLock` against the plain build's `ShareLock`.
            // It reads every row twice rather than once, which is the trade it
            // makes.
            //
            // Asked of the emitter rather than worked out again. `online` is a
            // request and not an answer: this dialect drops it for a filtered
            // index, because a concurrent build cannot share a batch with the
            // path a filter binds under (DECISIONS 262). An estimate that
            // decided for itself would name the lighter lock for a plan that
            // takes the heavier one, which is the direction that surprises an
            // operator.
            e(
                format!("building the index {name}"),
                table,
                Rewrite::No,
                Reads::EveryRow,
                if crate::emit::built_concurrently(index, strategy) {
                    Lock::ShareUpdateExclusive
                } else {
                    Lock::Share
                },
            )
        }

        // Catalog-only, measured: a default is a property of the column here
        // and existing rows keep whatever they hold.
        Change::AlterColumnDefault { column, .. } => e(
            format!("changing the default of {column}"),
            &column.table,
            Rewrite::No,
            Reads::Nothing,
            Lock::AccessExclusive,
        ),

        Change::RenameColumn { table, from, .. } => e(
            format!("renaming {}", table.column(from)),
            table,
            Rewrite::No,
            Reads::Nothing,
            Lock::AccessExclusive,
        ),
        Change::RenameTable { from, .. } => e(
            format!("renaming the table {from}"),
            from,
            Rewrite::No,
            Reads::Nothing,
            Lock::AccessExclusive,
        ),

        // Not about a table's stored rows, or not about a size at all.
        Change::CreateTable { .. }
        | Change::DropTable { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { to: None, .. }
        | Change::DropUnique { .. }
        | Change::DropForeignKey { .. }
        | Change::DropCheck { .. }
        | Change::DropIndex { .. }
        | Change::CreateModule { .. }
        | Change::AlterModule { .. }
        | Change::DropModule { .. }
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. } => None,
    }
}

/// What the table is, which decides whether the static answer survives contact
/// with it, and how many rows it holds.
///
/// The three shapes are the ones ADR-0012's Limits name as **not measured**, and
/// each can change the answer: a partitioned table, an inheritance parent, and
/// a type change on a column an index is built over. Where one is found the
/// estimate goes back to `unknown` with the reason, whatever the static half
/// said — an estimate that was measured on ordinary tables and is quoted about
/// a partitioned one is worse than no estimate, because the number carries the
/// authority of the measurement it did not come from.
const SHAPE: &str = "\
SELECT c.relkind::text AS relkind,
       c.relhassubclass AS inherited,
       c.reltuples::int8 AS reltuples,
       EXISTS (SELECT 1 FROM pg_catalog.pg_index i
                WHERE i.indrelid = c.oid
                  AND ($2 <> '' AND EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                        WHERE a.attrelid = c.oid AND a.attname = $2
                          AND NOT a.attisdropped
                          AND a.attnum = ANY (i.indkey::int2[])))) AS column_is_indexed,
       (SELECT k.conname::text FROM pg_catalog.pg_constraint k
         WHERE k.conrelid = c.oid AND k.contype = 'c' AND k.convalidated
           AND EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                        WHERE a.attrelid = c.oid AND a.attname = $2
                          AND NOT a.attisdropped AND a.attnum = ANY (k.conkey))
         ORDER BY k.conname LIMIT 1) AS validated_column_check
  FROM pg_catalog.pg_class c
 WHERE c.oid = pg_catalog.to_regclass($1)";

/// Fills in what only a connection knows, and takes the answer back where the
/// table is a shape ADR-0012 did not measure.
///
/// `column` is the target of a type or nullability change, or empty for other
/// changes. An indexed type change can rebuild the index too; a validated
/// check on a column being tightened may let the engine skip the scan, but
/// deciding that from its expression would violate SPEC §8.2.
pub async fn against(
    conn: &mut Conn,
    estimate: &mut Estimate,
    column: Option<&str>,
) -> Result<(), DbError> {
    let relation = match crate::emit::qualified(&estimate.stored) {
        Ok(name) => name,
        // A name this dialect cannot write is not a table with no rows in it.
        Err(_) => {
            estimate.unknown("this table's name cannot be written as an identifier");
            return Ok(());
        }
    };
    let rows = conn
        .query_with(
            SHAPE,
            &[relation.as_str().into(), column.unwrap_or_default().into()],
        )
        .await?;
    let Some(row) = rows.first() else {
        if estimate.created {
            // Static cost is a property of the statement and remains known.
            // The plan may insert rows before this statement, though, so its
            // provenance is not permission to claim an estimated zero.
            estimate.rows_unknown = Some(
                "this plan creates this table, so the database has no row count for it yet"
                    .to_owned(),
            );
        } else {
            let why = "this database has no table by that name to measure";
            estimate.rows_unknown = Some(why.to_owned());
            estimate.unknown(why);
        }
        return Ok(());
    };
    estimate.rows_unknown = None;
    estimate.rows = Some(match row.try_get::<i64>("reltuples")?.unwrap_or(-1) {
        -1 => Rows::NeverAnalyzed,
        n => Rows::Estimated(n),
    });
    if row.try_get::<&str>("relkind")?.unwrap_or_default() == "p" {
        estimate.unknown(
            "this is a partitioned table, and ADR-0012 records that partitioned tables were not \
             measured",
        );
        return Ok(());
    }
    if row.try_get::<bool>("inherited")?.unwrap_or(false) {
        estimate.unknown(
            "other tables inherit from this one, and ADR-0012 records that inheritance was not \
             measured",
        );
        return Ok(());
    }
    // Only where the change was going to rebuild the table anyway: that is
    // what drags the index along with it. A change already answered `unknown`
    // keeps the reason it was given, which is more specific than this one.
    if column.is_some()
        && estimate.rewrite == Rewrite::Yes
        && row.try_get::<bool>("column_is_indexed")?.unwrap_or(false)
    {
        estimate.unknown(
            "an index is built over the column this change retypes, so the index is rebuilt with \
             it, and ADR-0012 records that this was not measured",
        );
    }
    // Of column type/nullability changes, only tightening nullability reads
    // every row without a rewrite. A validated check is a possible proof, not
    // one we can interpret: keep the known rewrite and lock answers intact.
    if column.is_some()
        && estimate.rewrite == Rewrite::No
        && estimate.reads == Reads::EveryRow
        && let Some(name) = row.try_get::<&str>("validated_column_check")?
    {
        estimate.reads = Reads::Unknown(format!(
            "validated check constraint {name} refers to this column; the engine may use it \
             to skip the NOT NULL scan, but this tool does not parse its expression"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(s: &str) -> ColumnType {
        s.parse().expect("a type parses")
    }

    fn cref(s: &str) -> pbps_model::ColumnRef {
        s.parse().expect("a column reference")
    }

    fn tname(s: &str) -> TableName {
        s.parse().expect("a table name")
    }

    /// DECISIONS 409: a table this plan renames is described by its new name
    /// and measured under the one the catalog still has.
    ///
    /// The two names have to be right independently — the operator reads the
    /// first and [`against`] queries the second — so both are asserted for
    /// every arm that carries a table a rename can move.
    #[test]
    fn a_table_this_plan_renames_is_measured_under_the_name_the_catalog_has() {
        use pbps_model::{ChangeSet, PlannedChange};

        let cs = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::RenameTable {
                    uid: "t_aaaaaa".parse().expect("a uid"),
                    from: tname("app.client"),
                    to: tname("app.customer"),
                }),
                PlannedChange::new(Change::AlterColumnType {
                    uid: "c_aaaaaa".parse().expect("a uid"),
                    column: cref("app.customer.v"),
                    from: ty("integer"),
                    to: ty("bigint"),
                    from_nullable: true,
                    to_nullable: true,
                }),
                PlannedChange::new(Change::RenameColumn {
                    uid: "c_bbbbbb".parse().expect("a uid"),
                    table: tname("app.customer"),
                    from: "v".into(),
                    to: "amount".into(),
                }),
                // A table this plan leaves alone, to show the translation is a
                // lookup and not a rewrite of every name in sight.
                PlannedChange::new(Change::AlterColumnType {
                    uid: "c_cccccc".parse().expect("a uid"),
                    column: cref("app.invoice.v"),
                    from: ty("integer"),
                    to: ty("bigint"),
                    from_nullable: true,
                    to_nullable: true,
                }),
            ],
        };
        let got: Vec<(String, String)> = estimates(&cs, Strategy::default())
            .iter()
            .map(|e| (e.table.to_string(), e.stored.to_string()))
            .collect();
        assert_eq!(
            got,
            [
                // The rename's own estimate is about the table it finds, so it
                // names the old one on both sides already.
                ("app.client".to_owned(), "app.client".to_owned()),
                ("app.customer".to_owned(), "app.client".to_owned()),
                ("app.customer".to_owned(), "app.client".to_owned()),
                ("app.invoice".to_owned(), "app.invoice".to_owned()),
            ],
            "the plan's name to read, the catalog's name to measure"
        );
    }

    /// The rule the measured matrix draws: a rewrite is avoided only where the
    /// target imposes no new constraint on the bytes already stored.
    ///
    /// The pairs that look free and are not are the point of the test. The
    /// engine's own verdict on every one of them is re-measured by the live
    /// suite, which is what would catch this table going stale.
    #[test]
    fn only_a_target_that_constrains_no_byte_avoids_the_rebuild() {
        for (from, to) in [
            ("character varying(5)", "character varying(10)"),
            ("character varying(5)", "character varying"),
            ("character varying(5)", "text"),
            ("character varying", "text"),
            ("text", "character varying"),
            ("numeric(10,2)", "numeric"),
            ("numeric(10,2)", "numeric(12,2)"),
            ("integer", "integer"),
        ] {
            assert_eq!(
                rewrites(&ty(from), &ty(to)),
                Rewrite::No,
                "{from} -> {to} rewrites nothing"
            );
        }
        for (from, to) in [
            // The textbook widening, and the reason this module exists.
            ("integer", "bigint"),
            ("bigint", "integer"),
            ("real", "double precision"),
            // Widening the precision is free and widening the scale is not.
            ("numeric(10,2)", "numeric(10,4)"),
            ("numeric", "numeric(10,2)"),
            // `character` is blank-padded, so its width is in every row.
            ("character(5)", "character(10)"),
            ("character(5)", "text"),
            ("character varying(10)", "character varying(5)"),
            ("character varying(10)", "character(10)"),
            ("text", "character varying(20)"),
            ("jsonb", "json"),
            ("json", "jsonb"),
            ("date", "timestamp"),
        ] {
            assert_eq!(
                rewrites(&ty(from), &ty(to)),
                Rewrite::Yes,
                "{from} -> {to} rebuilds the table"
            );
        }
    }

    /// A change whose cost the applying session decides is never answered from
    /// the declaration, and the reason travels with the answer.
    #[test]
    fn a_cost_the_session_decides_is_unknown_and_says_which_setting() {
        let Rewrite::Unknown(why) = rewrites(&ty("timestamp"), &ty("timestamptz")) else {
            panic!("the session's TimeZone decides this one");
        };
        assert!(why.contains("TimeZone"), "{why}");
    }

    /// The limit ADR-0012 states in as many words: `relfilenode` answers "was
    /// the table rebuilt" and says nothing about a full scan. Two changes that
    /// rewrite nothing, and one of them reads every row.
    #[test]
    fn rewriting_nothing_is_not_the_same_as_reading_nothing() {
        let tightened = estimate(
            &Change::AlterColumnNullability {
                uid: "c_aaaaaa".parse().expect("a uid"),
                column: cref("app.t.v"),
                ty: ty("integer"),
                to_nullable: false,
            },
            Strategy::default(),
        )
        .expect("an estimate");
        assert_eq!(tightened.rewrite, Rewrite::No);
        assert_eq!(tightened.reads, Reads::EveryRow);
        assert!(
            !tightened.is_cheap(),
            "a full scan under an exclusive lock is not cheap"
        );

        let widened = estimate(
            &Change::AlterColumnType {
                uid: "c_aaaaaa".parse().expect("a uid"),
                column: cref("app.t.v"),
                from: ty("varchar(10)"),
                to: ty("varchar(20)"),
                from_nullable: true,
                to_nullable: true,
            },
            Strategy::default(),
        )
        .expect("an estimate");
        assert_eq!(widened.rewrite, Rewrite::No);
        assert_eq!(widened.reads, Reads::Nothing);
        assert!(widened.is_cheap());
    }

    /// ADR-0012 §4's ruled-out guess: an unparsed default expression is
    /// `unknown`, never free.
    #[test]
    fn an_unparsed_default_is_never_called_cheap() {
        let column = |default: &str| {
            let mut c = pbps_model::Column::new(ty("uuid"));
            c.default = Some(default.to_owned());
            Change::AddColumn {
                uid: "c_aaaaaa".parse().expect("a uid"),
                table: tname("app.t"),
                name: "v".into(),
                column: Box::new(c),
            }
        };
        let guessed =
            estimate(&column("gen_random_uuid()"), Strategy::default()).expect("an estimate");
        assert!(
            matches!(guessed.rewrite, Rewrite::Unknown(_)),
            "{guessed:#?}"
        );
        assert!(!guessed.is_cheap(), "{guessed:#?}");

        // A literal is a literal, and the engine writes no row for it.
        let literal = estimate(&column("'2026-01-02'"), Strategy::default()).expect("an estimate");
        assert_eq!(literal.rewrite, Rewrite::No);
        assert!(literal.is_cheap(), "{literal:#?}");
    }

    /// A foreign key locks the table nobody was looking at.
    #[test]
    fn a_foreign_key_names_the_referenced_table_it_also_locks() {
        let change = Change::AddForeignKey {
            table: tname("app.child"),
            name: "fk_child".into(),
            constraint: Box::new(pbps_model::ForeignKey {
                columns: vec!["parent_id".into()],
                references_table: tname("app.parent"),
                references_columns: vec!["id".into()],
                on_delete: pbps_model::ReferentialAction::NoAction,
                on_update: pbps_model::ReferentialAction::NoAction,
            }),
        };
        let e = estimate(&change, Strategy::default()).expect("an estimate");
        assert_eq!(e.lock, Lock::ShareRowExclusive);
        assert_eq!(e.also_locks, [tname("app.parent")]);
        assert_eq!(e.lock.blocks(), "writes");
        // A self-reference locks one table, and naming it twice would read as
        // two.
        let mut self_referencing = change.clone();
        if let Change::AddForeignKey { constraint, .. } = &mut self_referencing {
            constraint.references_table = tname("app.child");
        }
        assert!(
            estimate(&self_referencing, Strategy::default())
                .expect("an estimate")
                .also_locks
                .is_empty()
        );
    }

    /// `online` is a request, and whether the build honours it is the
    /// emitter's answer rather than this module's.
    ///
    /// Working it out again from the filter alone named the lighter lock for
    /// every ordinary index build — a plan taking `ShareLock`, which blocks
    /// writers, reported as taking one that blocks nothing.
    #[test]
    fn the_index_lock_is_the_one_the_emitter_will_actually_take() {
        let index = |filter: Option<&str>| Change::AddIndex {
            table: tname("app.t"),
            name: "ix".into(),
            index: Box::new(pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    name: "v".into(),
                    descending: false,
                }],
                include: Vec::new(),
                unique: false,
                filter: filter.map(str::to_owned),
            }),
        };
        let online = Strategy { online: true };
        let lock = |change: &Change, strategy: Strategy| {
            estimate(change, strategy).expect("an estimate").lock
        };
        // No `online` asked for: an ordinary build, whatever the filter says.
        assert_eq!(lock(&index(None), Strategy::default()), Lock::Share);
        assert_eq!(
            lock(&index(Some("v > 0")), Strategy::default()),
            Lock::Share
        );
        // Asked for and honoured.
        assert_eq!(lock(&index(None), online), Lock::ShareUpdateExclusive);
        // Asked for and *dropped*, because a concurrent build cannot share a
        // batch with the path a filter binds under (DECISIONS 262). The
        // estimate has to follow the emitter, not the request.
        assert_eq!(lock(&index(Some("v > 0")), online), Lock::Share);
        assert_eq!(Lock::Share.blocks(), "writes");
        assert_eq!(
            Lock::ShareUpdateExclusive.blocks(),
            "neither reads nor writes, only other schema changes"
        );
    }

    /// Nothing here may be read as a risk. The estimate has no risk class in
    /// it, and this is the test that says so out loud: the type that would
    /// carry one does not exist in this module.
    #[test]
    fn an_estimate_carries_no_risk_class() {
        // The prose above names `TypeChangeRisk` on purpose, to say what this
        // module is *not*. What must not appear is a line of code that reads or
        // produces one, so the comments are taken out before looking.
        let source = include_str!("estimate.rs");
        // Up to the tests, which name the forbidden words in order to forbid
        // them, and would otherwise be the only match.
        let source = &source[..source.find("#[cfg(test)]").expect("a test module")];
        let code: String = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["RiskClass", "TypeChangeRisk", "risk_class"] {
            assert!(
                !code.contains(forbidden),
                "cost must never reach the gate or reclassify a change (ADR-0012 §3): {forbidden}"
            );
        }
    }

    /// A row count nobody has taken is not a row count of nothing.
    #[test]
    fn never_analyzed_is_its_own_answer() {
        assert_ne!(Rows::NeverAnalyzed, Rows::Estimated(0));
    }
}
