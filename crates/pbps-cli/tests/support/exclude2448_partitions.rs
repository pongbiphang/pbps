//! GiST indexes and exclusion constraints on partitioned tables (#2448 PR E1, DEC-2448.6).
//! A GiST tree reads, plans and applies as any partition tree does, on both pinned engines. A
//! parent's exclusion is PostgreSQL 18's: 16 refuses it by name before any statement, and 18
//! holds it under the engine's key rule, with a clone on every partition, a drop that needs its
//! record, and no rebuild or detach beside it. A partition's own exclusion stays refused.
use super::{
    Demo, OwnDatabase, approved_apply, code, json_output, on_server, scalar, server, stderr,
    stdout, succeeds, try_on_server,
};

fn said(output: &std::process::Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

const BOTH: &[&str] = &["--allow", "destructive", "--allow", "constraint"];

/// A project over the database as it stands, with its identities adopted.
fn adopt(name: &str, connection: &str) -> Demo {
    let d = Demo::new(name);
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    d
}

fn plan_sql(d: &Demo, connection: &str) -> (std::path::PathBuf, String) {
    // A plan reads the identities its reviewers read: a partition the declarations add is
    // given its uid by a local plan first, and committed.
    let _ = d.run(&["plan"]);
    let git = |args: &[&str]| {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&d.dir)
            .args(args)
            .output();
    };
    git(&["add", "-A"]);
    git(&["commit", "-qm", "ids"]);
    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    let out = d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 0, "{}", said(&out));
    (plan, std::fs::read_to_string(&sql).unwrap())
}

/// The declarations plan nothing and the database verifies.
fn settled(d: &Demo, connection: &str) {
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
    succeeds(d.run(&["verify", "--db", connection]));
}

fn recorded(d: &Demo, connection: &str) -> usize {
    let listed = json_output(succeeds(
        d.run(&["state", "list", "--db", connection, "--format", "json"]),
    ));
    listed["data"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] != "failed")
        .count()
}

/// The constraints of the tree that are clones of the parent's: `conparentid` set.
fn clones(connection: &str) -> i64 {
    scalar(
        connection,
        "SELECT count(*) FROM pg_constraint WHERE contype = 'x' AND conparentid <> 0",
    )
}

fn parent_constraints(connection: &str, name: &str) -> i64 {
    scalar(
        connection,
        &format!(
            "SELECT count(*) FROM pg_constraint WHERE conname = '{name}' AND contype = 'x' \
             AND conparentid = 0"
        ),
    )
}

const TREE: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (k integer NOT NULL, v integer NOT NULL, note text) \
      PARTITION BY RANGE (k); \
    CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
    CREATE TABLE app.ev_rest PARTITION OF app.ev DEFAULT; \
    INSERT INTO app.ev VALUES (1, 1, 'a'), (2, 2, 'b'), (50, 1, 'c')";

const SLOT: &str = "exclude:\n  one_per_slot:\n    method: btree\n    keys:\n      - column: k\n        \
                    operator: \"=\"\n      - column: v\n        operator: \"=\"\n";

fn servers(mut check: impl FnMut(&str, bool)) {
    for (variable, current) in [("PBPS_TEST_PG_OLD_DB", false), ("PBPS_TEST_PG_DB", true)] {
        check(&std::env::var(variable).expect(variable), current);
    }
}

