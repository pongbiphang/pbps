//! Reading the ledger back: the three answers, the timeline's per-row carry,
//! and that a read writes nothing.

use pbps_db::DbError;
use pbps_db::ledger::LedgerError;
use pbps_db::sqlite::Connection;
use pbps_model::{StagedProgress, StateKind, StateSnapshot, Text, Unreadable};

use super::{in_both_journal_modes, number, snapshot, text};
use crate::state::{
    STATE_TABLE, deployed_revision, ensure_tables, entry, has_applied_plan, latest, prune, record,
    timeline,
};
use crate::tests::Fresh;

/// A hand-written ledger row: the columns `record` would project, as given, and
/// the JSON as given. `None` is NULL.
fn insert(
    c: &Connection,
    kind: &str,
    version: Option<&str>,
    counts: [Option<&str>; 4],
    json: &str,
    checksum: Option<&str>,
) -> i64 {
    let sql = |v: Option<&str>| v.map_or("NULL".to_owned(), |v| v.to_owned());
    c.execute_batch(&format!(
        "INSERT INTO {STATE_TABLE} (kind, plan_checksum, state_json, operator, state_version, \
         tables_count, modules_count, staged_completed, staged_total) \
         VALUES ('{kind}', {}, '{json}', 'hand', {}, {}, {}, {}, {})",
        checksum.map_or("NULL".to_owned(), |v| format!("'{v}'")),
        sql(version),
        sql(counts[0]),
        sql(counts[1]),
        sql(counts[2]),
        sql(counts[3]),
    ))
    .expect("a hand-written row");
    number(c, "SELECT max(id) FROM main.__pbps_state")
}

/// A hand-written projection: what it is, its version cell and its four count cells.
type Case<'a> = (&'a str, Option<String>, [Option<&'a str>; 4]);

/// The version this build writes. Not exported by the model, so it is asked of a
/// snapshot it made.
fn newest_version() -> u32 {
    snapshot(StateKind::Apply, "op").version
}

/// The oldest version this build reads, found by asking: nothing below it is
/// readable and it is.
fn oldest_version() -> u32 {
    (0..)
        .find(|v| pbps_model::check_readable_version(*v).is_ok())
        .expect("some version is readable")
}

fn current() -> String {
    newest_version().to_string()
}

#[test]
fn absent_empty_and_unreadable_are_three_answers() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    // Absent: every read says so, and none of them is an empty answer.
    assert!(matches!(latest(c), Err(LedgerError::NotInitialized)));
    assert!(matches!(entry(c, None), Err(LedgerError::NotInitialized)));
    assert!(matches!(
        entry(c, Some(1)),
        Err(LedgerError::NotInitialized)
    ));
    assert!(matches!(timeline(c, 10), Err(LedgerError::NotInitialized)));
    assert!(matches!(
        deployed_revision(c, 9),
        Err(LedgerError::NotInitialized)
    ));
    assert!(matches!(
        has_applied_plan(c, "x"),
        Err(LedgerError::NotInitialized)
    ));
    // Empty: initialized with nothing in it.
    ensure_tables(c).expect("created");
    assert!(latest(c).expect("read").is_none());
    assert!(entry(c, None).expect("read").is_none());
    assert!(entry(c, Some(1)).expect("read").is_none());
    assert!(timeline(c, 10).expect("read").is_empty());
    assert!(deployed_revision(c, 9).expect("read").is_none());
    assert!(!has_applied_plan(c, "x").expect("read"));
    // Unreadable: an entry this build cannot parse is its own error, with the id.
    let bad = insert(c, "apply", None, [None; 4], "not json", None);
    match latest(c) {
        Err(LedgerError::BadEntry { id, .. }) => assert_eq!(id, bad),
        other => panic!("{other:?}"),
    }
    // The raw entry is still there to be asked for: the caller learns why.
    assert_eq!(entry(c, Some(bad)).expect("read").expect("there").id, bad);
}

#[test]
fn something_that_is_not_a_table_in_the_ledgers_name_is_refused_not_absent() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch("CREATE VIEW __pbps_state AS SELECT 1 AS id")
        .expect("ddl");
    for result in [
        latest(c).map(drop),
        entry(c, None).map(drop),
        timeline(c, 1).map(drop),
        deployed_revision(c, 1).map(drop),
        has_applied_plan(c, "x").map(drop),
    ] {
        assert!(
            matches!(result, Err(LedgerError::Db(DbError::Refused(_)))),
            "{result:?}"
        );
    }
}

