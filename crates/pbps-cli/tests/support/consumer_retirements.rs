//! Retirement evidence shares the existing consumer evaluator on both dialects.
use super::*;

const COLUMNS: &str = "table: app.t\ncolumns:\n  old: {type: integer}\n  kept: {type: integer}\n";
const READS: &str =
    "consumers:\n  billing-v41: {uses: [app.t.old]}\n  unrelated: {uses: [app.t.kept]}\n";
const WRITES: &str = "consumers:\n  writer: {writes: [app.t]}\n";

fn ok(output: Output) -> String {
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    stdout(&output)
}
fn git(d: &Demo, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(&d.dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    stdout(&out).trim().into()
}
fn config(d: &Demo, contracts: &str) {
    let path = d.dir.join("pbps.yml");
    let old = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        path,
        format!("{}\n{contracts}", old.lines().next().unwrap()),
    )
    .unwrap();
}
fn start(slug: &str, contracts: &str) -> (Demo, String) {
    let d = Demo::new(slug);
    d.table(COLUMNS);
    config(&d, contracts);
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    let baseline = git(&d, &["rev-parse", "HEAD"]);
    (d, baseline)
}
fn retire(d: &Demo) -> String {
    config(d, "");
    d.commit();
    git(d, &["rev-parse", "HEAD"])
}
fn plan(d: &Demo, baseline: &str) -> (PathBuf, serde_json::Value) {
    let path = d.dir.join("retirement.json");
    let json = ok(d.run(&[
        "plan",
        "--no-dev",
        "--since",
        baseline,
        "--out",
        path.to_str().unwrap(),
        "--format",
        "json",
    ]));
    (path, serde_json::from_str(&json).unwrap())
}
fn notes(value: &serde_json::Value, id: &str) -> Vec<String> {
    value["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["id"] == id)
        .map(|f| {
            assert_eq!(f["severity"], "note");
            f["message"].as_str().unwrap().into()
        })
        .collect()
}
fn drop_old(d: &Demo) {
    d.table("table: app.t\ncolumns:\n  kept: {type: integer}\n");
    ok(d.run(&["drop", "app.t.old", "--reason", "old reader retired"]));
}
fn explain(path: &std::path::Path, json: bool) -> String {
    let mut command = Command::new(BIN);
    command
        .current_dir(std::env::temp_dir())
        .args(["explain", "--plan", path.to_str().unwrap()]);
    if json {
        command.args(["--format", "json"]);
    }
    ok(command.output().unwrap())
}

#[test]
fn consumer_retirements_list_only_relied_on_entries_and_survive_without_the_checkout() {
    let (d, baseline) = start("retirement-handoff", READS);
    let retiring = retire(&d);
    git(
        &d,
        &["commit", "--allow-empty", "-qm", "later unrelated work"],
    );
    drop_old(&d);
    let (path, report) = plan(&d, &baseline);
    let note = notes(&report, "consumer.retired");
    assert_eq!(note.len(), 1, "{report}");
    assert!(
        note[0].contains(&retiring)
            && note[0].contains("consumers.billing-v41.uses: app.t.old")
            && note[0].contains("consumer.breaking"),
        "{note:?}"
    );
    assert!(!note[0].contains("unrelated"));
    let mut saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let approved = saved.wire_fingerprint();
    for change in &mut saved.changes.changes {
        change.findings.retain(|f| f.id != "consumer.retired");
    }
    assert_ne!(
        approved,
        saved.wire_fingerprint(),
        "retirement evidence is checksum-pinned"
    );
    std::fs::rename(d.dir.join(".git"), d.dir.join("hidden-history")).unwrap();
    std::fs::write(d.dir.join("pbps.yml"), "invalid: [").unwrap();
    assert!(explain(&path, false).contains(&note[0]));
    let explained: serde_json::Value = serde_json::from_str(&explain(&path, true)).unwrap();
    assert_eq!(notes(&explained, "consumer.retired"), note);
}

#[test]
fn consumer_retirements_keep_other_entries_of_the_same_consumer_live() {
    let (d, baseline) = start(
        "retirement-one-entry",
        "consumers:\n  billing-v41: {uses: [app.t.old, app.t.kept]}\n",
    );
    config(&d, "consumers:\n  billing-v41: {uses: [app.t.kept]}\n");
    d.commit();
    let retiring = git(&d, &["rev-parse", "HEAD"]);
    drop_old(&d);
    let (_, report) = plan(&d, &baseline);
    let note = notes(&report, "consumer.retired");
    assert_eq!(note.len(), 1, "{report}");
    assert!(
        note[0].contains("consumers.billing-v41.uses: app.t.old") && note[0].contains(&retiring)
    );
    assert!(!note[0].contains("app.t.kept"));
    assert!(notes(&report, "consumer.retirement-unavailable").is_empty());
}

#[test]
fn consumer_retirements_use_the_latest_removal_after_reintroduction() {
    let (d, baseline) = start("retirement-reintroduced", READS);
    let earlier = retire(&d);
    config(&d, READS);
    d.commit();
    let last = retire(&d);
    git(&d, &["commit", "--allow-empty", "-qm", "after retirement"]);
    drop_old(&d);
    let (_, report) = plan(&d, &baseline);
    let note = notes(&report, "consumer.retired");
    assert_eq!(note.len(), 1);
    assert!(
        note[0].contains(&last) && !note[0].contains(&earlier),
        "{note:?}"
    );
}

#[test]
fn consumer_retirements_apply_the_same_absent_with_present_parent_rule_to_merges() {
    let (d, baseline) = start("retirement-merge", READS);
    let branch = git(&d, &["branch", "--show-current"]);
    git(&d, &["checkout", "-qb", "retiring"]);
    let side_removal = retire(&d);
    git(&d, &["checkout", &branch]);
    git(&d, &["commit", "--allow-empty", "-qm", "parallel work"]);
    git(
        &d,
        &["merge", "--no-ff", "-qm", "merge retirement", "retiring"],
    );
    let merge = git(&d, &["rev-parse", "HEAD"]);
    drop_old(&d);
    let (_, report) = plan(&d, &baseline);
    let note = notes(&report, "consumer.retired");
    assert_eq!(note.len(), 1);
    assert!(
        note[0].contains(&merge) && !note[0].contains(&side_removal),
        "{note:?}"
    );
}

#[test]
fn consumer_retirements_label_uncommitted_removals_and_omit_compatible_changes() {
    let (d, baseline) = start("retirement-working", WRITES);
    config(&d, "");
    d.table(&COLUMNS.replace(
        "old: {type: integer}",
        "old: {type: integer, nullable: false}",
    ));
    let (_, report) = plan(&d, &baseline);
    let note = notes(&report, "consumer.retired");
    assert_eq!(note.len(), 1);
    assert!(
        note[0].contains("working tree, not committed")
            && note[0].contains("consumer.unproven")
            && note[0].contains("cannot be proven compatible"),
        "{note:?}"
    );
    d.table(&format!(
        "{COLUMNS}  supplied: {{type: integer, nullable: false, default: '1'}}\n"
    ));
    let (_, report) = plan(&d, &baseline);
    assert!(notes(&report, "consumer.retired").is_empty(), "{report}");
}

#[test]
fn consumer_retirements_require_an_unsuppressed_error_under_this_plans_policy() {
    let (d, baseline) = start("retirement-policy", WRITES);
    let retiring = retire(&d);
    d.table(&COLUMNS.replace(
        "old: {type: integer}",
        "old: {type: integer, nullable: false}",
    ));
    for policy in [
        "rules: {consumer.unproven: warning}",
        "rules: {consumer.unproven: false}",
        "suppress: [{rule: consumer.unproven, reason: migration window, until: '9999-12-31'}]",
    ] {
        config(&d, &format!("policies:\n  {policy}\n"));
        let (_, report) = plan(&d, &baseline);
        assert!(
            notes(&report, "consumer.retired").is_empty(),
            "{policy}: {report}"
        );
    }
    config(
        &d,
        "policies:\n  suppress: [{rule: consumer.unproven, reason: old exception, until: '2000-01-01'}]\n",
    );
    let (_, report) = plan(&d, &baseline);
    let note = notes(&report, "consumer.retired");
    assert_eq!(note.len(), 1);
    assert!(note[0].contains(&retiring));
}

#[test]
fn consumer_retirements_follow_renamed_identities_instead_of_textual_removals() {
    let (d, baseline) = start(
        "retirement-identity",
        "consumers:\n  writer: {writes: [app.t.old]}\n",
    );
    d.table(&COLUMNS.replace(
        "  old: {type: integer}",
        "  renamed: {type: integer, renamed_from: old}",
    ));
    config(
        &d,
        "consumers:\n  writer: {writes: [app.t.renamed]}\npolicies:\n  rules: {consumer.breaking: warning}\n",
    );
    let (_, report) = plan(&d, &baseline);
    assert!(notes(&report, "consumer.retired").is_empty(), "{report}");
    assert!(
        notes(&report, "consumer.retirement-unavailable").is_empty(),
        "{report}"
    );
}

#[test]
fn consumer_retirements_distinguish_absent_config_from_unreadable_contract_history() {
    let (d, baseline) = start("retirement-bad-history", READS);
    // A real historical parse failure must never become an empty contract set.
    let valid = std::fs::read_to_string(d.dir.join("pbps.yml")).unwrap();
    std::fs::write(d.dir.join("pbps.yml"), "dialect: [").unwrap();
    d.commit();
    std::fs::write(d.dir.join("pbps.yml"), valid).unwrap();
    retire(&d);
    drop_old(&d);
    let (_, report) = plan(&d, &baseline);
    let unavailable = notes(&report, "consumer.retirement-unavailable");
    assert_eq!(unavailable.len(), 1, "{report}");
    assert!(unavailable[0].contains("does not parse"), "{unavailable:?}");
    assert!(notes(&report, "consumer.retired").is_empty());
}

#[test]
fn consumer_retirements_do_not_read_a_historical_config_directory_as_absent() {
    let (d, baseline) = start("retirement-config-directory", READS);
    let config = d.dir.join("pbps.yml");
    let valid = std::fs::read_to_string(&config).unwrap();
    std::fs::remove_file(&config).unwrap();
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join("nested"), "not a config file").unwrap();
    d.commit();
    std::fs::remove_dir_all(&config).unwrap();
    std::fs::write(config, valid).unwrap();
    retire(&d);
    drop_old(&d);
    let (_, report) = plan(&d, &baseline);
    let unavailable = notes(&report, "consumer.retirement-unavailable");
    assert_eq!(unavailable.len(), 1, "{report}");
    assert!(unavailable[0].contains("not a regular config file"));
    assert!(notes(&report, "consumer.retired").is_empty());
}