/// A GiST tree: a parent's index with clones on its partitions, a partition's own GiST index
/// and the DEFAULT partition round-trip; a partition created with its own index is planned
/// and applied; a parent's new index that a partition's own could be taken for is refused by
/// name, and planned once the own one is dropped (DEC-1688.1, DEC-2448.6). On 16 and 18.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_gist_partition_tree_reads_plans_and_keeps_a_partitions_own_index_its_own() {
    let _ = server();
    servers(|url, _| {
        let own = OwnDatabase::new(url, "exclude-gist-tree");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; \
             CREATE TABLE app.ev (k integer NOT NULL, s integer NOT NULL, e integer NOT NULL, \
               note text) PARTITION BY RANGE (k); \
             CREATE INDEX ev_span ON app.ev USING gist (int4range(s, e)) INCLUDE (note); \
             CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
             CREATE TABLE app.ev_rest PARTITION OF app.ev DEFAULT; \
             CREATE INDEX ev_1_own ON app.ev_1 USING gist (int4range(s, s + 5)); \
             INSERT INTO app.ev VALUES (1, 1, 5, 'a'), (50, 1, 5, 'b')",
        );
        let d = adopt("exclude-gist-tree", connection);
        let path = d.dir.join("schema/app.ev.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        // The parent's index is the parent's, the partition's own is the partition's, and
        // the clones are nobody's to declare.
        assert!(
            held.contains("indexes:\n  ev_span:\n    method: gist")
                && held.contains("      ev_1_own:\n        method: gist"),
            "{held}"
        );
        assert!(!held.contains("ev_1_int4range"), "{held}");
        settled(&d, connection);

        // A partition created with its own GiST index beside the DEFAULT one.
        let with_new = held.replacen(
            "  ev_rest: default",
            "  ev_2:\n    from: [\"10\"]\n    to: [\"20\"]\n    indexes:\n      ev_2_own:\n        \
             method: gist\n        keys:\n          - expression: \"int4range(s, s + 7)\"\n  \
             ev_rest: default",
            1,
        );
        assert_ne!(with_new, held, "{held}");
        std::fs::write(&path, &with_new).unwrap();
        d.commit();
        let (plan, script) = plan_sql(&d, connection);
        assert!(script.contains("\"app\".\"ev_2\""), "{script}");
        succeeds(approved_apply(&d, connection, &plan, &[]));
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_index i WHERE i.indrelid = 'app.ev_2'::regclass \
                 AND EXISTS (SELECT 1 FROM pg_inherits h WHERE h.inhrelid = i.indexrelid)"
            ),
            1,
            "the parent's clone"
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_index i WHERE i.indrelid = 'app.ev_2'::regclass \
                 AND NOT EXISTS (SELECT 1 FROM pg_inherits h WHERE h.inhrelid = i.indexrelid)"
            ),
            1,
            "its own"
        );
        settled(&d, connection);

        // A second parent index a partition's own could be taken for: refused by name,
        // and nothing runs.
        let with_parent = with_new.replacen(
            "indexes:\n  ev_span:",
            "indexes:\n  ev_span2:\n    method: gist\n    keys:\n      - expression: \
             \"int4range(s, e + 1)\"\n  ev_span:",
            1,
        );
        assert_ne!(with_parent, with_new, "{with_new}");
        std::fs::write(&path, &with_parent).unwrap();
        d.commit();
        let refused = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("which can take app.ev_1's `ev_1_own`"),
            "{}",
            said(&refused)
        );
        // Without the own ones it is planned, and every partition gets its clone.
        let without_own = with_parent
            .replacen(
                "    indexes:\n      ev_1_own:\n        method: gist\n        keys:\n          - \
                 expression: \"int4range(s, s + 5)\"\n",
                "",
                1,
            )
            .replacen(
                "    indexes:\n      ev_2_own:\n        method: gist\n        keys:\n          - \
                 expression: \"int4range(s, s + 7)\"\n",
                "",
                1,
            );
        assert_ne!(without_own, with_parent);
        std::fs::write(&path, &without_own).unwrap();
        d.commit();
        let (plan, script) = plan_sql(&d, connection);
        assert!(
            script.find("DROP INDEX").unwrap() < script.find("ev_span2").unwrap(),
            "the own indexes go before the parent's is built: {script}"
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
                "SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
                 WHERE c.relname LIKE 'ev%' AND EXISTS \
                   (SELECT 1 FROM pg_inherits h WHERE h.inhrelid = i.indexrelid)"
            ),
            6,
            "two parent indexes, three partitions"
        );
        settled(&d, connection);
    });
}