#[test]
fn reads_never_create_or_write_the_ledger() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let _ = latest(c);
    let _ = timeline(c, 5);
    let _ = entry(c, Some(3));
    assert_eq!(
        number(c, "SELECT count(*) FROM sqlite_schema"),
        0,
        "no read made a table"
    );
    // On a ledger with rows, a read changes nothing the file keeps.
    record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    let before = number(c, "PRAGMA data_version") + number(c, "PRAGMA schema_version");
    latest(c).expect("read");
    timeline(c, 5).expect("read");
    assert_eq!(
        before,
        number(c, "PRAGMA data_version") + number(c, "PRAGMA schema_version")
    );
}

#[test]
fn a_read_is_not_stopped_by_a_writer_elsewhere() {
    in_both_journal_modes(|fresh, second| {
        record(fresh.connection(), &snapshot(StateKind::Apply, "op")).expect("recorded");
        second.execute_batch("BEGIN IMMEDIATE").expect("held");
        let started = std::time::Instant::now();
        assert!(latest(fresh.connection()).expect("read").is_some());
        assert_eq!(timeline(fresh.connection(), 5).expect("read").len(), 1);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        second.execute_batch("ROLLBACK").expect("released");
    });
}

#[test]
fn what_record_writes_reads_back_through_every_reader() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let mut s = snapshot(StateKind::Staged, "leon");
    s.git_sha = Some(Text::from("a".repeat(40)));
    s.plan_checksum = Some("c".repeat(64));
    s.reason = Some(Text::from("why"));
    s.staged = Some(StagedProgress {
        completed: 1,
        total: 3,
        last_statement: "x".into(),
    });
    let first = record(c, &s).expect("recorded");
    let second = record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    let newest = latest(c).expect("read").expect("there");
    assert_eq!(newest.id, second);
    assert!(newest.applied_at.starts_with("20"));
    let old = entry(c, Some(first)).expect("read").expect("there");
    assert_eq!(old.read().expect("decoded").snapshot, s);
    assert_eq!(
        entry(c, None).expect("read").expect("there").id,
        second,
        "no id is the newest"
    );
    let rows = timeline(c, 10).expect("read");
    assert_eq!(
        rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        [second, first]
    );
    let staged = &rows[1];
    assert_eq!(staged.kind, "staged");
    assert_eq!(staged.operator, "leon");
    assert_eq!(
        staged.git_sha.as_ref().map(Text::raw),
        Some("a".repeat(40).as_str())
    );
    assert_eq!(
        staged.plan_checksum.as_deref(),
        Some("c".repeat(64).as_str())
    );
    assert_eq!(staged.reason.as_ref().map(Text::raw), Some("why"));
    let state = staged.state.as_ref().expect("readable");
    assert_eq!(state.version, s.version);
    assert_eq!(state.staged.map(|p| (p.completed, p.total)), Some((1, 3)));
    // Negative: a row that is not a staged checkpoint has no progress, and no
    // optional column that was NULL turned into an empty string.
    let plain = &rows[0];
    assert!(plain.state.as_ref().expect("readable").staged.is_none());
    assert!(plain.git_sha.is_none() && plain.plan_checksum.is_none() && plain.reason.is_none());
}

#[test]
fn a_projected_row_never_reads_its_snapshot() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    ensure_tables(c).expect("created");
    // The projection is complete and the JSON is nonsense: only the projection is
    // asked.
    let ok = insert(
        c,
        "apply",
        Some(&current()),
        [Some("4"), Some("2"), None, None],
        "garbage",
        None,
    );
    let rows = timeline(c, 5).expect("read");
    let state = rows[0].state.as_ref().expect("the projection was enough");
    assert_eq!((state.tables, state.modules), (4, 2));
    assert_eq!(rows[0].id, ok);
}

#[test]
fn a_row_with_no_projection_falls_back_to_its_own_json() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    ensure_tables(c).expect("created");
    let mut s = snapshot(StateKind::Apply, "op");
    s.reason = Some(Text::from("r"));
    let json = serde_json::to_string(&s).expect("json").replace('\'', "''");
    insert(c, "apply", None, [None; 4], &json, None);
    let rows = timeline(c, 5).expect("read");
    let state = rows[0].state.as_ref().expect("read from the JSON");
    assert_eq!(
        (state.version, state.tables, state.modules),
        (s.version, 0, 0)
    );
    // Negative: JSON that does not parse is that row's, and not the call's.
    insert(c, "apply", None, [None; 4], "garbage", None);
    let rows = timeline(c, 5).expect("the call still answers");
    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[0].state, Err(Unreadable::Malformed(_))));
    assert!(rows[1].state.is_ok(), "the row beside it is intact");
}

