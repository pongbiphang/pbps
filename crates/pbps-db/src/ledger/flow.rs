//! The ledger flow that is the same on every engine.
//!
//! This file is the part that needs no statement: decoding a row, the bound
//! values of an insert, and the clipping that keeps a value inside its column.
//! The submodules are the part that talks to the server: [`LedgerSpec`] is
//! what an engine supplies, [`Recoverable`] is the guard around a statement
//! whose failure is an answer, and the reads are the functions re-exported
//! below.
//!
//! # Why this is here and the statements are not
//!
//! `pbps_pg::state` and `pbps_mssql::state` used to hold two copies of this
//! code, kept in step by comments that pointed from one to the other for the
//! reasons. The flow needs [`Conn`](crate::Conn), [`Row`] and
//! [`LedgerError`], which this crate owns; the statement text and the
//! engine's error codes stay in the engine crates and arrive as values of a
//! [`LedgerSpec`], so `pbps-db` still holds no engine SQL (DEC-2988.1).
//!
//! The few places where the engines differ by a value, not by behaviour, are
//! arguments or fields of the spec: [`ClipUnit`] for the unit a column's width
//! is counted in and [`RowLimit`] for how a row limit is bound.

mod read;
mod spec;

pub use read::{
    deployed_revision, entry, has_applied_plan, history, is_initialized, latest, lock_holder,
};
pub use spec::{LedgerSpec, Recoverable, Savepoint, SpecStatements, Statement};

use std::borrow::Cow;

use pbps_model::{StateSnapshot, Unreadable};

use super::{LedgerEntry, LedgerError, StoredEntry, TimelineStaged, TimelineState, clip_utf16};
use crate::{DbError, Param, Row};

/// The unit a ledger column's width is measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipUnit {
    /// `varchar(n)` on PostgreSQL counts characters. Measured on 18.6:
    /// `varchar(4)` accepts four emoji and reports `length = 4,
    /// octet_length = 16`, so a cut in the other engine's unit would shorten
    /// a text for no reason.
    Characters,
    /// `NVARCHAR(n)` on SQL Server counts UTF-16 code units, not characters.
    /// Measured, not assumed: an `NVARCHAR(4)` refused two emoji plus one
    /// ASCII letter with Msg 2628. A Rust `char` above U+FFFF occupies two
    /// units, so a count of characters could hand the column twice its width.
    /// The cut never splits a character, so what remains is valid text.
    Utf16Units,
}

impl ClipUnit {
    /// How wide `text` is in this unit.
    #[must_use]
    pub fn measure(self, text: &str) -> usize {
        match self {
            ClipUnit::Characters => text.chars().count(),
            ClipUnit::Utf16Units => text.encode_utf16().count(),
        }
    }

    /// `text` cut to at most `width` of this unit, on a character boundary.
    #[must_use]
    pub fn clip(self, text: &str, width: usize) -> String {
        match self {
            ClipUnit::Characters => text.chars().take(width).collect(),
            ClipUnit::Utf16Units => clip_utf16(text, width),
        }
    }
}

/// Cuts a reason down to what the ledger column can hold.
///
/// Used on the best-effort audit paths, where a failed attempt that cannot be
/// recorded is the opposite of what the row exists for; a user-supplied
/// `--reason` is not truncated and fails loudly instead, since an audit text
/// silently shortened is a different kind of loss.
#[must_use]
pub fn truncate_reason(text: &str, unit: ClipUnit, width: usize) -> String {
    unit.clip(text, width)
}

/// The snapshot with its operator cut to the column's width, in the column's
/// unit, cloned only when it has to be (#1205).
///
/// The operator is git's `user.name`, which nothing bounds, and the insert of
/// a longer one fails — `22001` on PostgreSQL, and on SQL Server the engine
/// refuses even a cut inside a surrogate pair as invalid UTF-16 — on `apply`,
/// after the statements ran. Cut in the snapshot rather than only in the
/// bound column, so `state_json` and its projection agree on what was
/// recorded, and in the engine's own unit so a name the column can hold is
/// kept whole.
#[must_use]
pub fn bounded_operator(
    snapshot: &StateSnapshot,
    unit: ClipUnit,
    width: usize,
) -> Cow<'_, StateSnapshot> {
    if unit.measure(&snapshot.operator) <= width {
        return Cow::Borrowed(snapshot);
    }
    let mut bounded = snapshot.clone();
    bounded.operator = unit.clip(&snapshot.operator, width);
    Cow::Owned(bounded)
}

