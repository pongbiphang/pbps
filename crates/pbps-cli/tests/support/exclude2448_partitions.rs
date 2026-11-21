//! GiST indexes and exclusion constraints on partitioned tables (#2448 PR E, DEC-2448.6 and
//! DEC-2448.7). A GiST tree reads, plans and applies as any partition tree does, on both
//! pinned engines. A parent's exclusion is PostgreSQL 18's: 16 refuses it by name before any
//! statement, and 18 holds it under the engine's key rule, with a clone on every partition, a
//! drop that needs its record, a detach that keeps the clone as the partition's own and a
//! rebuild that builds it again. A partition's own exclusion is held on both engines, with the
//! record of its drop in its parent's identity file.
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
/// clones nothing) is left out of the pull.
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

/// The retirement of `constraint` on a partition, as the parent's identity file holds it: the
/// partition has no file of its own, so its section sits with the parent's (DEC-2448.7).
fn ids_holding(d: &Demo, constraint: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![d.dir.join(".pbps/ids")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if std::fs::read_to_string(&path)
                .is_ok_and(|t| t.contains("\"partition_exclusions\"") && t.contains(constraint))
            {
                found.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    found
}

/// A leaf's own exclusion on 16 and 18: read from the database, declared under its partition,
/// created on a partition added later, replaced, edited in its options (`ALTER INDEX` takes
/// those on a leaf) and dropped, whose drop needs its record, kept in the parent's identity
/// file under the partition's uid. Each step ends in an empty re-plan and a clean `verify`.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_partitions_own_exclusion_is_read_declared_created_replaced_and_dropped() {
    let _ = server();
    servers(|url, _| {
        let own = OwnDatabase::new(url, "exclude-own");
        let connection = own.connection();
        on_server(
            connection,
            &format!(
                "{TREE}; ALTER TABLE app.ev_1 ADD CONSTRAINT ev_1_own EXCLUDE USING btree (v WITH =)"
            ),
        );
        let d = adopt("exclude-own", connection);
        let path = d.dir.join("schema/app.ev.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        // Read as the partition's, under its entry, and the parent holds none.
        assert!(
            held.contains("ev_1_own:") && held.matches("exclude:").count() == 1,
            "{held}"
        );
        succeeds(d.run(&["validate"]));
        settled(&d, connection);

        // A partition added later with its own exclusion, beside the DEFAULT one.
        let with_new = held.replacen(
            "  ev_rest: default",
            "  ev_2:\n    from: [\"10\"]\n    to: [\"20\"]\n    exclude:\n      ev_2_own:\n        \
             method: btree\n        keys:\n          - column: v\n            operator: \"=\"\n  \
             ev_rest: default",
            1,
        );
        assert_ne!(with_new, held, "{held}");
        std::fs::write(&path, &with_new).unwrap();
        d.commit();
        let (plan, script) = plan_sql(&d, connection);
        assert!(
            script.contains("ALTER TABLE \"app\".\"ev_2\" ADD CONSTRAINT \"ev_2_own\" EXCLUDE"),
            "{script}"
        );
        succeeds(approved_apply(&d, connection, &plan, BOTH));
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_constraint WHERE contype = 'x' AND conislocal \
                 AND coninhcount = 0 AND conparentid = 0"
            ),
            2,
            "two own exclusions, no parent's"
        );
        assert!(try_on_server(connection, "INSERT INTO app.ev VALUES (11, 5, 'a')").is_ok());
        assert!(try_on_server(connection, "INSERT INTO app.ev VALUES (12, 5, 'b')").is_err());
        settled(&d, connection);

        // An option-only edit is `ALTER INDEX` on a leaf, unlike a partitioned parent's.
        let tuned = with_new.replacen(
            "      ev_2_own:\n        method: btree\n",
            "      ev_2_own:\n        method: btree\n        storage_parameters:\n          \
             fillfactor: 70\n",
            1,
        );
        assert_ne!(tuned, with_new);
        std::fs::write(&path, &tuned).unwrap();
        d.commit();
        let (plan, script) = plan_sql(&d, connection);
        assert!(
            script.contains("ALTER INDEX \"app\".\"ev_2_own\" SET (fillfactor")
                && !script.contains("DROP CONSTRAINT"),
            "{script}"
        );
        succeeds(approved_apply(&d, connection, &plan, &[]));
        settled(&d, connection);

        // A replacement: another key, one drop and one add in a transaction.
        let replaced = tuned.replacen(
            "      ev_1_own:\n        method: btree\n        keys:\n          - column: v\n            \
             operator: =\n",
            "      ev_1_own:\n        method: btree\n        keys:\n          - column: v\n            \
             operator: =\n          - column: note\n            operator: =\n",
            1,
        );
        assert_ne!(replaced, tuned, "{tuned}");
        std::fs::write(&path, &replaced).unwrap();
        d.commit();
        let (plan, script) = plan_sql(&d, connection);
        assert!(
            script.find("DROP CONSTRAINT \"ev_1_own\"").unwrap()
                < script.find("ADD CONSTRAINT \"ev_1_own\"").unwrap(),
            "{script}"
        );
        succeeds(approved_apply(&d, connection, &plan, BOTH));
        settled(&d, connection);

        // A drop is refused until its reason is recorded, in the parent's file.
        let dropped = replaced.replacen(
            "    exclude:\n      ev_2_own:\n        method: btree\n        storage_parameters:\n          \
             fillfactor: 70\n        keys:\n          - column: v\n            operator: \"=\"\n",
            "",
            1,
        );
        assert_ne!(dropped, replaced, "{replaced}");
        std::fs::write(&path, &dropped).unwrap();
        let refused = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("pbps drop-exclusion app.ev_2 ev_2_own --reason"),
            "{}",
            said(&refused)
        );
        succeeds(d.run(&[
            "drop-exclusion",
            "app.ev_2",
            "ev_2_own",
            "--reason",
            "the booking service enforces it",
        ]));
        d.commit();
        assert_eq!(
            ids_holding(&d, "ev_2_own"),
            ["app.ev.json"],
            "the parent's file"
        );
        let (plan, script) = plan_sql(&d, connection);
        assert!(
            script.contains("DROP CONSTRAINT \"ev_2_own\"") && !script.contains("DROP INDEX"),
            "{script}"
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
                "SELECT count(*) FROM pg_constraint WHERE conname = 'ev_2_own'"
            ),
            0
        );
        settled(&d, connection);
    });
}

