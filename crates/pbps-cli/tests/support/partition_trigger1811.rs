//! #1811. A trigger declared on a partition tree is held, not refused (#1632
//! made a partition tree's triggers modules): a trigger `on:` the DEFAULT
//! partition, added in one plan with a new range partition, plans, applies and
//! reads back, and the next plan is empty. The one thing still left out of a
//! pull is a clone whose enabled mode differs from its parent's, and the one
//! thing still refused at validation is an own trigger named as the parent's
//! row trigger. Each case runs on both pinned engines.
use super::{
    Demo, OwnDatabase, approved_apply, code, on_server, scalar, server, stderr, stdout, succeeds,
};

fn servers(mut check: impl FnMut(&str, &str)) {
    for (variable, tag) in [("PBPS_TEST_PG_OLD_DB", "old"), ("PBPS_TEST_PG_DB", "new")] {
        check(&std::env::var(variable).expect(variable), tag);
    }
}

fn said(output: &std::process::Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

const FUNCTION: &str = "function: app.f()\ndefinition: |-\n  () RETURNS trigger LANGUAGE plpgsql \
    SET search_path = pg_catalog AS $$ BEGIN RETURN NULL; END $$\n";

fn trigger(d: &Demo, name: &str, on: &str, definition: &str) {
    std::fs::write(
        d.dir.join(format!(
            "schema/app.{on_name}.{name}.trigger.yml",
            on_name = &on[4..]
        )),
        format!("trigger: app.{name}\non: {on}\n\ndefinition: |-\n  {definition}\n"),
    )
    .unwrap();
}

fn tree(d: &Demo, partitions: &str) {
    std::fs::write(
        d.dir.join("schema/app.ev.yml"),
        format!(
            "table: app.ev\ncolumns:\n  id: {{type: integer, nullable: false}}\n  \
             ts: {{type: date, nullable: false}}\nprimary_key: [id, ts]\n\
             partition_by: [ts]\npartitions:\n{partitions}"
        ),
    )
    .unwrap();
}

const P2025: &str = "  ev_2025: {from: [\"2025-01-01\"], to: [\"2026-01-01\"]}\n";
const P2026: &str = "  ev_2026: {from: [\"2026-01-01\"], to: [\"2027-01-01\"]}\n";
const REST: &str = "  ev_rest: default\n";

/// A database holding the tree with a populated DEFAULT partition, bootstrapped
/// from `d`'s declarations (the tree before the change under test).
fn started(d: &Demo, own: &OwnDatabase) -> String {
    let connection = own.connection().to_owned();
    on_server(&connection, "CREATE SCHEMA app");
    std::fs::write(d.dir.join("schema/app.f.function.yml"), FUNCTION).unwrap();
    tree(d, &format!("{P2025}{REST}"));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", &connection]));
    on_server(
        &connection,
        "INSERT INTO app.ev VALUES (1, '2025-06-01'), (2, '2030-01-01'), (3, '2031-03-03')",
    );
    connection
}

/// Plan, apply, verify and the follow-up plan, which must be empty.
fn applied(d: &Demo, connection: &str, name: &str) -> String {
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join(name);
    let planned = succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    let applied = succeeds(approved_apply(d, connection, &plan, &[]));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
    format!(
        "--- plan\n{}\n--- apply\n{}\n--- follow-up plan\n{}",
        said(&planned),
        said(&applied),
        said(&next)
    )
}

/// The tree and its triggers as a fresh pull reads them.
fn pulled(connection: &str, slug: &str) -> (String, Demo) {
    let pull = Demo::new(slug);
    let o = succeeds(pull.run(&["pull", "--db", connection]));
    (said(&o), pull)
}

fn schema_text(d: &Demo) -> String {
    let mut all = String::new();
    for entry in std::fs::read_dir(d.dir.join("schema")).unwrap() {
        all.push_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap_or_default());
    }
    all
}

