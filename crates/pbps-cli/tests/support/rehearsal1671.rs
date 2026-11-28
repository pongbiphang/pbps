//! With a resolver selected, an attach keeps each index, key and exclusion of
//! the table that the engine takes as a clone of its parent's (#1671,
//! DEC-1671.2). Without one, or with no answer, the plan is the loose drop's
//! (DEC-1671.1). `adoption1671.rs` holds apply to the record. Each case here
//! runs over a Docker scratch of the engine's own image, on both pinned engines
//! but for the exclusion's, which PostgreSQL 16 does not take on a partitioned
//! table.
use super::{
    Demo, OwnDatabase, code, generate_key, key_line, on_server, plan_checksum, stderr, stdout,
    succeeds, text_of,
};
use pbps_model::adoption::Skipped;
use pbps_model::{Change, ConstraintChange, IndexChange, PartitionChange, SavedPlan};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const TARGET: &str = "PBPS_ADOPT1671_TARGET";
const SCRATCH: &str = "PBPS_ADOPT1671_SCRATCH";

pub(super) fn engines(mut check: impl FnMut(&str, &str)) {
    for (server, image) in [
        ("PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_OLD_SCRATCH_IMAGE"),
        ("PBPS_TEST_PG_DB", "PBPS_TEST_PG_SCRATCH_IMAGE"),
    ] {
        let read = |variable| std::env::var(variable).expect(variable);
        check(&read(server), &read(image));
    }
}

/// What `pbps.yml` selects.
#[derive(Clone, Copy)]
pub(super) enum Resolver<'a> {
    None,
    Docker(&'a str),
    /// A supplied server nothing answers on.
    Unreachable,
}

/// A database holding `setup`, adopted, and its project.
pub(super) struct Project {
    own: OwnDatabase,
    pub(super) d: Demo,
}

