use super::{
    BIN, Demo, OwnDatabase, approved_apply, bootstrapped_demo, code, connected_artifact,
    old_server, on_server, plan_checksum, scalar, server, stderr, stdout, succeeds,
};
use pbps_model::{IdsFile, SavedPlan};
use std::io::Write as _;
use std::process::{Command, Output, Stdio};

fn with_stdin(d: &Demo, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn blank_non_utf8_and_competing_stdin_requirements_are_usage_errors_before_recording() {
    let d = Demo::new("require2400_stdin");
    for input in [b" \r\n".as_slice(), b"\xff"] {
        let out = with_stdin(
            &d,
            &[
                "drop",
                "app.t.amount",
                "--reason",
                "copied",
                "--require",
                "-",
            ],
            input,
        );
        assert_eq!(code(&out), 2, "{}", stderr(&out));
        assert!(
            stderr(&out).contains(if input == b"\xff" {
                "not UTF-8"
            } else {
                "row requirement is blank"
            }),
            "{}",
            stderr(&out)
        );
    }
    let out = with_stdin(
        &d,
        &["drop", "app.t.amount", "--reason", "-", "--require", "-"],
        b"",
    );
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("--reason and --require cannot both read standard input"));
    assert!(!d.dir.join("schema.ids.json").exists());
}

const BEFORE: &str = "table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  amount: {type: integer}\n  cents: {type: integer}\nprimary_key: {name: pk_t, columns: [id]}\n";
const AFTER: &str = "table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  cents: {type: integer}\n  marker: {type: integer}\nprimary_key: {name: pk_t, columns: [id]}\n";
const PREDICATE: &str = "cents = amount * 100";

fn bytes(d: &Demo) -> Vec<u8> {
    std::fs::read(d.dir.join("schema.ids.json")).unwrap()
}
fn ids(d: &Demo) -> IdsFile {
    serde_json::from_slice(&bytes(d)).unwrap()
}
fn record(d: &Demo, connection: &str, predicate: &str) {
    succeeds(d.run(&[
        "drop",
        "app.t.amount",
        "--reason",
        "copied",
        "--require",
        predicate,
        "--db",
        connection,
    ]));
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn guarded_drops_record_replace_clear_pin_and_apply_on_postgres_18() {
    outcome(&server());
}
#[test]
#[ignore = "needs live PostgreSQL 16"]
fn guarded_drops_record_replace_clear_pin_and_apply_on_postgres_16() {
    outcome(&old_server());
}

fn outcome(server: &str) {
    let own = OwnDatabase::new(server, "require2400_success");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "require2400_success", BEFORE);
    on_server(connection, "INSERT INTO app.t VALUES (1,2,200)");
    d.table(AFTER);
    succeeds(with_stdin(
        &d,
        &[
            "drop",
            "app.t.amount",
            "--reason",
            "copied",
            "--require",
            "-",
            "--db",
            connection,
        ],
        format!("{PREDICATE}\r\n").as_bytes(),
    ));
    let first = bytes(&d);
    record(&d, connection, PREDICATE);
    assert_eq!(bytes(&d), first, "the same requirement is idempotent");
    let plan = connected_artifact(&d, connection, false);
    let old_checksum = plan_checksum(&plan);
    let old: SavedPlan = serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
    let guard = old
        .changes
        .changes
        .iter()
        .find_map(|p| p.drop_guard.as_ref())
        .unwrap();
    assert_eq!(guard.requirements.len(), 1);
    assert_eq!(
        guard.requirements.values().next().unwrap().predicate,
        PREDICATE
    );
    let explained = succeeds(d.run(&["explain", "--plan", plan.to_str().unwrap()]));
    assert!(stdout(&explained).contains(PREDICATE));
    assert!(stdout(&explained).contains("exclusive table lock through the drop, so writers wait"));
    let sql = d.dir.join("guarded.sql");
    let out = d.run(&["plan", "--db", connection, "--sql", sql.to_str().unwrap()]);
    assert_ne!(code(&out), 0);
    assert!(
        stderr(&out).contains("standalone SQL export"),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(!sql.exists());
    let staged = d.run(&["plan", "--db", connection, "--staged"]);
    assert_ne!(code(&staged), 0);
    succeeds(d.run(&[
        "drop",
        "app.t.amount",
        "--reason",
        "copied",
        "--clear-require",
        "--db",
        connection,
    ]));
    assert!(ids(&d).tombstones.values().all(|t| t.require.is_none()));
    record(&d, connection, "cents = amount * 100 AND amount >= 0");
    let plan = connected_artifact(&d, connection, false);
    assert_ne!(plan_checksum(&plan), old_checksum);
    let stale = d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &old_checksum,
        "--allow",
        "destructive",
    ]);
    assert_ne!(code(&stale), 0);
    assert!(
        stderr(&stale).contains("no longer matches"),
        "{}",
        stderr(&stale)
    );
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "destructive"],
    ));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM information_schema.columns WHERE table_schema='app' AND table_name='t' AND column_name='amount'"
        ),
        0
    );
    assert_eq!(
        scalar(
            connection,
            &format!(
                "SELECT count(*)::bigint FROM public.__pbps_state WHERE plan_checksum='{}'",
                plan_checksum(&plan)
            )
        ),
        1
    );
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn requirements_parse_identically_at_record_plan_and_apply_under_dmy_defaults_on_postgres_18() {
    parser_settings(&server());
}
#[test]
#[ignore = "needs live PostgreSQL 16"]
fn requirements_parse_identically_at_record_plan_and_apply_under_dmy_defaults_on_postgres_16() {
    parser_settings(&old_server());
}

