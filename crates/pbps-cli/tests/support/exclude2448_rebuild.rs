//! An exclusion constraint through the rebuilds that tear indexes and constraints down and
//! put them back (#2448 PR D, DEC-2448.5): a function it calls is replaced, an enum it
//! names is rebuilt, a column it holds is retyped, a generated column it holds has its
//! expression set, and a replacement the rows refuse leaves the old constraint standing.
//! Each case runs on both pinned engines (the generated expression on 18 only: 16 has no
//! `SET EXPRESSION`), and ends with the constraint standing with its definition, an empty
//! re-plan and a clean `verify`.
use super::{
    Demo, OwnDatabase, approved_apply, code, json_output, on_server, scalar, server, stderr,
    stdout, succeeds, text_of, try_on_server,
};

fn servers(mut check: impl FnMut(&str, bool)) {
    for (variable, current) in [("PBPS_TEST_PG_OLD_DB", false), ("PBPS_TEST_PG_DB", true)] {
        check(&std::env::var(variable).expect(variable), current);
    }
}

fn said(output: &std::process::Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

const BOTH: &[&str] = &["--allow", "destructive", "--allow", "constraint"];
const NARROWING: &[&str] = &[
    "--allow",
    "destructive",
    "--allow",
    "constraint",
    "--allow",
    "narrowing",
];

/// The constraint's definition as the engine reads it back.
fn definition(connection: &str, name: &str) -> String {
    text_of(
        connection,
        &format!(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conname = '{name}' AND contype = 'x'"
        ),
    )
}

fn standing(connection: &str, name: &str) -> i64 {
    scalar(
        connection,
        &format!("SELECT count(*) FROM pg_constraint WHERE conname = '{name}' AND contype = 'x'"),
    )
}

/// How many entries of the state ledger record a state: a failed apply leaves an entry of
/// its own, of kind `failed`, which records none.
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

/// A project over the database as it stands, with its identities adopted.
fn adopt(name: &str, connection: &str) -> Demo {
    let d = Demo::new(name);
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    d
}

/// The plan's SQL, with the plan beside it.
fn plan_sql(d: &Demo, connection: &str) -> (std::path::PathBuf, String) {
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

/// The exclusion stands as it did, the declarations plan nothing and the database verifies.
fn settled(d: &Demo, connection: &str, name: &str, was: &str) {
    assert_eq!(standing(connection, name), 1);
    assert_eq!(definition(connection, name), was);
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
    succeeds(d.run(&["verify", "--db", connection]));
}

fn position(script: &str, needle: &str) -> usize {
    script
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` is not in:\n{script}"))
}

fn function_file(d: &Demo) -> std::path::PathBuf {
    std::fs::read_dir(d.dir.join("schema"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".function.yml")
        })
        .expect("a function file")
}

/// A function an exclusion's expression and predicate call is replaced: the plan drops the
/// constraint (never its index) before the function's drop and adds it again after the
/// create, and the constraint stands with its definition. A replacement the existing rows
/// refuse fails the apply with the old function, the old constraint and the rows standing
/// and no state recorded.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn an_exclusion_is_torn_down_and_restored_around_a_function_it_calls() {
    let _ = server();
    servers(|url, _| {
        let own = OwnDatabase::new(url, "exclude-function");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; \
             CREATE FUNCTION app.f(x integer) RETURNS integer LANGUAGE sql IMMUTABLE \
               AS 'SELECT x + 1'; \
             CREATE TABLE app.t (id integer PRIMARY KEY, n integer NOT NULL); \
             INSERT INTO app.t VALUES (1, 1), (2, 10); \
             ALTER TABLE app.t ADD CONSTRAINT x_fn EXCLUDE USING gist \
               (int4range(app.f(n), app.f(n) + 5) WITH &&) WHERE (app.f(n) > 0)",
        );
        let d = adopt("exclude-function", connection);
        let was = definition(connection, "x_fn");
        let file = function_file(&d);
        let held = std::fs::read_to_string(&file).unwrap();

        // The new body makes both rows collide: refused by the engine at the restoration,
        // so the whole apply rolls back.
        let colliding = held.replacen("SELECT x + 1", "SELECT 1", 1);
        assert_ne!(colliding, held, "{held}");
        std::fs::write(&file, &colliding).unwrap();
        d.commit();
        let (plan, script) = plan_sql(&d, connection);
        assert!(!script.contains("DROP INDEX"), "{script}");
        let before = recorded(&d, connection);
        let failed = approved_apply(&d, connection, &plan, BOTH);
        assert_ne!(code(&failed), 0, "{}", said(&failed));
        assert_eq!(standing(connection, "x_fn"), 1, "{}", said(&failed));
        assert_eq!(definition(connection, "x_fn"), was);
        assert_eq!(
            scalar(connection, "SELECT app.f(1)::bigint"),
            2,
            "the old body stands"
        );
        assert_eq!(scalar(connection, "SELECT count(*) FROM app.t"), 2);
        assert_eq!(recorded(&d, connection), before, "no state was recorded");

        // A body the rows accept: torn down, replaced, restored.
        let widened = held.replacen("SELECT x + 1", "SELECT x + 2", 1);
        std::fs::write(&file, &widened).unwrap();
        d.commit();
        let (plan, script) = plan_sql(&d, connection);
        let teardown = position(&script, "DROP CONSTRAINT \"x_fn\"");
        let dropped = position(&script, "DROP FUNCTION");
        let created = position(&script, "CREATE FUNCTION");
        let restored = position(&script, "ADD CONSTRAINT \"x_fn\" EXCLUDE USING gist");
        assert!(
            teardown < dropped && dropped < created && created < restored,
            "{script}"
        );
        assert!(!script.contains("DROP INDEX"), "{script}");
        assert!(!script.contains("USING INDEX"), "{script}");
        let refused = approved_apply(&d, connection, &plan, &[]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert_eq!(
            standing(connection, "x_fn"),
            1,
            "a refused apply changes nothing"
        );
        succeeds(approved_apply(&d, connection, &plan, BOTH));
        assert_eq!(scalar(connection, "SELECT app.f(1)::bigint"), 3);
        settled(&d, connection, "x_fn", &was);
        // It enforces again: n = 3 maps to [5, 10), which overlaps [12, 17)'s neighbour
        // at n = 8 only if inserted, so insert the clash with the row at n = 1.
        assert!(
            try_on_server(connection, "INSERT INTO app.t VALUES (3, 1)").is_err(),
            "the restored constraint refuses an overlap"
        );
    });
}

/// An enum or a domain an exclusion's predicate names is rebuilt (a label leaves the enum,
/// the domain's base type widens): the engine refuses to retype beside a constant of the old
/// type, so the plan drops the constraint before the retype and adds it after; an exclusion
/// that only holds an enum column by key is stated the same way, in place of the C2 refusal.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn an_exclusion_is_torn_down_and_restored_around_a_type_rebuild() {
    let _ = server();
    // (setup, constraint name, type's declaration file, its text before and after)
    let cases = [
        (
            "CREATE TYPE app.mood AS ENUM ('a', 'b', 'c'); \
             CREATE TABLE app.e (id integer PRIMARY KEY, m app.mood, a integer, b integer); \
             INSERT INTO app.e VALUES (1, 'a', 1, 2), (2, 'c', 1, 2); \
             ALTER TABLE app.e ADD CONSTRAINT x_predicate EXCLUDE USING gist \
               (int4range(a, b) WITH &&) WHERE (m = 'a'::app.mood)",
            "x_predicate",
            "app.mood.type.yml",
            ("a, b, c", "a, c"),
        ),
        (
            "CREATE TYPE app.mood AS ENUM ('a', 'b', 'c'); \
             CREATE TABLE app.e (id integer PRIMARY KEY, m app.mood, a integer, b integer); \
             INSERT INTO app.e VALUES (1, 'a', 1, 2), (2, 'c', 1, 2); \
             ALTER TABLE app.e ADD CONSTRAINT x_key EXCLUDE USING btree (m WITH =)",
            "x_key",
            "app.mood.type.yml",
            ("a, b, c", "a, c"),
        ),
        (
            "CREATE DOMAIN app.dom AS integer; \
             CREATE TABLE app.e (id integer PRIMARY KEY, v app.dom, a integer, b integer); \
             INSERT INTO app.e VALUES (1, 5, 1, 2), (2, 6, 1, 2); \
             ALTER TABLE app.e ADD CONSTRAINT x_domain EXCLUDE USING gist \
               (int4range(a, b) WITH &&) WHERE (v = 5::app.dom)",
            "x_domain",
            "app.dom.type.yml",
            ("type: integer", "type: bigint"),
        ),
    ];
    servers(|url, _| {
        for (setup, name, file, (from, to)) in cases {
            let own = OwnDatabase::new(url, "exclude-type");
            let connection = own.connection();
            on_server(connection, &format!("CREATE SCHEMA app; {setup}"));
            let d = adopt("exclude-type", connection);
            let was = definition(connection, name);
            let path = d.dir.join("schema").join(file);
            let text = std::fs::read_to_string(&path).unwrap();
            let rebuilt = text.replace(from, to);
            assert_ne!(text, rebuilt, "{text}");
            std::fs::write(&path, rebuilt).unwrap();
            d.commit();
            let (plan, script) = plan_sql(&d, connection);
            let teardown = position(&script, &format!("DROP CONSTRAINT \"{name}\""));
            let restored = position(&script, &format!("ADD CONSTRAINT \"{name}\" EXCLUDE"));
            assert!(teardown < restored, "{name}: {script}");
            assert!(
                !script.contains("DROP INDEX") && !script.contains("USING INDEX"),
                "{name}: {script}"
            );
            succeed_with(&d, connection, &plan);
            // The predicate's constant is the rebuilt type's, which prints the same.
            settled(&d, connection, name, &was);
            assert_eq!(
                scalar(connection, "SELECT count(*) FROM app.e"),
                2,
                "{name}"
            );
        }
    });
}

fn succeed_with(d: &Demo, connection: &str, plan: &std::path::Path) {
    succeeds(approved_apply(
        d,
        connection,
        plan,
        &[
            "--allow",
            "destructive",
            "--allow",
            "constraint",
            "--allow",
            "type-rebuild",
        ],
    ));
}

/// A column an exclusion's key expression holds is retyped: the engine would rebuild the
/// constraint unseen, so the plan states it, the drop before the retype and the add after;
/// a column the constraint does not hold plans none of it. Quoted names with a hyphen or a
/// space are found by the same scan.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn an_exclusion_is_torn_down_and_restored_around_a_retype_of_a_column_it_holds() {
    let _ = server();
    servers(|url, _| {
        for column in ["Start-Time", "end time", "free"] {
            let own = OwnDatabase::new(url, "exclude-retype");
            let connection = own.connection();
            on_server(
                connection,
                "CREATE SCHEMA app; \
                 CREATE TABLE app.q (id integer PRIMARY KEY, \"Start-Time\" integer NOT NULL, \
                   \"end time\" integer NOT NULL, free integer); \
                 INSERT INTO app.q VALUES (1, 1, 5, 0), (2, 6, 9, 0); \
                 ALTER TABLE app.q ADD CONSTRAINT x EXCLUDE USING btree \
                   ((\"Start-Time\" + \"end time\") WITH =)",
            );
            let d = adopt("exclude-retype", connection);
            let was = definition(connection, "x");
            let path = d.dir.join("schema/app.q.yml");
            let held = std::fs::read_to_string(&path).unwrap();
            let text = held.replacen(
                &format!("{column}:\n    type: integer"),
                &format!("{column}:\n    type: bigint"),
                1,
            );
            assert_ne!(text, held, "{column}:\n{held}");
            std::fs::write(&path, text).unwrap();
            d.commit();
            let (plan, script) = plan_sql(&d, connection);
            if column == "free" {
                // Negative: a column no constraint holds plans no teardown.
                assert!(
                    script.contains("ALTER COLUMN \"free\" TYPE") && !script.contains("\"x\""),
                    "{script}"
                );
                succeeds(approved_apply(&d, connection, &plan, &[]));
                assert_eq!(definition(connection, "x"), was);
                continue;
            }
            let teardown = position(&script, "DROP CONSTRAINT \"x\"");
            let altered = position(&script, "TYPE");
            let restored = position(&script, "ADD CONSTRAINT \"x\" EXCLUDE USING btree");
            assert!(
                teardown < altered && altered < restored && !script.contains("DROP INDEX"),
                "{column}: {script}"
            );
            let refused = approved_apply(&d, connection, &plan, &[]);
            assert_ne!(code(&refused), 0, "{column}: {}", said(&refused));
            assert_eq!(
                standing(connection, "x"),
                1,
                "a refused apply changes nothing"
            );
            succeeds(approved_apply(&d, connection, &plan, BOTH));
            settled(&d, connection, "x", &was);
            assert!(
                try_on_server(connection, "INSERT INTO app.q VALUES (3, 2, 4, 0)").is_err(),
                "{column}: the constraint still enforces"
            );
        }
    });
}

/// A column an exclusion holds is retyped while the exclusion's storage parameters change:
/// the restoring add carries the declared `fillfactor`, so the plan holds no `ALTER INDEX`
/// on an index that is not there between the teardown and the restoration. The apply
/// verifies and the declarations plan nothing after it.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_parameter_change_beside_a_retype_rides_on_the_restored_exclusion() {
    let _ = server();
    servers(|url, _| {
        let own = OwnDatabase::new(url, "exclude-retype-params");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; \
             CREATE TABLE app.q (id integer PRIMARY KEY, a integer NOT NULL, b integer NOT NULL); \
             INSERT INTO app.q VALUES (1, 1, 5), (2, 6, 9); \
             ALTER TABLE app.q ADD CONSTRAINT x EXCLUDE USING btree ((a + b) WITH =) \
               WITH (fillfactor = 90)",
        );
        let d = adopt("exclude-retype-params", connection);
        let path = d.dir.join("schema/app.q.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        let text = held
            .replacen("a:\n    type: integer", "a:\n    type: bigint", 1)
            .replacen("fillfactor: 90", "fillfactor: 70", 1);
        assert!(
            text.contains("fillfactor: 70") && text.contains("type: bigint"),
            "{held}"
        );
        std::fs::write(&path, text).unwrap();
        d.commit();
        let (plan, script) = plan_sql(&d, connection);
        let teardown = position(&script, "DROP CONSTRAINT \"x\"");
        let altered = position(&script, "TYPE");
        let restored = position(&script, "ADD CONSTRAINT \"x\" EXCLUDE USING btree");
        assert!(
            teardown < altered && altered < restored && !script.contains("ALTER INDEX"),
            "{script}"
        );
        assert!(script.contains("fillfactor"), "{script}");
        succeeds(approved_apply(&d, connection, &plan, BOTH));
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_class c JOIN pg_constraint k ON k.conindid = c.oid \
                 WHERE k.conname = 'x' AND c.reloptions @> ARRAY['fillfactor=70']"
            ),
            1
        );
        let was = definition(connection, "x");
        settled(&d, connection, "x", &was);
    });
}

/// A replacement the rows refuse leaves the old constraint, its definition and the rows
/// standing and records nothing; the same declarations then plan again.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_replacement_exclusion_the_rows_refuse_leaves_the_old_one_standing() {
    let _ = server();
    servers(|url, _| {
        let own = OwnDatabase::new(url, "exclude-rollback");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; \
             CREATE TABLE app.b (id integer PRIMARY KEY, room integer NOT NULL, \
               starts integer NOT NULL, ends integer NOT NULL); \
             INSERT INTO app.b VALUES (1, 1, 1, 5), (2, 2, 3, 8); \
             ALTER TABLE app.b ADD CONSTRAINT x EXCLUDE USING gist \
               (int4range(starts, ends) WITH &&) WHERE (room = 1)",
        );
        let d = adopt("exclude-rollback", connection);
        let was = definition(connection, "x");
        let oid = || {
            scalar(
                connection,
                "SELECT oid::bigint FROM pg_constraint WHERE conname = 'x'",
            )
        };
        let before_oid = oid();
        let path = d.dir.join("schema/app.b.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        // Without its predicate the constraint also holds room 2, whose range overlaps.
        let loosened: String = held
            .lines()
            .filter(|line| !line.trim_start().starts_with("where:"))
            .map(|line| format!("{line}\n"))
            .collect();
        assert_ne!(loosened, held, "{held}");
        std::fs::write(&path, loosened).unwrap();
        d.commit();
        let (plan, script) = plan_sql(&d, connection);
        assert!(
            position(&script, "DROP CONSTRAINT \"x\"")
                < position(&script, "ADD CONSTRAINT \"x\" EXCLUDE"),
            "{script}"
        );
        let before = recorded(&d, connection);
        let failed = approved_apply(&d, connection, &plan, BOTH);
        assert_ne!(code(&failed), 0, "{}", said(&failed));
        assert_eq!(oid(), before_oid, "the old constraint is the one standing");
        assert_eq!(definition(connection, "x"), was);
        assert_eq!(scalar(connection, "SELECT count(*) FROM app.b"), 2);
        assert_eq!(recorded(&d, connection), before, "no state was recorded");
        // Nothing was half done: the old constraint still enforces.
        assert!(try_on_server(connection, "INSERT INTO app.b VALUES (3, 1, 2, 4)").is_err());
    });
}

/// A generated key column's expression is set (18 only: 16 has no `SET EXPRESSION`, and
/// the pre-17 refusal stays): the exclusion is torn down before and restored after.
#[test]
#[ignore = "needs PostgreSQL 18; set PBPS_TEST_PG_DB"]
fn an_exclusion_is_torn_down_and_restored_around_a_generated_expression_change() {
    let _ = server();
    let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
    let own = OwnDatabase::new(&url, "exclude-generated");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
         CREATE TABLE app.g (id integer PRIMARY KEY, n integer NOT NULL, \
           g integer GENERATED ALWAYS AS (n * 2) STORED); \
         INSERT INTO app.g (id, n) VALUES (1, 1), (2, 10); \
         ALTER TABLE app.g ADD CONSTRAINT x EXCLUDE USING gist (int4range(g, g + 1) WITH &&)",
    );
    let d = adopt("exclude-generated", connection);
    let was = definition(connection, "x");
    let path = d.dir.join("schema/app.g.yml");
    let held = std::fs::read_to_string(&path).unwrap();
    let changed = held.replacen("n * 2", "n * 3", 1);
    assert_ne!(changed, held, "{held}");
    std::fs::write(&path, changed).unwrap();
    d.commit();
    let (plan, script) = plan_sql(&d, connection);
    let teardown = position(&script, "DROP CONSTRAINT \"x\"");
    let altered = position(&script, "SET EXPRESSION");
    let restored = position(&script, "ADD CONSTRAINT \"x\" EXCLUDE USING gist");
    assert!(teardown < altered && altered < restored, "{script}");
    succeeds(approved_apply(&d, connection, &plan, NARROWING));
    settled(&d, connection, "x", &was);
}
