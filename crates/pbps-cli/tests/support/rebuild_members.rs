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
        // Approval switched the tree: every source uid is retired and none of
        // them is live, while the replacements are.
        for uid in source_ids.tables.keys().chain(source_ids.columns.keys()) {
            assert!(approved.tombstones.contains_key(uid), "{}", case.name);
            assert!(
                !approved.tables.contains_key(uid) && !approved.columns.contains_key(uid),
                "{}",
                case.name
            );
        }
        approved.check_held_by(&case.desired).unwrap();
        d.commit();
        let check = || {
            let mut plans = vec![vec!["plan", "--no-dev", "--since", source.trim()]];
            if let Some(connection) = connection {
                plans.push(vec!["plan", "--db", connection]);
            }
            for args in plans {
                let out = d.run(&args);
                if args.contains(&"--db") {
                    // The opening row needs its covering leaf, and new
                    // columns are copied as NULL rather than invented data.
                    let source = member_cases::source();
                    let root = member_cases::root();
                    let required_new = case.desired.tables[&root]
                        .columns
                        .iter()
                        .any(|(n, c)| !c.nullable && !source.tables[&root].columns.contains_key(n));
                    let null_key = case.desired.tables[&root]
                        .partition_by
                        .as_ref()
                        .unwrap()
                        .columns
                        .iter()
                        .any(|n| !source.tables[&root].columns.contains_key(n));
                    if required_new
                        || null_key
                        || !case.desired.tables.contains_key(&"app.p1".parse().unwrap())
                    {
                        assert_ne!(code(&out), 0, "{}", case.name);
                        assert!(
                            stderr(&out).contains("max_rows"),
                            "{}: {}",
                            case.name,
                            stderr(&out)
                        );
                    } else {
                        assert_eq!(code(&out), 0, "{}: {}", case.name, stderr(&out));
                    }
                    continue;
                }
                assert_ne!(code(&out), 0, "{}: {args:?}", case.name);
                assert!(
                    stderr(&out).contains("requires connected expansion"),
                    "{}: {args:?}: {}",
                    case.name,
                    stderr(&out)
                );
                assert_eq!(std::fs::read(&ids_path).unwrap(), recorded, "{}", case.name);
            }
        };
        check();
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
