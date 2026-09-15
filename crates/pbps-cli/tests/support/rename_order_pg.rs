use super::*;
use pbps_model::Change;

fn table(name: &str, renamed_from: Option<&str>, constraints: &str) -> String {
    format!(
        "table: {name}\n{}columns:\n  id: {{type: integer, nullable: false}}\n{constraints}",
        renamed_from.map_or_else(String::new, |from| format!("renamed_from: {from}\n"))
    )
}

#[test]
#[ignore = "needs live PostgreSQL; set PBPS_TEST_PG_DB"]
fn saved_rename_dependencies_apply_and_record_only_surviving_objects() {
    for case in [
        "fk_unrelated",
        "fk_dependent",
        "index_same",
        "index_cross",
        "owner_unique",
        "owner_primary",
    ] {
        let own = OwnDatabase::new(&server(), &format!("rename_dependencies467_{case}"));
        let connection = own.connection();
        on_server(connection, "CREATE SCHEMA app; CREATE SCHEMA moved");
        let d = Demo::new(&format!("rename-dependencies467-{case}"));
        let mut before = Vec::new();
        let mut after = Vec::new();
        if case.starts_with("index_") {
            before.push(table("app.old", None, "indexes:\n  target: {columns: [id], where: 'id > 0'}\n  kept: {columns: [id], where: 'id > 0'}\n"));
            let target = if case == "index_cross" {
                "moved.target"
            } else {
                "app.target"
            };
            after.push(table(
                target,
                Some("app.old"),
                "indexes:\n  kept: {columns: [id], where: 'id > 0'}\n",
            ));
        } else if case.starts_with("owner_") {
            before.push(table(
                "app.owner_old",
                None,
                if case == "owner_primary" {
                    "primary_key: {name: target, columns: [id]}\n"
                } else {
                    "unique:\n  target: [id]\n"
                },
            ));
            before.push(table("app.claimant_old", None, ""));
            after.push(table("app.owner_new", Some("app.owner_old"), ""));
            after.push(table("app.target", Some("app.claimant_old"), ""));
        } else {
            before.push(table("app.owner", None, "unique:\n  target: [id]\n"));
            before.push(table(
                "app.parent",
                None,
                "primary_key: {name: pk_parent, columns: [id]}\n",
            ));
            let parent = if case == "fk_dependent" {
                "app.owner"
            } else {
                "app.parent"
            };
            before.push(table(
                "app.old",
                None,
                &format!(
                    "foreign_keys:\n  fk_child:\n    columns: [id]\n    references: {parent}(id)\n"
                ),
            ));
            after.push(table("app.owner", None, ""));
            after.push(before[1].clone());
            after.push(table("app.target", Some("app.old"), ""));
        }
        for (i, body) in before.iter().enumerate() {
            std::fs::write(d.dir.join(format!("schema/table{i}.yml")), body).unwrap();
        }
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", connection]));
        for body in &before {
            let name = body
                .lines()
                .next()
                .unwrap()
                .strip_prefix("table: ")
                .unwrap();
            on_server(connection, &format!("INSERT INTO {name} VALUES (7)"));
        }
        for (i, body) in after.iter().enumerate() {
            std::fs::write(d.dir.join(format!("schema/table{i}.yml")), body).unwrap();
        }
        let artifact = connected_artifact(&d, connection, false);
        let saved: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
        // The saved typed order and address are the ones approved and replayed.
        if case == "fk_dependent" {
            assert!(
                matches!(&saved.changes.changes[0].change, Change::DropForeignKey { table, .. } if table.to_string() == "app.old")
            );
        } else if case.starts_with("index_") {
            assert!(
                matches!(&saved.changes.changes[0].change, Change::DropIndex { table, .. } if table.to_string() == "app.old")
            );
        } else if case.starts_with("owner_") {
            assert!(
                matches!(&saved.changes.changes[0].change, Change::RenameTable { to, .. } if to.to_string() == "app.owner_new")
            );
        }
        let denied = approved_apply(&d, connection, &artifact, &[]);
        assert_eq!(code(&denied), 1, "{}", stderr(&denied));
        assert!(stderr(&denied).contains("--allow"), "{}", stderr(&denied));
        succeeds(approved_apply(
            &d,
            connection,
            &artifact,
            &["--allow", "rename,destructive"],
        ));
        let recorded = latest_snapshot(connection);
        assert_eq!(recorded.ids, saved.ids);
        for body in &after {
            let name = body
                .lines()
                .next()
                .unwrap()
                .strip_prefix("table: ")
                .unwrap();
            assert!(recorded.schema.tables.contains_key(&name.parse().unwrap()));
            assert_eq!(
                scalar(
                    connection,
                    &format!("SELECT count(*) FROM {name} WHERE id=7")
                ),
                1
            );
        }
        if case.starts_with("index_") {
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname IN ('app','moved') AND c.relkind='i' AND c.relname='target'"
                ),
                0
            );
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname IN ('app','moved') AND c.relkind='i' AND c.relname='kept'"
                ),
                1
            );
        }
        succeeds(d.run(&["verify", "--db", connection]));
        assert!(stdout(&succeeds(d.run(&["plan", "--db", connection]))).contains("No changes"));
    }
}