/// The issue's case: a statement trigger `on:` the DEFAULT partition, added in
/// the same plan as a new range partition, over a DEFAULT partition that holds
/// rows.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_trigger_on_the_default_partition_added_with_a_new_partition_applies_and_reads_back() {
    let _ = server();
    servers(|url, tag| {
        let own = OwnDatabase::new(url, &format!("part-tg-1811-{tag}"));
        let d = Demo::new(&format!("part-tg-1811-{tag}"));
        let connection = started(&d, &own);

        tree(&d, &format!("{P2025}{P2026}{REST}"));
        trigger(
            &d,
            "audit",
            "app.ev_rest",
            "AFTER DELETE ON app.ev_rest FOR EACH STATEMENT EXECUTE FUNCTION app.f()",
        );
        let record = applied(&d, &connection, "plan.json");
        eprintln!("== {tag}\n{record}");
        assert!(record.contains("app.ev_2026"), "{record}");
        assert!(record.contains("audit"), "{record}");
        assert_eq!(
            scalar(
                &connection,
                "SELECT count(*) FROM pg_trigger WHERE tgname = 'audit' \
                   AND tgrelid = 'app.ev_rest'::regclass AND NOT tgisinternal"
            ),
            1
        );
        assert_eq!(
            scalar(&connection, "SELECT count(*) FROM ONLY app.ev_rest"),
            2
        );

        let (said, pull) = pulled(&connection, &format!("part-tg-1811-pull-{tag}"));
        eprintln!("== {tag} pull\n{said}");
        let text = schema_text(&pull);
        assert!(pull.dir.join("schema/app.ev.yml").exists(), "{said}");
        assert!(
            !said.contains("other than one pbps holds"),
            "the tree was left out: {said}"
        );
        assert!(
            text.contains("trigger: app.audit") && text.contains("on: app.ev_rest"),
            "{text}"
        );
        assert!(
            text.contains("ev_2026") && text.contains("ev_rest"),
            "{text}"
        );
        succeeds(pull.run(&["fmt", "--check"]));
        // What was pulled is itself a valid declaration of the tree.
        succeeds(pull.run(&["validate"]));
    });
}

/// A statement trigger and a row trigger on the parent, and a trigger on
/// another member of the tree, in one plan with a new partition: the row
/// trigger is cloned onto every partition including the new one, the others
/// are not, and the tree reads back whole.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn parent_and_member_triggers_apply_with_a_new_partition_and_read_back() {
    let _ = server();
    servers(|url, tag| {
        let own = OwnDatabase::new(url, &format!("part-tg-1811b-{tag}"));
        let d = Demo::new(&format!("part-tg-1811b-{tag}"));
        let connection = started(&d, &own);

        tree(&d, &format!("{P2025}{P2026}{REST}"));
        trigger(
            &d,
            "stmt",
            "app.ev",
            "AFTER INSERT ON app.ev FOR EACH STATEMENT EXECUTE FUNCTION app.f()",
        );
        trigger(
            &d,
            "row",
            "app.ev",
            "BEFORE UPDATE ON app.ev FOR EACH ROW EXECUTE FUNCTION app.f()",
        );
        trigger(
            &d,
            "own",
            "app.ev_2025",
            "BEFORE DELETE ON app.ev_2025 FOR EACH ROW EXECUTE FUNCTION app.f()",
        );
        let record = applied(&d, &connection, "plan.json");
        eprintln!("== {tag}\n{record}");
        let triggers = |table: &str, clone: bool| {
            scalar(
                &connection,
                &format!(
                    "SELECT count(*) FROM pg_trigger WHERE tgrelid = '{table}'::regclass \
                       AND NOT tgisinternal AND (tgparentid <> 0) = {clone}"
                ),
            )
        };
        for table in ["app.ev_2025", "app.ev_2026", "app.ev_rest"] {
            assert_eq!(triggers(table, true), 1, "{table}: the row trigger's clone");
        }
        assert_eq!(triggers("app.ev_2025", false), 1, "its own trigger");
        assert_eq!(triggers("app.ev_2026", false), 0);
        assert_eq!(
            triggers("app.ev", false),
            2,
            "the parent's statement and row triggers"
        );

        let (said, pull) = pulled(&connection, &format!("part-tg-1811b-pull-{tag}"));
        let text = schema_text(&pull);
        assert!(pull.dir.join("schema/app.ev.yml").exists(), "{said}");
        for name in ["app.stmt", "app.row", "app.own"] {
            assert!(text.contains(&format!("trigger: {name}")), "{name}: {text}");
        }
    });
}