impl Project {
    pub(super) fn new(server: &str, slug: &str, setup: &str) -> Self {
        let own = OwnDatabase::new(server, slug);
        on_server(own.connection(), setup);
        let d = Demo::new(slug);
        succeeds(d.run(&["pull", "--db", own.connection()]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", own.connection(), "--reason", "adopt"]));
        generate_key(&d);
        let project = Self { own, d };
        project.select(Resolver::None);
        project
    }

    pub(super) fn connection(&self) -> &str {
        self.own.connection()
    }

    pub(super) fn select(&self, resolver: Resolver<'_>) {
        let (entry, selected) = match resolver {
            Resolver::None => (String::new(), ""),
            Resolver::Docker(image) => (
                format!(
                    "resolvers:\n  scratch:\n    kind: docker\n    image: '{image}'\n    \
                     pull: never\n"
                ),
                "    resolve_with: scratch\n",
            ),
            Resolver::Unreachable => (
                format!("resolvers:\n  scratch:\n    kind: server\n    url_env: {SCRATCH}\n"),
                "    resolve_with: scratch\n",
            ),
        };
        std::fs::write(
            self.d.dir.join("pbps.yml"),
            format!(
                "dialect: postgres\n{entry}environments:\n  prod:\n    url_env: {TARGET}\n\
                 {selected}{}",
                key_line()
            ),
        )
        .unwrap();
    }

    pub(super) fn run(&self, args: &[&str]) -> std::process::Output {
        self.d.run_with_env(
            args,
            &[
                (TARGET, self.connection()),
                (SCRATCH, "host=127.0.0.1 port=1 user=nobody dbname=none"),
            ],
        )
    }

    /// Removes `table`'s file and declares it a partition of `app.ev`, with
    /// `own` as the rest of its entry.
    fn attach(&self, table: &str, own: &str) {
        let parent = self.d.dir.join("schema/app.ev.yml");
        let tree = std::fs::read_to_string(&parent).unwrap();
        std::fs::remove_file(self.d.dir.join(format!("schema/app.{table}.yml"))).unwrap();
        std::fs::write(
            &parent,
            format!("{tree}partitions:\n  {table}:\n    from: [0]\n    to: [10]\n{own}"),
        )
        .unwrap();
    }

    /// The connected plan of the declarations, and what planning printed.
    pub(super) fn plan(&self, name: &str, extra: &[&str]) -> (PathBuf, String) {
        succeeds(self.d.run(&["plan"]));
        self.connected(name, extra)
    }

    /// [`Project::plan`] once the offline plan has written the identities.
    fn connected(&self, name: &str, extra: &[&str]) -> (PathBuf, String) {
        self.d.commit();
        let path = self.d.dir.join(format!("{name}.json"));
        let mut args = vec!["plan", "--env", "prod", "--out", path.to_str().unwrap()];
        args.extend_from_slice(extra);
        let planned = succeeds(self.run(&args));
        (path, stdout(&planned))
    }

    pub(super) fn apply(&self, plan: &Path, extra: &[&str]) -> std::process::Output {
        let checksum = plan_checksum(plan);
        let mut args = vec![
            "apply",
            "--env",
            "prod",
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &checksum,
            "--allow",
            "destructive,constraint",
        ];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    /// The database is what the declarations say, with nothing left to plan.
    pub(super) fn in_sync(&self) {
        succeeds(self.run(&["verify", "--env", "prod"]));
        let next = succeeds(self.run(&["plan", "--env", "prod"]));
        assert!(
            stdout(&next).contains("No changes"),
            "{}{}",
            stdout(&next),
            stderr(&next)
        );
    }

    pub(super) fn oid(&self, index: &str) -> String {
        text_of(
            self.connection(),
            &format!("SELECT 'app.{index}'::regclass::oid::text"),
        )
    }

    /// The index's parent in the partition tree, or nothing where it is the
    /// table's own.
    pub(super) fn parent_of(&self, index: &str) -> String {
        text_of(
            self.connection(),
            &format!(
                "SELECT coalesce((SELECT inhparent::regclass::text FROM pg_inherits \
                 WHERE inhrelid = 'app.{index}'::regclass), '')"
            ),
        )
    }
}

pub(super) fn saved(plan: &Path) -> SavedPlan {
    serde_json::from_slice(&std::fs::read(plan).unwrap()).unwrap()
}

/// The names the plan drops ahead of its attach, indexes and constraints.
pub(super) fn drops(plan: &SavedPlan) -> Vec<String> {
    let json = serde_json::to_value(&plan.changes).unwrap();
    let mut names: Vec<String> = json["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|change| {
            change["op"]
                .as_str()
                .is_some_and(|op| op.starts_with("drop_"))
        })
        .map(|change| change["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

/// Each own index the plan leaves to the attach, with the parent's.
fn kept(plan: &SavedPlan) -> BTreeMap<String, String> {
    plan.changes
        .changes
        .iter()
        .filter_map(|p| {
            if let Change::Partition(PartitionChange::AttachPartition { rehearsal, .. }) = &p.change
            {
                rehearsal.as_ref()
            } else {
                None
            }
        })
        .flat_map(|rehearsal| {
            rehearsal
                .kept
                .iter()
                .map(|(own, by)| (own.raw().to_owned(), by.raw().to_owned()))
        })
        .collect()
}

fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

/// A contested pair, a respelled expression with a predicate over a declared
/// enum, one calling a declared function, a key the loose rule drops, and an
/// own index the declaration keeps beside the clone.
const CONTESTED: &str = "CREATE SCHEMA app; \
    CREATE TYPE app.mood AS ENUM ('sad', 'ok'); \
    CREATE FUNCTION app.twice(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 * 2; \
    CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, m integer, n integer, \
      mood app.mood) PARTITION BY RANGE (k); \
    CREATE INDEX ev_m ON app.ev (m); \
    CREATE INDEX ev_n ON app.ev ((n + 1)) WHERE mood = 'ok'; \
    CREATE INDEX ev_f ON app.ev (app.twice(n)); \
    CREATE UNIQUE INDEX ev_u ON app.ev (id, k); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_uq UNIQUE (id, k); \
    CREATE TABLE app.late (id integer NOT NULL, k integer NOT NULL, m integer, n integer, \
      mood app.mood); \
    ALTER TABLE app.late ADD CONSTRAINT late_uq UNIQUE (id, k); \
    CREATE INDEX late_z ON app.late (m); \
    CREATE INDEX late_a ON app.late (m); \
    CREATE INDEX late_other ON app.late ((n + 2)) WHERE mood = 'ok'; \
    CREATE INDEX late_n ON app.late ((n+1)) WHERE mood='ok'::app.mood; \
    CREATE INDEX late_f ON app.late (app.twice( n )); \
    INSERT INTO app.late VALUES (1, 1, 1, 1, 'ok')";

const OWN_OTHER: &str = "    indexes:\n      late_other:\n        keys:\n          \
    - expression: (n + 2)\n        where: (mood = 'ok'::app.mood)\n";

/// An own index spelled otherwise than the parent's is the same index only to
/// an engine. With its answer the plan leaves it standing, and the attach takes
/// that very index as the clone.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_resolved_plan_keeps_a_respelled_expression_index_and_applies_it() {
    engines(|server, image| {
        let p = Project::new(server, "adopt-attach-1671", CONTESTED);
        let held = ["late_z", "late_n", "late_f"].map(|name| (name, p.oid(name)));
        p.attach("late", OWN_OTHER);
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("kept", &[]);
        let saved = saved(&plan);
        assert_eq!(
            kept(&saved),
            map(&[("late_f", "ev_f"), ("late_n", "ev_n"), ("late_z", "ev_m")]),
            "{said}"
        );
        // The loser of the pair, the key, and the own the declaration keeps.
        assert_eq!(drops(&saved), ["late_a", "late_other", "late_uq"], "{said}");
        assert!(
            said.contains("and its index late_z as the copy of ev_m"),
            "{said}"
        );
        succeeds(p.apply(&plan, &[]));
        for ((name, oid), theirs) in held.iter().zip(["ev_m", "ev_n", "ev_f"]) {
            assert_eq!(&p.oid(name), oid, "{name} was rebuilt");
            assert_eq!(p.parent_of(name), format!("app.{theirs}"), "{name}");
        }
        // The own the declaration holds is back, and is not a clone.
        assert_eq!(p.parent_of("late_other"), "");
        p.in_sync();
    });
}

/// A parent with two like indexes and a table with one: no certain case, and no
/// text an engine would have to parse, so the project needs no resolver for
/// anything but the rehearsal.
const TWO_OF_THE_PARENTS: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, m integer) \
      PARTITION BY RANGE (k); \
    CREATE INDEX ev_a ON app.ev (m); \
    CREATE INDEX ev_b ON app.ev (m); \
    CREATE TABLE app.late (id integer NOT NULL, k integer NOT NULL, m integer); \
    CREATE INDEX late_m ON app.late (m); \
    INSERT INTO app.late VALUES (1, 1, 1)";

#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn without_an_answer_the_plan_is_the_one_made_without_a_resolver() {
    engines(|server, image| {
        let p = Project::new(server, "adopt-none-1671", TWO_OF_THE_PARENTS);
        p.attach("late", "");
        let (without, _) = p.plan("without", &[]);
        let without = saved(&without);
        assert_eq!(drops(&without), ["late_m"]);
        p.select(Resolver::Unreachable);
        let (unreachable, _) = p.plan("unreachable", &["--no-rehearse"]);
        assert_eq!(saved(&unreachable).changes, without.changes);
        // The same project does have an answer to be had.
        p.select(Resolver::Docker(image));
        let (answered, _) = p.plan("answered", &[]);
        assert_eq!(kept(&saved(&answered)), map(&[("late_m", "ev_a")]));
        p.select(Resolver::Unreachable);
        let (unreachable, _) = p.plan("unreachable", &["--no-rehearse"]);
        succeeds(p.apply(&unreachable, &[]));
        p.select(Resolver::None);
        p.in_sync();
    });
}

/// The scratch's answer for `app.late` under `app.ev`, asked directly: what
/// planning reads of `target`, rehearsed on `scratch`.
fn rehearsed(
    target: &str,
    scratch: &str,
) -> Result<pbps_model::adoption::AttachRehearsal, Skipped> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let connect = |to| pbps_db::Conn::connect(pbps_db::Driver::Postgres, to);
            let name = |table| pbps_model::TableName::new("app", table);
            let mut conn = connect(target).await.unwrap();
            let wanted = [name("ev"), name("late")].into();
            let (_, relations) = pbps_pg::catalog::introspect_wanting(&mut conn, &wanted)
                .await
                .unwrap();
            let held = |table| relations[&name(table)].as_ref().unwrap();
            let question = pbps_diff::attach::Question {
                uid: "t_aaaaaa".parse().unwrap(),
                table: name("late"),
                parent: name("ev"),
                owns: ["late_m".into()].into(),
                dropped: Default::default(),
            };
            let mut conn = connect(scratch).await.unwrap();
            pbps_pg::adoption::rehearse(&mut conn, &question, held("ev"), held("late"))
                .await
                .unwrap()
        })
}

