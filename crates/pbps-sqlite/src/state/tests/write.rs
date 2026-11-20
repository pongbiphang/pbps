//! `record`, `prune` and the widths.

use pbps_db::DbError;
use pbps_db::ledger::LedgerError;
use pbps_db::sqlite::Connection;
use pbps_model::{StagedProgress, StateKind, Text};

use super::{number, snapshot, text};
use crate::TRANSACTION;
use crate::state::{
    OPERATOR_CHARS, REASON_CHARS, STATE_TABLE, ensure_tables, prune, record, truncate_reason,
};
use crate::tests::Fresh;

fn rows(c: &Connection) -> i64 {
    number(c, &format!("SELECT count(*) FROM {STATE_TABLE}"))
}

#[test]
fn record_round_trips_every_field_and_projects_the_columns() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let mut staged = snapshot(StateKind::Staged, "leon");
    staged.git_sha = Some(Text::from("a".repeat(40)));
    staged.plan_checksum = Some("b".repeat(64));
    staged.reason = Some(Text::from("because"));
    staged.staged = Some(StagedProgress {
        completed: 2,
        total: 5,
        last_statement: "ALTER TABLE t".into(),
    });
    let id = record(c, &staged).expect("recorded");
    let row = c
        .query(
            &format!(
                "SELECT kind, git_sha, plan_checksum, operator, reason, state_version, \
                 tables_count, modules_count, staged_completed, staged_total, state_json \
                 FROM {STATE_TABLE} WHERE id = {id}"
            ),
            &[],
        )
        .expect("read")
        .remove(0);
    assert_eq!(row.text(0).unwrap(), "staged");
    assert_eq!(row.text(1).unwrap(), "a".repeat(40));
    assert_eq!(row.text(2).unwrap(), "b".repeat(64));
    assert_eq!(row.text(3).unwrap(), "leon");
    assert_eq!(row.text(4).unwrap(), "because");
    assert_eq!(row.integer(5).unwrap(), i64::from(staged.version));
    assert_eq!(row.integer(6).unwrap(), 0);
    assert_eq!(row.integer(7).unwrap(), 0);
    assert_eq!(row.integer(8).unwrap(), 2);
    assert_eq!(row.integer(9).unwrap(), 5);
    // The JSON is the snapshot, whole.
    let back = pbps_model::StateSnapshot::read_json(row.text(10).unwrap()).expect("reads back");
    assert_eq!(back, staged);

    // A state that is not a staged checkpoint has NULL for the pair and for what
    // it does not carry.
    let plain = record(c, &snapshot(StateKind::Apply, "leon")).expect("recorded");
    assert!(plain > id, "ids grow");
    let nulls = number(
        c,
        &format!(
            "SELECT (staged_completed IS NULL) + (staged_total IS NULL) + (git_sha IS NULL) \
             + (plan_checksum IS NULL) + (reason IS NULL) FROM {STATE_TABLE} WHERE id = {plain}"
        ),
    );
    assert_eq!(nulls, 5);
}

#[test]
fn applied_at_is_utc_iso_8601_with_three_decimals() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let id = record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    let at = text(
        c,
        &format!("SELECT applied_at FROM {STATE_TABLE} WHERE id = {id}"),
    );
    // `YYYY-MM-DDTHH:MM:SS.mmm`, which is what the other engines return.
    let shape: Vec<char> = at
        .chars()
        .map(|ch| if ch.is_ascii_digit() { '9' } else { ch })
        .collect();
    assert_eq!(shape.iter().collect::<String>(), "9999-99-99T99:99:99.999");
    // And it is the clock in UTC: within a minute of the engine's own `now`.
    assert_eq!(
        number(
            c,
            &format!(
                "SELECT julianday(applied_at) BETWEEN julianday('now', '-1 minute') \
                 AND julianday('now', '+1 second') FROM {STATE_TABLE} WHERE id = {id}"
            )
        ),
        1
    );
}