#[test]
fn consumer_retirements_treat_a_missing_historical_config_as_empty() {
    let (d, _) = start("retirement-absent-config", READS);
    let path = d.dir.join("pbps.yml");
    let valid = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    d.commit();
    let baseline = git(&d, &["rev-parse", "HEAD"]);
    std::fs::write(path, valid).unwrap();
    retire(&d);
    drop_old(&d);
    let (_, report) = plan(&d, &baseline);
    assert!(notes(&report, "consumer.retired").is_empty(), "{report}");
    assert!(
        notes(&report, "consumer.retirement-unavailable").is_empty(),
        "{report}"
    );
}

#[test]
fn consumer_retirements_name_snapshot_provenance_and_missing_revision_evidence() {
    let (d, baseline) = start("retirement-file-baseline", WRITES);
    let retiring = retire(&d);
    let ids: pbps_model::IdsFile =
        serde_json::from_str(&std::fs::read_to_string(d.dir.join("schema.ids.json")).unwrap())
            .unwrap();
    let mut shape = pbps_model::Table::default();
    for name in ["old", "kept"] {
        shape.columns.insert(
            name.into(),
            pbps_model::Column::new(pbps_model::ColumnType::simple("integer")),
        );
    }
    let schema = pbps_model::Schema {
        tables: [("app.t".parse().unwrap(), shape)].into(),
        ..Default::default()
    };
    let mut snapshot =
        pbps_model::StateSnapshot::new(pbps_model::StateKind::Baseline, schema, ids, "fixture");
    let source = d.dir.join("baseline.json");
    d.table(&COLUMNS.replace(
        "old: {type: integer}",
        "old: {type: integer, nullable: false}",
    ));
    for revision in [
        None,
        Some("ffffffffffffffffffffffffffffffffffffffff"),
        Some(baseline.as_str()),
    ] {
        snapshot.git_sha = revision.map(Into::into);
        std::fs::write(&source, serde_json::to_string(&snapshot).unwrap()).unwrap();
        let report: serde_json::Value = serde_json::from_str(&ok(d.run(&[
            "plan",
            "--no-dev",
            "--base",
            source.to_str().unwrap(),
            "--format",
            "json",
        ])))
        .unwrap();
        if revision == Some(baseline.as_str()) {
            let relied = notes(&report, "consumer.retired");
            assert_eq!(relied.len(), 1, "{report}");
            assert!(relied[0].contains(&retiring));
            assert!(notes(&report, "consumer.retirement-unavailable").is_empty());
        } else {
            let unavailable = notes(&report, "consumer.retirement-unavailable");
            assert_eq!(unavailable.len(), 1, "{report}");
            assert!(
                unavailable[0].contains(if revision.is_none() {
                    "no recorded Git commit"
                } else {
                    "ffffffffffffffffffffffffffffffffffffffff"
                }),
                "{unavailable:?}"
            );
            assert!(notes(&report, "consumer.retired").is_empty());
        }
    }
}