/// PostgreSQL 16 has no exclusion on a partitioned table (`0A000`, measured on the raw
/// statement here). A declaration is valid offline, and a plan against 16 is refused by
/// name before any statement and writes nothing; 18 takes the same declaration.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_parents_exclusion_is_refused_by_name_on_16_and_planned_on_18() {
    let _ = server();
    servers(|url, current| {
        let own = OwnDatabase::new(url, "exclude-parent-version");
        let connection = own.connection();
        on_server(connection, TREE);
        let d = adopt("exclude-parent-version", connection);
        let path = d.dir.join("schema/app.ev.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{held}{SLOT}")).unwrap();
        d.commit();
        succeeds(d.run(&["validate"]));
        let before = recorded(&d, connection);
        if current {
            let (_, script) = plan_sql(&d, connection);
            assert!(
                script.contains("ADD CONSTRAINT \"one_per_slot\" EXCLUDE USING btree"),
                "{script}"
            );
            return;
        }
        let refused = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        let text = said(&refused);
        assert!(
            text.contains(
                "app.ev: an exclusion constraint on a partitioned table needs PostgreSQL 18"
            ),
            "{text}"
        );
        // The engine's own answer, the backstop.
        let raw = try_on_server(
            connection,
            "ALTER TABLE app.ev ADD CONSTRAINT x EXCLUDE USING btree (k WITH =)",
        )
        .unwrap_err();
        assert!(
            raw.contains("exclusion constraints are not supported on partitioned tables"),
            "{raw}"
        );
        assert_eq!(recorded(&d, connection), before);
        assert_eq!(parent_constraints(connection, "one_per_slot"), 0);
    });
}

/// The gate asks the catalog about the tables as it holds them now: a plan that renames a
/// standing partitioned parent and adds an exclusion to it names a table the catalog does not
/// hold yet, and 16 must still refuse it by name before any statement, not with `0A000`
/// mid-apply. 18 plans the same declaration.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_renamed_parents_exclusion_is_refused_by_its_old_name_on_16_and_planned_on_18() {
    let _ = server();
    servers(|url, current| {
        let own = OwnDatabase::new(url, "exclude-renamed-parent");
        let connection = own.connection();
        on_server(connection, TREE);
        let d = adopt("exclude-renamed-parent", connection);
        let old = d.dir.join("schema/app.ev.yml");
        let held = std::fs::read_to_string(&old).unwrap();
        let renamed = held.replacen("ev", "ev2", 1);
        assert_ne!(renamed, held, "{held}");
        std::fs::remove_file(&old).unwrap();
        std::fs::write(d.dir.join("schema/app.ev2.yml"), format!("{renamed}{SLOT}")).unwrap();
        succeeds(d.run(&["rename-table", "app.ev", "app.ev2"]));
        d.commit();
        succeeds(d.run(&["validate"]));
        let before = recorded(&d, connection);
        if current {
            let (_, script) = plan_sql(&d, connection);
            assert!(
                script.contains("RENAME TO \"ev2\"")
                    && script.contains("ADD CONSTRAINT \"one_per_slot\" EXCLUDE USING btree"),
                "{script}"
            );
            return;
        }
        let refused = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        let text = said(&refused);
        assert!(
            text.contains(
                "app.ev2: an exclusion constraint on a partitioned table needs PostgreSQL 18"
            ),
            "{text}"
        );
        // Nothing ran: the parent keeps its old name, and the engine's own answer is the
        // backstop for the statement the gate stood in front of.
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_class WHERE relname = 'ev' AND relkind = 'p'"
            ),
            1
        );
        let raw = try_on_server(
            connection,
            "ALTER TABLE app.ev ADD CONSTRAINT x EXCLUDE USING btree (k WITH =)",
        )
        .unwrap_err();
        assert!(
            raw.contains("exclusion constraints are not supported on partitioned tables"),
            "{raw}"
        );
        assert_eq!(recorded(&d, connection), before);
    });
}

