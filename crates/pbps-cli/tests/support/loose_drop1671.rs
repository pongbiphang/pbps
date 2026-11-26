//! A plan drops every own index, key and exclusion that a parent's might take,
//! and predicts no adoption (#1671, DEC-1671.1): ahead of an attach, and ahead
//! of a parent's new index or exclusion over standing partitions. What the
//! declarations keep is built again, and is the partition's own on the engine.
//! The index cases run on both pinned engines; the exclusion ones on 18, which
//! alone takes an exclusion on a partitioned table.
use super::{
    Demo, OwnDatabase, approved_apply, on_server, scalar, stderr, stdout, succeeds, text_of,
};

fn servers(mut check: impl FnMut(&str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).expect(variable));
    }
}

/// A project over the database as it stands, with its identities adopted.
fn adopt(name: &str, connection: &str) -> Demo {
    let d = Demo::new(name);
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    d
}

/// Plans the declarations against the database, applies the plan, and holds the
/// database to them: verified, and nothing left to plan. The script it ran.
fn applied(d: &Demo, connection: &str, name: &str) -> String {
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join(format!("{name}.json"));
    let sql = d.dir.join(format!("{name}.sql"));
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    succeeds(approved_apply(
        d,
        connection,
        &plan,
        &["--allow", "destructive,constraint"],
    ));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(
        stdout(&next).contains("No changes"),
        "{}{}",
        stdout(&next),
        stderr(&next)
    );
    std::fs::read_to_string(sql).unwrap()
}

/// Where `statement` starts in `script`, which holds it.
fn at(script: &str, statement: &str) -> usize {
    script
        .find(statement)
        .unwrap_or_else(|| panic!("missing {statement}:\n{script}"))
}

fn count(connection: &str, from: &str) -> i64 {
    scalar(connection, &format!("SELECT count(*) {from}"))
}

/// The index's parent in the partition tree, or nothing where it is the table's
/// own.
fn parent_of(connection: &str, index: &str) -> String {
    text_of(
        connection,
        &format!(
            "SELECT coalesce((SELECT inhparent::regclass::text FROM pg_inherits \
             WHERE inhrelid = '{index}'::regclass), '')"
        ),
    )
}

fn oid(connection: &str, index: &str) -> String {
    text_of(
        connection,
        &format!("SELECT '{index}'::regclass::oid::text"),
    )
}

