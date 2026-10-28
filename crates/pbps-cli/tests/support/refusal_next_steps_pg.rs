use super::{
    Demo, ONE_COLUMN, OwnDatabase, approved_apply, code, on_server, scalar, server, stderr, stdout,
    succeeds,
};

const SPELLING_GUIDANCE: &str = "Different spellings are treated as different zones";
const LOCK_GUIDANCE: &str = "write-blocking lock while it builds";

#[test]
fn partition_rebuild_refusals_name_the_open_tracking_issue_and_keep_their_exit_code() {
    let d = Demo::new("refusal2333-partition");
    let base = "table: app.p\ncolumns:\n  k: {type: integer}\n  bucket: {type: integer}\n\
                partition_by: [k]\npartitions:\n  p1: {from: [0], to: [10]}\n";
    d.table(base);
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    let clean = succeeds(d.run(&["plan", "--no-dev"]));
    assert!(!stdout(&clean).contains("#2298"));
    assert!(!stderr(&clean).contains("#2298"));
    assert!(!stdout(&clean).contains("#2296"));
    assert!(!stderr(&clean).contains("#2296"));

    let ids_path = d.dir.join("schema.ids.json");
    let ids = std::fs::read(&ids_path).unwrap();
    d.table(
        &base
            .replace("  bucket:", "  extra: {type: integer}\n  bucket:")
            .replace("partitions:\n  p1: {from: [0], to: [10]}\n", ""),
    );
    succeeds(d.run(&["drop-table", "app.p1", "--reason", "retired partition"]));
    let transition = d.run(&["plan", "--no-dev"]);
    assert_eq!(code(&transition), 1, "{}", stderr(&transition));
    assert!(stderr(&transition).contains("separate plans"));
    assert!(stderr(&transition).contains("issue #2296"));
    assert!(!stderr(&transition).contains("issue #2298"));

    std::fs::write(&ids_path, ids).unwrap();

    // A changed partition bound is still refused, but key-rebuild support
    // does not track that shape and must not be advertised for it.
    d.table(&base.replace("to: [10]", "to: [20]"));
    let bound = d.run(&["plan", "--no-dev"]);
    assert_eq!(code(&bound), 1, "{}", stderr(&bound));
    assert!(stderr(&bound).contains("change its partitioning"));
    assert!(!stderr(&bound).contains("issue #2298"));
    assert!(!stderr(&bound).contains("issue #2296"));

    for desired in [
        base.replace("partition_by: [k]", "partition_by: [bucket]"),
        base.replace("k: {type: integer}", "k: {type: bigint}"),
    ] {
        d.table(&desired);
        let refused = d.run(&["plan", "--no-dev"]);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(stderr(&refused).contains("partitioned table or a partition"));
        assert!(
            stderr(&refused).contains("issue #2298"),
            "{}",
            stderr(&refused)
        );
    }
    d.table(&base.replace("partition_by: [k]", "partition_by: [bucket]"));
    succeeds(d.run(&[
        "rebuild-table",
        "app.p",
        "--max-rows",
        "20",
        "--reason",
        "maintenance",
    ]));
    let refused = d.run(&["plan", "--no-dev"]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("requires connected expansion"));
    assert!(stderr(&refused).contains("pbps plan --db"));
}

#[test]
fn a_partial_index_keeps_its_emitted_lock_and_success_exit_code() {
    let d = Demo::new("refusal2333-index-sql");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    for (filter, concurrent) in [("    where: id > 0\n", false), ("", true)] {
        d.table(&format!(
            "{ONE_COLUMN}strategy: {{online: true}}\nindexes:\n  ix: \n    columns: [id]\n{filter}"
        ));
        let sql = d.dir.join("preview.sql");
        let out = succeeds(d.run(&["plan", "--no-dev", "--sql", sql.to_str().unwrap()]));
        assert_eq!(code(&out), 0);
        let script = std::fs::read_to_string(sql).unwrap();
        assert_eq!(
            script.contains("CREATE INDEX CONCURRENTLY"),
            concurrent,
            "{script}"
        );
        assert!(script.contains("CREATE INDEX"), "{script}");
    }
}

#[test]
#[ignore = "needs live PostgreSQL; set PBPS_TEST_PG_DB"]
fn alias_time_zones_refuse_with_exact_spelling_guidance_and_exit_one() {
    for staged in [false, true] {
        let own = OwnDatabase::new(&server(), &format!("refusal2333_zone_{staged}"));
        let connection = own.connection();
        let set_zone = |zone: &str| {
            on_server(
                connection,
                &format!("ALTER DATABASE \"{}\" SET timezone = '{zone}'", own.name),
            );
        };
        set_zone("UTC");
        on_server(connection, "CREATE SCHEMA app");
        let d = Demo::new(&format!("refusal2333-zone-{staged}"));
        d.table(ONE_COLUMN);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", connection]));
        d.table(&format!("{ONE_COLUMN}indexes:\n  ix: {{columns: [id]}}\n"));
        let plan = d.dir.join("plan.json");
        let mut args = vec!["plan", "--db", connection, "--out", plan.to_str().unwrap()];
        if staged {
            args.push("--staged");
        }
        succeeds(d.run(&args));
        let flags: &[&str] = if staged { &["--staged"] } else { &[] };
        set_zone("Etc/UTC");
        let refused = approved_apply(&d, connection, &plan, flags);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        for text in [
            "`Etc/UTC` now",
            "computed in `UTC`",
            SPELLING_GUIDANCE,
            "texts must match exactly",
        ] {
            assert!(stderr(&refused).contains(text), "{}", stderr(&refused));
        }
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_class WHERE oid = to_regclass('app.ix')"
            ),
            0
        );
        set_zone("UTC");
        let accepted = succeeds(approved_apply(&d, connection, &plan, flags));
        assert!(!stdout(&accepted).contains(SPELLING_GUIDANCE));
        assert!(!stderr(&accepted).contains(SPELLING_GUIDANCE));
    }
}