fn parser_settings(server: &str) {
    let own = OwnDatabase::new(server, "require2400_parser");
    let connection = own.connection();
    on_server(
        server,
        &format!("ALTER DATABASE \"{}\" SET DateStyle = 'ISO, DMY'", own.name),
    );
    let before = "table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  d: {type: date}\n  amount: {type: integer}\nprimary_key: {name: pk_t, columns: [id]}\n";
    let d = bootstrapped_demo(connection, "require2400_parser", before);
    on_server(
        connection,
        "INSERT INTO app.t VALUES (1,DATE '2026-01-01',7),(2,DATE '2026-01-15',8)",
    );
    d.table(&before.replace("  amount: {type: integer}\n", ""));
    // The first date cannot parse under DMY; the second means a different
    // threshold there. Recording, planning and counting must all use MDY.
    let predicate = "d < DATE '12/31/2026' AND d < DATE '01/02/2026'";
    record(&d, connection, predicate);
    let plan = connected_artifact(&d, connection, false);
    let expected = scalar(
        connection,
        "SELECT count(*) FROM app.t WHERE (d < DATE '2026-12-31' AND d < DATE '2026-01-02') IS NOT TRUE",
    );
    assert_eq!(expected, 1);
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM app.t WHERE (d < DATE '2026-12-31' AND d < DATE '2026-02-01') IS NOT TRUE"
        ),
        0,
        "the DMY interpretation would count a different number of violations"
    );
    let refused = approved_apply(&d, connection, &plan, &["--allow", "destructive"]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains(&format!(
            "{expected} row(s) of app.t violate the row requirement `{predicate}`"
        )),
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM information_schema.columns WHERE table_schema='app' AND table_name='t' AND column_name='amount'"
        ),
        1
    );
    on_server(connection, "DELETE FROM app.t WHERE id=2");
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "destructive"],
    ));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM information_schema.columns WHERE table_schema='app' AND table_name='t' AND column_name='amount'"
        ),
        0
    );
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn a_violating_row_after_plan_refuses_and_rolls_back_the_drop_on_postgres_18() {
    late_row(&server());
}
#[test]
#[ignore = "needs live PostgreSQL 16"]
fn a_violating_row_after_plan_refuses_and_rolls_back_the_drop_on_postgres_16() {
    late_row(&old_server());
}

