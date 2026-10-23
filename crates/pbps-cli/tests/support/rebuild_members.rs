//! The same member table must reach qualification through files and catalogs.

use super::*;
use pbps_diff::rebuild::member_cases;

fn declaration(schema: &pbps_model::Schema) -> String {
    let root = member_cases::root();
    let partitions: Vec<_> = schema
        .tables
        .iter()
        .filter(|(name, _)| *name != &root)
        .collect();
    pbps_load::render_partitioned(&root, &schema.tables[&root], &partitions, &[], None)
}

pub(super) fn check(connection: Option<&str>) {
    for (index, case) in member_cases::cases().into_iter().enumerate() {
        let mode = if connection.is_some() {
            "live"
        } else {
            "offline"
        };
        let d = Demo::new(&format!("rebuild2298-members-{mode}-{index}"));
        let path = d.dir.join("schema/app.p.yml");
        if let Some(connection) = connection {
            succeeds(d.run(&["pull", "--db", connection]));
        } else {
            std::fs::write(&path, declaration(&member_cases::source())).unwrap();
            succeeds(d.run(&["plan", "--no-dev"]));
        }
        d.commit();
        let rev = Command::new("git")
            .arg("-C")
            .arg(&d.dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(rev.status.success());
        let source = String::from_utf8(rev.stdout).unwrap();
        if let Some(connection) = connection {
            succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt source"]));
        }
        let ids_path = d.dir.join("schema.ids.json");
        let source_bytes = std::fs::read(&ids_path).unwrap();
        let source_ids: pbps_model::IdsFile = serde_json::from_slice(&source_bytes).unwrap();
        std::fs::write(&path, declaration(&case.desired)).unwrap();
        let record = d.run(&[
            "rebuild-table",
            "app.p",
            "--max-rows",
            "20",
            "--reason",
            "maintenance",
        ]);
        if let Some(why) = case.refusal {
            assert_ne!(code(&record), 0, "{}", case.name);
            assert!(
                stderr(&record).contains(why),
                "{}: {}",
                case.name,
                stderr(&record)
            );
            assert_eq!(
                std::fs::read(&ids_path).unwrap(),
                source_bytes,
                "{}",
                case.name
            );
            continue;
        }
        assert_eq!(code(&record), 0, "{}: {}", case.name, stderr(&record));
        let recorded = std::fs::read(&ids_path).unwrap();
        let approved: pbps_model::IdsFile = serde_json::from_slice(&recorded).unwrap();
        approved.validate().unwrap();
        assert_eq!(approved.tables, source_ids.tables, "{}", case.name);
        assert_eq!(approved.columns, source_ids.columns, "{}", case.name);
        assert_eq!(approved.tombstones, source_ids.tombstones, "{}", case.name);
        d.commit();
        let check = || {
            let mut plans = vec![vec!["plan", "--no-dev", "--since", source.trim()]];
            if let Some(connection) = connection {
                plans.push(vec!["plan", "--db", connection]);
            }
            for args in plans {
                let out = d.run(&args);
                assert_ne!(code(&out), 0, "{}: {args:?}", case.name);
                assert!(
                    stderr(&out).contains(
                        "recorded intent is valid; table rebuild execution is not available yet"
                    ),
                    "{}: {args:?}: {}",
                    case.name,
                    stderr(&out)
                );
                assert_eq!(std::fs::read(&ids_path).unwrap(), recorded, "{}", case.name);
            }
        };
        check();
        if let Some(connection) = connection {
            for args in [
                vec!["snapshot", "--db", connection, "--force"],
                vec!["baseline", "--db", connection, "--reason", "retain source"],
            ] {
                let out = d.run(&args);
                assert_eq!(code(&out), 0, "{}: {args:?}: {}", case.name, stderr(&out));
                let snapshot = latest_snapshot(connection);
                snapshot.ids.validate().unwrap();
                assert_eq!(snapshot.ids, approved, "{}: {args:?}", case.name);
                check();
            }
        }
    }
}

pub(super) fn live() {
    let own = OwnDatabase::new(&server(), "rebuild2298-members");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE app.p(k integer, bucket integer, payload integer) PARTITION BY RANGE(k); CREATE TABLE app.p1 PARTITION OF app.p FOR VALUES FROM(0) TO(10); INSERT INTO app.p VALUES(1,2,3)",
    );
    check(Some(connection));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM app.p WHERE k=1 AND bucket=2 AND payload=3"
        ),
        1
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM app.p1"), 1);
}