/// What the engine takes for its parent's clone it takes on `ATTACH` and when the parent's is
/// added, and what it takes goes with the parent's exclusion (measured on 18.6, DEC-2448.7).
/// So an own equal to the parent's is refused offline whatever its timing, an attach of a table
/// whose own the parent's would take is refused by name, and an own that differs (here by its
/// `include`) coexists with the clone: it attaches, stays the partition's own and re-plans clean.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_partitions_own_exclusion_the_parents_would_take_is_refused_and_one_it_would_not_stays() {
    let _ = server();
    let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
    const SLOT_SQL: &str = "ALTER TABLE app.ev ADD CONSTRAINT one_per_slot EXCLUDE USING btree \
                            (k WITH =, v WITH =)";
    // The table is declared as a partition of the tree, in its own database.
    let attach = |name: &str, own_sql: &str| {
        let own = OwnDatabase::new(&url, name);
        on_server(
            own.connection(),
            &format!(
                "{TREE}; {SLOT_SQL}; CREATE TABLE app.late (k integer NOT NULL, v integer NOT NULL, \
                 note text, {own_sql})"
            ),
        );
        let d = adopt(name, own.connection());
        let path = d.dir.join("schema/app.ev.yml");
        let tree = std::fs::read_to_string(&path).unwrap();
        let late = d.dir.join("schema/app.late.yml");
        let late_text = std::fs::read_to_string(&late).unwrap();
        (own, d, path, tree, late, late_text)
    };

    // An own equal to the parent's in method, keys, operators, `include` and `where` is
    // refused offline, with another name and another timing alike; one that differs is not.
    let (own, d, path, tree, late, late_text) = attach(
        "exclude-own-equal",
        "CONSTRAINT late_own EXCLUDE USING btree (k WITH =, v WITH =)",
    );
    let connection = own.connection();
    assert_eq!(parent_constraints(connection, "one_per_slot"), 1);
    let twin = |extra: &str, include: &str| {
        tree.replacen(
            "  ev_rest: default",
            &format!(
                "  ev_rest: default\n  ev_2:\n    from: [\"10\"]\n    to: [\"20\"]\n    exclude:\n      \
                 twin:\n        method: btree\n{extra}        keys:\n          - column: k\n            \
                 operator: \"=\"\n          - column: v\n            operator: \"=\"\n{include}"
            ),
            1,
        )
    };
    for (extra, include) in [
        ("", ""),
        (
            "        deferrable: true\n        initially: deferred\n",
            "",
        ),
    ] {
        std::fs::write(&path, twin(extra, include)).unwrap();
        let refused = d.run(&["validate"]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("exclusion constraint `twin` is its parent's `one_per_slot`"),
            "{}",
            said(&refused)
        );
    }
    std::fs::write(&path, twin("", "        include: [note]\n")).unwrap();
    succeeds(d.run(&["validate"]));

    // `late` holds an own the parent's would take: attached, the engine would make it the
    // parent's clone. Declared as a partition without it, its drop needs its record; with the
    // record, the attach is refused by name (the base still holds the own), and nothing ran.
    std::fs::remove_file(&late).unwrap();
    let declared = tree.replacen(
        "  ev_rest: default",
        "  late: {from: [\"20\"], to: [\"30\"]}\n  ev_rest: default",
        1,
    );
    std::fs::write(&path, &declared).unwrap();
    succeeds(d.run(&[
        "drop-exclusion",
        "app.late",
        "late_own",
        "--reason",
        "the clone replaces it",
    ]));
    let refused = d.run(&["plan"]);
    assert_ne!(code(&refused), 0, "{}", said(&refused));
    assert!(
        said(&refused)
            .contains("its exclusion constraint `late_own` is its parent's `one_per_slot`"),
        "{}",
        said(&refused)
    );
    assert_eq!(clones(connection), 2, "nothing ran");
    let _ = (late_text, declared);

    // An own that differs by its `include` attaches, keeps its own, and gains a clone.
    let (own, d, path, tree, late, late_text) = attach(
        "exclude-own-differs",
        "CONSTRAINT late_own EXCLUDE USING btree (k WITH =, v WITH =) INCLUDE (note)",
    );
    let connection = own.connection();
    assert!(late_text.contains("late_own:"), "{late_text}");
    let own_entry: String = {
        let at = late_text.find("exclude:\n").unwrap();
        late_text[at..]
            .lines()
            .map(|l| format!("    {l}\n"))
            .collect()
    };
    std::fs::remove_file(&late).unwrap();
    std::fs::write(
        &path,
        tree.replacen(
            "  ev_rest: default",
            &format!(
                "  late:\n    from: [\"20\"]\n    to: [\"30\"]\n{own_entry}  ev_rest: default"
            ),
            1,
        ),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let (plan, script) = plan_sql(&d, connection);
    assert!(
        script.contains("ATTACH PARTITION \"app\".\"late\""),
        "{script}"
    );
    succeeds(approved_apply(&d, connection, &plan, BOTH));
    assert_eq!(clones(connection), 3, "the engine built late's clone");
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_constraint WHERE contype = 'x' \
             AND conrelid = 'app.late'::regclass"
        ),
        2,
        "its own beside the clone"
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_constraint WHERE conname = 'late_own' \
             AND conislocal AND conparentid = 0"
        ),
        1
    );
    settled(&d, connection);

    // A partition created with an own that differs from the parent's: the clone and the own
    // coexist, and the re-plan is empty.
    let held = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        held.replacen(
            "  ev_rest: default",
            "  made:\n    from: [\"30\"]\n    to: [\"40\"]\n    exclude:\n      made_own:\n        \
             method: btree\n        include: [note]\n        keys:\n          - column: k\n            \
             operator: \"=\"\n          - column: v\n            operator: \"=\"\n  ev_rest: default",
            1,
        ),
    )
    .unwrap();
    d.commit();
    let (plan, _) = plan_sql(&d, connection);
    succeeds(approved_apply(&d, connection, &plan, BOTH));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_constraint WHERE contype = 'x' \
             AND conrelid = 'app.made'::regclass"
        ),
        2,
        "the clone beside its own"
    );
    settled(&d, connection);
}