#[test]
fn an_unreadable_row_is_carried_and_does_not_erase_the_history_around_it() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    ensure_tables(c).expect("created");
    let good = |c: &Connection| {
        insert(
            c,
            "apply",
            Some(&current()),
            [Some("1"), Some("0"), None, None],
            "{}",
            None,
        )
    };
    good(c);
    let older = (oldest_version() - 1).to_string();
    let newer = (newest_version() + 1).to_string();
    for version in [&older, &newer] {
        insert(
            c,
            "apply",
            Some(version),
            [Some("1"), Some("0"), None, None],
            "{}",
            None,
        );
    }
    good(c);
    let rows = timeline(c, 10).expect("the call answers");
    let verdicts: Vec<bool> = rows.iter().map(|r| r.state.is_ok()).collect();
    assert_eq!(verdicts, [true, false, false, true], "newest first");
    for row in &rows[1..3] {
        assert!(
            matches!(row.state, Err(Unreadable::UnsupportedVersion(_))),
            "{:?}",
            row.state
        );
    }
}

#[test]
fn a_value_this_build_cannot_read_is_that_rows_and_the_version_is_asked_first() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    ensure_tables(c).expect("created");
    let v = current();
    let cases: Vec<Case> = vec![
        (
            "a negative count",
            Some(v.clone()),
            [Some("-1"), Some("0"), None, None],
        ),
        (
            "a NULL count beside a version",
            Some(v.clone()),
            [None, Some("0"), None, None],
        ),
        (
            "a half staged pair",
            Some(v.clone()),
            [Some("1"), Some("0"), Some("1"), None],
        ),
        (
            "a negative staged total",
            Some(v.clone()),
            [Some("1"), Some("0"), Some("1"), Some("-2")],
        ),
        (
            "text in a count",
            Some(v.clone()),
            [Some("'many'"), Some("0"), None, None],
        ),
        (
            "a negative version",
            Some("-4".into()),
            [Some("1"), Some("0"), None, None],
        ),
        (
            "text in the version",
            Some("'x'".into()),
            [Some("1"), Some("0"), None, None],
        ),
    ];
    for (what, version, counts) in &cases {
        let id = insert(c, "apply", version.as_deref(), *counts, "{}", None);
        let rows = timeline(c, 1).expect("the call answers");
        assert_eq!(rows[0].id, id);
        assert!(
            matches!(rows[0].state, Err(Unreadable::Malformed(_))),
            "{what}: {:?}",
            rows[0].state
        );
    }
    // The version first: a newer build's row with a count this build cannot read
    // is refused as the version it is.
    insert(
        c,
        "apply",
        Some(&(newest_version() + 1).to_string()),
        [Some("-1"), None, Some("1"), None],
        "{}",
        None,
    );
    let rows = timeline(c, 1).expect("read");
    assert!(
        matches!(rows[0].state, Err(Unreadable::UnsupportedVersion(_))),
        "{:?}",
        rows[0].state
    );
    // Negative: the same counts with a readable shape are fine.
    insert(
        c,
        "apply",
        Some(&v),
        [Some("1"), Some("0"), Some("1"), Some("2")],
        "{}",
        None,
    );
    assert!(timeline(c, 1).expect("read")[0].state.is_ok());
}

#[test]
fn a_column_the_table_requires_that_is_not_there_is_thrown_not_defaulted() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    // A ledger-named table that was made by hand, with no constraints.
    c.execute_batch(
        "CREATE TABLE __pbps_state (id, applied_at, kind, git_sha, plan_checksum, state_json, \
         operator, reason, state_version, tables_count, modules_count, staged_completed, \
         staged_total); \
         INSERT INTO __pbps_state (id, kind, operator, state_json) VALUES (1, 'apply', 'x', '{}')",
    )
    .expect("ddl");
    for result in [latest(c).map(drop), timeline(c, 5).map(drop)] {
        match result {
            Err(LedgerError::Db(error)) => {
                assert!(error.to_string().contains("applied_at"), "{error}")
            }
            other => panic!("expected the missing column to be named, got {other:?}"),
        }
    }
}

