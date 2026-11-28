//! A parent's new index over standing partitions (#1671, DEC-1671.3): apply
//! holds each partition a record names to it, and the plan says why each own the
//! loose rule drops is dropped, for such an addition and for an attach. No plan
//! carries a record yet, so each test writes one by hand onto a parent's
//! addition that drops none of the owns it names. The definitions are written out
//! here as the catalog prints them. Each case runs on both pinned engines.
use super::{
    Demo, OwnDatabase, approved_apply, code, generate_key, key_line, on_server, scalar, stderr,
    stdout, succeeds, text_of,
};
use pbps_model::adoption::{AdditionRehearsal, PartitionOwns, RelationIndex};
use pbps_model::{Change, IndexChange, SavedPlan};
use std::path::{Path, PathBuf};

fn engines(mut check: impl FnMut(&str, &str)) {
    for (server, image) in [
        ("PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_OLD_SCRATCH_IMAGE"),
        ("PBPS_TEST_PG_DB", "PBPS_TEST_PG_SCRATCH_IMAGE"),
    ] {
        let read = |variable| std::env::var(variable).expect(variable);
        check(&read(server), &read(image));
    }
}

fn servers(mut check: impl FnMut(&str)) {
    engines(|server, _| check(server));
}

/// A database holding `setup`, adopted, and its project.
struct Project {
    own: OwnDatabase,
    d: Demo,
}

impl Project {
    fn new(server: &str, slug: &str, setup: &str) -> Self {
        let database = OwnDatabase::new(server, slug);
        on_server(database.connection(), setup);
        let d = Demo::new(slug);
        succeeds(d.run(&["pull", "--db", database.connection()]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            database.connection(),
            "--reason",
            "adopt",
        ]));
        Self { own: database, d }
    }

    fn connection(&self) -> &str {
        self.own.connection()
    }

    /// The parent's declaration file, edited.
    fn edit(&self, with: impl FnOnce(String) -> String) {
        let path = self.d.dir.join("schema/app.ev.yml");
        let tree = std::fs::read_to_string(&path).unwrap();
        let edited = with(tree.clone());
        assert_ne!(edited, tree, "the declaration was not edited:\n{tree}");
        std::fs::write(&path, edited).unwrap();
    }

    /// Gives the parent `ev_m` over `m`.
    fn parent_gains_an_index(&self) {
        self.edit(|tree| {
            tree.replacen(
                "\npartition_by:",
                "\nindexes:\n  ev_m:\n    columns: [m]\n\npartition_by:",
                1,
            )
        });
    }

    /// Gives the parent a column, so that a statement runs ahead of its index.
    fn parent_gains_a_column(&self) {
        self.edit(|tree| tree.replacen("columns:\n", "columns:\n  extra:\n    type: int\n", 1));
    }

    /// Takes the partition's own `name` out of the declarations.
    fn undeclare(&self, name: &str) {
        self.edit(|tree| {
            let indent = |line: &str| line.len() - line.trim_start().len();
            let mut kept: Vec<&str> = Vec::new();
            let mut under: Option<usize> = None;
            for line in tree.lines() {
                if under.is_some_and(|at| indent(line) > at) {
                    continue;
                }
                under = None;
                if line.trim_start() == format!("{name}:") {
                    under = Some(indent(line));
                    continue;
                }
                kept.push(line);
            }
            format!("{}\n", kept.join("\n"))
        });
    }

    /// The connected plan of the declarations, and what planning printed.
    fn plan(&self, name: &str, extra: &[&str]) -> (PathBuf, String) {
        succeeds(self.d.run(&["plan"]));
        self.d.commit();
        let path = self.d.dir.join(format!("{name}.json"));
        let mut args = vec![
            "plan",
            "--db",
            self.connection(),
            "--out",
            path.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let planned = succeeds(self.d.run(&args));
        (path, stdout(&planned))
    }

    fn apply(&self, plan: &Path, extra: &[&str]) -> std::process::Output {
        let mut args = vec!["--allow", "destructive,constraint"];
        args.extend_from_slice(extra);
        approved_apply(&self.d, self.connection(), plan, &args)
    }

    /// The apply refuses, saying `reasons`, and the parent's index is not made.
    fn refused(&self, plan: &Path, extra: &[&str], reasons: &[&str]) {
        let output = self.apply(plan, extra);
        assert_ne!(code(&output), 0, "{}", stdout(&output));
        let said = stderr(&output);
        for reason in reasons {
            assert!(said.contains(reason), "{reason}: {said}");
        }
        assert_eq!(
            scalar(
                self.connection(),
                "SELECT count(*) FROM pg_class WHERE relname = 'ev_m'"
            ),
            0,
            "{said}"
        );
    }

    /// The database is what the declarations say, with nothing left to plan.
    fn in_sync(&self) {
        succeeds(self.d.run(&["verify", "--db", self.connection()]));
        let next = succeeds(self.d.run(&["plan", "--db", self.connection()]));
        assert!(
            stdout(&next).contains("No changes"),
            "{}{}",
            stdout(&next),
            stderr(&next)
        );
    }

    fn oid(&self, index: &str) -> String {
        text_of(
            self.connection(),
            &format!("SELECT 'app.{index}'::regclass::oid::text"),
        )
    }

    /// The index's parent in the partition tree, or nothing where it is the
    /// table's own.
    fn parent_of(&self, index: &str) -> String {
        text_of(
            self.connection(),
            &format!(
                "SELECT coalesce((SELECT inhparent::regclass::text FROM pg_inherits \
                 WHERE inhrelid = 'app.{index}'::regclass), '')"
            ),
        )
    }
}