/// Negative: a clone whose enabled mode differs from its parent's is a fact no
/// declaration holds, so the tree is left out of a pull, by name.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_clone_in_another_enabled_mode_leaves_the_tree_out_by_name() {
    let _ = server();
    servers(|url, tag| {
        let own = OwnDatabase::new(url, &format!("part-tg-1811c-{tag}"));
        let connection = own.connection().to_owned();
        on_server(
            &connection,
            "CREATE SCHEMA app; \
             CREATE FUNCTION app.f() RETURNS trigger LANGUAGE plpgsql \
                 SET search_path = pg_catalog AS $$BEGIN RETURN NULL; END$$; \
             CREATE TABLE app.ev (id integer NOT NULL, ts date NOT NULL, PRIMARY KEY (id, ts)) \
                 PARTITION BY RANGE (ts); \
             CREATE TABLE app.ev_a PARTITION OF app.ev FOR VALUES FROM ('2025-01-01') TO ('2026-01-01'); \
             CREATE TABLE app.ev_rest PARTITION OF app.ev DEFAULT; \
             CREATE TRIGGER ev_row BEFORE UPDATE ON app.ev FOR EACH ROW EXECUTE FUNCTION app.f()",
        );
        // Control: the same tree with every clone in its parent's mode is pulled.
        let (said_whole, whole) = pulled(&connection, &format!("part-tg-1811c-whole-{tag}"));
        assert!(whole.dir.join("schema/app.ev.yml").exists(), "{said_whole}");

        on_server(
            &connection,
            "ALTER TABLE app.ev_rest DISABLE TRIGGER ev_row",
        );
        let (said, left) = pulled(&connection, &format!("part-tg-1811c-left-{tag}"));
        assert!(!left.dir.join("schema/app.ev.yml").exists(), "{said}");
        assert!(
            said.contains("`app.ev_rest` is a partition")
                && said.contains("a clone of its parent's trigger in another mode"),
            "{said}"
        );
    });
}

/// Negative: an own trigger under the name the parent's row trigger gives its
/// clones is refused by name by `validate` (a connected plan reaches the same check, pinned by
/// `a_partition_trees_triggers_go_through_the_cli`), while the
/// same name on a statement trigger (not cloned) is accepted.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn an_own_trigger_named_like_the_parents_row_trigger_is_refused_by_name() {
    let _ = server();
    let d = Demo::new("part-tg-1811d");
    std::fs::write(d.dir.join("schema/app.f.function.yml"), FUNCTION).unwrap();
    tree(&d, &format!("{P2025}{REST}"));
    trigger(
        &d,
        "audit",
        "app.ev",
        "BEFORE UPDATE ON app.ev FOR EACH ROW EXECUTE FUNCTION app.f()",
    );
    // Accepted: nothing of the partition's own shares the name yet.
    succeeds(d.run(&["validate"]));
    let own = d.dir.join("schema/app.ev_rest.audit.trigger.yml");
    std::fs::write(
        &own,
        "trigger: app.audit\non: app.ev_rest\n\ndefinition: |-\n  \
         BEFORE DELETE ON app.ev_rest FOR EACH ROW EXECUTE FUNCTION app.f()\n",
    )
    .unwrap();
    let needle = "trigger `audit` on `app.ev_rest` is named as its parent's row trigger";
    let o = d.run(&["validate"]);
    assert_ne!(code(&o), 0, "{}", said(&o));
    assert!(said(&o).contains(needle), "{}", said(&o));
    // Negative of the negative: a statement trigger's name is free.
    std::fs::write(
        d.dir.join("schema/app.ev.audit.trigger.yml"),
        "trigger: app.audit\non: app.ev\n\ndefinition: |-\n  \
         AFTER UPDATE ON app.ev FOR EACH STATEMENT EXECUTE FUNCTION app.f()\n",
    )
    .unwrap();
    std::fs::write(
        &own,
        "trigger: app.audit\non: app.ev_rest\n\ndefinition: |-\n  \
         BEFORE DELETE ON app.ev_rest FOR EACH ROW EXECUTE FUNCTION app.f()\n",
    )
    .unwrap();
    succeeds(d.run(&["validate"]));
}