#[test]
fn a_snapshot_that_cannot_be_read_back_is_refused_before_anything_is_written() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    // A data-work entry without its work record.
    let broken = snapshot(StateKind::DataWork, "op");
    let refused = record(c, &broken).expect_err("refused");
    assert!(
        matches!(refused, LedgerError::BadEntry { id: 0, .. }),
        "{refused:?}"
    );
    assert_eq!(
        number(c, "SELECT count(*) FROM sqlite_schema"),
        0,
        "not even the ledger was created"
    );
    // Negative: a well-formed one records.
    record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
}

#[test]
fn record_and_ensure_join_the_callers_transaction() {
    let mut fresh = Fresh::file();
    // Rolled back with the transaction, the ledger's creation included.
    let failed = fresh.connection().transaction(&TRANSACTION, |tx| {
        record(tx, &snapshot(StateKind::Apply, "op"))
            .map_err(|e| DbError::Refused(e.to_string()))?;
        assert_eq!(rows(tx), 1, "visible inside");
        Err::<(), _>(DbError::Refused("stop".into()))
    });
    assert!(failed.is_err());
    let c = fresh.connection();
    assert!(!c.in_transaction());
    assert_eq!(
        number(c, "SELECT count(*) FROM sqlite_schema"),
        0,
        "neither the table nor the row survived the rollback"
    );
    // Committed with it.
    fresh
        .connection()
        .transaction(&TRANSACTION, |tx| {
            record(tx, &snapshot(StateKind::Apply, "op"))
                .map_err(|e| DbError::Refused(e.to_string()))?;
            Ok(())
        })
        .expect("committed");
    assert_eq!(rows(fresh.connection()), 1);
    // Outside a transaction a record is a statement of its own and persists.
    record(fresh.connection(), &snapshot(StateKind::Apply, "op")).expect("recorded");
    fresh.reopen();
    assert_eq!(rows(fresh.connection()), 2);
}

#[test]
fn a_record_made_inside_a_savepoint_that_is_undone_is_undone() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    ensure_tables(c).expect("created");
    c.execute_batch("BEGIN IMMEDIATE; SAVEPOINT s")
        .expect("open");
    record(c, &snapshot(StateKind::Failed, "op")).expect("recorded");
    c.execute_batch("ROLLBACK TO s; RELEASE s; COMMIT")
        .expect("undone");
    assert_eq!(rows(c), 0);
}

#[test]
fn ids_are_never_reused_after_the_newest_is_deleted() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    for _ in 0..3 {
        record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    }
    let newest = number(c, &format!("SELECT max(id) FROM {STATE_TABLE}"));
    c.execute(
        &format!("DELETE FROM {STATE_TABLE} WHERE id = ?1"),
        &[pbps_db::sqlite::Value::Integer(newest)],
    )
    .expect("deleted");
    let next = record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    assert!(next > newest, "{next} was handed out again after {newest}");
}

fn emoji(n: usize) -> String {
    "\u{1F600}".repeat(n)
}