/// An ordinary table attached over what the parent's indexes might take: two
/// candidates of one index (S2), an expression and a predicate (S3), a unique
/// constraint whose index a plain unique index might take (S4), and a parent
/// index written `((o))`, which the engine and the model hold as the column
/// (S6). Each candidate goes ahead of the attach and the engine builds the
/// clone; the lone certain one is adopted as it stands; what the declaration
/// keeps is added after, as the partition's own. Negative: an own index no
/// index of the parent's might take is neither dropped nor rebuilt.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn an_attach_drops_what_the_parents_indexes_might_take_and_adds_back_what_is_declared() {
    servers(|url| {
        let own = OwnDatabase::new(url, "loose-attach-1671");
        let conn = own.connection().to_owned();
        on_server(
            &conn,
            "CREATE SCHEMA app; \
             CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, m integer, \
               n integer, o integer) PARTITION BY RANGE (k); \
             CREATE INDEX ev_m ON app.ev (m); \
             CREATE INDEX ev_n ON app.ev ((n + 1), k) WHERE n > 0; \
             CREATE INDEX ev_o ON app.ev (((o))); \
             CREATE UNIQUE INDEX ev_u ON app.ev (id, k); \
             ALTER TABLE app.ev ADD CONSTRAINT ev_uq UNIQUE (id, k); \
             CREATE TABLE app.late (id integer NOT NULL, k integer NOT NULL, m integer, \
               n integer, o integer); \
             ALTER TABLE app.late ADD CONSTRAINT late_uq UNIQUE (id, k); \
             CREATE INDEX late_m_desc ON app.late (m DESC); \
             CREATE INDEX late_m ON app.late (m); \
             CREATE INDEX late_n ON app.late ((n+1), k) WHERE n>0; \
             CREATE INDEX late_o ON app.late (o); \
             CREATE INDEX late_two ON app.late ((m * 2), o); \
             INSERT INTO app.late VALUES (1, 1, 1, 1, 1)",
        );
        let (adopted, unrelated) = (oid(&conn, "app.late_o"), oid(&conn, "app.late_two"));
        let d = adopt("loose-attach-1671", &conn);
        let parent = d.dir.join("schema/app.ev.yml");
        let tree = std::fs::read_to_string(&parent).unwrap();
        std::fs::remove_file(d.dir.join("schema/app.late.yml")).unwrap();
        // `late` keeps three indexes of its own; the engine's clones are
        // nobody's to declare.
        std::fs::write(
            &parent,
            format!(
                "{tree}partitions:\n  late:\n    from: [0]\n    to: [10]\n    indexes:\n      \
                 late_m:\n        columns: [m]\n      late_n:\n        keys:\n          \
                 - expression: (n + 1)\n          - column: k\n        where: (n > 0)\n      \
                 late_two:\n        keys:\n          - expression: (m * 2)\n          \
                 - column: o\n"
            ),
        )
        .unwrap();
        let script = applied(&d, &conn, "attach");
        let attach = at(&script, "ATTACH PARTITION \"app\".\"late\"");
        for name in ["late_m", "late_m_desc", "late_n"] {
            assert!(
                at(&script, &format!("DROP INDEX \"app\".\"{name}\"")) < attach,
                "{script}"
            );
        }
        assert!(
            at(&script, "DROP CONSTRAINT \"late_uq\"") < attach,
            "{script}"
        );
        for name in ["late_m", "late_n"] {
            assert!(
                attach < at(&script, &format!("CREATE INDEX \"{name}\"")),
                "{script}"
            );
            assert_eq!(parent_of(&conn, &format!("app.{name}")), "", "{name}");
        }
        // Negative: neither the certain one nor the unrelated one is touched.
        for name in ["late_o", "late_two"] {
            assert!(!script.contains(&format!("\"{name}\"")), "{script}");
        }
        assert_eq!(parent_of(&conn, "app.late_o"), "app.ev_o");
        assert_eq!(oid(&conn, "app.late_o"), adopted);
        assert_eq!(parent_of(&conn, "app.late_two"), "");
        assert_eq!(oid(&conn, "app.late_two"), unrelated);
        // Every index of the parent's has its clone on the partition, and the
        // partition holds no key of its own.
        assert_eq!(
            count(
                &conn,
                "FROM pg_index i JOIN pg_inherits h ON h.inhrelid = i.indexrelid \
                 WHERE i.indrelid = 'app.late'::regclass"
            ),
            5
        );
        assert_eq!(
            count(
                &conn,
                "FROM pg_constraint WHERE conrelid = 'app.late'::regclass \
                 AND contype IN ('p', 'u') AND conparentid = 0"
            ),
            0
        );
    });
}

/// A standing parent's new indexes over a partition's own they might take: an
/// equal one, one whose predicate differs, an expression under a parent's other
/// expression, a column a parent's expression reads, and one whose predicate is
/// respelled (#2961's cases among them). Each own goes ahead of the parent's and
/// is added after it, and stays the partition's own, with a clone of the
/// parent's beside it. Negatives: a column no parent's key reads, and an own
/// over another number of keys, are not touched.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_parents_new_index_rebuilds_each_standing_own_it_might_take() {
    servers(|url| {
        let own = OwnDatabase::new(url, "loose-parent-1671");
        let conn = own.connection().to_owned();
        on_server(
            &conn,
            "CREATE SCHEMA app; \
             CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, m integer, \
               n integer, o integer, p integer) PARTITION BY RANGE (k); \
             CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
             CREATE TABLE app.ev_2 PARTITION OF app.ev FOR VALUES FROM (10) TO (20); \
             CREATE INDEX ev_1_m ON app.ev_1 (m); \
             CREATE INDEX ev_1_n ON app.ev_1 (n) WHERE n > 1; \
             CREATE INDEX ev_1_o ON app.ev_1 (o); \
             CREATE INDEX ev_1_e ON app.ev_1 ((m * 2)); \
             CREATE INDEX ev_1_s ON app.ev_1 (id) WHERE o > 0; \
             CREATE INDEX ev_1_two ON app.ev_1 (n, o); \
             CREATE INDEX ev_1_p ON app.ev_1 (p); \
             INSERT INTO app.ev VALUES (1, 1, 1, 1, 1, 1), (2, 11, 2, 2, 2, 2)",
        );
        let untouched = ["ev_1_p", "ev_1_two"].map(|name| {
            let index = format!("app.{name}");
            (name, oid(&conn, &index))
        });
        let d = adopt("loose-parent-1671", &conn);
        let parent = d.dir.join("schema/app.ev.yml");
        let tree = std::fs::read_to_string(&parent).unwrap();
        assert!(tree.contains("\npartition_by:"), "{tree}");
        let indexes = "\nindexes:\n  ev_m:\n    columns: [m]\n  ev_n:\n    columns: [\"n\"]\n    \
                       where: \"n > 0\"\n  ev_o:\n    keys:\n      - expression: \"o + 1\"\n  \
                       ev_s:\n    columns: [id]\n    where: \"o>0\"\n\npartition_by:";
        std::fs::write(&parent, tree.replacen("\npartition_by:", indexes, 1)).unwrap();
        let script = applied(&d, &conn, "add");
        for (theirs, mine) in [
            ("ev_m", "ev_1_m"),
            ("ev_n", "ev_1_n"),
            ("ev_o", "ev_1_e"),
            ("ev_o", "ev_1_o"),
            ("ev_s", "ev_1_s"),
        ] {
            let dropped = at(&script, &format!("DROP INDEX \"app\".\"{mine}\""));
            let built = at(&script, &format!("CREATE INDEX \"{theirs}\""));
            let again = at(&script, &format!("CREATE INDEX \"{mine}\""));
            assert!(dropped < built && built < again, "{mine}: {script}");
            assert_eq!(parent_of(&conn, &format!("app.{mine}")), "", "{mine}");
            assert_eq!(
                count(
                    &conn,
                    &format!("FROM pg_inherits WHERE inhparent = 'app.{theirs}'::regclass")
                ),
                2,
                "{theirs}: a clone on each partition"
            );
        }
        for (name, held) in untouched {
            assert!(!script.contains(&format!("\"{name}\"")), "{script}");
            assert_eq!(oid(&conn, &format!("app.{name}")), held, "{name}");
            assert_eq!(parent_of(&conn, &format!("app.{name}")), "", "{name}");
        }
    });
}