fn late_row(server: &str) {
    let own = OwnDatabase::new(server, "require2400_late");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "require2400_late", BEFORE);
    on_server(connection, "INSERT INTO app.t VALUES (1,2,200)");
    d.table(AFTER);
    record(&d, connection, PREDICATE);
    let plan = connected_artifact(&d, connection, false);
    on_server(connection, "INSERT INTO app.t VALUES (2,2,0)");
    let refused = approved_apply(&d, connection, &plan, &["--allow", "destructive"]);
    assert_ne!(
        code(&refused),
        0,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused)
            .contains("1 row(s) of app.t violate the row requirement `cents = amount * 100`"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM information_schema.columns WHERE table_schema='app' AND table_name='t' AND column_name='amount'"
        ),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM information_schema.columns WHERE table_schema='app' AND table_name='t' AND column_name='marker'"
        ),
        0,
        "the earlier ADD COLUMN rolls back too"
    );
    assert_eq!(scalar(connection, "SELECT count(*)::bigint FROM app.t"), 2);
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn unsafe_recording_writes_nothing_and_referenced_renames_split_the_plan_on_postgres_18() {
    recording(&server());
}
#[test]
#[ignore = "needs live PostgreSQL 16"]
fn unsafe_recording_writes_nothing_and_referenced_renames_split_the_plan_on_postgres_16() {
    recording(&old_server());
}

fn recording(server: &str) {
    let own = OwnDatabase::new(server, "require2400_record");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "require2400_record", BEFORE);
    d.table(AFTER);
    on_server(
        connection,
        "CREATE TABLE app.other (amount integer); CREATE FUNCTION app.user_check(integer) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true'",
    );
    let before = bytes(&d);
    for predicate in [
        "app.user_check(amount)",
        "app.other.amount = amount",
        "random() > 0",
        "EXISTS(SELECT 1 FROM app.other)",
        "false) AND (false",
    ] {
        let refused = d.run(&[
            "drop",
            "app.t.amount",
            "--reason",
            "copied",
            "--require",
            predicate,
            "--db",
            connection,
        ]);
        assert_ne!(
            code(&refused),
            0,
            "{predicate}: {}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(!stderr(&refused).is_empty());
        assert_eq!(
            bytes(&d),
            before,
            "{predicate} must be refused before the ids write"
        );
    }
    on_server(
        connection,
        "DROP FUNCTION app.user_check(integer); DROP TABLE app.other",
    );
    record(&d, connection, PREDICATE);
    let guarded = bytes(&d);
    let mut malformed = ids(&d);
    for tombstone in malformed.tombstones.values_mut() {
        if let Some(requirement) = &mut tombstone.require {
            requirement.predicate = "false) AND (false".into();
        }
    }
    std::fs::write(
        d.dir.join("schema.ids.json"),
        serde_json::to_vec_pretty(&malformed).unwrap(),
    )
    .unwrap();
    d.commit();
    let refused = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("has unbalanced parentheses"),
        "{}",
        stderr(&refused)
    );
    std::fs::write(d.dir.join("schema.ids.json"), guarded).unwrap();
    d.commit();
    d.table(&AFTER.replace(
        "  cents: {type: integer}",
        "  amount_cents: {type: integer}",
    ));
    succeeds(d.run(&["rename", "app.t.cents", "amount_cents"]));
    d.commit();
    let refused = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("split the plan"),
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn a_table_drop_honors_its_removed_columns_requirements_on_postgres_18() {
    table_drop(&server());
}
#[test]
#[ignore = "needs live PostgreSQL 16"]
fn a_table_drop_honors_its_removed_columns_requirements_on_postgres_16() {
    table_drop(&old_server());
}