/// An index of the partition's as the catalog prints it under an empty search
/// path.
fn index(name: &str, columns: &str) -> RelationIndex {
    RelationIndex {
        name: name.into(),
        index: format!("CREATE INDEX {name} ON app.ev_1 USING btree ({columns})").into(),
        constraint: None,
    }
}

/// Writes onto the plan's addition of `ev_m` that `own` is what `app.ev_1`
/// holds, and that the names in `beside` stand beside it.
fn record(plan: &Path, own: Vec<RelationIndex>, beside: &[&str]) {
    let mut saved: SavedPlan = serde_json::from_slice(&std::fs::read(plan).unwrap()).unwrap();
    let record = AdditionRehearsal {
        partitions: vec![PartitionOwns {
            table: pbps_model::TableName::new("app", "ev_1"),
            own,
            adopted: Default::default(),
            beside: beside.iter().map(|name| (*name).into()).collect(),
        }],
    };
    let mut additions = 0;
    for planned in &mut saved.changes.changes {
        if let Change::Index(IndexChange::AddIndex {
            name, rehearsal, ..
        }) = &mut planned.change
            && name.raw() == "ev_m"
        {
            *rehearsal = Some(Box::new(record.clone()));
            additions += 1;
        }
    }
    assert_eq!(additions, 1);
    std::fs::write(plan, serde_json::to_string_pretty(&saved).unwrap()).unwrap();
}

/// Two owns of the partition's that `ev_m` has no key to take, in the order they
/// were made.
const PAIR: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (k integer NOT NULL, m integer) PARTITION BY RANGE (k); \
    CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
    CREATE INDEX ev_1_a ON app.ev_1 (k); \
    CREATE INDEX ev_1_b ON app.ev_1 (k, m); \
    INSERT INTO app.ev VALUES (1, 1)";

fn pair() -> Vec<RelationIndex> {
    vec![index("ev_1_a", "k"), index("ev_1_b", "k, m")]
}

fn make_again(p: &Project, name: &str, columns: &str) {
    on_server(
        p.connection(),
        &format!("DROP INDEX app.{name}; CREATE INDEX {name} ON app.ev_1 ({columns})"),
    );
}