/// A partition's own unique index that a foreign key references, with an
/// `INCLUDE` column the parent's lacks, is not rebuilt: the engine does not take
/// it, so the parent's new one is planned alone, and the key keeps referencing
/// the partition's own (DEC-1671.1). Rebuilt, the key would be bound to the
/// parent's clone and the apply refused.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_referenced_own_unique_index_the_engine_leaves_is_not_rebuilt_under_a_parents() {
    servers(|url| {
        let own = OwnDatabase::new(url, "loose-referenced-1671");
        let conn = own.connection().to_owned();
        on_server(
            &conn,
            "CREATE SCHEMA app; \
             CREATE TABLE app.ev (k integer NOT NULL, m integer) PARTITION BY RANGE (k); \
             CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
             CREATE UNIQUE INDEX ev_1_u ON app.ev_1 (k) INCLUDE (m); \
             CREATE TABLE app.r (k integer); \
             ALTER TABLE app.r ADD CONSTRAINT r_fk FOREIGN KEY (k) REFERENCES app.ev_1 (k); \
             INSERT INTO app.ev VALUES (1, 1); INSERT INTO app.r VALUES (1)",
        );
        let held = oid(&conn, "app.ev_1_u");
        let d = adopt("loose-referenced-1671", &conn);
        let parent = d.dir.join("schema/app.ev.yml");
        let tree = std::fs::read_to_string(&parent).unwrap();
        assert!(tree.contains("\npartition_by:"), "{tree}");
        let index = "\nindexes:\n  ev_u:\n    columns: [k]\n    unique: true\n\npartition_by:";
        std::fs::write(&parent, tree.replacen("\npartition_by:", index, 1)).unwrap();
        let script = applied(&d, &conn, "add");
        assert!(script.contains("CREATE UNIQUE INDEX \"ev_u\""), "{script}");
        for name in ["ev_1_u", "r_fk"] {
            assert!(!script.contains(&format!("\"{name}\"")), "{script}");
        }
        assert_eq!(oid(&conn, "app.ev_1_u"), held);
        assert_eq!(parent_of(&conn, "app.ev_1_u"), "");
        assert_eq!(
            count(
                &conn,
                "FROM pg_constraint WHERE conname = 'r_fk' \
                 AND conrelid = 'app.r'::regclass AND conindid = 'app.ev_1_u'::regclass"
            ),
            1,
            "the key still references the partition's own"
        );
        assert_eq!(
            count(
                &conn,
                "FROM pg_inherits WHERE inhparent = 'app.ev_u'::regclass"
            ),
            1,
            "a clone on the partition"
        );
    });
}

