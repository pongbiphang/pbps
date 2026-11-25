//! A new index on a standing partitioned parent over an expression key, beside a
//! partition's own index on the same expression (#2961, DEC-2961.1). The engine
//! compares an expression key's operator class, so under another class the own
//! is not taken as the clone: the plan builds the parent's index, and the own
//! stays the partition's. Under the same class it would be taken, which stays
//! refused by name, nothing changed. Each case runs on both pinned engines.
use super::{Demo, OwnDatabase, approved_apply, code, on_server, scalar, stderr, stdout, succeeds};

fn servers(mut check: impl FnMut(&str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).expect(variable));
    }
}

const SETUP: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, n integer) \
        PARTITION BY RANGE (k); \
    CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
    CREATE TABLE app.ev_2 PARTITION OF app.ev FOR VALUES FROM (10) TO (20); \
    CREATE INDEX ev_1_x ON app.ev_1 USING brin ((n + 1) int4_minmax_multi_ops); \
    INSERT INTO app.ev VALUES (1, 1, 1), (2, 11, 2)";

/// A parent index over an expression under another class than a partition's own
/// is planned and built, and the own is kept; under the same class the plan is
/// refused by name.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_parent_index_over_an_expression_under_another_class_leaves_a_partitions_own() {
    servers(|url| {
        let own = OwnDatabase::new(url, "adoption-class-2961");
        let conn = own.connection().to_owned();
        on_server(&conn, SETUP);
        let d = Demo::new("adoption-class-2961");
        succeeds(d.run(&["pull", "--db", &conn]));
        let path = d.dir.join("schema/app.ev.yml");
        d.commit();
        succeeds(d.run(&["baseline", "--db", &conn, "--reason", "adopt"]));
        // Each declaration is the pulled file with one parent index added.
        let pulled = std::fs::read_to_string(&path).unwrap();
        assert!(pulled.contains("\npartition_by:"), "{pulled}");
        let declare = |class: &str| {
            let index = format!(
                "\nindexes:\n  ev_x:\n    method: brin\n    keys:\n      \
                 - {{expression: \"n + 1\"{class}}}\n\npartition_by:"
            );
            std::fs::write(&path, pulled.replacen("\npartition_by:", &index, 1)).unwrap();
        };
        let held = |sql: &str| scalar(&conn, &format!("SELECT ({sql})::int::int8"));
        let taken =
            || held("SELECT count(*) FROM pg_inherits WHERE inhrelid = 'app.ev_1_x'::regclass");

        // Negative: under the same class the own would be the clone.
        declare(", opclass: int4_minmax_multi_ops");
        let o = d.run(&["plan"]);
        assert_ne!(code(&o), 0, "{}", stdout(&o));
        assert!(
            stderr(&o).contains("which can take app.ev_1's `ev_1_x`"),
            "{}",
            stderr(&o)
        );

        // Under the default class it is another index to the engine.
        declare("");
        succeeds(d.run(&["plan"]));
        d.commit();
        let plan = d.dir.join("add.json");
        succeeds(d.run(&["plan", "--db", &conn, "--out", plan.to_str().unwrap()]));
        succeeds(approved_apply(
            &d,
            &conn,
            &plan,
            &["--allow", "destructive,constraint"],
        ));
        succeeds(d.run(&["verify", "--db", &conn]));
        let next = succeeds(d.run(&["plan", "--db", &conn]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        assert_eq!(taken(), 0, "the partition's own index is still its own");
        assert_eq!(
            held("SELECT count(*) FROM pg_inherits WHERE inhparent = 'app.ev_x'::regclass"),
            2,
            "a clone on each partition"
        );
    });
}
