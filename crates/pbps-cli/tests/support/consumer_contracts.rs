//! These cases run with both flow harnesses; the public CLI owns the boundary.
use super::*;

fn config(d: &Demo, contracts: &str) {
    let path = d.dir.join("pbps.yml");
    let old = std::fs::read_to_string(&path).unwrap();
    let dialect = old.lines().next().unwrap();
    std::fs::write(path, format!("{dialect}\n{contracts}")).unwrap();
}
fn table(d: &Demo, columns: &str) {
    d.table(&format!("table: app.t\ncolumns:\n{columns}"));
}
fn ok(o: Output) {
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}
fn refused(d: &Demo, id: &str, args: &[&str]) -> serde_json::Value {
    let mut args = args.to_vec();
    args.extend(["--format", "json"]);
    let o = d.run(&args);
    assert_eq!(code(&o), 2, "{}{}", stdout(&o), stderr(&o));
    let json: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert!(
        json["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == id),
        "{json}"
    );
    json
}

#[test]
fn consumer_contracts_refuse_recorded_drops_without_publishing_and_retirement_permits_them() {
    let d = Demo::new("consumer-drop");
    table(&d, "  old: {type: integer}\n");
    ok(d.run(&["plan"]));
    d.commit();
    config(&d, "consumers:\n  billing-v41: {uses: [app.t.old]}\n");
    table(&d, "  kept: {type: integer}\n");
    ok(d.run(&["drop", "app.t.old", "--reason", "retired"]));
    let ids = d.dir.join("schema.ids.json");
    let before = std::fs::read(&ids).unwrap();
    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    std::fs::write(&plan, "keep plan").unwrap();
    std::fs::write(&sql, "keep sql").unwrap();
    let json = refused(
        &d,
        "consumer.breaking",
        &[
            "plan",
            "--out",
            plan.to_str().unwrap(),
            "--sql",
            sql.to_str().unwrap(),
        ],
    );
    let message = json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "consumer.breaking")
        .unwrap()["message"]
        .as_str()
        .unwrap();
    assert!(
        message.contains("billing-v41")
            && message.contains("consumers.billing-v41.uses: app.t.old")
            && message.contains("alongside"),
        "{message}"
    );
    assert_eq!(std::fs::read(&ids).unwrap(), before);
    assert_eq!(std::fs::read_to_string(&plan).unwrap(), "keep plan");
    assert_eq!(std::fs::read_to_string(&sql).unwrap(), "keep sql");
    config(&d, "");
    ok(d.run(&["plan", "--out", plan.to_str().unwrap()]));
}

#[test]
fn consumer_contracts_follow_renamed_uids_and_distinguish_narrowing_from_widening() {
    let d = Demo::new("consumer-rename");
    table(&d, "  old: {type: varchar(50)}\n");
    ok(d.run(&["plan"]));
    d.commit();
    table(&d, "  new: {type: varchar(50), renamed_from: old}\n");
    config(&d, "consumers:\n  billing-v41: {uses: [app.t.new]}\n");
    refused(&d, "consumer.breaking", &["plan"]);
    config(&d, "");
    ok(d.run(&["plan"]));
    ok(d.run(&["fmt"]));
    d.commit();
    config(&d, "consumers:\n  billing-v41: {uses: [app.t.new]}\n");
    table(&d, "  new: {type: varchar(20)}\n");
    refused(&d, "consumer.unproven", &["plan"]);
    table(
        &d,
        "  new: {type: varchar(100)}\n  another: {type: integer}\n",
    );
    ok(d.run(&["plan"]));
}