/// An own the plan drops ahead of the addition, made again, would not show to
/// the comparison ahead of the addition: the whole record is held before the
/// first statement. A record claiming what the plan drops is refused at
/// admission.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn apply_refuses_a_partition_made_again_since_a_parents_record() {
    servers(|server| {
        let setup = "CREATE SCHEMA app; \
            CREATE TABLE app.ev (k integer NOT NULL, m integer) PARTITION BY RANGE (k); \
            CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
            CREATE INDEX ev_1_old ON app.ev_1 (m); \
            CREATE INDEX ev_1_a ON app.ev_1 (k); \
            INSERT INTO app.ev VALUES (1, 1)";
        let p = Project::new(server, "adds-start-1671", setup);
        p.undeclare("ev_1_old");
        p.parent_gains_an_index();
        let (plan, _) = p.plan("plan", &[]);
        let own = || vec![index("ev_1_old", "m"), index("ev_1_a", "k")];
        let states = || scalar(p.connection(), "SELECT count(*) FROM public.__pbps_state");
        let before = states();
        for claimed in ["ev_1_old", "ev_1_nothing"] {
            record(&plan, own(), &[claimed]);
            p.refused(&plan, &[], &["invalid parent rehearsal on app.ev"]);
            assert_eq!(states(), before);
        }
        record(&plan, own(), &["ev_1_a"]);
        make_again(&p, "ev_1_old", "m");
        p.refused(
            &plan,
            &[],
            &[
                "`app.ev_1` is not as the parent's addition's rehearsal read it",
                "made in another order: `ev_1_a`",
            ],
        );
        make_again(&p, "ev_1_a", "k");
        let held = p.oid("ev_1_a");
        succeeds(p.apply(&plan, &[]));
        assert_eq!(p.oid("ev_1_a"), held);
        assert_eq!(p.parent_of("ev_1_a"), "");
        p.in_sync();
    });
}

/// What changes a partition between the first comparison and the addition, here
/// beside the plan's own first statement, is refused just before the addition,
/// with everything before it rolled back.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn apply_refuses_an_own_added_or_invalidated_before_the_recorded_addition() {
    servers(|server| {
        let p = Project::new(server, "adds-second-1671", PAIR);
        p.parent_gains_a_column();
        p.parent_gains_an_index();
        let (plan, _) = p.plan("plan", &[]);
        record(&plan, pair(), &["ev_1_a"]);
        for (statement, reason) in [
            (
                "CREATE INDEX ev_1_x ON app.ev_1 (m, k)",
                "it has the index `ev_1_x`, which the plan did not see there",
            ),
            (
                "UPDATE pg_catalog.pg_index SET indisvalid = false \
                 WHERE indexrelid = 'app.ev_1_b'::regclass",
                "its index `ev_1_b` is invalid",
            ),
        ] {
            on_server(
                p.connection(),
                &format!(
                    "CREATE FUNCTION public.change1671() RETURNS event_trigger LANGUAGE plpgsql \
                     AS $f$ BEGIN {statement}; END $f$; \
                     CREATE EVENT TRIGGER change1671 ON ddl_command_end \
                       WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION public.change1671()"
                ),
            );
            p.refused(
                &plan,
                &[],
                &[
                    "`app.ev_1` is not as the parent's addition's rehearsal read it",
                    reason,
                ],
            );
            on_server(
                p.connection(),
                "DROP EVENT TRIGGER change1671; DROP FUNCTION public.change1671()",
            );
        }
        succeeds(p.apply(&plan, &[]));
        p.in_sync();
    });
}

