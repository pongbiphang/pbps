//! Apply holds an attach that records a rehearsal to that record (#1671,
//! DEC-1671.2). No plan carries a record yet, so each test writes one by hand
//! into a plan, over an index the plan already leaves to the attach. The
//! definitions are written out here as the catalog prints them. Each case runs
//! on both pinned engines.
use super::{
    Demo, OwnDatabase, approved_apply, code, on_server, scalar, stderr, stdout, succeeds, text_of,
};
use pbps_model::adoption::{AttachRehearsal, RelationIndex};
use pbps_model::{Change, PartitionChange, SavedPlan};
use std::path::{Path, PathBuf};

fn servers(mut check: impl FnMut(&str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).expect(variable));
    }
}

/// A plain index as the catalog prints it under an empty search path.
fn index(name: &str, table: &str, columns: &str) -> RelationIndex {
    // The index of a partitioned table is printed with `ONLY`.
    let only = if table == "ev" { "ONLY " } else { "" };
    RelationIndex {
        name: name.into(),
        index: format!("CREATE INDEX {name} ON {only}app.{table} USING btree ({columns})").into(),
        constraint: None,
    }
}

fn rehearsal(
    parent: &[RelationIndex],
    table: &[RelationIndex],
    kept: &[(&str, &str)],
) -> AttachRehearsal {
    AttachRehearsal {
        parent: parent.to_vec(),
        table: table.to_vec(),
        kept: kept
            .iter()
            .map(|&(own, by)| (own.into(), by.into()))
            .collect(),
    }
}

/// Writes `record` onto the plan's one attach.
fn record(plan: &Path, record: AttachRehearsal) {
    let mut saved: SavedPlan = serde_json::from_slice(&std::fs::read(plan).unwrap()).unwrap();
    let mut attaches = 0;
    for planned in &mut saved.changes.changes {
        if let Change::Partition(PartitionChange::AttachPartition { rehearsal, .. }) =
            &mut planned.change
        {
            *rehearsal = Some(Box::new(record.clone()));
            attaches += 1;
        }
    }
    assert_eq!(attaches, 1);
    std::fs::write(plan, serde_json::to_string_pretty(&saved).unwrap()).unwrap();
}

/// A database holding `setup`, adopted, and its project, which declares
/// `app.late` a partition of `app.ev`.
struct Project {
    own: OwnDatabase,
    d: Demo,
}

impl Project {
    /// `own` is the rest of the partition's entry.
    fn new(server: &str, slug: &str, setup: &str, own: &str) -> Self {
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
        let parent = d.dir.join("schema/app.ev.yml");
        let tree = std::fs::read_to_string(&parent).unwrap();
        std::fs::remove_file(d.dir.join("schema/app.late.yml")).unwrap();
        std::fs::write(
            &parent,
            format!("{tree}partitions:\n  late:\n    from: [0]\n    to: [10]\n{own}"),
        )
        .unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        Self { own: database, d }
    }

    fn connection(&self) -> &str {
        self.own.connection()
    }