/// On 18 a parent's exclusion is added behind the `constraint` risk with a clone on every
/// partition, a partition created later and the DEFAULT one, it enforces, an option edit is a
/// replacement (a partitioned index takes no `ALTER INDEX SET`), and a drop needs its record
/// and takes every clone with it.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_parents_exclusion_is_added_enforced_replaced_and_dropped_on_18() {
    let _ = server();
    let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
    let own = OwnDatabase::new(&url, "exclude-parent-18");
    let connection = own.connection();
    on_server(connection, TREE);
    let d = adopt("exclude-parent-18", connection);
    let path = d.dir.join("schema/app.ev.yml");
    let held = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{held}{SLOT}")).unwrap();
    d.commit();

    // Added: refused without the risk, then applied; one clone per partition.
    let (plan, script) = plan_sql(&d, connection);
    assert!(
        script.contains("ADD CONSTRAINT \"one_per_slot\" EXCLUDE USING btree"),
        "{script}"
    );
    assert_ne!(code(&approved_apply(&d, connection, &plan, &[])), 0);
    assert_eq!(parent_constraints(connection, "one_per_slot"), 0);
    succeeds(approved_apply(&d, connection, &plan, BOTH));
    assert_eq!(parent_constraints(connection, "one_per_slot"), 1);
    assert_eq!(clones(connection), 2);
    settled(&d, connection);
    // It reads back as the parent's alone: no clone is declared.
    let pulled = std::fs::read_to_string(&path).unwrap();
    assert_eq!(pulled.matches("one_per_slot:").count(), 1, "{pulled}");

    // It enforces, in a partition and through the DEFAULT one.
    assert!(try_on_server(connection, "INSERT INTO app.ev VALUES (1, 1, 'dup')").is_err());
    assert!(try_on_server(connection, "INSERT INTO app.ev VALUES (50, 1, 'dup')").is_err());
    assert!(try_on_server(connection, "INSERT INTO app.ev VALUES (1, 3, 'ok')").is_ok());

    // A partition created later gets a clone, and the plan states nothing about it.
    let with_new = std::fs::read_to_string(&path).unwrap().replacen(
        "  ev_rest: default",
        "  ev_2:\n    from: [\"10\"]\n    to: [\"20\"]\n  ev_rest: default",
        1,
    );
    std::fs::write(&path, &with_new).unwrap();
    d.commit();
    let (plan, script) = plan_sql(&d, connection);
    assert!(!script.contains("EXCLUDE"), "{script}");
    succeeds(approved_apply(&d, connection, &plan, &[]));
    assert_eq!(clones(connection), 3);
    settled(&d, connection);

    // An option edit is a drop and an add carrying the option, in one transaction.
    let tuned = with_new.replacen(
        "    method: btree\n",
        "    method: btree\n    storage_parameters:\n      fillfactor: 70\n",
        1,
    );
    assert_ne!(tuned, with_new, "{with_new}");
    std::fs::write(&path, &tuned).unwrap();
    d.commit();
    let (plan, script) = plan_sql(&d, connection);
    assert!(
        script.find("DROP CONSTRAINT \"one_per_slot\"").unwrap()
            < script.find("ADD CONSTRAINT \"one_per_slot\"").unwrap()
            && !script.contains("ALTER INDEX"),
        "{script}"
    );
    succeeds(approved_apply(&d, connection, &plan, BOTH));
    assert_eq!(clones(connection), 3);
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE relname LIKE 'ev%excl' \
             AND reloptions @> ARRAY['fillfactor=70']"
        ),
        3,
        "the clones are made with the option"
    );
    settled(&d, connection);

    // A drop is refused until its reason is recorded; then it takes every clone.
    std::fs::write(&path, with_new.replace(SLOT, "")).unwrap();
    let refused = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&refused), 0, "{}", said(&refused));
    assert!(
        said(&refused).contains("pbps drop-exclusion app.ev one_per_slot --reason"),
        "{}",
        said(&refused)
    );
    succeeds(d.run(&[
        "drop-exclusion",
        "app.ev",
        "one_per_slot",
        "--reason",
        "the booking service enforces it",
    ]));
    d.commit();
    let (plan, script) = plan_sql(&d, connection);
    assert!(
        script.contains("DROP CONSTRAINT \"one_per_slot\"") && !script.contains("DROP INDEX"),
        "{script}"
    );
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "destructive"],
    ));
    assert_eq!(parent_constraints(connection, "one_per_slot"), 0);
    assert_eq!(clones(connection), 0);
    settled(&d, connection);
}

