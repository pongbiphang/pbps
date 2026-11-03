//! Creation-time domain DEFAULT copies remain physical evidence, not authored overrides.
use super::*;
use pbps_model::{Change, ManagedType, SavedPlan, StateSnapshot};

fn domain(d: &Demo, name: &str, base: &str, default: Option<&str>) {
    let default = default
        .map(|v| format!("default: {v:?}\n"))
        .unwrap_or_default();
    let nullability = if name == "inner" {
        "not_null: true\n"
    } else {
        ""
    };
    std::fs::write(
        d.dir.join(format!("schema/app.{name}.type.yml")),
        format!("domain: app.{name}\ntype: {base}\n{nullability}{default}"),
    )
    .unwrap();
}
fn plan(d: &Demo, db: &str) -> (PathBuf, SavedPlan) {
    let path = d.dir.join("copy.plan.json");
    succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
    let saved = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    (path, saved)
}
fn snapshot(db: &str) -> StateSnapshot {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, db)
                .await
                .unwrap();
            let rows = conn
                .query("SELECT state_json FROM public.__pbps_state ORDER BY id DESC LIMIT 1")
                .await
                .unwrap();
            StateSnapshot::read_json(rows[0].try_get::<&str>("state_json").unwrap().unwrap())
                .unwrap()
        })
}
fn default(definition: &ManagedType) -> Option<&str> {
    let ManagedType::Domain { default, .. } = definition else {
        panic!("domain expected")
    };
    default.as_deref()
}
fn assert_default(db: &str, name: &str, value: Option<&str>) {
    assert_eq!(
        default(&snapshot(db).schema.types[&name.parse().unwrap()]),
        value
    );
}
fn only_default(saved: &SavedPlan, name: &str, value: Option<&str>) {
    assert_eq!(saved.changes.changes.len(), 1, "{:?}", saved.changes);
    let Change::AlterDomain { name: at, to, .. } = &saved.changes.changes[0].change else {
        panic!("DEFAULT change expected")
    };
    assert_eq!(at, &name.parse::<pbps_model::TypeRef>().unwrap());
    assert_eq!(default(to), value);
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn copied_defaults_bootstrap_and_keep_their_creation_values_after_base_edits() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        let own = OwnDatabase::new(&server, "copy2537");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE SCHEMA ext; CREATE DOMAIN ext.base AS integer DEFAULT 13",
        );
        let d = Demo::new("copy2537");
        std::fs::write(
            d.dir.join("pbps.yml"),
            "dialect: postgres\ntrusted_roles: [postgres]\n",
        )
        .unwrap();
        domain(&d, "external_bootstrap", "ext.base", None);
        domain(&d, "inner", "integer", Some("7"));
        domain(&d, "copied", "app.inner", None);
        domain(&d, "deeper", "app.copied", None);
        domain(&d, "same", "app.inner", Some("7"));
        domain(&d, "other", "app.inner", Some("9"));
        domain(&d, "array_base", "app.inner[]", None);
        d.table("table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  a: {type: app.copied}\n  b: {type: app.same}\n  c: {type: app.other}\n  d: {type: app.array_base}\n  e: {type: 'app.inner[]'}\n  f: {type: app.deeper}\n  g: {type: app.inner}\nprimary_key: [id]\n");
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        let state = snapshot(db);
        let copied = "app.copied".parse().unwrap();
        assert_eq!(default(&state.schema.types[&copied]), Some("7"));
        assert_eq!(default(&state.declared.types[&copied]), None);
        assert_default(db, "app.external_bootstrap", Some("13"));
        assert!(
            !state
                .schema
                .types
                .contains_key(&"ext.base".parse().unwrap())
        );
        assert_default(db, "app.deeper", Some("7"));
        assert_default(db, "app.array_base", None);
        succeeds(d.run(&["verify", "--db", db]));
        assert!(plan(&d, db).1.changes.is_empty());
        on_server(db, "INSERT INTO app.t(id) VALUES(1)");
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM app.t WHERE a=7 AND b=7 AND c=9 AND d IS NULL AND e IS NULL AND f=7 AND g=7"
            ),
            1
        );

        for value in [None, Some("11")] {
            domain(&d, "inner", "integer", value);
            d.commit();
            let (path, saved) = plan(&d, db);
            only_default(&saved, "app.inner", value);
            succeeds(apply_plan(&d, db, &path, false));
            for (name, value) in [
                ("app.copied", "7"),
                ("app.deeper", "7"),
                ("app.same", "7"),
                ("app.other", "9"),
            ] {
                assert_default(db, name, Some(value));
            }
            succeeds(d.run(&["verify", "--db", db]));
            assert!(plan(&d, db).1.changes.is_empty());
        }
        on_server(db, "INSERT INTO app.t(id) VALUES(2)");
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM app.t WHERE id=2 AND a=7 AND f=7 AND g=11"
            ),
            1
        );
        domain(&d, "fresh", "app.inner", None);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let (path, saved) = plan(&d, db);
        assert_eq!(saved.changes.changes.len(), 1);
        succeeds(apply_plan(&d, db, &path, false));
        assert_default(db, "app.fresh", Some("11"));
        assert!(plan(&d, db).1.changes.is_empty());
        domain(&d, "external_fresh", "ext.base", None);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let (path, saved) = plan(&d, db);
        assert_eq!(saved.changes.changes.len(), 1);
        succeeds(apply_plan(&d, db, &path, false));
        assert_default(db, "app.external_fresh", Some("13"));
        assert!(plan(&d, db).1.changes.is_empty());
        // Verify uses raw stored evidence: overlaying omission cannot hide drift.
        on_server(db, "ALTER DOMAIN app.copied DROP DEFAULT");
        assert_ne!(code(&d.run(&["verify", "--db", db])), 0);
        assert_ne!(code(&d.run(&["plan", "--db", db])), 0);
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn explicit_defaults_equal_to_a_copy_are_written_and_can_be_removed() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        let own = OwnDatabase::new(&server, "override2537");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app");
        let d = Demo::new("override2537");
        domain(&d, "inner", "integer", Some("7"));
        domain(&d, "outer", "app.inner", None);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        for value in [Some("7"), Some("9"), None] {
            domain(&d, "outer", "app.inner", value);
            d.commit();
            let (path, saved) = plan(&d, db);
            only_default(&saved, "app.outer", value);
            succeeds(apply_plan(&d, db, &path, false));
            assert_default(db, "app.outer", value);
            if value.is_some() {
                assert_eq!(
                    scalar(
                        db,
                        "SELECT count(*) FROM pg_type WHERE oid='app.outer'::regtype AND typdefaultbin::text LIKE '{COERCETODOMAIN %'"
                    ),
                    1,
                    "equal-valued SET DEFAULT must still execute"
                );
            }
            succeeds(d.run(&["verify", "--db", db]));
            assert!(plan(&d, db).1.changes.is_empty());
        }
        // An unrelated in-place facet must not drop a copied DEFAULT.
        domain(&d, "another", "app.inner", None);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let (path, _) = plan(&d, db);
        succeeds(apply_plan(&d, db, &path, false));
        let file = d.dir.join("schema/app.another.type.yml");
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str("checks: {positive: 'VALUE > 0'}\n");
        std::fs::write(file, text).unwrap();
        d.commit();
        let (path, saved) = plan(&d, db);
        assert!(saved.changes.changes.iter().all(|p| matches!(&p.change, Change::AlterDomain { from, to, .. } if default(from).is_none() && default(to).is_none())));
        succeeds(d.run(&[
            "apply",
            "--db",
            db,
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&path),
            "--allow",
            "constraint",
        ]));
        assert_default(db, "app.another", Some("7"));
        assert!(plan(&d, db).1.changes.is_empty());
        // Both committed rename statements and the final closing read retain
        // the copy under the UID, including after the last checkpoint.
        on_server(db, "CREATE SCHEMA moved");
        std::fs::remove_file(d.dir.join("schema/app.another.type.yml")).unwrap();
        std::fs::write(d.dir.join("schema/moved.final.type.yml"),
            "domain: moved.final\nrenamed_from: app.another\ntype: app.inner\nchecks: {positive: 'VALUE > 0'}\n").unwrap();
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("staged-copy.plan.json");
        succeeds(d.run(&[
            "plan",
            "--db",
            db,
            "--staged",
            "--out",
            path.to_str().unwrap(),
        ]));
        succeeds(d.run(&[
            "apply",
            "--db",
            db,
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&path),
            "--allow",
            "rename",
            "--staged",
        ]));
        assert_default(db, "moved.final", Some("7"));
        succeeds(d.run(&["verify", "--db", db]));
        assert!(plan(&d, db).1.changes.is_empty());
    }
}