    fn plan(&self, extra: &[&str]) -> PathBuf {
        let path = self.d.dir.join("plan.json");
        let mut args = vec![
            "plan",
            "--db",
            self.connection(),
            "--out",
            path.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        succeeds(self.d.run(&args));
        path
    }

    fn apply(&self, plan: &Path, extra: &[&str]) -> std::process::Output {
        let mut args = vec!["--allow", "destructive,constraint"];
        args.extend_from_slice(extra);
        approved_apply(&self.d, self.connection(), plan, &args)
    }

    /// The apply refuses, saying `reasons`.
    fn refused(&self, plan: &Path, extra: &[&str], reasons: &[&str]) {
        let output = self.apply(plan, extra);
        assert_ne!(code(&output), 0, "{}", stdout(&output));
        let said = stderr(&output);
        for reason in reasons {
            assert!(said.contains(reason), "{reason}: {said}");
        }
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

    fn attached(&self) -> bool {
        scalar(
            self.connection(),
            "SELECT count(*) FROM pg_class WHERE oid = 'app.late'::regclass AND relispartition",
        ) == 1
    }

    fn recorded_states(&self) -> i64 {
        scalar(
            self.connection(),
            "SELECT count(*) FROM public.__pbps_state",
        )
    }
}

/// A contested pair under `ev_m`, which the plan drops, and a lone equal under
/// `ev_n`, which it leaves to the attach.
const PAIRS: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, m integer, n integer) \
      PARTITION BY RANGE (k); \
    CREATE INDEX ev_m ON app.ev (m); \
    CREATE INDEX ev_n ON app.ev (n); \
    CREATE TABLE app.late (id integer NOT NULL, k integer NOT NULL, m integer, n integer); \
    CREATE INDEX late_z ON app.late (m); \
    CREATE INDEX late_a ON app.late (m); \
    CREATE INDEX late_n ON app.late (n); \
    INSERT INTO app.late VALUES (1, 1, 1, 1)";

fn pairs_record() -> AttachRehearsal {
    rehearsal(
        &[index("ev_m", "ev", "m"), index("ev_n", "ev", "n")],
        &[
            index("late_z", "late", "m"),
            index("late_a", "late", "m"),
            index("late_n", "late", "n"),
        ],
        &[("late_n", "ev_n")],
    )
}

/// Which index an attach takes is decided by the order they were made in, which
/// the drift check cannot see: apply compares it before anything runs, and again
/// just before the attach.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn apply_refuses_a_target_whose_indexes_moved_since_a_recorded_rehearsal() {
    servers(|server| {
        let p = Project::new(server, "adopt-pin-1671", PAIRS, "");
        let plan = p.plan(&[]);
        record(&plan, pairs_record());
        let refused = |relation: &str, index: &str| {
            p.refused(
                &plan,
                &[],
                &[
                    &format!("`app.{relation}` is not as the attach's rehearsal read it"),
                    &format!("made in another order: `{index}`"),
                ],
            );
            // Nothing changed.
            assert!(!p.attached());
            assert_eq!(p.parent_of("late_n"), "");
            p.oid("late_z");
        };
        // An index the plan drops, made again: the record in full is held
        // before anything runs, though the attach would not see this one.
        on_server(
            p.connection(),
            "DROP INDEX app.late_a; CREATE INDEX late_a ON app.late (m)",
        );
        refused("late", "late_n");
        on_server(
            p.connection(),
            "DROP INDEX app.late_n; CREATE INDEX late_n ON app.late (n)",
        );
        // The parent's list is held the same way.
        on_server(
            p.connection(),
            "DROP INDEX app.ev_m; CREATE INDEX ev_m ON app.ev (m)",
        );
        refused("ev", "ev_n");
        on_server(
            p.connection(),
            "DROP INDEX app.ev_n; CREATE INDEX ev_n ON app.ev (n)",
        );
        // A change between the two compares, here made beside the plan's own
        // first drop: both relations are as recorded when apply starts, and the
        // table is not when the attach is about to run.
        on_server(
            p.connection(),
            "CREATE FUNCTION public.add1671() RETURNS event_trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF to_regclass('app.late_x') IS NULL THEN \
                 CREATE INDEX late_x ON app.late (n); \
               END IF; \
             END $$; \
             CREATE EVENT TRIGGER add1671 ON ddl_command_end WHEN TAG IN ('DROP INDEX') \
               EXECUTE FUNCTION public.add1671()",
        );
        let held = p.oid("late_n");
        p.refused(
            &plan,
            &[],
            &[
                "`app.late` is not as the attach's rehearsal read it",
                "it has the index `late_x`, which the plan did not see there",
            ],
        );
        assert!(!p.attached());
        // The drops ahead of the compare are rolled back with it.
        p.oid("late_z");
        p.oid("late_a");
        on_server(p.connection(), "DROP EVENT TRIGGER add1671");
        succeeds(p.apply(&plan, &[]));
        assert_eq!(p.oid("late_n"), held, "late_n was rebuilt");
        assert_eq!(p.parent_of("late_n"), "app.ev_n");
        p.in_sync();
    });
}

/// Two lone equals, and an index of the table's own that stays, made first: the
/// plan is the attach alone, which a staged plan can hold.
const ALONE: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, m integer) \
      PARTITION BY RANGE (k); \
    CREATE INDEX ev_m ON app.ev (m); \
    CREATE INDEX ev_k ON app.ev (k); \
    CREATE TABLE app.late (id integer NOT NULL, k integer NOT NULL, m integer); \
    CREATE INDEX late_two ON app.late (k, m); \
    CREATE INDEX late_m ON app.late (m); \
    CREATE INDEX late_k ON app.late (k); \
    INSERT INTO app.late VALUES (1, 1, 1)";