const REBUILD_TREE: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.p (k integer NOT NULL, bucket integer NOT NULL, label text) \
      PARTITION BY RANGE (k); \
    CREATE TABLE app.p1 PARTITION OF app.p FOR VALUES FROM (0) TO (10); \
    CREATE TABLE app.p2 PARTITION OF app.p DEFAULT; \
    ALTER TABLE app.p1 ADD CONSTRAINT p1_own EXCLUDE USING btree (label WITH =); \
    INSERT INTO app.p VALUES (1, 2, 'a'), (3, 4, 'b'), (30, 5, 'c')";

/// A rebuild keeps a partition's own exclusion (16 and 18) and builds a parent's again on 18:
/// built on the parent alone, then on each leaf and attached, so every leaf has its clone and
/// the parent's index is valid. The tree is re-keyed, the rows copied, and the empty re-plan and
/// `verify` find nothing to say.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_rebuild_keeps_a_partitions_own_exclusion_and_builds_a_parents_again_on_18() {
    let _ = server();
    servers(|url, current| {
        let own = OwnDatabase::new(url, "exclude-rebuild");
        let connection = own.connection();
        on_server(connection, REBUILD_TREE);
        if current {
            on_server(
                connection,
                "ALTER TABLE app.p ADD CONSTRAINT p_x EXCLUDE USING btree \
                 (k WITH =, bucket WITH =)",
            );
        }
        let d = adopt("exclude-rebuild", connection);
        let path = d.dir.join("schema/app.p.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        assert!(
            held.contains("p1_own:") && held.contains("partition_by: [k]"),
            "{held}"
        );
        std::fs::write(
            &path,
            held.replacen("partition_by: [k]", "partition_by: [bucket]", 1),
        )
        .unwrap();
        succeeds(d.run(&[
            "rebuild-table",
            "app.p",
            "--max-rows",
            "10",
            "--reason",
            "re-key",
        ]));
        d.commit();
        let artifact = d.dir.join("rebuild.json");
        let planned = succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--out",
            artifact.to_str().unwrap(),
        ]));
        assert!(
            stdout(&planned).contains("blocking rewrite"),
            "{}",
            stdout(&planned)
        );
        succeeds(approved_apply(
            &d,
            connection,
            &artifact,
            &["--allow", "destructive,rename,constraint,narrowing,revoke"],
        ));
        assert_eq!(scalar(connection, "SELECT count(*) FROM app.p"), 3);
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_constraint WHERE conname = 'p1_own' AND conislocal \
                 AND conparentid = 0 AND conrelid = 'app.p1'::regclass"
            ),
            1,
            "the own exclusion is rebuilt on the new leaf"
        );
        if current {
            assert_eq!(parent_constraints(connection, "p_x"), 1);
            assert_eq!(clones(connection), 2, "one clone for each leaf");
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_index WHERE indexrelid = 'app.p_x'::regclass \
                     AND indisvalid"
                ),
                1,
                "the parent's index is valid"
            );
            assert!(try_on_server(connection, "INSERT INTO app.p VALUES (1, 2, 'z')").is_err());
        }
        assert!(try_on_server(connection, "INSERT INTO app.p VALUES (4, 9, 'a')").is_err());
        settled(&d, connection);
    });
}