#[test]
fn a_reason_over_the_width_fails_loudly_and_the_best_effort_cut_fits() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let with_reason = |reason: String| {
        let mut s = snapshot(StateKind::Failed, "op");
        s.reason = Some(Text::from(reason));
        s
    };
    // The width is in characters: 1000 emoji are 4000 bytes and fit.
    record(c, &with_reason(emoji(REASON_CHARS))).expect("a full width fits");
    // One more does not, and it is refused, never cut.
    let refused = record(c, &with_reason(emoji(REASON_CHARS + 1))).expect_err("too long");
    assert!(
        refused
            .to_string()
            .contains("the reason to record is 1001 characters and the ledger holds 1000"),
        "{refused}"
    );
    assert_eq!(rows(c), 1, "the refused entry was not recorded");
    // The column's CHECK is the backstop for a row written past this function.
    let by_hand = c.execute_batch(&format!(
        "INSERT INTO {STATE_TABLE} (kind, state_json, operator, reason) \
         VALUES ('apply', '{{}}', 'op', printf('%.*c', 1001, 'x'))"
    ));
    assert!(
        by_hand
            .expect_err("the CHECK")
            .to_string()
            .contains("CHECK constraint failed")
    );
    // And it is not the bound: `length()` stops at a NUL, so the CHECK takes a
    // text with one however long it is. The count made here does not.
    assert_eq!(
        number(c, "SELECT length('ab' || char(0) || 'cd')"),
        2,
        "the engine's count stops at the NUL"
    );
    let hidden = format!("a\0{}", "x".repeat(REASON_CHARS + 5));
    let refused = record(c, &with_reason(hidden)).expect_err("too long behind a NUL");
    assert!(
        refused.to_string().contains("the ledger holds 1000"),
        "{refused}"
    );
    let mut long_checksum = snapshot(StateKind::Apply, "op");
    long_checksum.plan_checksum = Some(format!("\0{}", "c".repeat(70)));
    let refused = record(c, &long_checksum).expect_err("a checksum too long behind a NUL");
    assert!(
        refused
            .to_string()
            .contains("the plan checksum to record is 71 characters"),
        "{refused}"
    );
    assert_eq!(rows(c), 1, "nothing refused was recorded");
    // The best-effort path cuts first, in characters, and then it records.
    let cut = truncate_reason(&emoji(3000));
    assert_eq!(cut.chars().count(), REASON_CHARS);
    record(c, &with_reason(cut)).expect("the cut text records");
    // Negative: a short text is untouched, and so is one at the width.
    assert_eq!(truncate_reason("short"), "short");
    assert_eq!(truncate_reason(&emoji(REASON_CHARS)), emoji(REASON_CHARS));
}

#[test]
fn an_operator_over_the_width_is_cut_in_the_snapshot_and_the_column_together() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let long = "o".repeat(OPERATOR_CHARS + 50);
    let id = record(c, &snapshot(StateKind::Apply, &long)).expect("recorded");
    let column = text(
        c,
        &format!("SELECT operator FROM {STATE_TABLE} WHERE id = {id}"),
    );
    assert_eq!(column.chars().count(), OPERATOR_CHARS);
    let json = text(
        c,
        &format!("SELECT state_json FROM {STATE_TABLE} WHERE id = {id}"),
    );
    let back = pbps_model::StateSnapshot::read_json(&json).expect("reads");
    assert_eq!(back.operator, column, "the JSON and the column agree");
    // Negative: exactly the width is kept whole.
    let exact = "é".repeat(OPERATOR_CHARS);
    let id = record(c, &snapshot(StateKind::Apply, &exact)).expect("recorded");
    assert_eq!(
        text(
            c,
            &format!("SELECT operator FROM {STATE_TABLE} WHERE id = {id}")
        ),
        exact
    );
}

#[test]
fn prune_keeps_the_newest_and_never_empties_the_ledger() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    for _ in 0..5 {
        record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    }
    assert_eq!(
        prune(c, 10).expect("pruned"),
        0,
        "fewer than asked kept: nothing goes"
    );
    assert_eq!(prune(c, 2).expect("pruned"), 3);
    assert_eq!(rows(c), 2);
    let kept = number(c, &format!("SELECT min(id) FROM {STATE_TABLE}"));
    assert_eq!(kept, 4, "the two newest remain");
    assert_eq!(prune(c, 2).expect("again"), 0);
    // `--keep 0` still keeps the baseline.
    assert_eq!(prune(c, 0).expect("pruned"), 1);
    assert_eq!(rows(c), 1);
    // And a record after a prune is newer than everything that was there.
    let next = record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    assert!(next > 5, "{next}");
}

#[test]
fn prune_of_a_database_with_no_ledger_is_not_initialized_and_creates_nothing() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    assert!(matches!(prune(c, 5), Err(LedgerError::NotInitialized)));
    assert_eq!(number(c, "SELECT count(*) FROM sqlite_schema"), 0);
    // Negative: an empty ledger is initialized and has nothing to prune.
    ensure_tables(c).expect("created");
    assert_eq!(prune(c, 5).expect("pruned"), 0);
}

#[test]
fn the_statements_name_the_table_main_qualified() {
    // The insert is a constant and the others are built from `STATE_TABLE`, so
    // the two spellings are tied here.
    assert!(crate::state::write::INSERT_STATE.contains(STATE_TABLE));
}