/// A staged attach compares both relations just before its `ATTACH`, in that
/// step's transaction. A resume makes that comparison and no other.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_staged_attach_is_held_to_its_record_and_a_resume_only_before_the_attach() {
    servers(|server| {
        let own = "    indexes:\n      late_two:\n        columns: [k, m]\n";
        let p = Project::new(server, "adopt-staged-1671", ALONE, own);
        let held = p.oid("late_m");
        let plan = p.plan(&["--staged"]);
        record(
            &plan,
            rehearsal(
                &[index("ev_m", "ev", "m"), index("ev_k", "ev", "k")],
                &[
                    index("late_two", "late", "k, m"),
                    index("late_m", "late", "m"),
                    index("late_k", "late", "k"),
                ],
                &[("late_m", "ev_m"), ("late_k", "ev_k")],
            ),
        );
        // After the temporary CHECK is validated, one step ahead of the
        // `ATTACH`, the parent's older index is made again.
        on_server(
            p.connection(),
            "CREATE FUNCTION public.move1671() RETURNS event_trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF EXISTS (SELECT FROM pg_constraint WHERE conname LIKE 'pbps\\_attach\\_%' \
                          AND convalidated) \
                  AND (SELECT 'app.ev_m'::regclass::oid < 'app.ev_k'::regclass::oid) THEN \
                 DROP INDEX app.ev_m; CREATE INDEX ev_m ON app.ev (m); \
               END IF; \
             END $$; \
             CREATE EVENT TRIGGER move1671 ON ddl_command_end WHEN TAG IN ('ALTER TABLE') \
               EXECUTE FUNCTION public.move1671()",
        );
        let moved = [
            "`app.ev` is not as the attach's rehearsal read it",
            "made in another order: `ev_k`",
        ];
        p.refused(&plan, &["--staged"], &moved);
        assert!(!p.attached());
        on_server(p.connection(), "DROP EVENT TRIGGER move1671");
        // Still out of order, so the resume's own compare refuses too.
        p.refused(&plan, &["--staged", "--resume"], &moved);
        assert!(!p.attached());
        // Put back in order, then stopped after the attach.
        on_server(
            p.connection(),
            "DROP INDEX app.ev_k; CREATE INDEX ev_k ON app.ev (k); \
             CREATE FUNCTION public.stop1671() RETURNS event_trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF EXISTS (SELECT FROM pg_event_trigger_dropped_objects() \
                          WHERE object_identity LIKE 'pbps\\_attach\\_%') THEN \
                 RAISE 'stopped after the attach'; \
               END IF; \
             END $$; \
             CREATE EVENT TRIGGER stop1671 ON sql_drop EXECUTE FUNCTION public.stop1671()",
        );
        p.refused(
            &plan,
            &["--staged", "--resume"],
            &["stopped after the attach"],
        );
        assert!(p.attached());
        // The table's list is no longer the recorded one. With the attach done
        // there is nothing left to hold it for.
        on_server(
            p.connection(),
            "DROP EVENT TRIGGER stop1671; \
             DROP INDEX app.late_two; CREATE INDEX late_two ON app.late (k, m)",
        );
        succeeds(p.apply(&plan, &["--staged", "--resume"]));
        assert_eq!(p.oid("late_m"), held, "late_m was rebuilt");
        assert_eq!(p.parent_of("late_m"), "app.ev_m");
        p.in_sync();
    });
}

/// A record that does not agree with its own plan is refused at admission, ahead
/// of any statement, whatever checksum the file is approved under.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_plan_whose_recorded_index_was_altered_is_refused_at_admission() {
    servers(|server| {
        let p = Project::new(server, "adopt-tamper-1671", PAIRS, "");
        let plan = p.plan(&[]);
        let states = p.recorded_states();
        // To an index the plan drops, and to one the table does not hold.
        for other in ["late_a", "late_nothing"] {
            let mut altered = pairs_record();
            altered.kept = [(other.into(), "ev_n".into())].into();
            record(&plan, altered);
            p.refused(&plan, &[], &["invalid attach rehearsal on app.late"]);
            assert!(!p.attached());
            assert_eq!(p.recorded_states(), states);
        }
        record(&plan, pairs_record());
        succeeds(p.apply(&plan, &[]));
        p.in_sync();
    });
}