/// How a row limit (`--limit`, a `u32`) is bound into a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLimit {
    /// `LIMIT` takes a `bigint`: every `u32` fits, so there is nothing to
    /// saturate and nothing to check.
    I64,
    /// `TOP (n)` takes a signed 32-bit integer, so the count saturates
    /// rather than wrapping: `--limit 4294967295` became `-1` and the server
    /// refused the query, so a number too large to mean anything turned into
    /// a failure rather than into "all of them" (DECISIONS 217).
    SaturatingI32,
}

impl RowLimit {
    /// The bound value for `limit`.
    #[must_use]
    pub fn param(self, limit: u32) -> Param<'static> {
        match self {
            RowLimit::I64 => Param::I64(i64::from(limit)),
            RowLimit::SaturatingI32 => Param::I32(i32::try_from(limit).unwrap_or(i32::MAX)),
        }
    }
}

/// A count or version as the `integer` columns hold it: saturating — a
/// table, module or staged-statement count that does not fit `i32` is not a
/// real schema, and a wrapped negative would misreport the count rather than
/// merely cap it.
#[must_use]
pub fn saturating_i32(n: u64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// [`record`](crate)'s parameters; `true` when the two staged columns are
/// bound.
///
/// `staged_completed`/`staged_total` cannot be bound as NULL through
/// [`Param`] — it has no nullable-integer variant, and adding one would mean
/// updating the binder `Param` is matched in by *every* driver for two columns
/// that are NULL on precisely the rows that are not staged. Two static
/// statements, chosen by whether this snapshot has staged progress at all,
/// cost nothing the engine's `no_statement_interpolates_a_value` test would
/// catch — neither text is built from a value — and need no new binder
/// variant.
#[must_use]
pub fn ordinary_params<'a>(
    snapshot: &'a StateSnapshot,
    state_json: &'a str,
) -> (Vec<Param<'a>>, bool) {
    let kind = snapshot.kind.as_str();
    let state_version = saturating_i32(u64::from(snapshot.version));
    let tables_count = saturating_i32(snapshot.schema.tables.len() as u64);
    let modules_count = saturating_i32(snapshot.schema.modules.len() as u64);
    let mut params: Vec<Param<'a>> = vec![
        kind.into(),
        snapshot.git_sha.as_ref().map(pbps_model::Text::raw).into(),
        snapshot.plan_checksum.as_deref().into(),
        state_json.into(),
        snapshot.operator.as_str().into(),
        snapshot.reason.as_ref().map(pbps_model::Text::raw).into(),
        state_version.into(),
        tables_count.into(),
        modules_count.into(),
    ];
    let staged = match &snapshot.staged {
        Some(progress) => {
            params.push(saturating_i32(progress.completed as u64).into());
            params.push(saturating_i32(progress.total as u64).into());
            true
        }
        None => false,
    };
    (params, staged)
}

/// A column the ledger declares `NOT NULL` that came back NULL, or one the
/// statement did not return at all.
///
/// Reported rather than defaulted: an empty string here would put a blank date
/// on a status screen or an empty operator in an audit record, which is the
/// silent wrong answer in the one place that exists to be read after the fact.
#[must_use]
pub fn missing(column: &str) -> DbError {
    DbError::BadRow(format!(
        "the ledger row has no `{column}`, which means the statement and this code have gone out \
         of step"
    ))
}

/// A `NOT NULL` text column.
pub fn text(row: &Row, column: &str) -> Result<String, DbError> {
    optional_text(row, column)?.ok_or_else(|| missing(column))
}

/// A text column that may be NULL.
pub fn optional_text(row: &Row, column: &str) -> Result<Option<String>, DbError> {
    Ok(row.try_get::<&str>(column)?.map(str::to_owned))
}

/// A `NOT NULL` integer column, widened to `i64`.
pub fn number(row: &Row, column: &str) -> Result<i64, DbError> {
    row.try_get::<i64>(column)?.ok_or_else(|| missing(column))
}

fn optional_i32(row: &Row, column: &str) -> Result<Option<i32>, DbError> {
    row.try_get::<i32>(column)
}