/// A rebuild whose target parent holds an exclusion is PostgreSQL 18's too: the parent's
/// `ADD .. EXCLUDE` on a partitioned table is `0A000` on 16, and the rebuild recipe carries it as a
/// part of its own kind, so the version gate reads that kind as it reads an added exclusion. 16
/// refuses by name before any statement and nothing changes; the partition's own exclusion,
/// which 16 holds, is not what it names.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_rebuild_with_a_parents_exclusion_is_refused_by_name_on_16_before_any_statement() {
    let _ = server();
    let url = std::env::var("PBPS_TEST_PG_OLD_DB").expect("PBPS_TEST_PG_OLD_DB");
    let own = OwnDatabase::new(&url, "exclude-rebuild-16");
    let connection = own.connection();
    on_server(connection, REBUILD_TREE);
    let d = adopt("exclude-rebuild-16", connection);
    let path = d.dir.join("schema/app.p.yml");
    let held = std::fs::read_to_string(&path).unwrap();
    assert!(held.contains("p1_own:"), "{held}");
    std::fs::write(
        &path,
        format!(
            "{}\nexclude:\n  p_x:\n    method: btree\n    keys:\n      - column: k\n        \
             operator: \"=\"\n      - column: bucket\n        operator: \"=\"\n",
            held.replacen("partition_by: [k]", "partition_by: [bucket]", 1)
                .trim_end()
        ),
    )
    .unwrap();
    succeeds(d.run(&["validate"]));
    succeeds(d.run(&[
        "rebuild-table",
        "app.p",
        "--max-rows",
        "10",
        "--reason",
        "re-key",
    ]));
    d.commit();
    let before = recorded(&d, connection);
    let oid = scalar(connection, "SELECT 'app.p'::regclass::oid::bigint");
    let artifact = d.dir.join("rebuild.json");
    let refused = d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        artifact.to_str().unwrap(),
    ]);
    assert_ne!(code(&refused), 0, "{}", said(&refused));
    assert!(
        said(&refused)
            .contains("app.p: an exclusion constraint on a partitioned table needs PostgreSQL 18"),
        "{}",
        said(&refused)
    );
    assert!(
        !said(&refused).contains("p1_own"),
        "a partition's own exclusion is held on 16 and is not named: {}",
        said(&refused)
    );
    assert_eq!(recorded(&d, connection), before);
    assert_eq!(
        scalar(connection, "SELECT 'app.p'::regclass::oid::bigint"),
        oid,
        "the table was not replaced"
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM app.p"), 3);
    assert_eq!(parent_constraints(connection, "p_x"), 0);
}