#[test]
fn deployed_revision_counts_only_the_kinds_that_establish_a_schema() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let with_sha = |kind: StateKind, sha: &str| {
        let mut s = snapshot(kind, "op");
        s.git_sha = Some(Text::from(sha));
        s
    };
    let base = record(c, &with_sha(StateKind::Baseline, "base")).expect("recorded");
    let apply = record(c, &with_sha(StateKind::Apply, "applied")).expect("recorded");
    let failed = record(c, &with_sha(StateKind::Failed, "failed")).expect("recorded");
    let revision = |observed| {
        deployed_revision(c, observed)
            .expect("read")
            .map(|r| (r.id, r.git_sha.map(|s| s.raw().to_owned())))
    };
    // A failed attempt after the apply is not the deployed revision.
    assert_eq!(revision(failed), Some((apply, Some("applied".into()))));
    // An entry above the observed one is not looked at.
    assert_eq!(revision(base), Some((base, Some("base".into()))));
    // Nothing at or before it: none, and not an error.
    assert_eq!(revision(0), None);
    // A row with no commit evidence is a revision with no SHA, not an absent one.
    let bare = record(c, &snapshot(StateKind::Bootstrap, "op")).expect("recorded");
    assert_eq!(revision(bare), Some((bare, None)));
}

#[test]
fn has_applied_plan_counts_only_a_retained_apply() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let checksum = "d".repeat(64);
    let with = |kind: StateKind| {
        let mut s: StateSnapshot = snapshot(kind, "op");
        s.plan_checksum = Some(checksum.clone());
        s
    };
    record(c, &with(StateKind::Baseline)).expect("recorded");
    record(c, &with(StateKind::Failed)).expect("recorded");
    assert!(
        !has_applied_plan(c, &checksum).expect("read"),
        "a baseline or a failure naming the checksum is not an apply"
    );
    record(c, &with(StateKind::Apply)).expect("recorded");
    assert!(has_applied_plan(c, &checksum).expect("read"));
    // Negative: another checksum, and a pruned apply.
    assert!(!has_applied_plan(c, &"e".repeat(64)).expect("read"));
    record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    prune(c, 1).expect("pruned");
    assert!(
        !has_applied_plan(c, &checksum).expect("read"),
        "pruned away"
    );
}

#[test]
fn the_limit_is_a_count_of_rows_and_never_everything_by_accident() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    for _ in 0..4 {
        record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    }
    assert!(timeline(c, 0).expect("read").is_empty(), "0 is none");
    assert_eq!(timeline(c, 3).expect("read").len(), 3);
    assert_eq!(timeline(c, u32::MAX).expect("read").len(), 4);
    let ids: Vec<i64> = timeline(c, 2).expect("read").iter().map(|r| r.id).collect();
    assert_eq!(ids, [4, 3], "the newest");
}

#[test]
fn a_pruned_entry_is_not_there_and_latest_is_the_newest_that_is() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    for _ in 0..3 {
        record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    }
    prune(c, 1).expect("pruned");
    assert!(
        entry(c, Some(1)).expect("read").is_none(),
        "a pruned id is not an error"
    );
    assert!(entry(c, Some(3)).expect("read").is_some());
    assert_eq!(latest(c).expect("read").expect("there").id, 3);
    assert_eq!(text(c, "SELECT kind FROM main.__pbps_state"), "apply");
}

#[test]
fn a_statement_that_returned_the_wrong_columns_is_thrown_not_read() {
    use crate::state::rows::{stored, timeline_entry};
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let narrow = c.query("SELECT 1, 'x'", &[]).expect("rows");
    for result in [
        stored(&narrow[0]).map(drop),
        timeline_entry(&narrow[0]).map(drop),
    ] {
        match result {
            Err(DbError::BadRow(message)) => {
                assert!(message.contains("gone out of step"), "{message}")
            }
            other => panic!("{other:?}"),
        }
    }
    // Negative: a row of the right width is read.
    let right = c.query("SELECT 7, 't', '{}'", &[]).expect("rows");
    let entry = stored(&right[0]).expect("read");
    assert_eq!((entry.id, entry.applied_at.as_str()), (7, "t"));
}

/// A reader sets the ledger's wait for its own statements only.
#[test]
fn a_read_puts_the_callers_wait_back() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.set_busy_timeout(std::time::Duration::from_millis(321))
        .expect("set");
    let wait_now = |c: &Connection| number(c, "PRAGMA busy_timeout");
    // Absent: every reader refuses, and still puts it back.
    assert!(matches!(latest(c), Err(LedgerError::NotInitialized)));
    assert_eq!(wait_now(c), 321);
    crate::state::ensure_tables(c).expect("created");
    latest(c).expect("read");
    entry(c, None).expect("read");
    deployed_revision(c, 1).expect("read");
    has_applied_plan(c, "x").expect("read");
    timeline(c, 5).expect("read");
    assert_eq!(wait_now(c), 321);
}