/// A column rename meeting the attach would change what the definitions print
/// between the read and the attach, so nothing is asked. And an invalid index
/// is a fact the model does not hold: the plan is refused with or without a
/// resolver, before any rehearsal.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_meeting_rename_and_an_invalid_index_get_no_answer() {
    engines(|server, image| {
        let p = Project::new(
            server,
            "adopt-rename-1671",
            "CREATE SCHEMA app; \
             CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, m integer) \
               PARTITION BY RANGE (k); \
             CREATE INDEX ev_a ON app.ev (m); \
             CREATE INDEX ev_b ON app.ev (m); \
             CREATE TABLE app.late (id integer NOT NULL, k integer NOT NULL, moved integer); \
             CREATE INDEX late_m ON app.late (moved)",
        );
        p.attach("late", "");
        let parent = p.d.dir.join("schema/app.ev.yml");
        let renamed = std::fs::read_to_string(&parent)
            .unwrap()
            .replace("  m:", "  moved:")
            .replace("[m]", "[moved]");
        std::fs::write(&parent, renamed).unwrap();
        succeeds(p.d.run(&["rename", "app.ev.m", "moved"]));
        p.select(Resolver::Docker(image));
        let preview = p.d.run(&["plan"]);
        assert!(
            stderr(&preview).contains("membership boundary"),
            "{}",
            stderr(&preview)
        );
        let (plan, said) = p.connected("renamed", &[]);
        assert_eq!(kept(&saved(&plan)), map(&[]), "{said}");
        assert_eq!(drops(&saved(&plan)), ["late_m"], "{said}");
        succeeds(p.apply(&plan, &["--allow", "rename"]));
        p.in_sync();

        let p = Project::new(server, "adopt-invalid-1671", TWO_OF_THE_PARENTS);
        // The rehearsal itself, on an empty database that holds the schema.
        let scratch = OwnDatabase::new(server, "adopt-shells-1671");
        on_server(scratch.connection(), "CREATE SCHEMA app");
        let answered = rehearsed(p.connection(), scratch.connection());
        assert_eq!(
            answered.map(|answer| answer.kept),
            Ok([("late_m".into(), "ev_a".into())].into())
        );
        on_server(
            p.connection(),
            "UPDATE pg_index SET indisvalid = false \
             WHERE indexrelid = 'app.late_m'::regclass",
        );
        assert_eq!(
            rehearsed(p.connection(), scratch.connection()),
            Err(Skipped::InvalidIndex)
        );
        p.attach("late", "");
        succeeds(p.d.run(&["plan"]));
        p.d.commit();
        let refusals = [Resolver::None, Resolver::Docker(image)].map(|resolver| {
            p.select(resolver);
            let refused = p.run(&["plan", "--env", "prod"]);
            assert_ne!(code(&refused), 0, "{}", stdout(&refused));
            let said = stderr(&refused);
            assert!(said.contains("indisvalid = false"), "{said}");
            said
        });
        assert_eq!(refusals[0], refusals[1]);
    });
}