/// A partition's retired own exclusion survives a rebuild of its tree: the record of the drop
/// is the partition's, and the rebuild replaces the partition, so the record follows the
/// replacement of the same name instead of leaving the identity file refusing the approval as an
/// orphan (DEC-2448.4). On both engines, which hold a partition's own exclusion.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_rebuild_keeps_the_record_of_a_partitions_retired_own_exclusion() {
    let _ = server();
    servers(|url, _| {
        let own = OwnDatabase::new(url, "exclude-rebuild-retired");
        let connection = own.connection();
        on_server(connection, REBUILD_TREE);
        on_server(
            connection,
            "ALTER TABLE app.p1 ADD CONSTRAINT p1_old EXCLUDE USING btree (bucket WITH =)",
        );
        let d = adopt("exclude-rebuild-retired", connection);
        let path = d.dir.join("schema/app.p.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        assert!(
            held.contains("p1_old:") && held.contains("p1_own:"),
            "{held}"
        );
        // Retire `p1_old`: declared away, with the human's record of the drop.
        let at = held.find("p1_old:").unwrap();
        let end = held.find("p1_own:").unwrap();
        assert!(end > at, "p1_old is read before p1_own: {held}");
        std::fs::write(&path, format!("{}{}", &held[..at], &held[end..])).unwrap();
        succeeds(d.run(&[
            "drop-exclusion",
            "app.p1",
            "p1_old",
            "--reason",
            "not needed",
        ]));
        d.commit();
        let tree = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            tree.replacen("partition_by: [k]", "partition_by: [bucket]", 1),
        )
        .unwrap();
        succeeds(d.run(&[
            "rebuild-table",
            "app.p",
            "--max-rows",
            "10",
            "--reason",
            "re-key",
        ]));
        succeeds(d.run(&["validate"]));
        d.commit();
        let artifact = d.dir.join("rebuild.json");
        succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--out",
            artifact.to_str().unwrap(),
        ]));
        succeeds(approved_apply(
            &d,
            connection,
            &artifact,
            &["--allow", "destructive,rename,constraint,narrowing,revoke"],
        ));
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_constraint WHERE conname = 'p1_old'"
            ),
            0
        );
        assert_eq!(scalar(connection, "SELECT count(*) FROM app.p"), 3);
        settled(&d, connection);
    });
}