/// A staged addition is compared again just before its statement, in that
/// step's transaction: a partition changed after the first comparison, by a
/// writer whose lock apply then waits for, is refused. A staged plan holds one
/// change and the addition is one statement, so no checkpoint stands ahead of it
/// and the apply is started again.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_staged_parents_addition_is_compared_again_before_its_statement() {
    servers(|server| {
        let p = Project::new(server, "adds-staged-1671", PAIR);
        p.parent_gains_an_index();
        let (plan, _) = p.plan("plan", &["--staged"]);
        record(&plan, pair(), &["ev_1_a"]);
        let connection = p.connection().to_owned();
        let (holding, held) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            // A writer's lock, which the first comparison reads past and the
            // lock ahead of the statement waits for.
            let writer = scope.spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let mut conn = runtime.block_on(async {
                    let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &connection)
                        .await
                        .unwrap();
                    conn.execute("BEGIN; INSERT INTO app.ev_1 VALUES (2, 2)")
                        .await
                        .unwrap();
                    conn
                });
                holding.send(()).unwrap();
                let waiting = "SELECT count(*)::text FROM pg_locks \
                               WHERE relation = 'app.ev_1'::regclass AND NOT granted";
                let started = std::time::Instant::now();
                while text_of(&connection, waiting) == "0" {
                    assert!(
                        started.elapsed().as_secs() < 120,
                        "the apply never waited for the partition"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                runtime.block_on(async {
                    conn.execute("CREATE INDEX ev_1_x ON app.ev_1 (m, k); COMMIT")
                        .await
                        .unwrap();
                });
            });
            held.recv().unwrap();
            p.refused(
                &plan,
                &["--staged"],
                &[
                    "`app.ev_1` is not as the parent's addition's rehearsal read it",
                    "it has the index `ev_1_x`, which the plan did not see there",
                ],
            );
            writer.join().unwrap();
        });
        p.refused(
            &plan,
            &["--staged", "--resume"],
            &["has no staged apply in progress"],
        );
        on_server(p.connection(), "DROP INDEX app.ev_1_x");
        succeeds(p.apply(&plan, &["--staged"]));
        assert_eq!(p.parent_of("ev_1_a"), "");
        p.in_sync();
    });
}

/// What running `sql` says, where it fails.
fn attempt(connection: &str, sql: &str) -> String {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            conn.execute(sql)
                .await
                .map_or_else(|error| error.to_string(), |_| "succeeded".to_owned())
        })
}

/// The parent's own `CREATE INDEX` would let another index be made on a
/// partition until it takes its locks. Apply's lock holds that off from its
/// comparison to the commit.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn apply_locks_the_partitions_from_its_comparison_to_the_commit() {
    servers(|server| {
        let p = Project::new(server, "adds-lock-1671", PAIR);
        p.parent_gains_an_index();
        let (plan, _) = p.plan("plan", &[]);
        record(&plan, pair(), &["ev_1_a"]);
        // The parent's statement has taken no lock when its start trigger fires.
        on_server(
            p.connection(),
            "CREATE FUNCTION public.wait1671() RETURNS event_trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF current_query() LIKE '%\"ev_m\"%' THEN PERFORM pg_sleep(10); END IF; \
             END $$; \
             CREATE EVENT TRIGGER wait1671 ON ddl_command_start WHEN TAG IN ('CREATE INDEX') \
               EXECUTE FUNCTION public.wait1671()",
        );
        let connection = p.connection().to_owned();
        std::thread::scope(|scope| {
            // Beside the apply: once it waits ahead of the parent's index,
            // another index on the partition.
            let racing = scope.spawn(move || {
                let sleeping = "SELECT count(*)::text FROM pg_stat_activity \
                                WHERE datname = current_database() AND wait_event = 'PgSleep'";
                let started = std::time::Instant::now();
                while text_of(&connection, sleeping) == "0" {
                    assert!(
                        started.elapsed().as_secs() < 120,
                        "the apply never reached the parent's index"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                attempt(
                    &connection,
                    "SET lock_timeout = '1s'; CREATE INDEX ev_1_race ON app.ev_1 (m)",
                )
            });
            succeeds(p.apply(&plan, &[]));
            let blocked = racing.join().unwrap();
            assert!(blocked.contains("lock timeout"), "{blocked}");
        });
        on_server(p.connection(), "DROP EVENT TRIGGER wait1671");
        p.in_sync();
    });
}

/// Each change of a plan as its operation and the name it is made on.
fn operations(plan: &Path) -> Vec<(String, String)> {
    let saved: SavedPlan = serde_json::from_slice(&std::fs::read(plan).unwrap()).unwrap();
    let json = serde_json::to_value(&saved.changes).unwrap();
    json["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| {
            let text = |field: &str| change[field].as_str().map(str::to_owned);
            (
                text("op").unwrap(),
                text("name").or_else(|| text("table")).unwrap(),
            )
        })
        .collect()
}