fn table_drop(server: &str) {
    let own = OwnDatabase::new(server, "require2400_table");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "require2400_table", BEFORE);
    on_server(connection, "INSERT INTO app.t VALUES (1,2,0)");
    d.table(AFTER);
    record(&d, connection, PREDICATE);
    std::fs::remove_file(d.dir.join("schema/app.t.yml")).unwrap();
    succeeds(d.run(&[
        "drop-table",
        "app.t",
        "--reason",
        "archived",
        "--require",
        "id > 0",
        "--db",
        connection,
    ]));
    let plan = connected_artifact(&d, connection, false);
    let saved: SavedPlan = serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
    assert_eq!(
        saved
            .changes
            .changes
            .iter()
            .find_map(|p| p.drop_guard.as_ref())
            .unwrap()
            .requirements
            .len(),
        2
    );
    let refused = approved_apply(&d, connection, &plan, &["--allow", "destructive"]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("violate the row requirement"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(scalar(connection, "SELECT count(*)::bigint FROM app.t"), 1);
    on_server(connection, "UPDATE app.t SET cents=200");
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "destructive"],
    ));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM information_schema.tables WHERE table_schema='app' AND table_name='t'"
        ),
        0
    );
    succeeds(d.run(&[
        "drop-table",
        "app.t",
        "--reason",
        "archived",
        "--clear-require",
        "--db",
        connection,
    ]));
    assert!(
        ids(&d)
            .tombstones
            .iter()
            .filter(|(u, _)| u.kind() == pbps_model::UidKind::Table)
            .all(|(_, t)| t.require.is_none())
    );
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn guarded_column_drops_follow_their_table_through_a_rename_on_postgres_18() {
    renamed_table(&server());
}
#[test]
#[ignore = "needs live PostgreSQL 16"]
fn guarded_column_drops_follow_their_table_through_a_rename_on_postgres_16() {
    renamed_table(&old_server());
}

fn renamed_table(server: &str) {
    let own = OwnDatabase::new(server, "require2400_rename");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "require2400_rename", BEFORE);
    on_server(connection, "INSERT INTO app.t VALUES (1,2,200)");
    d.table(AFTER);
    record(&d, connection, PREDICATE);
    d.table(&AFTER.replace("table: app.t", "table: app.renamed"));
    succeeds(d.run(&["rename-table", "app.t", "app.renamed"]));
    let plan = connected_artifact(&d, connection, false);
    // Negative: execution checks the renamed holder, and a refusal rolls its
    // preceding rename back together with the drop.
    on_server(connection, "INSERT INTO app.t VALUES (2,2,0)");
    let refused = approved_apply(&d, connection, &plan, &["--allow", "rename,destructive"]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("1 row(s) of app.renamed violate"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM information_schema.columns WHERE table_schema='app' AND table_name='t' AND column_name='amount'"
        ),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM information_schema.tables WHERE table_schema='app' AND table_name='renamed'"
        ),
        0
    );
    on_server(connection, "UPDATE app.t SET cents=amount*100");
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename,destructive"],
    ));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM information_schema.columns WHERE table_schema='app' AND table_name='renamed' AND column_name='amount'"
        ),
        0
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM app.renamed WHERE cents=200"
        ),
        2
    );
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn recording_after_a_table_rename_follows_its_target_uid_on_postgres_18() {
    record_after_rename(&server());
}
#[test]
#[ignore = "needs live PostgreSQL 16"]
fn recording_after_a_table_rename_follows_its_target_uid_on_postgres_16() {
    record_after_rename(&old_server());
}