/// One ledger row read whole: the stored text, then parsed.
pub fn entry_from_row(row: &Row) -> Result<LedgerEntry, LedgerError> {
    let stored = stored_entry_from_row(row)?;
    let id = stored.id;
    stored.read().map_err(|error| LedgerError::BadEntry {
        id,
        message: error.to_string(),
    })
}

/// One ledger row as stored, `state_json` not yet parsed.
pub fn stored_entry_from_row(row: &Row) -> Result<StoredEntry, LedgerError> {
    Ok(StoredEntry {
        id: number(row, "id")?,
        applied_at: text(row, "applied_at")?,
        state_json: text(row, "state_json")?,
    })
}

/// A ledger row's projected columns, without `state_json` (DECISIONS 435):
/// the engine's timeline statement never asks for it. `state` is `None`
/// exactly when this row predates issue #103's migration —
/// `state_version IS NULL` — and `timeline` must still ask the legacy
/// `state_json` statement for it.
///
/// `Some(Err(_))` is a third shape, distinct from `None`: a row whose columns
/// *are* populated but whose version this build does not read — a row a
/// newer pbps wrote. That is not "legacy" and must never reach the JSON
/// fallback, which would find nothing wrong with a `state_json` this build
/// simply has not been asked to parse; refusing it here, before the fallback
/// even runs, is what [`TimelineState::from_projected`] makes unavoidable (a
/// round-1 review finding on #103's own PR).
pub struct ProjectedRow {
    pub id: i64,
    pub applied_at: String,
    pub kind: String,
    pub git_sha: Option<String>,
    pub plan_checksum: Option<String>,
    pub operator: String,
    pub reason: Option<String>,
    pub state: Option<Result<TimelineState, Unreadable>>,
}

/// A count or a version read back from an `integer` column. Negative is
/// impossible by construction — `record` only ever writes what
/// [`saturating_i32`] produces — so it is reported rather than clamped: a
/// negative here means the row and this reader have gone out of step, not
/// that the count was merely large.
///
/// Reported **for that row**, as `Unreadable::Malformed`, and not thrown: a
/// `BadRow` here used to `?` out of [`projected_row`] and take every other row
/// of the `timeline()` batch with it, which is the "thrown, not carried"
/// shape DECISIONS 218 rules out (#372, DEC-372.1).
fn as_count(n: i32, column: &str) -> Result<usize, Unreadable> {
    usize::try_from(n).map_err(|_| Unreadable::Malformed(format!("`{column}` is {n}, not a count")))
}

/// The version's counterpart of [`as_count`], carried the same way: a
/// negative `state_version` is a malformed row like a negative count, not
/// evidence the whole read is wrong — a query that had drifted from the table
/// would show on every row and every column, not on one value (DEC-372.1).
fn as_version(n: i32, column: &str) -> Result<u32, Unreadable> {
    u32::try_from(n).map_err(|_| Unreadable::Malformed(format!("`{column}` is {n}, not a version")))
}

/// A count column of a migrated row: NULL beside a non-NULL `state_version`
/// is a row `record` never writes, and is carried as malformed with the same
/// disposition as a negative count — two routes into one kind of bad row
/// (#372). Only a failure to read the column at all is thrown.
fn count(row: &Row, column: &str) -> Result<Result<usize, Unreadable>, DbError> {
    Ok(match optional_i32(row, column)? {
        Some(n) => as_count(n, column),
        None => Err(Unreadable::Malformed(format!(
            "`{column}` is NULL beside a `state_version`"
        ))),
    })
}

/// A row from the engine's timeline statement, with the timeline columns.
pub fn projected_row(row: &Row) -> Result<ProjectedRow, LedgerError> {
    let id: i64 = number(row, "id")?;

    // `state_version` is the migration's own marker: written by every
    // `record` from here on, and NULL on every row from before it. It is the
    // one column this test asks about rather than `tables_count` or the
    // staged pair, because those three project independently of whether a
    // row is staged and would each need their own "is this really absent"
    // rule; `state_version` has only one meaning either way.
    let state = match optional_i32(row, "state_version")? {
        None => None,
        Some(version) => Some(migrated_state(row, version)?),
    };

    ledger_row(row, id, state)
}