fn listed(entries: &[(&str, &str)]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|(op, name)| ((*op).to_owned(), (*name).to_owned()))
        .collect()
}

/// The loose plan of DEC-1671.1, change for change, with each drop's reason on
/// its line: a parent's new index offers no resolver yet; an attach offers one
/// only for an own it could keep, and none where one was asked.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn each_loose_drop_says_why_and_the_plan_is_the_loose_one() {
    engines(|server, image| {
        let p = Project::new(
            server,
            "adds-reasons-1671",
            "CREATE SCHEMA app; \
             CREATE TABLE app.ev (k integer NOT NULL, m integer) PARTITION BY RANGE (k); \
             CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
             CREATE INDEX ev_1_m ON app.ev_1 (m) WHERE m > 1; \
             CREATE TABLE app.late (k integer NOT NULL, m integer); \
             CREATE INDEX late_m ON app.late (m); \
             CREATE INDEX late_old ON app.late (m) WHERE m > 2; \
             INSERT INTO app.ev VALUES (1, 1)",
        );
        p.parent_gains_an_index();
        let (plan, said) = p.plan("added", &[]);
        assert_eq!(
            operations(&plan),
            listed(&[
                ("drop_index", "ev_1_m"),
                ("add_index", "ev_m"),
                ("add_index", "ev_1_m"),
            ]),
            "{said}"
        );
        let reason = "ev_1_m, built again: the parent's new index ev_m may take it";
        for line in [format!("- index {reason}"), format!("+ index {reason}")] {
            assert!(said.contains(&line), "{line}: {said}");
        }
        assert!(!said.contains("a resolver can keep it"), "{said}");
        assert!(!said.contains("+ index ev_m,"), "{said}");
        succeeds(p.apply(&plan, &[]));
        p.in_sync();

        p.edit(|tree| {
            tree.replacen(
                "partitions:\n",
                "partitions:\n  late:\n    from: [10]\n    to: [20]\n    indexes:\n      \
                 late_m:\n        columns: [m]\n",
                1,
            )
        });
        std::fs::remove_file(p.d.dir.join("schema/app.late.yml")).unwrap();
        let (plan, said) = p.plan("attached", &[]);
        let loose = listed(&[
            ("drop_index", "late_m"),
            ("drop_index", "late_old"),
            ("attach_partition", "app.late"),
            ("add_index", "late_m"),
        ]);
        assert_eq!(operations(&plan), loose, "{said}");
        for line in [
            "- index late_m, built again: the parent's index ev_m may take it  [",
            "+ index late_m, built again: the parent's index ev_m may take it",
            "- index late_old, since the parent's index ev_m may take it (a resolver can keep \
             it)  [",
        ] {
            assert!(said.contains(line), "{line}: {said}");
        }

        // A resolver that is asked and keeps nothing, here because the engine
        // takes no index with another predicate, leaves the same plan and is not
        // offered.
        generate_key(&p.d);
        std::fs::write(
            p.d.dir.join("pbps.yml"),
            format!(
                "dialect: postgres\nresolvers:\n  scratch:\n    kind: docker\n    image: \
                 '{image}'\n    pull: never\nenvironments:\n  prod:\n    url_env: \
                 PBPS_ADDS1671_TARGET\n    resolve_with: scratch\n{}",
                key_line()
            ),
        )
        .unwrap();
        p.d.commit();
        let path = p.d.dir.join("asked.json");
        let asked = succeeds(p.d.run_with_env(
            &["plan", "--env", "prod", "--out", path.to_str().unwrap()],
            &[("PBPS_ADDS1671_TARGET", p.connection())],
        ));
        let said = stdout(&asked);
        assert_eq!(operations(&path), loose, "{said}");
        assert!(
            said.contains("- index late_old, since the parent's index ev_m may take it  ["),
            "{said}"
        );
        assert!(!said.contains("a resolver can keep it"), "{said}");
        succeeds(p.apply(&plan, &[]));
        p.in_sync();
    });
}