/// A contested pair under each of two indexes of the parent's, and one of the
/// table's own that none might take, over plain columns alone.
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

#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_resolver_leaves_the_taken_one_of_a_contested_pair_to_the_attach() {
    engines(|server, image| {
        let p = Project::new(server, "adopt-pair-1671", PAIRS);
        // `late_n` is the certain case, left to the attach with or without a
        // resolver (DEC-1671.1); the pair's older one only with an answer.
        let held = ["late_z", "late_n"].map(|name| (name, p.oid(name)));
        p.attach("late", "");
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("kept", &[]);
        let saved = saved(&plan);
        assert_eq!(kept(&saved), map(&[("late_z", "ev_m")]), "{said}");
        assert_eq!(drops(&saved), ["late_a"], "{said}");
        assert!(
            said.contains("and its index late_z as the copy of ev_m"),
            "{said}"
        );
        succeeds(p.apply(&plan, &[]));
        for ((name, oid), theirs) in held.iter().zip(["ev_m", "ev_n"]) {
            assert_eq!(&p.oid(name), oid, "{name} was rebuilt");
            assert_eq!(p.parent_of(name), format!("app.{theirs}"), "{name}");
        }
        p.in_sync();
    });
}

/// A staged plan holds one change. Without an answer the attach comes with a
/// drop beside it; with every own index kept it is the attach alone, which a
/// staged apply holds to the rehearsal's record and runs.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_staged_plan_can_hold_an_attach_that_keeps_every_own_index() {
    engines(|server, image| {
        let p = Project::new(server, "adopt-staged-answer-1671", TWO_OF_THE_PARENTS);
        let held = p.oid("late_m");
        p.attach("late", "");
        succeeds(p.d.run(&["plan"]));
        p.d.commit();
        let refused = p.run(&["plan", "--env", "prod", "--staged"]);
        assert_ne!(code(&refused), 0, "{}", stdout(&refused));
        assert!(
            stderr(&refused).contains("this plan has 2"),
            "{}",
            stderr(&refused)
        );
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("staged", &["--staged"]);
        assert_eq!(kept(&saved(&plan)), map(&[("late_m", "ev_a")]), "{said}");
        succeeds(p.apply(&plan, &["--staged"]));
        assert_eq!(p.oid("late_m"), held, "late_m was rebuilt");
        assert_eq!(p.parent_of("late_m"), "app.ev_a");
        p.in_sync();
    });
}