/// A rebuild builds a parent's exclusion again over the copied rows, so the routine its key
/// expression calls is judged before the ADD runs, as a plain `ADD .. EXCLUDE` over the same
/// expression is: a routine with no fixed `search_path` is refused with the fix and nothing is
/// changed, and the same routine with one is admitted and the tree is rebuilt. PostgreSQL 18's:
/// 16 holds no parent exclusion.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_rebuilds_parent_exclusion_calling_a_function_is_judged_before_it_runs_on_18() {
    let _ = server();
    let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
    for fixed in [false, true] {
        let name = format!("exclude-rebuild-fn-{fixed}");
        let own = OwnDatabase::new(&url, &name);
        let connection = own.connection().to_owned();
        let connection = connection.as_str();
        let path = if fixed {
            "SET search_path = pg_catalog, pg_temp "
        } else {
            ""
        };
        on_server(connection, REBUILD_TREE);
        on_server(
            connection,
            &format!(
                "CREATE FUNCTION app.ok(x integer) RETURNS integer LANGUAGE sql IMMUTABLE \
                   {path}AS 'SELECT x + 0'; \
                 ALTER TABLE app.p ADD CONSTRAINT p_x EXCLUDE USING btree \
                   (k WITH =, bucket WITH =, (app.ok(bucket)) WITH =)"
            ),
        );
        let d = adopt(&name, connection);
        let file = d.dir.join("schema/app.p.yml");
        let held = std::fs::read_to_string(&file).unwrap();
        assert!(held.contains("partition_by: [k]"), "{held}");
        std::fs::write(
            &file,
            held.replacen("partition_by: [k]", "partition_by: [bucket]", 1),
        )
        .unwrap();
        succeeds(d.run(&[
            "rebuild-table",
            "app.p",
            "--max-rows",
            "10",
            "--reason",
            "re-key",
        ]));
        d.commit();
        let oid = scalar(connection, "SELECT 'app.p'::regclass::oid::bigint");
        let artifact = d.dir.join("rebuild.json");
        let planned = succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--out",
            artifact.to_str().unwrap(),
        ]));
        assert!(
            stdout(&planned).contains("blocking rewrite"),
            "{}",
            stdout(&planned)
        );
        let allow = ["--allow", "destructive,rename,constraint,narrowing,revoke"];
        let applied = approved_apply(&d, connection, &artifact, &allow);
        let shown = said(&applied);
        let replaced = scalar(connection, "SELECT 'app.p'::regclass::oid::bigint") != oid;
        if fixed {
            assert_eq!(code(&applied), 0, "{shown}");
            assert!(replaced, "the table was replaced");
            assert_eq!(scalar(connection, "SELECT count(*) FROM app.p"), 3);
        } else {
            assert_ne!(code(&applied), 0, "{shown}");
            // Judged where the constraint is added, by the key's own refusal, and not later at
            // the copy that writes into the index, whose write guard speaks of an existing index.
            assert!(
                shown.contains("key 3 of exclusion constraint `p_x` on app.__pbps_rebuild_")
                    && shown.contains(
                        "ALTER FUNCTION app.ok(integer) SET search_path = pg_catalog, pg_temp"
                    )
                    && shown.contains("nothing has been changed"),
                "{shown}"
            );
            assert!(!replaced, "the table was not replaced");
        }
        assert_eq!(parent_constraints(connection, "p_x"), 1);
    }
}