/// On 18, an exclusion of a partition's own that differs from the parent's only
/// in its `INCLUDE`: rebuilt around a parent's new one, and dropped ahead of an
/// attach and added after it. Either way it is the partition's own afterwards,
/// beside the parent's clone. Negative: one over another number of keys is not
/// touched.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn an_own_exclusion_is_rebuilt_around_a_parents_new_one_and_an_attach_on_18() {
    let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
    let own = OwnDatabase::new(&url, "loose-exclusion-1671");
    let conn = own.connection().to_owned();
    on_server(
        &conn,
        "CREATE SCHEMA app; \
         CREATE TABLE app.ev (k integer NOT NULL, v integer NOT NULL, note text) \
           PARTITION BY RANGE (k); \
         CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
         ALTER TABLE app.ev_1 ADD CONSTRAINT ev_1_own EXCLUDE USING btree \
           (k WITH =, v WITH =) INCLUDE (note); \
         ALTER TABLE app.ev_1 ADD CONSTRAINT ev_1_one EXCLUDE USING btree (v WITH =); \
         CREATE TABLE app.late (k integer NOT NULL, v integer NOT NULL, note text); \
         ALTER TABLE app.late ADD CONSTRAINT late_own EXCLUDE USING btree \
           (k WITH =, v WITH =) INCLUDE (note); \
         INSERT INTO app.ev VALUES (1, 1, 'a'); INSERT INTO app.late VALUES (11, 2, 'b')",
    );
    let d = adopt("loose-exclusion-1671", &conn);
    let parent = d.dir.join("schema/app.ev.yml");
    let tree = std::fs::read_to_string(&parent).unwrap();
    let untouched = oid(&conn, "app.ev_1_one");
    // The constraints of `table` that are its own, by name.
    let owned = |table: &str| {
        text_of(
            &conn,
            &format!(
                "SELECT coalesce(string_agg(conname, ',' ORDER BY conname), '') \
                 FROM pg_constraint WHERE conrelid = '{table}'::regclass AND contype = 'x' \
                 AND conparentid = 0 AND conislocal"
            ),
        )
    };
    let clones = || {
        count(
            &conn,
            "FROM pg_constraint WHERE contype = 'x' AND conparentid <> 0",
        )
    };

    let with_parent = format!(
        "{tree}exclude:\n  ev_x:\n    method: btree\n    keys:\n      - column: k\n        \
         operator: \"=\"\n      - column: v\n        operator: \"=\"\n"
    );
    std::fs::write(&parent, &with_parent).unwrap();
    let script = applied(&d, &conn, "add");
    let dropped = at(&script, "DROP CONSTRAINT \"ev_1_own\"");
    let built = at(&script, "ADD CONSTRAINT \"ev_x\"");
    let again = at(&script, "ADD CONSTRAINT \"ev_1_own\"");
    assert!(dropped < built && built < again, "{script}");
    assert!(!script.contains("\"ev_1_one\""), "{script}");
    assert_eq!(oid(&conn, "app.ev_1_one"), untouched);
    assert_eq!(owned("app.ev_1"), "ev_1_one,ev_1_own");
    assert_eq!(clones(), 1);

    // Attached: its own goes ahead of the attach and comes back after.
    let late = d.dir.join("schema/app.late.yml");
    let held = std::fs::read_to_string(&late).unwrap();
    let entry: String = held[held.find("exclude:\n").unwrap()..]
        .lines()
        .map(|line| format!("    {line}\n"))
        .collect();
    std::fs::remove_file(&late).unwrap();
    let declared = std::fs::read_to_string(&parent).unwrap();
    assert!(
        declared.contains("  ev_1: {from: [\"0\"], to: [\"10\"]}")
            || declared.contains("  ev_1:\n"),
        "{declared}"
    );
    std::fs::write(
        &parent,
        declared.replacen(
            "partitions:\n",
            &format!("partitions:\n  late:\n    from: [\"10\"]\n    to: [\"20\"]\n{entry}"),
            1,
        ),
    )
    .unwrap();
    let script = applied(&d, &conn, "attach");
    let attach = at(&script, "ATTACH PARTITION \"app\".\"late\"");
    assert!(
        at(&script, "DROP CONSTRAINT \"late_own\"") < attach
            && attach < at(&script, "ADD CONSTRAINT \"late_own\""),
        "{script}"
    );
    assert_eq!(owned("app.late"), "late_own");
    assert_eq!(clones(), 2);
}