/// Every index, key and exclusion the plan drops, by name, in order of name.
pub(super) fn taken_off(plan: &SavedPlan) -> Vec<String> {
    let mut names: Vec<String> = plan
        .changes
        .changes
        .iter()
        .filter_map(|p| {
            if let Change::Index(IndexChange::DropIndex { name, .. })
            | Change::Constraint(ConstraintChange::DropUnique { name, .. })
            | Change::Constraint(ConstraintChange::SetExclusion { name, to: None, .. }) =
                &p.change
            {
                Some(name.raw().to_owned())
            } else if let Change::Constraint(ConstraintChange::SetPrimaryKey {
                from: Some(key),
                to: None,
                ..
            }) = &p.change
            {
                key.name.as_ref().map(|name| name.raw().to_owned())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names
}

impl Project {
    /// The kind of the constraint on one of `table`'s indexes, and whether it
    /// is a copy of one of the parent's; nothing where none stands on it.
    fn constraint_on(&self, table: &str, index: &str) -> String {
        text_of(
            self.connection(),
            &format!(
                "SELECT coalesce((SELECT contype::text || ' ' || (conparentid <> 0)::text \
                 FROM pg_constraint WHERE conrelid = 'app.{table}'::regclass \
                 AND conindid = 'app.{index}'::regclass), '')"
            ),
        )
    }
}

/// A key and a unique constraint of each side, made before the unique indexes of
/// the parent's, which the loose rule cannot tell apart from them.
const KEYS: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, m integer NOT NULL) \
      PARTITION BY RANGE (k); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_pkey PRIMARY KEY (id, k); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_uq UNIQUE (m, k); \
    CREATE UNIQUE INDEX ev_pk_ix ON app.ev (id, k); \
    CREATE UNIQUE INDEX ev_uq_ix ON app.ev (m, k); \
    CREATE TABLE app.late (id integer NOT NULL, k integer NOT NULL, m integer NOT NULL); \
    ALTER TABLE app.late ADD CONSTRAINT late_pkey PRIMARY KEY (id, k); \
    ALTER TABLE app.late ADD CONSTRAINT late_uq UNIQUE (m, k); \
    INSERT INTO app.late VALUES (1, 1, 1)";

#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_resolved_plan_keeps_a_primary_key_and_a_unique_constraint_and_applies_them() {
    engines(|server, image| {
        let p = Project::new(server, "adopt-keys-1671", KEYS);
        let held = ["late_pkey", "late_uq"].map(|name| (name, p.oid(name)));
        p.attach("late", "");
        let (without, _) = p.plan("without", &[]);
        let without = saved(&without);
        assert_eq!(taken_off(&without), ["late_pkey", "late_uq"]);
        assert_eq!(kept(&without), map(&[]));
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("kept", &[]);
        let saved = saved(&plan);
        assert_eq!(
            kept(&saved),
            map(&[("late_pkey", "ev_pkey"), ("late_uq", "ev_uq")]),
            "{said}"
        );
        assert_eq!(taken_off(&saved), Vec::<String>::new(), "{said}");
        succeeds(p.apply(&plan, &[]));
        for ((name, oid), (theirs, kind)) in held.iter().zip([("ev_pkey", "p"), ("ev_uq", "u")]) {
            assert_eq!(&p.oid(name), oid, "{name} was rebuilt");
            assert_eq!(p.parent_of(name), format!("app.{theirs}"), "{name}");
            assert_eq!(p.constraint_on("late", name), format!("{kind} true"));
        }
        p.in_sync();
    });
}

/// A unique index of the parent's made before its two unique constraints takes
/// the older of the table's, which stays the table's own; without that one it
/// takes the other the same way. So neither may be kept, which one round would
/// not show. One of the table's carries the name the engine gives a copy it
/// builds, which a later round must not read as the table's.
const TWO_ROUNDS: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (k integer NOT NULL, n integer NOT NULL) PARTITION BY RANGE (k); \
    CREATE UNIQUE INDEX ev_ui ON app.ev (k, n); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_u1 UNIQUE (k, n); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_u2 UNIQUE (k, n); \
    CREATE TABLE app.late (k integer NOT NULL, n integer NOT NULL); \
    ALTER TABLE app.late ADD CONSTRAINT late_k_n_key UNIQUE (k, n); \
    ALTER TABLE app.late ADD CONSTRAINT late_u2 UNIQUE (k, n); \
    INSERT INTO app.late VALUES (1, 1)";

/// A key the parent's unique index takes has the engine give the table a second
/// one and refuse the attach. The rehearsal then asks again without the key, and
/// still answers for the plain index beside it.
const SECOND_KEY: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, m integer) \
      PARTITION BY RANGE (k); \
    CREATE UNIQUE INDEX ev_ui ON app.ev (id, k); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_pkey PRIMARY KEY (id, k); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_uq UNIQUE (id, k); \
    CREATE INDEX ev_a ON app.ev (m); \
    CREATE INDEX ev_b ON app.ev (m); \
    CREATE TABLE app.late (id integer NOT NULL, k integer NOT NULL, m integer); \
    ALTER TABLE app.late ADD CONSTRAINT late_uq UNIQUE (id, k); \
    ALTER TABLE app.late ADD CONSTRAINT late_pkey PRIMARY KEY (id, k); \
    CREATE INDEX late_m ON app.late (m); \
    INSERT INTO app.late VALUES (1, 1, 1)";