/// A partition of a parent that holds an exclusion is detached on 18: the partition's clone
/// becomes its own, as the declaration of the detached table names it, behind the `destructive`
/// risk, with its rows kept and the parent's exclusion still holding the partitions left. A
/// declaration without the clone is refused by name before a plan exists.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_partition_of_a_parent_with_an_exclusion_is_detached_and_its_clone_becomes_its_own() {
    let _ = server();
    let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
    let own = OwnDatabase::new(&url, "exclude-detach");
    let connection = own.connection().to_owned();
    on_server(&connection, "CREATE SCHEMA app");
    let d = Demo::new("exclude-detach");
    let columns = "columns:\n  k: {type: integer, nullable: false}\n  v: {type: integer, nullable: false}\n  \
                   note: {type: text}\n";
    let slot = "  one_per_slot:\n    method: btree\n    keys:\n      - column: k\n        operator: \"=\"\n      \
                - column: v\n        operator: \"=\"\n";
    let tree = |partitions: &str| {
        std::fs::write(
            d.dir.join("schema/app.ev.yml"),
            format!(
                "table: app.ev\n{columns}partition_by: [k]\npartitions:\n{partitions}exclude:\n{slot}"
            ),
        )
        .unwrap();
    };
    let p1 = "  ev_1: {from: [\"0\"], to: [\"10\"]}\n";
    let p2 = "  ev_2: {from: [\"10\"], to: [\"20\"]}\n";
    tree(&format!("{p1}{p2}"));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", &connection]));
    assert_eq!(clones(&connection), 2);
    on_server(
        &connection,
        "INSERT INTO app.ev VALUES (1, 1, 'a'), (11, 1, 'b')",
    );

    // The clone is missing from the detached table's declaration: refused by name.
    tree(p2);
    let archived = d.dir.join("schema/app.ev_1.yml");
    std::fs::write(&archived, format!("table: app.ev_1\n{columns}")).unwrap();
    let refused = d.run(&["plan"]);
    assert_ne!(code(&refused), 0, "{}", said(&refused));
    assert!(
        said(&refused).contains("its parent's exclusion constraint `one_per_slot` is missing"),
        "{}",
        said(&refused)
    );

    // Declared with the clone under a name of its own: detached, the clone renamed.
    std::fs::write(
        &archived,
        format!(
            "table: app.ev_1\n{columns}exclude:\n  archived_slot:\n    method: btree\n    keys:\n      \
             - column: k\n        operator: \"=\"\n      - column: v\n        operator: \"=\"\n"
        ),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let planned = succeeds(d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]));
    assert!(
        stdout(&planned).contains("detach table app.ev_1 from app.ev"),
        "{}",
        stdout(&planned)
    );
    assert_ne!(
        code(&approved_apply(&d, &connection, &plan, &[])),
        0,
        "needs its risk"
    );
    succeeds(approved_apply(
        &d,
        &connection,
        &plan,
        &["--allow", "destructive"],
    ));
    assert_eq!(
        scalar(
            &connection,
            "SELECT count(*) FROM pg_constraint WHERE conname = 'archived_slot' AND contype = 'x' \
             AND conislocal AND coninhcount = 0 AND conparentid = 0 \
             AND conrelid = 'app.ev_1'::regclass"
        ),
        1,
        "the clone became the partition's own under the declared name"
    );
    assert_eq!(clones(&connection), 1, "the parent still clones to ev_2");
    assert_eq!(scalar(&connection, "SELECT count(*) FROM app.ev_1"), 1);
    assert!(try_on_server(&connection, "INSERT INTO app.ev_1 VALUES (1, 1, 'dup')").is_err());
    settled(&d, &connection);
}

/// A partition's own exclusion created out of band, of the parent's planned shape and not
/// declared, is read now (DEC-2448.7), so the database is seen as it is: drifted from the state
/// recorded, refused before any statement; adopted, the own is a drop that needs its record, and
/// with the record the plan drops it first, so the engine has nothing to take for the parent's
/// clone (#2971).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn an_undeclared_own_exclusion_the_parents_new_one_would_take_is_named_before_anything_runs() {
    let _ = server();
    let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
    let own = OwnDatabase::new(&url, "exclude-own-oob");
    let connection = own.connection();
    on_server(connection, TREE);
    let d = adopt("exclude-own-oob", connection);
    // Out of band, after the baseline: the partition's own, of the shape the parent will take.
    on_server(
        connection,
        "ALTER TABLE app.ev_1 ADD CONSTRAINT oob EXCLUDE USING btree (k WITH =, v WITH =)",
    );
    let path = d.dir.join("schema/app.ev.yml");
    let held = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{held}{SLOT}")).unwrap();
    d.commit();

    // The database differs from the state recorded, and says so before any statement.
    let before = recorded(&d, connection);
    let drifted = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&drifted), 0, "{}", said(&drifted));
    assert!(
        said(&drifted).contains("has drifted from the state recorded"),
        "{}",
        said(&drifted)
    );
    assert_eq!(parent_constraints(connection, "one_per_slot"), 0);
    assert_eq!(recorded(&d, connection), before);

    // Adopted as it is, the own is the base's and the declarations lack it: a drop, which
    // needs its record, and nothing runs until it has one.
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "the own was added by hand",
    ]));
    let refused = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&refused), 0, "{}", said(&refused));
    assert!(
        said(&refused).contains("pbps drop-exclusion app.ev_1 oob --reason"),
        "{}",
        said(&refused)
    );
    assert_eq!(parent_constraints(connection, "one_per_slot"), 0);

    // A drop is recorded against a declaration that held the constraint, and this one never
    // did: declare it as it stands first (the parent's own not yet added), then drop it.
    let declared_own = held.replacen(
        "  ev_1: {from: [\"0\"], to: [\"10\"]}",
        "  ev_1:\n    from: [\"0\"]\n    to: [\"10\"]\n    exclude:\n      oob:\n        method: btree\n        \
         keys:\n          - column: k\n            operator: \"=\"\n          - column: v\n            \
         operator: \"=\"",
        1,
    );
    assert_ne!(declared_own, held, "{held}");
    std::fs::write(&path, &declared_own).unwrap();
    d.commit();
    settled(&d, connection);
    std::fs::write(&path, format!("{held}{SLOT}")).unwrap();

    // With the drop recorded the own goes first, and every clone of the parent's is the engine's.
    succeeds(d.run(&[
        "drop-exclusion",
        "app.ev_1",
        "oob",
        "--reason",
        "the parent's replaces it",
    ]));
    d.commit();
    let (plan, script) = plan_sql(&d, connection);
    assert!(
        script.find("DROP CONSTRAINT \"oob\"").unwrap()
            < script.find("ADD CONSTRAINT \"one_per_slot\"").unwrap(),
        "{script}"
    );
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "destructive", "--allow", "constraint"],
    ));
    assert_eq!(parent_constraints(connection, "one_per_slot"), 1);
    assert_eq!(clones(connection), 2);
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_constraint WHERE conname = 'oob'"
        ),
        0
    );
    settled(&d, connection);
}