/// What 18 holds a parent's exclusion to, refused by name before the engine: the partition key
/// as a plain `=` key (offline, at `validate`); a rows-refused add rolls back whole, with no
/// constraint and no state; a parent built `ONLY` (the engine leaves its index invalid and
/// clones nothing) is left out of the pull; and a partition's own exclusion stays refused.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_parents_exclusion_is_held_to_the_engines_rules_and_a_failed_add_changes_nothing() {
    let _ = server();
    let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
    let own = OwnDatabase::new(&url, "exclude-parent-rules");
    let connection = own.connection();
    on_server(connection, TREE);
    let d = adopt("exclude-parent-rules", connection);
    let path = d.dir.join("schema/app.ev.yml");
    let held = std::fs::read_to_string(&path).unwrap();

    // The partition key missing, as an expression, with another operator: each refused
    // offline, naming the column.
    for keys in [
        "      - column: v\n        operator: \"=\"\n",
        "      - expression: \"k + 0\"\n        operator: \"=\"\n      - column: v\n        \
         operator: \"=\"\n",
        "      - column: k\n        operator: \"<>\"\n",
    ] {
        let declared = format!("{held}exclude:\n  bad:\n    method: btree\n    keys:\n{keys}");
        std::fs::write(&path, declared).unwrap();
        let out = d.run(&["validate"]);
        assert_ne!(code(&out), 0, "{keys}: {}", said(&out));
        assert!(
            said(&out).contains("must hold the partition key column `k`"),
            "{}",
            said(&out)
        );
    }

    // A partition's own exclusion is not declared yet.
    let own_declared = held.replacen(
        "  ev_rest: default",
        "  ev_2:\n    from: [\"10\"]\n    to: [\"20\"]\n    exclude:\n      own:\n        method: \
         btree\n        keys:\n          - column: v\n            operator: \"=\"\n  ev_rest: \
         default",
        1,
    );
    std::fs::write(&path, own_declared).unwrap();
    assert_ne!(code(&d.run(&["validate"])), 0);

    // A rows-refused add: two rows of one partition collide, the engine refuses the whole
    // statement, and the apply leaves the database and the ledger as they were.
    on_server(
        connection,
        "INSERT INTO app.ev VALUES (3, 7, 'x'), (3, 7, 'y')",
    );
    std::fs::write(&path, format!("{held}{SLOT}")).unwrap();
    d.commit();
    let (plan, _) = plan_sql(&d, connection);
    let before = recorded(&d, connection);
    let failed = approved_apply(&d, connection, &plan, BOTH);
    assert_ne!(code(&failed), 0, "{}", said(&failed));
    assert_eq!(parent_constraints(connection, "one_per_slot"), 0);
    assert_eq!(clones(connection), 0);
    assert_eq!(recorded(&d, connection), before, "no state was recorded");

    // `ONLY` on the parent: the engine accepts it and leaves the parent's index invalid,
    // with no clone on any partition. The pull does not read it as a standing guarantee.
    on_server(connection, "DELETE FROM app.ev WHERE k = 3");
    on_server(
        connection,
        "ALTER TABLE ONLY app.ev ADD CONSTRAINT only_slot EXCLUDE USING btree (k WITH =, v WITH =)",
    );
    assert_eq!(clones(connection), 0);
    let fresh = Demo::new("exclude-parent-only");
    let pulled = succeeds(fresh.run(&["pull", "--db", connection]));
    assert!(
        said(&pulled).contains("indisvalid = false"),
        "{}",
        said(&pulled)
    );
    let text = std::fs::read_to_string(fresh.dir.join("schema/app.ev.yml")).unwrap();
    assert!(!text.contains("only_slot"), "{text}");
}