#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_constraint_left_local_over_an_inherited_index_is_asked_about_again_and_dropped() {
    engines(|server, image| {
        let p = Project::new(server, "adopt-rounds-1671", TWO_ROUNDS);
        p.attach("late", "");
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("rounds", &[]);
        let rounds = saved(&plan);
        assert_eq!(kept(&rounds), map(&[]), "{said}");
        assert_eq!(taken_off(&rounds), ["late_k_n_key", "late_u2"], "{said}");
        succeeds(p.apply(&plan, &[]));
        p.in_sync();

        let p = Project::new(server, "adopt-second-key-1671", SECOND_KEY);
        let held = p.oid("late_m");
        p.attach("late", "");
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("second", &[]);
        let second = saved(&plan);
        assert_eq!(kept(&second), map(&[("late_m", "ev_a")]), "{said}");
        assert_eq!(taken_off(&second), ["late_pkey", "late_uq"], "{said}");
        succeeds(p.apply(&plan, &[]));
        assert_eq!(p.oid("late_m"), held, "late_m was rebuilt");
        p.in_sync();
    });
}

/// One unique constraint of the parent's holds one of the table's: a second
/// equal one has nothing to be the copy of, so the attach is refused with a
/// resolver as without one.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_second_equal_constraint_of_the_tables_is_refused_with_or_without_a_resolver() {
    engines(|server, image| {
        let setup = TWO_ROUNDS.replace(
            "ALTER TABLE app.ev ADD CONSTRAINT ev_u2 UNIQUE (k, n); ",
            "",
        );
        assert_ne!(setup, TWO_ROUNDS);
        let p = Project::new(server, "adopt-unmatched-1671", &setup);
        p.attach("late", "");
        for resolver in [Resolver::None, Resolver::Docker(image)] {
            p.select(resolver);
            let refused = p.d.run(&["plan"]);
            assert_ne!(code(&refused), 0, "{}", stdout(&refused));
            assert!(
                stderr(&refused).contains("unique constraint `late_u2` is not its parent's"),
                "{}",
                stderr(&refused)
            );
        }
    });
}

