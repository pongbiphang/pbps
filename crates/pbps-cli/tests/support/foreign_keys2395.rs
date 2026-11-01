//! New FK type compatibility, through offline and connected planning (#2395).
use super::{
    Demo, OwnDatabase, approved_apply, bootstrapped_demo, code, connected_artifact, old_server,
    on_server, scalar, server, stderr, stdout, succeeds,
};

const ANCHOR: &str = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\nprimary_key: {name: pk_t, columns: [id]}\n";

fn put(d: &Demo, name: &str, text: &str) {
    std::fs::write(d.dir.join("schema").join(name), text).unwrap();
}

fn parent(ty: &str) -> String {
    format!(
        "table: app.p\ncolumns:\n  id: {{type: {ty}, nullable: false}}\nprimary_key: {{name: pk_p, columns: [id]}}\n"
    )
}

fn child(ty: &str, key: bool) -> String {
    let mut text = format!(
        "table: app.c\ncolumns:\n  id: {{type: bigint, nullable: false}}\n  pid: {{type: {ty}}}\nprimary_key: {{name: pk_c, columns: [id]}}\n"
    );
    if key {
        text.push_str("foreign_keys:\n  fk_c_p:\n    columns: [pid]\n    references: app.p(id)\n");
    }
    text
}

fn pair(d: &Demo, parent_type: &str, child_type: &str, key: bool) {
    put(d, "app.p.yml", &parent(parent_type));
    put(d, "app.c.yml", &child(child_type, key));
}

fn refused(d: &Demo, args: &[&str]) {
    let out = d.run(args);
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert_ne!(code(&out), 0, "{args:?}: {text}");
    for needed in [
        "fk_c_p",
        "app.c.pid",
        "app.p.id",
        "`integer`",
        "`text`",
        "Declare types for app.c.pid and app.p.id that a key can join",
    ] {
        assert!(text.contains(needed), "{args:?}: {needed} in {text}");
    }
}

#[test]
fn offline_created_and_added_unjoinable_keys_are_named_with_a_repair() {
    for added in [false, true] {
        let d = Demo::new(&format!("fk2395-offline-refuse-{added}"));
        if added {
            pair(&d, "text", "integer", false);
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
        }
        pair(&d, "text", "integer", true);
        refused(&d, &["plan", "--no-dev"]);
    }
}

#[test]
fn offline_joinable_created_and_added_keys_still_plan() {
    for added in [false, true] {
        let d = Demo::new(&format!("fk2395-offline-join-{added}"));
        if added {
            pair(&d, "bigint", "integer", false);
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
        }
        pair(&d, "bigint", "integer", true);
        succeeds(d.run(&["plan", "--no-dev"]));
    }
}

// A sequence survives rollback, so zero proves the plan did not merely execute
// DDL and roll it back. The tracked tables are the only ones this plan changes.
const WITNESS: &str = "CREATE SCHEMA witness; CREATE SEQUENCE witness.executed; \
    CREATE FUNCTION witness.count_ddl() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN \
    IF EXISTS (SELECT FROM pg_event_trigger_ddl_commands() \
    WHERE schema_name = 'app') THEN PERFORM nextval('witness.executed'); END IF; END $$; \
    CREATE EVENT TRIGGER count_key_ddl ON ddl_command_end \
    WHEN TAG IN ('CREATE TABLE', 'ALTER TABLE') EXECUTE FUNCTION witness.count_ddl();";

fn executed(connection: &str) -> i64 {
    scalar(
        connection,
        "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM witness.executed",
    )
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn new_unjoinable_keys_are_refused_before_ddl_and_joinable_keys_apply() {
    for (version, url) in [(16, old_server()), (18, server())] {
        for added in [false, true] {
            for joinable in [false, true] {
                let slug = format!("fk2395-{version}-{added}-{joinable}");
                let own = OwnDatabase::new(&url, &slug);
                let connection = own.connection();
                let d = bootstrapped_demo(connection, &slug, ANCHOR);
                let parent_type = if joinable { "bigint" } else { "text" };
                if added {
                    pair(&d, parent_type, "integer", false);
                    let artifact = connected_artifact(&d, connection, false);
                    succeeds(approved_apply(&d, connection, &artifact, &[]));
                    on_server(
                        connection,
                        "INSERT INTO app.p VALUES ('1'); INSERT INTO app.c VALUES (1,1)",
                    );
                }
                pair(&d, parent_type, "integer", true);
                if !joinable {
                    on_server(connection, WITNESS);
                    refused(&d, &["plan", "--no-dev"]);
                    // A refused local plan writes no identities. Allocate them
                    // using a joinable declaration, without applying it, then
                    // commit the same object names with the refused types.
                    pair(&d, "text", "text", true);
                    succeeds(d.run(&["plan", "--no-dev"]));
                    pair(&d, "text", "integer", true);
                    d.commit();
                    refused(&d, &["plan", "--db", connection]);
                    assert_eq!(executed(connection), 0, "PG{version} added={added}");
                    assert_eq!(
                        scalar(
                            connection,
                            "SELECT count(*) FROM pg_constraint WHERE conname = 'fk_c_p'"
                        ),
                        0
                    );
                } else {
                    let artifact = connected_artifact(&d, connection, false);
                    on_server(connection, WITNESS);
                    succeeds(approved_apply(
                        &d,
                        connection,
                        &artifact,
                        &["--allow", "constraint"],
                    ));
                    assert!(executed(connection) > 0, "PG{version} added={added}");
                    assert_eq!(
                        scalar(
                            connection,
                            "SELECT count(*) FROM pg_constraint WHERE conname = 'fk_c_p' AND convalidated"
                        ),
                        1
                    );
                    succeeds(d.run(&["verify", "--db", connection]));
                }
            }
        }
    }
}