/// The state of a migrated row, or why this build cannot read it.
///
/// Every value `record` never writes — a negative or NULL count, a negative
/// version, a half-populated staged pair — is carried as this row's
/// `Unreadable`, and `timeline()` answers with every other row intact
/// (DECISIONS 218, #372). Only a failure to read a column at all is thrown.
///
/// The version is checked before any other projected column is even read,
/// not only before `TimelineState` is built from them (a round-3 review
/// finding on #103's own PR): `from_projected` already refuses an
/// unsupported version, but a newer pbps that wrote both an unsupported
/// version and a count this build cannot parse must refuse just that row —
/// the one ordering the JSON fallback never got wrong (`read_json` checks
/// the version before it touches the rest of the document at all). This is a
/// second call to the same `check_readable_version` `from_projected` already
/// calls, not a second check that could disagree with it.
fn migrated_state(row: &Row, version: i32) -> Result<Result<TimelineState, Unreadable>, DbError> {
    let version = match as_version(version, "state_version") {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    if let Err(e) = pbps_model::check_readable_version(version) {
        return Ok(Err(e));
    }
    let tables = match count(row, "tables_count")? {
        Ok(n) => n,
        Err(e) => return Ok(Err(e)),
    };
    let modules = match count(row, "modules_count")? {
        Ok(n) => n,
        Err(e) => return Ok(Err(e)),
    };
    // `(Some, Some)` and `(None, None)` are the only two pairs `record` ever
    // writes — it always writes both columns together or neither, driven from
    // the JSON side's `Option<StagedProgress>`, where `completed`/`total` are
    // one field, not two, and cannot come apart (a round-4 review finding on
    // #103's own PR). These two independently nullable columns can still
    // represent a pair no write path produces — a hand-edited row, or a
    // migration gone wrong — and it is refused as `Malformed` rather than read
    // as "not staged" the same as a genuine `(None, None)`, by the same
    // reasoning `read_json`'s strict parse refuses an inconsistent document.
    let staged = match (
        optional_i32(row, "staged_completed")?,
        optional_i32(row, "staged_total")?,
    ) {
        (Some(completed), Some(total)) => match (
            as_count(completed, "staged_completed"),
            as_count(total, "staged_total"),
        ) {
            (Ok(completed), Ok(total)) => Some(TimelineStaged { completed, total }),
            (Err(e), _) | (_, Err(e)) => return Ok(Err(e)),
        },
        // A migrated row that is not a staged checkpoint: both are NULL
        // together, which is what "not mid-deployment" means
        // (`pbps_model::StagedProgress`'s own doc comment).
        (None, None) => None,
        (Some(completed), None) => {
            return Ok(Err(Unreadable::Malformed(format!(
                "`staged_completed` is {completed} but `staged_total` is NULL"
            ))));
        }
        (None, Some(total)) => {
            return Ok(Err(Unreadable::Malformed(format!(
                "`staged_total` is {total} but `staged_completed` is NULL"
            ))));
        }
    };
    Ok(Ok(TimelineState::from_projected(
        version, tables, modules, staged,
    )
    .expect("the version was already checked as readable above")))
}

/// A row from the engine's unmigrated timeline statement: the six columns a
/// `__pbps_state` from before issue #103 has, and nothing else — `state` is
/// always `None`, because a table with no timeline columns has no row that
/// could be anything but legacy.
pub fn projected_row_unmigrated(row: &Row) -> Result<ProjectedRow, LedgerError> {
    let id: i64 = number(row, "id")?;
    ledger_row(row, id, None)
}

/// The six columns every shape of `__pbps_state` this crate reads has, shared
/// by [`projected_row`] and [`projected_row_unmigrated`] so the two never
/// drift on how one of them is read.
fn ledger_row(
    row: &Row,
    id: i64,
    state: Option<Result<TimelineState, Unreadable>>,
) -> Result<ProjectedRow, LedgerError> {
    Ok(ProjectedRow {
        id,
        applied_at: text(row, "applied_at")?,
        // The projected columns, which are what makes an unreadable row still
        // a row worth showing.
        kind: text(row, "kind")?,
        git_sha: optional_text(row, "git_sha")?,
        plan_checksum: optional_text(row, "plan_checksum")?,
        operator: text(row, "operator")?,
        reason: optional_text(row, "reason")?,
        state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A count too large for the column saturates rather than wrapping: a
    /// wrapped negative would misreport the count rather than merely cap it.
    #[test]
    fn a_count_too_large_for_the_column_saturates() {
        assert_eq!(saturating_i32(0), 0);
        assert_eq!(saturating_i32(i32::MAX as u64), i32::MAX);
        assert_eq!(saturating_i32(i32::MAX as u64 + 1), i32::MAX);
        assert_eq!(saturating_i32(u64::MAX), i32::MAX);
    }

    /// A negative value in a column this build only ever writes non-negative
    /// numbers to is reported rather than clamped to zero — clamping would
    /// read as "no tables" about a row that is actually corrupted.
    #[test]
    fn a_negative_count_or_version_is_reported_not_clamped() {
        assert!(as_count(-1, "tables_count").is_err());
        assert_eq!(as_count(5, "tables_count").unwrap(), 5);
        assert!(as_version(-1, "state_version").is_err());
        assert_eq!(as_version(7, "state_version").unwrap(), 7);
    }

    fn bound(param: Param<'_>) -> i64 {
        match param {
            Param::I64(n) => n,
            Param::I32(n) => i64::from(n),
            Param::Str(_) | Param::OptStr(_) => panic!("a row limit is bound as an integer"),
        }
    }

    /// Every count a caller can ask for survives the conversion where the
    /// engine takes a `bigint`, and none wraps negative where it takes a
    /// signed 32-bit integer: a wrapped count would silently shorten the
    /// timeline, which reads as "that is all there is".
    #[test]
    fn a_row_limit_never_wraps_in_either_binding() {
        for limit in [1, i32::MAX as u32, i32::MAX as u32 + 1, u32::MAX] {
            assert_eq!(bound(RowLimit::I64.param(limit)), i64::from(limit));
            let saturated = bound(RowLimit::SaturatingI32.param(limit));
            assert_eq!(saturated, i64::from(limit.min(i32::MAX as u32)));
            assert!(saturated > 0);
        }
        assert!(matches!(RowLimit::I64.param(1), Param::I64(1)));
        assert!(matches!(
            RowLimit::SaturatingI32.param(u32::MAX),
            Param::I32(i32::MAX)
        ));
    }

    /// The two units disagree exactly on a character outside the Basic
    /// Multilingual Plane, and the cut never falls inside one.
    #[test]
    fn a_text_is_cut_in_the_columns_unit_and_never_inside_a_character() {
        let ascii = "x".repeat(10);
        for unit in [ClipUnit::Characters, ClipUnit::Utf16Units] {
            assert_eq!(truncate_reason(&ascii, unit, 10), ascii);
            assert_eq!(truncate_reason(&format!("{ascii}y"), unit, 10), ascii);
            assert_eq!(truncate_reason("anything", unit, 0), "");
        }

        let emoji = "😀".repeat(10);
        assert_eq!(truncate_reason(&emoji, ClipUnit::Characters, 10), emoji);
        let cut = truncate_reason(&emoji, ClipUnit::Utf16Units, 10);
        assert_eq!(cut.chars().count(), 5);
        assert_eq!(ClipUnit::Utf16Units.measure(&cut), 10);

        // An odd unit left over cannot hold half a pair: the cut falls short
        // by one rather than splitting the character.
        let cut = truncate_reason(&format!("x{emoji}"), ClipUnit::Utf16Units, 10);
        assert_eq!(ClipUnit::Utf16Units.measure(&cut), 9);
        assert!(cut.chars().all(|c| c == 'x' || c == '😀'));
    }

    /// The operator is cut in the column's unit, and only when it has to be:
    /// a name the column can hold is borrowed untouched.
    #[test]
    fn an_operator_is_cut_only_when_it_exceeds_the_columns_width() {
        let mut snapshot = StateSnapshot::new(
            pbps_model::StateKind::Apply,
            pbps_model::Schema::default(),
            pbps_model::IdsFile::default(),
            "😀".repeat(4),
        );
        snapshot.operator = "😀".repeat(4);

        // Four emoji are four characters but eight UTF-16 units.
        assert!(matches!(
            bounded_operator(&snapshot, ClipUnit::Characters, 4),
            Cow::Borrowed(_)
        ));
        let cut = bounded_operator(&snapshot, ClipUnit::Utf16Units, 4);
        assert!(matches!(cut, Cow::Owned(_)));
        assert_eq!(cut.operator, "😀".repeat(2));
        assert_eq!(snapshot.operator, "😀".repeat(4), "the input is not edited");
    }
}
