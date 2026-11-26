//! A resolver-settled plan that makes the engine keep copies on a partition,
//! or takes them away, applies like an unresolved one (#3075, DEC-3075.1).
//! Each case runs on both pinned engines, over a Docker scratch of the
//! engine's own image.
//!
//! The declarations a case plans are read from a second database that has had
//! the change run on it, so each case says only what the engine is asked to do.
use super::{
    Demo, OwnDatabase, code, generate_key, key_line, on_server, plan_checksum, scalar, stderr,
    stdout, succeeds, text_of,
};
use std::process::Output;

const TARGET: &str = "PBPS_RES3075_TARGET";

fn engines(mut check: impl FnMut(&str, &str)) {
    for (server, image) in [
        ("PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_OLD_SCRATCH_IMAGE"),
        ("PBPS_TEST_PG_DB", "PBPS_TEST_PG_SCRATCH_IMAGE"),
    ] {
        let read = |variable| std::env::var(variable).expect(variable);
        check(&read(server), &read(image));
    }
}

/// A partitioned `app.ev` with a column index and an expression index, which
/// makes a change under it one only the resolver can settle.
const PARENT: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, m integer, \
      n integer DEFAULT 1, CONSTRAINT ev_ck CHECK (m > 0)) PARTITION BY RANGE (k); \
    CREATE INDEX ev_m ON app.ev (m); \
    CREATE INDEX ev_e ON app.ev ((n + 1))";

/// An ordinary table with the parent's columns and check.
const LATE: &str = "CREATE TABLE app.late (id integer NOT NULL, k integer NOT NULL, m integer, \
      n integer DEFAULT 1, CONSTRAINT ev_ck CHECK (m > 0)); \
    INSERT INTO app.late VALUES (1, 1, 1, 1)";

const ATTACH: &str = "ALTER TABLE app.ev ATTACH PARTITION app.late FOR VALUES FROM (0) TO (10)";

/// Everything else the engine copies: keys, a foreign key either way, a
/// partial index and a row trigger.
const REST: &str = "CREATE TABLE app.ref (id integer PRIMARY KEY); \
    INSERT INTO app.ref VALUES (1); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_pk PRIMARY KEY (id, k); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_uq UNIQUE (k, m); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_fk FOREIGN KEY (m) REFERENCES app.ref; \
    CREATE INDEX ev_w ON app.ev (id) WHERE n > 0; \
    CREATE FUNCTION app.tg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$; \
    CREATE TRIGGER ev_tg BEFORE INSERT ON app.ev FOR EACH ROW EXECUTE FUNCTION app.tg(); \
    CREATE TABLE app.child (id integer, k integer, \
      CONSTRAINT child_fk FOREIGN KEY (id, k) REFERENCES app.ev (id, k))";

const SECOND: &str = "CREATE TABLE app.p1 PARTITION OF app.ev FOR VALUES FROM (10) TO (20)";

/// A database holding `before`, adopted, and a project that declares what
/// `before` becomes once `change` has run.
struct Project {
    own: OwnDatabase,
    d: Demo,
}