#[test]
fn consumer_contracts_validate_every_reference_and_plain_strings_have_no_kind_escape() {
    let d = Demo::new("consumer-validation");
    table(&d, "  old: {type: integer}\n");
    ok(d.run(&["plan"]));
    d.commit();
    config(&d, "consumers:\n  bad: {uses: [app.absent]}\n");
    refused(&d, "consumer.invalid", &["validate", "--since", "HEAD"]);
    config(&d, "consumers:\n  bad: {uses: [app.t.old, app.t.old]}\n");
    refused(&d, "consumer.invalid", &["validate"]);
    for contracts in [
        "consumers:\n  bad: {uses: [{kind: column, name: app.t.old}]}\n",
        "consumers:\n  bad: {uses: [app.t]}\n  bad: {uses: [app.t]}\n",
    ] {
        config(&d, contracts);
        let o = d.run(&["validate"]);
        assert_ne!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    }
    config(
        &d,
        "consumers:\n  good: {owner: team, note: rolling, uses: [app.t.old], writes: [app.t]}\n",
    );
    ok(d.run(&["validate", "--since", "HEAD"]));
}

#[test]
fn consumer_contracts_writes_have_their_own_unproven_rule_and_default_exception() {
    let d = Demo::new("consumer-writes");
    table(&d, "  old: {type: integer}\n");
    ok(d.run(&["plan"]));
    d.commit();
    config(&d, "consumers:\n  billing-v41: {writes: [app.t]}\n");
    table(&d, "  old: {type: integer, nullable: false}\n");
    let json = refused(&d, "consumer.unproven", &["plan"]);
    assert!(
        json["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "consumer.unproven"
                && f["message"]
                    .as_str()
                    .unwrap()
                    .contains("cannot be proven compatible")),
        "{json}"
    );
    config(
        &d,
        "consumers:\n  billing-v41: {writes: [app.t]}\npolicies:\n  rules: {consumer.unproven: warning}\n",
    );
    ok(d.run(&["plan"]));
    config(&d, "consumers:\n  billing-v41: {writes: [app.t]}\n");
    table(
        &d,
        "  old: {type: integer}\n  added: {type: integer, nullable: false, default: '0'}\n",
    );
    let o = d.run(&["plan", "--format", "json"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(!stdout(&o).contains("consumer.unproven"));
    table(
        &d,
        "  old: {type: integer}\n  added: {type: integer, nullable: false}\n",
    );
    refused(&d, "consumer.unproven", &["plan"]);
}

#[test]
fn consumer_contracts_suppressions_do_not_spill_between_unrelated_written_tables() {
    let d = Demo::new("consumer-suppression-roots");
    table(&d, "  old: {type: integer}\n");
    let second = d.dir.join("schema/second.yml");
    let declaration = "table: app.second\ncolumns:\n  old: {type: integer}\n";
    std::fs::write(&second, declaration).unwrap();
    ok(d.run(&["plan"]));
    d.commit();
    let contracts = "consumers:\n  writer: {writes: [app.t.old, app.second.old]}\npolicies:\n  suppress:\n    - {rule: consumer.unproven, on: app.second.old, reason: reviewed exception, until: '2099-01-01'}\n";
    config(&d, contracts);
    std::fs::write(second, format!("{declaration}checks:\n  ck: old > 0\n")).unwrap();
    let o = d.run(&["plan", "--format", "json"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(!stdout(&o).contains("consumer.unproven"));
    config(&d, &contracts.replace("2099-01-01", "2020-01-01"));
    let json = refused(&d, "consumer.unproven", &["plan"]);
    let findings: Vec<_> = json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["id"] == "consumer.unproven")
        .collect();
    assert_eq!(findings.len(), 1, "{json}");
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap()
            .contains("consumers.writer.writes: app.second.old")
    );
}

#[test]
fn consumer_contracts_accept_new_written_tables_with_foreign_keys() {
    let d = Demo::new("consumer-new-writer");
    table(&d, "  old: {type: integer, nullable: false}\n");
    d.table(
        "table: app.t\ncolumns:\n  old: {type: integer, nullable: false}\nprimary_key: [old]\n",
    );
    ok(d.run(&["plan"]));
    d.commit();
    std::fs::write(d.dir.join("schema/new.yml"), "table: app.new\ncolumns:\n  old: {type: integer}\nforeign_keys:\n  fk: {columns: [old], references: app.t(old)}\n").unwrap();
    config(&d, "consumers:\n  writer: {writes: [app.new.old]}\n");
    ok(d.run(&["plan"]));
    config(&d, "consumers:\n  writer: {writes: [app.t]}\n");
    refused(&d, "consumer.unproven", &["plan"]);
}

/// Called by one owned-database case on MSSQL and both supported PG engines.
pub(super) fn live(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "consumer2398");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("consumer-connected");
    d.table("table: app.t\ncolumns:\n  old: {type: varchar(50)}\n  other: {type: integer}\nchecks:\n  ck: old <> ''\n");
    std::fs::write(
        d.dir.join("schema/restored.yml"),
        format!("view: app.restored\ndescription: consumer report\n{}definition: SELECT old FROM app.t\n",
            if pg { "" } else { "schemabinding: true\n" }),
    ).unwrap();
    ok(d.run(&["plan"]));
    d.commit();
    ok(d.run(&["bootstrap", "--db", connection]));
    let contracts = "consumers:\n  billing-v41: {uses: [app.restored], writes: [app.t.old]}\n";
    config(&d, contracts);
    d.table("table: app.t\ncolumns:\n  old: {type: varchar(100)}\n  other: {type: integer}\nchecks:\n  ck: old <> ''\n");
    let restored = d.dir.join("schema/restored.yml");
    let declaration = std::fs::read_to_string(&restored).unwrap();
    for (case, desired) in [
        (
            "description",
            declaration.replace("consumer report", "consumer report updated"),
        ),
        (
            "formatting",
            declaration.replace(
                "definition: SELECT old FROM app.t",
                "definition: |\n  SELECT old\n  FROM app.t",
            ),
        ),
    ] {
        std::fs::write(&restored, desired).unwrap();
        let plan = d.dir.join(format!("{case}-restoration.json"));
        let o = d.run(&[
            "plan",
            "--db",
            connection,
            "--format",
            "json",
            "--out",
            plan.to_str().unwrap(),
        ]);
        assert_eq!(
            code(&o),
            0,
            "{case} restoration: {}{}",
            stdout(&o),
            stderr(&o)
        );
        if !pg {
            assert!(
                std::fs::read_to_string(&plan)
                    .unwrap()
                    .contains("drop_module"),
                "{case} must exercise a physical restoration: {}",
                stdout(&o)
            );
        }
    }
    std::fs::write(
        &restored,
        declaration.replace("SELECT old FROM app.t", "SELECT other FROM app.t"),
    )
    .unwrap();
    refused(&d, "consumer.unproven", &["plan", "--db", connection]);
    std::fs::write(&restored, declaration).unwrap();
    for declaration in [
        "table: app.t\ncolumns:\n  old: {type: varchar(50)}\n  other: {type: integer, nullable: false}\n",
        "table: app.t\ncolumns:\n  old: {type: varchar(50)}\n  other: {type: bigint, nullable: false}\n",
        "table: app.t\ncolumns:\n  old: {type: varchar(50)}\n  other: {type: integer}\nchecks:\n  ck: old <> 'blocked'\n",
        "table: app.t\ncolumns:\n  old: {type: varchar(50)}\n  other: {type: integer}\nunique:\n  uq: [other]\n",
        "table: app.t\ncolumns:\n  old: {type: varchar(50)}\n  other: {type: integer}\nindexes:\n  ux: {columns: [other], unique: true}\n",
    ] {
        d.table(declaration);
        refused(&d, "consumer.unproven", &["plan", "--db", connection]);
        config(&d, "");
        ok(d.run(&["plan", "--db", connection]));
        config(&d, contracts);
    }
    // Seal before adding a contract that would refuse the same plan now.
    config(&d, "");
    table(
        &d,
        "  old: {type: varchar(50), nullable: false}\n  other: {type: integer}\n",
    );
    let plan = d.dir.join("sealed.json");
    ok(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    let checksum = plan_checksum(&plan);
    config(&d, contracts);
    refused(&d, "consumer.unproven", &["plan", "--db", connection]);
    // Restoring the bound view keeps the ordinary destructive approval gate.
    ok(d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
        "--allow",
        "destructive,not-null",
    ]));
    ok(d.run(&["verify", "--db", connection]));
    {
        let own = OwnDatabase::new(server, "consumer_key2398");
        let connection = own.connection();
        on_server(connection, "CREATE SCHEMA app");
        let d = Demo::new("consumer-unnamed-key");
        let declared = "table: app.t\ncolumns:\n  old: {type: integer, nullable: false}\n  other: {type: integer, nullable: false}\nprimary_key: [old]\n";
        d.table(declared);
        ok(d.run(&["plan"]));
        d.commit();
        ok(d.run(&["bootstrap", "--db", connection]));
        config(&d, "consumers:\n  writer: {writes: [app.t]}\n");
        let wider = declared.replace("old: {type: integer", "old: {type: bigint");
        d.table(&wider);
        let plan = d.dir.join("unnamed-key.json");
        ok(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
        if !pg {
            assert!(
                std::fs::read_to_string(&plan)
                    .unwrap()
                    .contains("set_primary_key"),
                "the generated SQL Server key must be physically restored"
            );
        }
        ok(d.run(&[
            "apply",
            "--db",
            connection,
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&plan),
            "--allow",
            "destructive,constraint",
        ]));
        ok(d.run(&["verify", "--db", connection]));
        d.table(&wider.replace("primary_key: [old]", "primary_key: [other]"));
        refused(&d, "consumer.unproven", &["plan", "--db", connection]);
    }
    if pg {
        let own = OwnDatabase::new(server, "consumer_partitions2398");
        let connection = own.connection();
        on_server(connection, "CREATE SCHEMA app");
        let d = Demo::new("consumer-partitions");
        let parent = "table: app.t\ncolumns:\n  old: {type: integer}\npartition_by: [old]\npartitions:\n  leaf: {from: [0], to: [10]}\n";
        d.table(parent);
        let unrelated = d.dir.join("schema/unrelated.yml");
        let other = "table: app.unrelated\ncolumns:\n  old: {type: integer}\n";
        std::fs::write(&unrelated, other).unwrap();
        ok(d.run(&["plan"]));
        d.commit();
        ok(d.run(&["bootstrap", "--db", connection]));
        config(&d, "consumers:\n  writer: {writes: [app.leaf]}\n");
        d.table(&format!("{parent}checks:\n  ck: old >= 0\n"));
        refused(&d, "consumer.unproven", &["plan", "--db", connection]);
        config(&d, "");
        ok(d.run(&["plan", "--db", connection]));
        config(&d, "consumers:\n  writer: {writes: [app.leaf]}\n");
        d.table(parent);
        std::fs::write(&unrelated, format!("{other}checks:\n  ck: old >= 0\n")).unwrap();
        ok(d.run(&["plan", "--db", connection]));
        d.table(&parent.replace(
            "  old: {type: integer}\n",
            "  old: {type: integer}\n  added: {type: integer, nullable: false, default: '0'}\n",
        ));
        ok(d.run(&["plan"]));
        d.commit();
        ok(d.run(&["plan", "--db", connection]));
        let supplied = parent.replace(
            "  old: {type: integer}\n",
            "  old: {type: integer}\n  added: {type: integer, default: '0'}\n",
        ).replace("  leaf: {from: [0], to: [10]}",
            "  leaf:\n    from: [0]\n    to: [10]\n    columns:\n      added: {nullable: false, default: '1'}");
        d.table(&supplied);
        ok(d.run(&["plan", "--db", connection]));
        d.table(
            &supplied
                .replace(", default: '0'", "")
                .replace(", default: '1'", ""),
        );
        refused(&d, "consumer.unproven", &["plan", "--db", connection]);
        for attach in [false, true] {
            let own = OwnDatabase::new(server, "consumer_transition2398");
            let connection = own.connection();
            on_server(connection, "CREATE SCHEMA app");
            let d = Demo::new("consumer-attach-detach");
            let plain = "table: app.t\ncolumns:\n  old: {type: integer}\npartition_by: [old]\npartitions: {}\n";
            let leaf = d.dir.join("schema/leaf.yml");
            let set = |attached: bool| {
                d.table(if attached { parent } else { plain });
                if attached {
                    if leaf.exists() {
                        std::fs::remove_file(&leaf).unwrap();
                    }
                } else {
                    std::fs::write(&leaf, "table: app.leaf\ncolumns:\n  old: {type: integer}\n")
                        .unwrap();
                }
            };
            set(!attach);
            ok(d.run(&["plan"]));
            d.commit();
            ok(d.run(&["bootstrap", "--db", connection]));
            config(&d, "consumers:\n  reader: {uses: [app.leaf.old]}\n");
            set(attach);
            ok(d.run(&["plan"]));
            d.commit();
            let plan = d.dir.join("transition.json");
            let o = d.run(&[
                "plan",
                "--db",
                connection,
                "--format",
                "json",
                "--out",
                plan.to_str().unwrap(),
            ]);
            ok(o);
            assert!(std::fs::read_to_string(plan).unwrap().contains(if attach {
                "attach_partition"
            } else {
                "detach_partition"
            }));
        }
        let d = Demo::new("consumer-new-partition-writer");
        d.table(parent);
        config(&d, "consumers:\n  writer: {writes: [app.t]}\n");
        ok(d.run(&["plan"]));
    }
    // A view-column removal and a routine-interface change are opaque, not safe.
    let d = Demo::new("consumer-modules");
    table(&d, "  old: {type: integer}\n");
    let view = d.dir.join("schema/view.yml");
    std::fs::write(&view, "view: app.v\ndefinition: SELECT 1 AS a, 2 AS b\n").unwrap();
    let routine = d.dir.join("schema/routine.yml");
    let (name, body, changed) = if pg {
        (
            "app.f(integer)",
            "(i integer) RETURNS integer LANGUAGE sql AS $$ SELECT i $$",
            "(i integer) RETURNS integer LANGUAGE sql AS $$ SELECT i + 1 $$",
        )
    } else {
        (
            "app.f",
            "(@i int) RETURNS int AS BEGIN RETURN @i END",
            "(@i int, @j int) RETURNS int AS BEGIN RETURN @i + @j END",
        )
    };
    std::fs::write(
        &routine,
        format!("function: {name}\ndefinition: |\n  {body}\n"),
    )
    .unwrap();
    ok(d.run(&["plan"]));
    d.commit();
    let own = OwnDatabase::new(server, "consumer_modules2398");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    ok(d.run(&["bootstrap", "--db", connection]));
    config(
        &d,
        &format!(
            "consumers:\n  report: {{uses: [app.v, '{}']}}\n",
            if pg { "app.f(int)" } else { name }
        ),
    );
    std::fs::write(&view, "view: app.v\ndefinition: SELECT 1 AS a\n").unwrap();
    std::fs::write(
        &routine,
        format!("function: {name}\ndefinition: |\n  {changed}\n"),
    )
    .unwrap();
    let json = refused(&d, "consumer.unproven", &["plan", "--db", connection]);
    assert_eq!(
        json["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["id"] == "consumer.unproven")
            .count(),
        2,
        "{json}"
    );
    if pg {
        std::fs::write(&routine, "function: app.f(text)\ndefinition: (i text) RETURNS text LANGUAGE sql AS $$ SELECT i $$\n").unwrap();
        refused(&d, "consumer.breaking", &["plan", "--db", connection]);
    }
}
