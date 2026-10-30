//! Every measured accepted-but-different profile is accounted for here.
use super::*;
#[path = "partition_moves/manifest.rs"]
mod manifest;
use manifest::{Native, Product, Profile, manifest, profiles};
fn capture(db: &str) -> serde_json::Value {
    on_server(db, &manifest().helpers);
    let mut value: serde_json::Value =
        serde_json::from_str(&text_of(db, "SELECT app.capture()::text")).unwrap();
    value["sequences"]=serde_json::from_str(&text_of(db,"SELECT coalesce(jsonb_agg(jsonb_build_object('name',s.relname,'type',pg_catalog.format_type(q.seqtypid,NULL),'start',q.seqstart,'increment',q.seqincrement,'min',q.seqmin,'max',q.seqmax,'cycle',q.seqcycle,'cache',q.seqcache,'owner',o.relname,'column',a.attname) ORDER BY s.relname),'[]'::jsonb)::text FROM pg_catalog.pg_sequence q JOIN pg_catalog.pg_class s ON s.oid=q.seqrelid JOIN pg_catalog.pg_namespace n ON n.oid=s.relnamespace LEFT JOIN pg_catalog.pg_depend d ON d.classid='pg_catalog.pg_class'::regclass AND d.objid=s.oid AND d.refclassid='pg_catalog.pg_class'::regclass AND d.deptype IN ('a','i') LEFT JOIN pg_catalog.pg_class o ON o.oid=d.refobjid LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid=o.oid AND a.attnum=d.refobjsubid WHERE n.nspname='app'")).unwrap();
    on_server(
        db,
        "DROP FUNCTION app.attempt(text); DROP FUNCTION app.capture(); DROP FUNCTION app.probe(text,integer)",
    );
    value
}
fn native_change(db: &str, p: &Profile, change_first: bool) {
    if change_first {
        on_server(
            db,
            &format!(
                "SET search_path=app,pg_catalog;{};{}",
                p.change, p.move_after
            ),
        );
    } else {
        on_server(
            db,
            &format!(
                "SET search_path=app,pg_catalog;{};{}",
                p.move_before, p.change
            ),
        );
    }
}
/// A fresh database retains the exact native sequence/probe evaluation
/// order without changing the endpoint used for pull or relying on rollback
/// to undo nextval (which PostgreSQL deliberately does not do).
fn native_probes(server: &str, p: &Profile, change_first: bool) -> serde_json::Value {
    let own = OwnDatabase::new(server, "moves2296_native_probes");
    let db = own.connection();
    on_server(db, &p.setup);
    native_change(db, p, change_first);
    on_server(db, &manifest().helpers);
    let mut answer = serde_json::Map::new();
    for (table, key) in [(p.parent.as_str(), 5), ("app.b", 6), ("app.a", 15)] {
        if table == "app.a" && p.motion == "drop" {
            continue;
        }
        let value = serde_json::from_str(&text_of(
            db,
            &format!("SELECT app.probe('{table}',{key})::text"),
        ))
        .unwrap();
        answer.insert(table.into(), value);
    }
    serde_json::Value::Object(answer)
}
fn files(d: &Demo) -> Vec<(PathBuf, Vec<u8>)> {
    pbps_load::schema_files(&d.dir.join("schema"))
        .unwrap()
        .into_iter()
        .map(|p| {
            (
                p.strip_prefix(d.dir.join("schema")).unwrap().to_path_buf(),
                std::fs::read(p).unwrap(),
            )
        })
        .collect()
}
fn install(d: &Demo, goal: &[(PathBuf, Vec<u8>)]) {
    for p in pbps_load::schema_files(&d.dir.join("schema")).unwrap() {
        std::fs::remove_file(p).unwrap();
    }
    for (p, bytes) in goal {
        let path = d.dir.join("schema").join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}
fn refusal(output: &Output, label: &str) {
    assert!(
        matches!(code(output), 1 | 2),
        "{label}: {}{}",
        stdout(output),
        stderr(output)
    );
    assert!(
        stderr(output).contains("unrepresentable")
            || stderr(output).contains("cannot be represented")
            || stderr(output).contains("identity")
            || stderr(output).contains("cannot share")
            || stderr(output).contains("separate plans")
            || stderr(output).contains("NOT NULL"),
        "{label}: {}",
        stderr(output)
    );
}
fn reached(expected: Product, actual: Product, label: &str) {
    assert_eq!(expected, actual, "{label}: product outcome");
}
#[test]
fn an_expected_outcome_cannot_turn_an_earlier_refusal_into_success() {
    let mut p = profiles().remove(0);
    let actual = p.outcome(true);
    p.expected.product[0] = if actual == Product::Apply {
        Product::Plan
    } else {
        Product::Apply
    };
    assert!(
        std::panic::catch_unwind(|| reached(p.outcome(true), actual, "corrupted expected"))
            .is_err()
    );
}
fn refused(output: &Output, db: &str, state: &serde_json::Value, label: &str) {
    refusal(output, label);
    assert_eq!(&capture(db), state, "{label}: refusal mutated database");
    println!("CAPABILITY {label}: {}", stderr(output));
}
#[path = "partition_moves/identity.rs"]
mod identity;
use identity::identity_meeting_refusal;

pub(super) fn measured(server: &str) {
    run_profiles(server, profiles());
}
fn run_profiles(server: &str, selected: Vec<Profile>) {
    let major = scalar(
        server,
        "SELECT current_setting('server_version_num')::bigint/10000",
    );
    let expected_count: usize = selected
        .iter()
        .filter(|p| p.major == major)
        .map(|p| p.orders.len())
        .sum();
    let mut accounted = 0;
    for p in selected.into_iter().filter(|p| p.major == major) {
        let mut prior = None;
        for &change_first in &p.orders {
            let actual = run_profile(server, &p, change_first, &mut prior);
            reached(
                p.outcome(change_first),
                actual,
                &format!("{:?}/change-first={change_first}", p.key()),
            );
            accounted += 1;
        }
    }
    assert_eq!(accounted, expected_count);
}
fn run_profile(
    server: &str,
    p: &Profile,
    change_first: bool,
    prior: &mut Option<(serde_json::Value, serde_json::Value)>,
) -> Product {
    let major = p.major;
    let label = format!(
        "PG{major}/{}/{}/{}/change-first={change_first}",
        p.name, p.motion, p.profile
    );
    println!("BEGIN {label}");
    let native = OwnDatabase::new(server, "moves2296_native");
    let n = native.connection();
    on_server(n, &p.setup);
    native_change(n, p, change_first);
    let expected = capture(n);
    let expected_probes = native_probes(server, p, change_first);
    println!("NATIVE-PROBES {label}: {expected_probes}");
    if p.expected.native == Native::Different {
        if let Some(previous) = prior.as_ref() {
            assert_ne!(
                previous,
                &(expected.clone(), expected_probes.clone()),
                "{label}: the measured D pair no longer differs"
            );
        } else {
            *prior = Some((expected.clone(), expected_probes.clone()));
        }
    }

    let outcome = p.outcome(change_first);
    let target = Demo::new("moves2296-goal");
    let pulled = target.run(&["pull", "--db", n]);
    let identity_reader_limit = outcome == Product::IdentityPlan;
    if outcome == Product::Identity {
        identity_meeting_refusal(major);
        assert_eq!(
            capture(n),
            expected,
            "{label}: identity refusal mutated native endpoint"
        );
        return Product::Identity;
    }
    if code(&pulled) != 0 && !identity_reader_limit {
        refused(&pulled, n, &expected, &label);
        return Product::GoalPull;
    }
    if identity_reader_limit {
        assert!(
            stderr(&pulled).contains("seq_start")
                || stderr(&pulled).to_ascii_lowercase().contains("identity"),
            "{label}: unexpected reader result: {}",
            stderr(&pulled)
        );
    }
    let goal = files(&target);
    let goal_schema = pbps_load::load_schema_dir(&target.dir.join("schema"))
        .unwrap()
        .schema;
    let parent_name: pbps_model::TableName = p.parent.parse().unwrap();
    if !identity_reader_limit && !goal_schema.tables.contains_key(&parent_name) {
        assert!(
            stderr(&pulled).contains("left out of the pull entirely")
                || stderr(&pulled).contains("holds only the type"),
            "{label}: missing parent without an explicit reader limitation"
        );
        // The reader's partial result cannot stand in for the full requested
        // managed set. Naming that parent forces the ordinary baseline route
        // to refuse it before recording a partial snapshot.
        std::fs::write(target.dir.join("schema/app.p.yml"),"table: app.p\ncolumns:\n  k: {type: integer, nullable: false}\n  d: {type: integer}\npartition_by: [k]\npartitions:\n  b: {from: [0], to: [10]}\n").unwrap();
        succeeds(target.run(&["plan"]));
        target.commit();
        let rejected = target.run(&[
            "baseline",
            "--db",
            n,
            "--reason",
            "request the whole native endpoint",
        ]);
        refused(&rejected, n, &expected, &label);
        return Product::GoalRecord;
    }
    let hidden_local = expected["constraints"].as_array().is_some_and(|cs| {
        cs.iter().any(|c| {
            c["table"] == "a"
                && c["kind"] == "n"
                && c["local"] == true
                && c["inherited"].as_i64().is_some_and(|n| n > 0)
        })
    });
    let own = OwnDatabase::new(server, "moves2296_product");
    let db = own.connection();
    on_server(db, &p.setup);
    let initial = capture(db);
    let d = Demo::new("moves2296-product");
    let baseline = d.run(&["pull", "--db", db]);
    if code(&baseline) != 0 {
        refused(&baseline, db, &initial, &label);
        return Product::BaselinePull;
    }
    if !pbps_load::load_schema_dir(&d.dir.join("schema"))
        .unwrap()
        .schema
        .tables
        .contains_key(&"app.p".parse::<pbps_model::TableName>().unwrap())
    {
        assert!(
            stderr(&baseline).contains("left out of the pull entirely")
                || stderr(&baseline).contains("was left out"),
            "{label}: partial baseline without a named limitation: {}",
            stderr(&baseline)
        );
        if stderr(&baseline).contains("pbps validate` refuses") {
            let raw = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut c = pbps_db::Conn::connect(pbps_db::Driver::Postgres, db)
                        .await
                        .unwrap();
                    pbps_pg::catalog::introspect(&mut c).await.unwrap().schema
                });
            let parts: Vec<_> = raw
                .tables
                .iter()
                .filter(|(_, t)| {
                    t.partition_of
                        .as_ref()
                        .is_some_and(|of| of.parent == parent_name)
                })
                .collect();
            std::fs::write(
                d.dir.join("schema/app.p.yml"),
                pbps_load::render_partitioned(
                    &parent_name,
                    &raw.tables[&parent_name],
                    &parts,
                    &[],
                    None,
                ),
            )
            .unwrap();
            let rejected = d.run(&["validate"]);
            assert_eq!(
                code(&rejected),
                2,
                "{label}: {}{}",
                stdout(&rejected),
                stderr(&rejected)
            );
            assert!(
                stdout(&rejected).contains("already has as a clone")
                    || stderr(&rejected).contains("already has as a clone"),
                "{label}: {}{}",
                stdout(&rejected),
                stderr(&rejected)
            );
            assert_eq!(
                capture(db),
                initial,
                "{label}: refused native declarations mutated baseline"
            );
            println!(
                "CAPABILITY {label}: {}{}",
                stdout(&rejected),
                stderr(&rejected)
            );
            return Product::BaselineValidate;
        }
        std::fs::write(d.dir.join("schema/app.p.yml"),"table: app.p\ncolumns:\n  k: {type: integer, nullable: false}\n  d: {type: integer}\npartition_by: [k]\npartitions:\n  b: {from: [0], to: [10]}\n").unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        let rejected = d.run(&[
            "baseline",
            "--db",
            db,
            "--reason",
            "request the whole native baseline",
        ]);
        refused(&rejected, db, &initial, &label);
        return Product::BaselineRecord;
    }
    d.commit();
    let adopted = d.run(&[
        "baseline",
        "--db",
        db,
        "--reason",
        "adopt measured native state",
    ]);
    if code(&adopted) != 0 {
        refused(&adopted, db, &initial, &label);
        return Product::Adopt;
    }
    install(&d, &goal);
    if identity_reader_limit {
        std::fs::write(d.dir.join("schema/app.p.yml"),"table: app.p\ncolumns:\n  k: {type: integer, nullable: false}\n  d: {type: integer, nullable: false, default: '1'}\n  x: {type: integer, nullable: false, identity: [1, 1]}\npartition_by: [k]\npartitions:\n  b: {from: [0], to: [10]}\n").unwrap();
        if p.motion == "detach" {
            std::fs::write(d.dir.join("schema/app.a.yml"),format!("table: app.a\ncolumns:\n  k: {{type: integer, nullable: false}}\n  d: {{type: integer, nullable: false, default: '1'}}\n{}",if change_first{"  x: {type: integer}\n"}else{""})).unwrap();
        }
        // Resolve declaration identities using the same public identity entry
        // as the CLI. This creates no execution/order certificate.
        let loaded = pbps_load::load_schema_dir(&d.dir.join("schema")).unwrap();
        let path = d.dir.join("schema.ids.json");
        let old: pbps_model::IdsFile =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let intents: Vec<_> = old
            .tables
            .iter()
            .filter(|(_, n)| !loaded.schema.tables.contains_key(*n))
            .map(|(_, n)| pbps_model::Intent::DropTable {
                table: n.clone(),
                reason: "measured partition retirement".parse().unwrap(),
            })
            .collect();
        let resolved = pbps_diff::resolve(
            &loaded.schema,
            &old,
            &intents,
            &pbps_diff::Context {
                operator: "fixture".into(),
                today: "2026-10-24".into(),
            },
        )
        .unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(&resolved.ids).unwrap()).unwrap();
    }
    for command in &p.intents {
        succeeds(d.run(&command.iter().map(String::as_str).collect::<Vec<_>>()));
    }

    if p.motion == "drop" {
        succeeds(d.run(&[
            "drop-table",
            "app.a",
            "--reason",
            "measured partition retirement",
        ]));
    }
    // Pulling the measured development endpoint resolves added/detached
    // identities through the public command before reviewing the deployment.
    // An unavailable offline order must not mint an execution certificate.
    if !identity_reader_limit {
        succeeds(d.run(&["pull", "--db", n, "--force"]));
    }
    if outcome == Product::LocalValidate {
        assert!(hidden_local, "{label}: missing recorded local NOT NULL");
        let p_file = d.dir.join("schema/app.p.yml");
        let mut schema = pbps_load::load_schema_dir(&d.dir.join("schema"))
            .unwrap()
            .schema;
        schema
            .tables
            .get_mut(&"app.a".parse::<pbps_model::TableName>().unwrap())
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns
            .entry("d".into())
            .or_default()
            .not_null = true;
        let parent: pbps_model::TableName = "app.p".parse().unwrap();
        let parts: Vec<_> = schema
            .tables
            .iter()
            .filter(|(_, t)| t.partition_of.as_ref().is_some_and(|p| p.parent == parent))
            .collect();
        std::fs::write(
            p_file,
            pbps_load::render_partitioned(&parent, &schema.tables[&parent], &parts, &[], None),
        )
        .unwrap();
        let rejected = d.run(&["validate"]);
        refused(&rejected, db, &initial, &label);
        return Product::LocalValidate;
    }
    let preview = d.run(&["plan"]);
    if code(&preview) != 0 {
        assert_eq!(
            code(&preview),
            1,
            "{label}: preview crashed: {}",
            stderr(&preview)
        );
    }
    d.commit();
    let path = d.dir.join("plan.json");
    let script = d.dir.join("plan.sql");
    let plan = d.run(&[
        "plan",
        "--db",
        db,
        "--out",
        path.to_str().unwrap(),
        "--sql",
        script.to_str().unwrap(),
    ]);
    if matches!(outcome, Product::Plan | Product::IdentityPlan) {
        refused(&plan, db, &initial, &label);
        return if identity_reader_limit {
            Product::IdentityPlan
        } else {
            Product::Plan
        };
    }
    succeeds(plan);
    println!(
        "PLAN {label}: {}",
        std::fs::read_to_string(&script).unwrap()
    );
    succeeds(approved_apply(
        &d,
        db,
        &path,
        &[
            "--allow",
            "rename",
            "--allow",
            "destructive",
            "--allow",
            "narrowing",
            "--allow",
            "not-null",
            "--allow",
            "constraint",
            "--allow",
            "numbering",
        ],
    ));
    assert_eq!(
        capture(db),
        expected,
        "{label}: applied catalog/rows differ from the declared native endpoint"
    );
    // Default INSERT observations are part of the evidence too, after the
    // structural equality check so their rows do not alter that comparison.
    for (table, key) in [(p.parent.as_str(), 5), ("app.b", 6), ("app.a", 15)] {
        if table == "app.a" && p.motion == "drop" {
            continue;
        }
        on_server(db, &manifest().helpers);
        let query = format!("SELECT app.probe('{table}',{key})::text");
        let actual: serde_json::Value = serde_json::from_str(&text_of(db, &query)).unwrap();
        assert_eq!(
            &actual, &expected_probes[table],
            "{label}: {table} default INSERT"
        );
        on_server(
            db,
            "DROP FUNCTION app.attempt(text); DROP FUNCTION app.capture(); DROP FUNCTION app.probe(text,integer)",
        );
    }
    succeeds(d.run(&["verify", "--db", db]));
    let empty = d.dir.join("empty.json");
    succeeds(d.run(&["plan", "--db", db, "--out", empty.to_str().unwrap()]));
    let empty: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(empty).unwrap()).unwrap();
    assert!(
        empty.changes.is_empty(),
        "{label}: replan was not empty: {:?}",
        empty.changes
    );
    println!("APPLIED-VERIFIED-EMPTY {label}");
    Product::Apply
}

