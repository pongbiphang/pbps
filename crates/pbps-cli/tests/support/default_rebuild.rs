use super::*;

pub(super) fn rename_and_attach(server: &str) {
    scenario(server, false);
}
pub(super) fn unrelated_rename(server: &str) {
    scenario(server, true);
}
fn scenario(server: &str, unrelated: bool) {
    let own = OwnDatabase::new(server, "default-rebuild-1896");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
        CREATE FUNCTION app.f(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$; \
        CREATE TABLE app.t(k integer NOT NULL,d integer DEFAULT app.f(1)) PARTITION BY RANGE(k); \
        CREATE TABLE app.p PARTITION OF app.t FOR VALUES FROM (0) TO (10); \
        ALTER TABLE app.p ALTER COLUMN d SET DEFAULT app.f(2); \
        CREATE TABLE app.a(k integer NOT NULL,e integer DEFAULT app.f(3)); \
        INSERT INTO app.t(k) VALUES(1); INSERT INTO app.a(k) VALUES(11)",
    );
    if unrelated {
        on_server(
            connection,
            "CREATE TABLE app.u(n integer DEFAULT app.f(4), CONSTRAINT ck CHECK(app.f(n)>=0))",
        );
    }
    let old_routine = text_of(
        connection,
        "SELECT 'app.f(integer)'::regprocedure::oid::text",
    );
    let d = Demo::new("default-rebuild-1896");
    succeeds(d.run(&["pull", "--db", connection]));
    for name in ["app.t.yml", "app.a.yml"] {
        println!(
            "BEFORE {name}:\n{}",
            std::fs::read_to_string(d.dir.join("schema").join(name)).unwrap()
        );
    }
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    let table = "table: app.t\ncolumns:\n  k: {type: integer, nullable: false}\n  \
        e: {type: integer, default: app.f(1)}\npartition_by: [k]\npartitions:\n  \
        p:\n    from: [0]\n    to: [10]\n    columns:\n      e: {default: app.f(2)}\n  \
        a:\n    from: [10]\n    to: [20]\n    columns:\n      e: {default: app.f(3)}\n";
    let function = "function: app.f(integer)\npublic_execute: true\ndefinition: |-\n  \
        (x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x * 10 $$\n";
    println!("AFTER app.t.yml:\n{table}\nAFTER function:\n{function}");
    std::fs::write(d.dir.join("schema/app.t.yml"), table).unwrap();
    std::fs::write(
        d.dir.join("schema/app.f%28integer%29.function.yml"),
        function,
    )
    .unwrap();
    std::fs::remove_file(d.dir.join("schema/app.a.yml")).unwrap();
    if unrelated {
        std::fs::write(
            d.dir.join("schema/app.v.yml"),
            "table: app.v\ncolumns:\n  n: {type: integer, default: app.f(4)}\nchecks:\n  ck: app.f(n)>=0\n",
        )
        .unwrap();
        std::fs::remove_file(d.dir.join("schema/app.u.yml")).unwrap();
        succeeds(d.run(&[
            "intent",
            "--rename",
            "app.t.d",
            "e",
            "--rename-table",
            "app.u",
            "app.v",
        ]));
    } else {
        succeeds(d.run(&["rename", "app.t.d", "e"]));
    }
    // Offline planning writes the identity intent, but cannot certify a
    // membership cut whose version/binding facts have not been read.
    let preview = d.run(&["plan"]);
    assert_eq!(
        code(&preview),
        1,
        "{}{}",
        stdout(&preview),
        stderr(&preview)
    );
    assert!(
        stderr(&preview).contains("membership boundary"),
        "{}",
        stderr(&preview)
    );
    d.commit();
    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    let script = std::fs::read_to_string(sql).unwrap();
    println!(
        "SAVED PLAN:\n{}\nSQL:\n{script}",
        std::fs::read_to_string(&plan).unwrap()
    );
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle}:\n{script}"))
    };
    let drop = at("DROP FUNCTION");
    let create = at("CREATE FUNCTION");
    let rename = at("RENAME COLUMN \"d\" TO \"e\"");
    let attach = at("ATTACH PARTITION \"app\".\"a\"");
    let parent = at("ALTER TABLE \"app\".\"t\" ALTER COLUMN \"e\" SET DEFAULT");
    assert!(
        drop < create && create < parent && rename < parent,
        "{script}"
    );
    for part in ["p", "a"] {
        let own = at(&format!(
            "ALTER TABLE \"app\".\"{part}\" ALTER COLUMN \"e\" SET DEFAULT"
        ));
        assert!(parent < own, "{part}: {script}");
        if part == "a" {
            assert!(attach < own, "{script}");
        }
    }
    for (table, column) in [("t", "d"), ("p", "d"), ("a", "e")] {
        assert!(
            at(&format!(
                "ALTER TABLE \"app\".\"{table}\" ALTER COLUMN \"{column}\" DROP DEFAULT"
            )) < drop,
            "{script}"
        );
    }
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename", "--allow", "constraint"],
    ));
    let new_routine = text_of(
        connection,
        "SELECT 'app.f(integer)'::regprocedure::oid::text",
    );
    assert_ne!(new_routine, old_routine, "the routine was really rebuilt");
    assert_eq!(
        text_of(
            connection,
            "SELECT string_agg(c.relname||'='||pg_get_expr(d.adbin,d.adrelid),',' ORDER BY c.relname) \
        FROM pg_attrdef d JOIN pg_class c ON c.oid=d.adrelid WHERE c.relnamespace='app'::regnamespace"
        ),
        if unrelated {
            "a=app.f(3),p=app.f(2),t=app.f(1),v=app.f(4)"
        } else {
            "a=app.f(3),p=app.f(2),t=app.f(1)"
        }
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_attrdef d JOIN pg_class c ON c.oid=d.adrelid \
        JOIN pg_depend p ON p.classid='pg_attrdef'::regclass AND p.objid=d.oid AND p.refclassid='pg_proc'::regclass \
        WHERE c.relnamespace='app'::regnamespace AND p.refobjid='app.f(integer)'::regprocedure"
        ),
        if unrelated { 4 } else { 3 }
    );
    if unrelated {
        on_server(connection, "INSERT INTO app.v DEFAULT VALUES");
        assert_eq!(scalar(connection, "SELECT n::bigint FROM app.v"), 40);
    }
    // An insert through the parent takes its default even when routed to A.
    on_server(
        connection,
        "INSERT INTO app.t(k) VALUES(2),(13); INSERT INTO app.p(k) VALUES(3); INSERT INTO app.a(k) VALUES(12)",
    );
    assert_eq!(
        text_of(
            connection,
            "SELECT string_agg(k||':'||e,',' ORDER BY k) FROM app.t"
        ),
        "1:1,2:10,3:20,11:3,12:30,13:10"
    );
    succeeds(d.run(&["verify", "--db", connection]));
    let again = succeeds(d.run(&["plan", "--db", connection]));
    assert!(
        stdout(&again).contains("No changes"),
        "{}{}",
        stdout(&again),
        stderr(&again)
    );
}
