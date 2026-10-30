use super::*;

#[test]
#[ignore = "needs PostgreSQL 18; set PBPS_TEST_PG_DB"]
fn when_overload_rebuilds_only_the_trigger_that_binds_it_on_pg18() {
    check(&server());
}

#[test]
#[ignore = "needs PostgreSQL 16; set PBPS_TEST_PG_OLD_DB"]
fn when_overload_rebuilds_only_the_trigger_that_binds_it_on_pg16() {
    check(&std::env::var("PBPS_TEST_PG_OLD_DB").unwrap());
}

fn check(server: &str) {
    let own = OwnDatabase::new(server, "trigger1907");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let scratch = Confined::new(server);
    let d = Demo::new("trigger1907");
    write_declarations(
        &d,
        &[
            (
                "app.t.yml",
                "table: app.t\ncolumns:\n  c: {type: numeric}\n",
            ),
            (
                "app.f%28numeric%29.function.yml",
                "function: app.f(numeric)\npublic_execute: true\ndefinition: |-\n  (numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1\n",
            ),
            (
                "app.audit%28%29.function.yml",
                "function: app.audit()\npublic_execute: true\ndefinition: |-\n  () RETURNS trigger LANGUAGE plpgsql AS $body$ BEGIN RETURN NEW; END $body$\n",
            ),
            (
                "app.tr_call.trigger.yml",
                "trigger: app.tr_call\non: app.t\ndefinition: |-\n  BEFORE UPDATE ON app.t FOR EACH ROW WHEN (NEW.c > app.f(1) AND OLD.c IS NOT NULL) EXECUTE FUNCTION app.audit()\n",
            ),
            (
                "app.tr_plain.trigger.yml",
                "trigger: app.tr_plain\non: app.t\ndefinition: |-\n  BEFORE UPDATE ON app.t FOR EACH ROW WHEN (NEW.c > 0) EXECUTE FUNCTION app.audit()\n",
            ),
        ],
    );
    successful(run(&d, &["plan"], None));
    d.commit();
    successful(run(&d, &["bootstrap", "--db", connection], None));
    write_declarations(
        &d,
        &[(
            "app.f%28integer%29.function.yml",
            "function: app.f(integer)\npublic_execute: true\ndefinition: |-\n  (integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1\n",
        )],
    );
    successful(run(&d, &["plan"], None));
    d.commit();
    configure(&d);
    successful(run(
        &d,
        &[
            "key",
            "generate",
            "--out",
            d.dir.join("key").to_str().unwrap(),
        ],
        None,
    ));
    let artifact = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    let targets = Targets {
        target: connection,
        scratch: &scratch.connection,
    };
    successful(run(&d, &plan_args(&artifact, &sql), Some(&targets)));
    let script = std::fs::read_to_string(sql).unwrap();
    // Compare the complete published statement order, including the absence
    // of the plain WHEN trigger: table-column references alone cannot make
    // an unrelated arriving routine capture it.
    let statements: Vec<String> = script
        .split("SET search_path = '';")
        .filter_map(|block| block.split_once("SET search_path = \"app\", \"pg_temp\";"))
        .map(|(_, statement)| {
            statement
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .trim_end_matches(';')
                .trim_end()
                .to_owned()
        })
        .collect();
    assert_eq!(
        statements,
        [
            r#"CREATE FUNCTION "app"."f" (integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1"#,
            r#"GRANT EXECUTE ON ROUTINE "app"."f"(integer) TO PUBLIC"#,
            r#"DROP TRIGGER "tr_call" ON "app"."t""#,
            r#"CREATE TRIGGER "tr_call" BEFORE UPDATE ON app.t FOR EACH ROW WHEN (NEW.c > app.f(1) AND OLD.c IS NOT NULL) EXECUTE FUNCTION app.audit()"#,
        ],
        "{script}"
    );
    assert!(
        !script.contains("tr_plain"),
        "a WHEN without a routine binding is not rebuilt: {script}"
    );
}