fn record_after_rename(server: &str) {
    let own = OwnDatabase::new(server, "require2400_record_rename");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "require2400_record_rename", BEFORE);
    let table_uid = ids(&d).tables.keys().next().unwrap().clone();
    d.table(&BEFORE.replace("app.t", "app.renamed"));
    succeeds(d.run(&["rename-table", "app.t", "app.renamed"]));
    d.table(&AFTER.replace("app.t", "app.renamed"));
    // A previous holder of this spelling is audit history, not this intent.
    let mut history = ids(&d);
    history.tombstones.insert(
        "c_999999".parse().unwrap(),
        pbps_model::Tombstone {
            was: "app.renamed.amount".into(),
            dropped_at: "2026-10-28".into(),
            reason: "older holder".into(),
            operator: "reviewer".into(),
            require: None,
        },
    );
    std::fs::write(
        d.dir.join("schema.ids.json"),
        serde_json::to_vec_pretty(&history).unwrap(),
    )
    .unwrap();
    succeeds(d.run(&[
        "drop",
        "app.renamed.amount",
        "--reason",
        "copied",
        "--require",
        PREDICATE,
        "--db",
        connection,
    ]));
    let recorded = ids(&d);
    assert_eq!(
        recorded
            .tombstones
            .values()
            .find_map(|t| t.require.as_ref())
            .unwrap()
            .table_uid,
        table_uid
    );
    let plan = connected_artifact(&d, connection, false);
    let saved: SavedPlan = serde_json::from_slice(&std::fs::read(plan).unwrap()).unwrap();
    assert_eq!(
        saved
            .changes
            .changes
            .iter()
            .find_map(|s| s.drop_guard.as_ref())
            .unwrap()
            .table_uid,
        table_uid
    );
    // Negative: the dependency follows this UID through the rename too.
    d.table(
        &AFTER
            .replace("app.t", "app.renamed")
            .replace("  cents:", "  amount_cents:"),
    );
    succeeds(d.run(&["rename", "app.renamed.cents", "amount_cents"]));
    d.commit();
    let refused = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("names a column changed by this plan; split the plan"),
        "{}",
        stderr(&refused)
    );
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn a_reused_table_name_does_not_match_the_dropped_holder_on_postgres_18() {
    reused_name(&server());
}
#[test]
#[ignore = "needs live PostgreSQL 16"]
fn a_reused_table_name_does_not_match_the_dropped_holder_on_postgres_16() {
    reused_name(&old_server());
}

fn reused_name(server: &str) {
    let own = OwnDatabase::new(server, "require2400_reuse");
    let connection = own.connection();
    let d = bootstrapped_demo(
        connection,
        "require2400_reuse",
        &BEFORE.replace("app.t", "app.old"),
    );
    let old_uid = ids(&d).tables.keys().next().unwrap().clone();
    on_server(connection, "INSERT INTO app.old VALUES (1,0,100)");
    let other = BEFORE
        .replace("app.t", "app.other")
        .replace("pk_t", "pk_other");
    std::fs::write(d.dir.join("schema/app.other.yml"), &other).unwrap();
    let setup = connected_artifact(&d, connection, false);
    succeeds(approved_apply(&d, connection, &setup, &[]));
    on_server(connection, "INSERT INTO app.other VALUES (2,2,200)");
    std::fs::remove_file(d.dir.join("schema/app.t.yml")).unwrap();
    succeeds(d.run(&[
        "drop-table",
        "app.old",
        "--reason",
        "archived",
        "--require",
        "amount > 0",
        "--db",
        connection,
    ]));
    std::fs::write(
        d.dir.join("schema/app.other.yml"),
        other
            .replace("app.other", "app.old")
            .replace("amount: {type: integer}", "amount: {type: bigint}"),
    )
    .unwrap();
    succeeds(d.run(&["rename-table", "app.other", "app.old"]));
    let plan = connected_artifact(&d, connection, false);
    let saved: SavedPlan = serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
    let guard = saved
        .changes
        .changes
        .iter()
        .find_map(|s| s.drop_guard.as_ref())
        .unwrap();
    assert_eq!(guard.table_uid, old_uid);
    assert_ne!(saved.ids.tables.keys().next().unwrap(), &old_uid);
    // Negative: the replacement has only good rows. It must not answer the
    // requirement belonging to the old holder, whose bad row still refuses.
    let refused = approved_apply(&d, connection, &plan, &["--allow", "rename,destructive"]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("1 row(s) of app.old violate"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM app.old WHERE amount=0"
        ),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM app.other WHERE amount=2"
        ),
        1
    );
    on_server(connection, "UPDATE app.old SET amount=1");
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename,destructive"],
    ));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM app.old WHERE id=2 AND amount=2"
        ),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM information_schema.columns WHERE table_schema='app' AND table_name='old' AND column_name='amount' AND data_type='bigint'"
        ),
        1
    );
}