/// Native serial numbering uses the same type and continuation contract as
/// the product; the original bigint-sequence D profiles stay unchanged.
pub(super) fn supported_numbering(server: &str) {
    let mut selected: Vec<_> = profiles()
        .into_iter()
        .filter(|p| p.name == "sequence-add" && p.motion == "attach")
        .collect();
    for p in &mut selected {
        p.expected.product = [Product::GoalRecord, Product::Apply];
        p.change = p
            .change
            .replace("CREATE SEQUENCE seq", "CREATE SEQUENCE seq AS integer");
        p.change
            .push_str("; SELECT pg_catalog.setval('app.seq', (SELECT max(d)+1 FROM app.p), false)");
    }
    // The change-first defaultless endpoint retains its reader refusal; the
    // move-first endpoint exercises actual certified AlterColumnNumbering.
    run_profiles(server, selected);
}

/// The issue's accepted single-cut ADD, plus both native name-move orders.
pub(super) fn issue_cases(server: &str) {
    let mut selected = Vec::new();
    for major in [16, 18] {
        let base = profiles()
            .into_iter()
            .find(|p| p.major == major && p.name == "column-add" && p.motion == "detach")
            .unwrap();
        let mut add = base.clone();
        add.name = "issue-column-add-attach".into();
        add.motion = "attach".into();
        add.profile = "final".into();
        add.orders = vec![true];
        add.expected.native = Native::Accepted;
        add.expected.product = [Product::Apply; 2];
        add.setup = add.setup.replace(
            "CREATE TABLE a PARTITION OF p FOR VALUES FROM(10) TO(20)",
            "CREATE TABLE a(k integer NOT NULL,d integer NOT NULL DEFAULT 1,x integer DEFAULT 7)",
        );
        add.move_before =
            "ALTER TABLE app.p ATTACH PARTITION app.a FOR VALUES FROM(10) TO(20)".into();
        add.move_after = add.move_before.clone();
        let mut generated = add.clone();
        generated.name = "issue-generated-add-attach".into();
        generated.change =
            "ALTER TABLE app.p ADD COLUMN x integer GENERATED ALWAYS AS (d*2) STORED".into();
        generated.setup = generated.setup.replace(
            "x integer DEFAULT 7",
            "x integer GENERATED ALWAYS AS (d*2) STORED",
        );
        selected.push(add);
        selected.push(generated);
        for schema_move in [false, true] {
            let mut p = base.clone();
            p.name = if schema_move {
                "issue-parent-schema"
            } else {
                "issue-parent-rename"
            }
            .into();
            p.kind = "RenameTable".into();
            p.expected.native = Native::Accepted;
            p.expected.product = [Product::Apply; 2];
            p.intents = vec![vec![
                "rename-table".into(),
                "app.p".into(),
                if schema_move { "other.p" } else { "app.p2" }.into(),
            ]];
            p.motion = "attach".into();
            p.profile = "base".into();
            p.parent = if schema_move { "other.p" } else { "app.p2" }.into();
            p.setup = p.setup.replace(
                "CREATE TABLE a PARTITION OF p FOR VALUES FROM(10) TO(20)",
                "CREATE TABLE a(k integer NOT NULL,d integer NOT NULL DEFAULT 1)",
            );
            p.change = if schema_move {
                "ALTER TABLE app.p SET SCHEMA other"
            } else {
                "ALTER TABLE app.p RENAME TO p2"
            }
            .into();
            p.move_before =
                "ALTER TABLE app.p ATTACH PARTITION app.a FOR VALUES FROM(10) TO(20)".into();
            p.move_after = format!(
                "ALTER TABLE {} ATTACH PARTITION app.a FOR VALUES FROM(10) TO(20)",
                p.parent
            );
            selected.push(p);
        }
    }
    run_profiles(server, selected);
}