/// PostgreSQL 16 gives a key of the table's to a unique constraint of the
/// parent's made before the parent's key, and the other way round; 18 pairs each
/// with its own kind. Only a pair of one kind is kept, and the plain index
/// beside them is kept on both.
const CROSSED: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (id integer NOT NULL, k integer NOT NULL, m integer) \
      PARTITION BY RANGE (k); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_uq UNIQUE (id, k); \
    ALTER TABLE app.ev ADD CONSTRAINT ev_pkey PRIMARY KEY (id, k); \
    CREATE UNIQUE INDEX ev_ui ON app.ev (id, k); \
    CREATE INDEX ev_a ON app.ev (m); \
    CREATE INDEX ev_b ON app.ev (m); \
    CREATE TABLE app.late (id integer NOT NULL, k integer NOT NULL, m integer); \
    ALTER TABLE app.late ADD CONSTRAINT late_pkey PRIMARY KEY (id, k); \
    ALTER TABLE app.late ADD CONSTRAINT late_uq UNIQUE (id, k); \
    CREATE INDEX late_m ON app.late (m); \
    INSERT INTO app.late VALUES (1, 1, 1)";

#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_constraint_taken_by_one_of_another_kind_is_not_kept() {
    engines(|server, image| {
        let p = Project::new(server, "adopt-crossed-1671", CROSSED);
        let old = text_of(server, "SHOW server_version_num").starts_with("16");
        p.attach("late", "");
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("crossed", &[]);
        let saved = saved(&plan);
        let plain = ("late_m", "ev_a");
        let (expected, dropped): (_, &[&str]) = if old {
            (map(&[plain]), &["late_pkey", "late_uq"])
        } else {
            let keys = [("late_pkey", "ev_pkey"), ("late_uq", "ev_uq")];
            (map(&[plain, keys[0], keys[1]]), &[])
        };
        assert_eq!(kept(&saved), expected, "{said}");
        assert_eq!(taken_off(&saved), dropped, "{said}");
        succeeds(p.apply(&plan, &[]));
        for (index, kind) in [("late_pkey", "p"), ("late_uq", "u")] {
            if !old {
                assert_eq!(p.constraint_on("late", index), format!("{kind} true"));
            }
        }
        p.in_sync();
    });
}

/// A retired exclusion the parent's takes stays, as the parent's copy under its
/// own name. Without a resolver its retirement drops it ahead of the attach.
#[test]
#[ignore = "needs PostgreSQL 18 and a Docker daemon; set PBPS_TEST_PG_DB and its scratch image"]
fn a_resolved_plan_keeps_a_retired_exclusion_the_parents_takes_on_18() {
    let read = |variable| std::env::var(variable).expect(variable);
    let p = Project::new(
        &read("PBPS_TEST_PG_DB"),
        "adopt-exclusion-1671",
        "CREATE SCHEMA app; \
         CREATE TABLE app.ev (k integer NOT NULL, v integer NOT NULL) PARTITION BY RANGE (k); \
         ALTER TABLE app.ev ADD CONSTRAINT ev_x EXCLUDE USING btree (k WITH =, v WITH =); \
         CREATE TABLE app.late (k integer NOT NULL, v integer NOT NULL); \
         ALTER TABLE app.late ADD CONSTRAINT late_x EXCLUDE USING btree (k WITH =, v WITH =); \
         INSERT INTO app.late VALUES (1, 1)",
    );
    let held = p.oid("late_x");
    p.attach("late", "");
    succeeds(p.d.run(&[
        "drop-exclusion",
        "app.late",
        "late_x",
        "--reason",
        "the parent's holds it",
    ]));
    let (without, _) = p.plan("without", &[]);
    assert_eq!(taken_off(&saved(&without)), ["late_x"]);
    p.select(Resolver::Docker(&read("PBPS_TEST_PG_SCRATCH_IMAGE")));
    let (plan, said) = p.plan("kept", &[]);
    let saved = saved(&plan);
    assert_eq!(kept(&saved), map(&[("late_x", "ev_x")]), "{said}");
    assert_eq!(taken_off(&saved), Vec::<String>::new(), "{said}");
    succeeds(p.apply(&plan, &[]));
    assert_eq!(p.oid("late_x"), held, "late_x was rebuilt");
    assert_eq!(p.parent_of("late_x"), "app.ev_x");
    assert_eq!(p.constraint_on("late", "late_x"), "x true");
    p.in_sync();
}