#[test]
#[ignore = "needs live PostgreSQL; set PBPS_TEST_PG_DB"]
fn each_drifted_object_prints_three_remedies_without_changing_exit_two_or_json_ids() {
    let own = OwnDatabase::new(&server(), "refusal2333_drift");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("refusal2333-drift");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/app.u.yml"),
        ONE_COLUMN.replace("app.t", "app.u").replace("pk_t", "pk_u"),
    )
    .unwrap();
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    let clean = succeeds(d.run(&["verify", "--db", connection]));
    assert!(!stdout(&clean).contains("Three ways out"));
    let clean_json = succeeds(d.run(&["verify", "--db", connection, "--format", "json"]));
    let clean_json: serde_json::Value = serde_json::from_slice(&clean_json.stdout).unwrap();
    assert_eq!(clean_json["result"], "ok");
    assert_eq!(
        database_findings(&clean_json),
        Vec::<&serde_json::Value>::new()
    );
    on_server(
        connection,
        "ALTER TABLE app.t ADD COLUMN stray integer; ALTER TABLE app.u ADD COLUMN stray integer",
    );
    let drift = d.run(&["verify", "--db", connection]);
    assert_eq!(code(&drift), 2, "{}{}", stdout(&drift), stderr(&drift));
    let text = stdout(&drift);
    for subject in ["app.t", "app.u"] {
        let after = text.split_once(subject).unwrap().1;
        let remedy = after.lines().nth(1).unwrap();
        for command in [
            "`pbps pull`",
            "`pbps plan --db`",
            "`pbps baseline --reason …`",
        ] {
            assert!(remedy.contains(command), "{text}");
        }
    }
    let json = d.run(&["verify", "--db", connection, "--format", "json"]);
    assert_eq!(code(&json), 2);
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["result"], "findings");
    let findings = database_findings(&report);
    assert_eq!(findings.len(), 1, "{report}");
    assert_eq!(findings[0]["id"], "state.drift");
    let remedy = findings[0]["remedy"].as_str().unwrap();
    for command in [
        "`pbps pull`",
        "`pbps plan --db`",
        "`pbps baseline --reason …`",
    ] {
        assert!(remedy.contains(command), "{report}");
    }
    assert_eq!(
        report["data"]["changes"]["changes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
#[ignore = "needs live PostgreSQL; set PBPS_TEST_PG_DB"]
fn partial_index_estimates_explain_the_write_lock_and_unfiltered_neighbours_do_not() {
    let own = OwnDatabase::new(&server(), "refusal2333_cost");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("refusal2333-cost");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    for (filter, blocked) in [("    where: id > 0\n", true), ("", false)] {
        d.table(&format!(
            "{ONE_COLUMN}strategy: {{online: true}}\nindexes:\n  ix:\n    columns: [id]\n{filter}"
        ));
        // Unfiltered concurrent builds require a staged plan, as before.
        let args = ["plan", "--db", connection, "--staged"];
        let human = succeeds(d.run(&args));
        assert_eq!(
            stdout(&human).contains(LOCK_GUIDANCE),
            blocked,
            "{}",
            stdout(&human)
        );
        let mut args = args.to_vec();
        args.extend(["--format", "json"]);
        let json = succeeds(d.run(&args));
        let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        let costs = report["data"]["cost"]["changes"].as_array().unwrap();
        let cost = costs
            .iter()
            .find(|c| {
                c["about"]
                    .as_str()
                    .is_some_and(|a| a.starts_with("building the index"))
            })
            .unwrap();
        assert_eq!(
            cost["about"].as_str().unwrap().contains(LOCK_GUIDANCE),
            blocked
        );
        assert_eq!(
            cost["blocks"],
            if blocked {
                "writes"
            } else {
                "neither reads nor writes, only other schema changes"
            }
        );
        if blocked {
            assert!(
                cost["about"]
                    .as_str()
                    .unwrap()
                    .contains("filter requires a pinned search_path")
            );
        }
    }
}

/// The findings about this test's own database. Roles are cluster-wide, so a
/// role created by a concurrently running test is reported as a
/// `state.unmanaged` note on every verify; it says nothing about this schema.
fn database_findings(report: &serde_json::Value) -> Vec<&serde_json::Value> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| finding["id"] != "state.unmanaged")
        .collect()
}