pub fn live(server: &str) {
    let own = OwnDatabase::new(server, "consumer_retirement");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("retirement-live");
    let pg = std::fs::read_to_string(d.dir.join("pbps.yml"))
        .unwrap()
        .contains("postgres");
    let columns = COLUMNS.replace("old: {type: integer}", "old: {type: varchar(50)}");
    d.table(&columns);
    std::fs::write(
        d.dir.join("schema/restored.yml"),
        format!(
            "view: app.restored\n{}definition: SELECT old FROM app.t\n",
            if pg { "" } else { "schemabinding: true\n" },
        ),
    )
    .unwrap();
    config(
        &d,
        "consumers:\n  writer: {writes: [app.t]}\n  reader: {uses: [app.restored]}\n",
    );
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    ok(d.run(&["bootstrap", "--db", connection]));
    // A later data-work entry stamps a different commit without establishing
    // schema. Retirement provenance must still use the successful apply below.
    let columns = format!("{columns}  expanded: {{type: integer}}\n");
    d.table(&columns);
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    let baseline = git(&d, &["rev-parse", "HEAD"]);
    let expand = d.dir.join("expand.json");
    ok(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        expand.to_str().unwrap(),
    ]));
    let expand_checksum = plan_checksum(&expand);
    ok(d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        expand.to_str().unwrap(),
        "--checksum",
        &expand_checksum,
    ]));
    let retiring = retire(&d);
    ok(d.run(&[
        "state",
        "record-data-work",
        "--db",
        connection,
        "--external-version",
        "retirement-fixture",
        "--expand-checksum",
        &expand_checksum,
        "--outcome",
        "completed",
        "--reason",
        "external work after retirement",
    ]));
    git(
        &d,
        &["commit", "--allow-empty", "-qm", "not the retiring commit"],
    );
    d.table(&columns.replace("varchar(50)", "varchar(100)"));
    let restored = d.dir.join("restoration.json");
    let compatible: serde_json::Value = serde_json::from_str(&ok(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        restored.to_str().unwrap(),
        "--format",
        "json",
    ])))
    .unwrap();
    assert!(
        notes(&compatible, "consumer.retired").is_empty(),
        "{compatible}"
    );
    assert!(
        notes(&compatible, "consumer.retirement-unavailable").is_empty(),
        "{compatible}"
    );
    if !pg {
        assert!(
            std::fs::read_to_string(&restored)
                .unwrap()
                .contains("drop_module")
        );
    }
    d.table(&columns.replace("varchar(50)}", "varchar(50), nullable: false}"));
    let path = d.dir.join("live-retirement.json");
    let report: serde_json::Value = serde_json::from_str(&ok(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        path.to_str().unwrap(),
        "--format",
        "json",
    ])))
    .unwrap();
    let note = notes(&report, "consumer.retired");
    assert_eq!(note.len(), 1, "deployed baseline {baseline}: {report}");
    assert!(note[0].contains(&retiring));
    let explained: serde_json::Value = serde_json::from_str(&explain(&path, true)).unwrap();
    assert_eq!(notes(&explained, "consumer.retired"), note);
    config(&d, WRITES);
    let refused = d.run(&["plan", "--db", connection, "--format", "json"]);
    assert_eq!(
        code(&refused),
        2,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    // Evidence is descriptive: a later contract never re-gates saved apply.
    ok(d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&path),
        "--allow",
        "destructive,not-null",
    ]));
    ok(d.run(&["verify", "--db", connection]));
}