/// A partition's own exclusion that the parent's new one would take, replaced by another that
/// it would not, with the parent's added in the same plan: the own's drop runs before the
/// parent's add, which the engine needs (`cannot drop inherited constraint` otherwise,
/// measured on 18.7), so the plan applies and the next one is empty. The own kept as declared
/// is refused by name instead, and nothing runs (DEC-2448.7).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_standing_own_exclusion_the_plan_replaces_is_dropped_before_the_parents_new_one_on_18() {
    let _ = server();
    let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
    let own = OwnDatabase::new(&url, "exclude-own-replaced");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "{TREE}; ALTER TABLE app.ev_1 ADD CONSTRAINT ev_1_own EXCLUDE USING btree \
             (k WITH =, v WITH =)"
        ),
    );
    let d = adopt("exclude-own-replaced", connection);
    let path = d.dir.join("schema/app.ev.yml");
    let held = std::fs::read_to_string(&path).unwrap();
    assert!(held.contains("ev_1_own:"), "{held}");

    // Negative: the own kept as declared, beside the parent's new one, is refused by name.
    std::fs::write(&path, format!("{held}{SLOT}")).unwrap();
    let refused = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&refused), 0, "{}", said(&refused));
    assert!(
        said(&refused).contains("is its parent's `one_per_slot`"),
        "{}",
        said(&refused)
    );
    assert_eq!(
        parent_constraints(connection, "one_per_slot"),
        0,
        "nothing ran"
    );

    // The own replaced by one that differs (by its `include`), and the parent's added.
    let replaced = held.replacen(
        "ev_1_own:\n        method: btree\n",
        "ev_1_wide:\n        method: btree\n        include: [note]\n",
        1,
    );
    assert_ne!(replaced, held, "{held}");
    std::fs::write(&path, format!("{replaced}{SLOT}")).unwrap();
    succeeds(d.run(&[
        "drop-exclusion",
        "app.ev_1",
        "ev_1_own",
        "--reason",
        "the wider one replaces it",
    ]));
    d.commit();
    let (plan, script) = plan_sql(&d, connection);
    assert!(
        script.find("DROP CONSTRAINT \"ev_1_own\"").unwrap()
            < script.find("ADD CONSTRAINT \"one_per_slot\"").unwrap(),
        "{script}"
    );
    succeeds(approved_apply(&d, connection, &plan, BOTH));
    assert_eq!(parent_constraints(connection, "one_per_slot"), 1);
    assert_eq!(clones(connection), 2, "a clone on each partition");
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_constraint WHERE contype = 'x' \
             AND conrelid = 'app.ev_1'::regclass"
        ),
        2,
        "the wider own beside the clone"
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_constraint WHERE conname = 'ev_1_own'"
        ),
        0
    );
    settled(&d, connection);
}