impl Project {
    fn new(server: &str, image: &str, slug: &str, before: &[&str], change: &str) -> Self {
        let own = OwnDatabase::new(server, slug);
        let after = OwnDatabase::new(server, &format!("{slug}-after"));
        for statement in before {
            on_server(own.connection(), statement);
            on_server(after.connection(), statement);
        }
        on_server(after.connection(), change);
        let d = Demo::new(slug);
        succeeds(d.run(&["pull", "--db", own.connection()]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", own.connection(), "--reason", "adopt"]));
        generate_key(&d);
        let wanted = Demo::new(&format!("{slug}-wanted"));
        succeeds(wanted.run(&["pull", "--db", after.connection()]));
        let schema = d.dir.join("schema");
        std::fs::remove_dir_all(&schema).unwrap();
        std::fs::create_dir(&schema).unwrap();
        for file in std::fs::read_dir(wanted.dir.join("schema")).unwrap() {
            let file = file.unwrap();
            std::fs::copy(file.path(), schema.join(file.file_name())).unwrap();
        }
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!(
                "dialect: postgres\nresolvers:\n  scratch:\n    kind: docker\n    \
                 image: '{image}'\n    pull: never\nenvironments:\n  prod:\n    \
                 url_env: {TARGET}\n    resolve_with: scratch\n{}",
                key_line()
            ),
        )
        .unwrap();
        Self { own, d }
    }

    fn connection(&self) -> &str {
        self.own.connection()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.d.run_with_env(args, &[(TARGET, self.connection())])
    }

    /// The connected plan, which the resolver settled.
    fn plan(&self) -> std::path::PathBuf {
        // A meeting or a drop says what it must offline first.
        self.d.run(&["plan"]);
        self.d.commit();
        let path = self.d.dir.join("plan.json");
        succeeds(self.run(&["plan", "--env", "prod", "--out", path.to_str().unwrap()]));
        let plan: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(plan["analysis"]["kind"], "resolved", "{plan}");
        path
    }

    fn apply(&self, plan: &std::path::Path) -> Output {
        let checksum = plan_checksum(plan);
        self.run(&[
            "apply",
            "--env",
            "prod",
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &checksum,
            "--allow",
            "destructive,constraint",
        ])
    }

    /// Applied, and the database is what the declarations say, with nothing
    /// left to plan.
    fn applies(&self, plan: &std::path::Path) {
        succeeds(self.apply(plan));
        succeeds(self.run(&["verify", "--env", "prod"]));
        let next = succeeds(self.run(&["plan", "--env", "prod"]));
        assert!(
            stdout(&next).contains("No changes"),
            "{}{}",
            stdout(&next),
            stderr(&next)
        );
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

    fn partitions(&self) -> i64 {
        scalar(
            self.connection(),
            "SELECT count(*) FROM pg_inherits WHERE inhparent = 'app.ev'::regclass",
        )
    }
}

#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_resolved_attach_applies_whatever_the_table_held() {
    engines(|server, image| {
        for (slug, own, taken) in [
            ("res3075-bare", "", None),
            // The engine takes the table's own index, which keeps its name.
            (
                "res3075-taken",
                "CREATE INDEX late_z ON app.late (m)",
                Some("late_z"),
            ),
            // An expression index of the table's, dropped and built again.
            (
                "res3075-expr",
                "CREATE INDEX late_e ON app.late ((n + 1))",
                None,
            ),
            (
                "res3075-keep",
                "CREATE INDEX late_z ON app.late (m); CREATE INDEX late_e ON app.late ((n + 1))",
                Some("late_z"),
            ),
        ] {
            let mut before = vec![PARENT, LATE];
            if !own.is_empty() {
                before.push(own);
            }
            let p = Project::new(server, image, slug, &before, ATTACH);
            p.applies(&p.plan());
            assert_eq!(p.partitions(), 1, "{slug}");
            if let Some(name) = taken {
                assert_eq!(p.parent_of(name), "app.ev_m", "{slug}");
            }
        }
    });
}

#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_resolved_plan_applies_beside_every_copy_the_engine_keeps() {
    engines(|server, image| {
        let tree = [PARENT, REST, SECOND];
        for (slug, before, change) in [
            ("res3075-rich", &[PARENT, REST, LATE][..], ATTACH),
            (
                "res3075-create",
                &tree[..],
                "CREATE TABLE app.p2 PARTITION OF app.ev FOR VALUES FROM (20) TO (30)",
            ),
            ("res3075-drop", &tree[..], "DROP INDEX app.ev_w"),
            (
                "res3075-detach",
                &tree[..],
                "ALTER TABLE app.ev DETACH PARTITION app.p1",
            ),
        ] {
            let p = Project::new(server, image, slug, before, change);
            p.applies(&p.plan());
        }
    });
}

/// The engine names a copy, and not always as it did on the scratch: here a
/// relation only the target holds has the name the scratch's copy got.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_resolved_attach_applies_where_the_target_names_a_copy_otherwise() {
    engines(|server, image| {
        let p = Project::new(server, image, "res3075-name", &[PARENT, LATE], ATTACH);
        on_server(p.connection(), "CREATE TABLE app.late_expr_idx (x integer)");
        p.applies(&p.plan());
        assert_eq!(p.parent_of("late_expr_idx1"), "app.ev_e");
    });
}

/// An index no change of the plan explains is refused on the very table the
/// attach credits, whether it was made before apply or while it ran.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_resolved_attach_still_refuses_an_index_no_change_explains() {
    engines(|server, image| {
        let p = Project::new(server, image, "res3075-stray", &[PARENT, LATE], ATTACH);
        let plan = p.plan();
        let refused = |why: &str| {
            let output = p.apply(&plan);
            assert_ne!(code(&output), 0, "{}", stdout(&output));
            assert!(stderr(&output).contains(why), "{}", stderr(&output));
            assert_eq!(p.partitions(), 0, "{}", stderr(&output));
        };
        on_server(p.connection(), "CREATE INDEX late_stray ON app.late (id)");
        // The drift check sees it before anything runs.
        refused("is no longer the database this plan was computed against");
        on_server(p.connection(), "DROP INDEX app.late_stray");
        // Made by the attach's own statement, after the opening check, on the
        // very table the attach credits: an index, which the read-back
        // refuses before the resolver's closing check, and a statistics
        // object, which the declarations cannot hold, so that only the
        // closing check meets it, and cannot read it as the plan's.
        for (made, why) in [
            (
                "CREATE INDEX late_stray ON app.late (id)",
                "index `late_stray` is there, and this plan declares no such index",
            ),
            (
                "CREATE STATISTICS app.late_stray ON id, k FROM app.late",
                "the closing capture of `prod` could not be read",
            ),
        ] {
            on_server(
                p.connection(),
                &format!(
                    "CREATE FUNCTION public.stray3075() RETURNS event_trigger \
                     LANGUAGE plpgsql AS $$ BEGIN {made}; END $$; \
                     CREATE EVENT TRIGGER stray3075 ON ddl_command_end \
                       WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION public.stray3075()"
                ),
            );
            refused(why);
            on_server(
                p.connection(),
                "DROP EVENT TRIGGER stray3075; DROP FUNCTION public.stray3075()",
            );
        }
        p.applies(&plan);
    });
}
