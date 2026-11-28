//! With a resolver selected, a parent's new index or exclusion leaves standing
//! each own of a partition that the engine does not take, and takes the one the
//! declarations no longer hold (#1671, DEC-1671.3). Without one, or with no
//! answer, the plan is the loose drop's (DEC-1671.1), and the report says why.
//! `adds1671.rs` holds apply to the record. Each case here runs over a Docker
//! scratch of the engine's own image, on both pinned engines but for the
//! exclusion's, which PostgreSQL 16 does not take on a partitioned table.
use super::rehearsal1671::{Project, Resolver, drops, engines, saved, taken_off};
use super::{OwnDatabase, code, on_server, stderr, stdout, succeeds, text_of};
use pbps_model::adoption::{AdditionRehearsal, Skipped, record};
use pbps_model::{SavedPlan, TableName};
use std::collections::BTreeMap;

/// Each recorded partition's own indexes the plan decides about, under the
/// parent's addition that decides it: what it takes, then what it leaves beside.
fn decided(plan: &SavedPlan) -> BTreeMap<String, (Vec<String>, Vec<String>)> {
    let mut found: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for planned in &plan.changes.changes {
        let Some((_, AdditionRehearsal { partitions })) = record(&planned.change) else {
            continue;
        };
        let (_, addition) = pbps_model::adoption::addition(&planned.change).unwrap();
        for of in partitions {
            let names = |list: &std::collections::BTreeSet<pbps_model::ObjectLocalName>| {
                list.iter()
                    .map(|name| name.raw().to_owned())
                    .collect::<Vec<_>>()
            };
            found.insert(
                format!("{}/{}", of.table.name, addition.shown()),
                (names(&of.adopted), names(&of.beside)),
            );
        }
    }
    found
}

fn expected(entries: &[(&str, &[&str], &[&str])]) -> BTreeMap<String, (Vec<String>, Vec<String>)> {
    let owned = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect();
    entries
        .iter()
        .map(|(key, adopted, beside)| ((*key).to_owned(), (owned(adopted), owned(beside))))
        .collect()
}

/// The parent's declaration file, edited.
fn edit(p: &Project, with: impl FnOnce(String) -> String) {
    let path = p.d.dir.join("schema/app.ev.yml");
    let tree = std::fs::read_to_string(&path).unwrap();
    let edited = with(tree.clone());
    assert_ne!(edited, tree, "the declaration was not edited:\n{tree}");
    std::fs::write(&path, edited).unwrap();
}

/// Gives the parent `entries`, each indented under `section`.
fn parent_gains(p: &Project, section: &str, entries: &str) {
    edit(p, |tree| {
        let heading = format!("\n{section}:\n");
        if tree.contains(&heading) {
            tree.replacen(&heading, &format!("{heading}{entries}"), 1)
        } else {
            tree.replacen(
                "\npartition_by:",
                &format!("\n{section}:\n{entries}\npartition_by:"),
                1,
            )
        }
    });
}

/// Takes the partition's own `name` out of the declarations, and the heading it
/// leaves empty.
fn undeclare(p: &Project, name: &str) {
    edit(p, |tree| {
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
        let empty = |at: usize, kept: &[&str]| {
            matches!(kept[at].trim(), "indexes:" | "exclude:")
                && kept
                    .get(at + 1)
                    .is_none_or(|next| indent(next) <= indent(kept[at]))
        };
        let lines: Vec<_> = (0..kept.len())
            .filter(|at| !empty(*at, &kept))
            .map(|at| kept[at])
            .collect();
        format!("{}\n", lines.join("\n"))
    });
}

/// The parent's `column` declared as `ty`.
fn retype(p: &Project, column: &str, ty: &str) {
    edit(p, |tree| {
        let mut inside = false;
        let lines: Vec<String> = tree
            .lines()
            .map(|line| {
                if line.starts_with("  ") && !line.starts_with("   ") {
                    inside = line.trim_start().starts_with(&format!("{column}:"));
                }
                if inside && line.contains("integer") {
                    line.replacen("integer", ty, 1)
                } else if inside && line.contains("type: int") {
                    line.replacen("type: int", &format!("type: {ty}"), 1)
                } else {
                    line.to_owned()
                }
            })
            .collect();
        format!("{}\n", lines.join("\n"))
    });
}

/// How many indexes of partitions are clones of the parent's `index`.
fn clones(p: &Project, index: &str) -> String {
    text_of(
        p.connection(),
        &format!(
            "SELECT count(*)::text FROM pg_inherits WHERE inhparent = 'app.{index}'::regclass"
        ),
    )
}

/// The own indexes of every partition, with the parent's each is a clone of.
fn standing(p: &Project, indexes: &[(&str, &str)]) {
    for (index, parent) in indexes {
        let parent = match *parent {
            "" => String::new(),
            name => format!("app.{name}"),
        };
        assert_eq!(p.parent_of(index), parent, "{index}");
    }
}

fn oids<'a>(p: &Project, names: &[&'a str]) -> Vec<(&'a str, String)> {
    names.iter().map(|name| (*name, p.oid(name))).collect()
}

fn unchanged(p: &Project, held: &[(&str, String)]) {
    for (name, oid) in held {
        assert_eq!(&p.oid(name), oid, "{name} was rebuilt");
    }
}

/// An own with another predicate, an expression key under a column, a column
/// under an expression and a respelled predicate (#2961's three cases).
const APART: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (k integer NOT NULL, m integer, n integer, o integer) \
      PARTITION BY RANGE (k); \
    CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
    CREATE INDEX ev_1_n ON app.ev_1 (n) WHERE n > 1; \
    CREATE INDEX ev_1_e ON app.ev_1 ((m + 0)); \
    CREATE INDEX ev_1_o ON app.ev_1 (o); \
    CREATE INDEX ev_1_r ON app.ev_1 ((k + 1)) WHERE m>0; \
    INSERT INTO app.ev VALUES (1, 1, 1, 1)";

const APART_PARENTS: &str = "  ev_n:\n    columns: [\"n\"]\n    where: \"n > 0\"\n  ev_m:\n    \
    columns: [m]\n  ev_o:\n    keys:\n      - expression: \"o + 0\"\n  ev_r:\n    keys:\n      \
    - expression: \"k + 1\"\n    where: \"(m > 0)\"\n";

#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_resolved_parent_index_leaves_each_own_it_does_not_take_standing() {
    engines(|server, image| {
        let p = Project::new(server, "adds-apart-1671", APART);
        let held = oids(&p, &["ev_1_n", "ev_1_e", "ev_1_o"]);
        let rebuilt = p.oid("ev_1_r");
        parent_gains(&p, "indexes", APART_PARENTS);
        // Without a resolver every one is rebuilt, and each says why.
        let (without, said) = p.plan("without", &[]);
        let without = saved(&without);
        assert_eq!(drops(&without), ["ev_1_e", "ev_1_n", "ev_1_o", "ev_1_r"]);
        assert_eq!(decided(&without), expected(&[]));
        for line in [
            "- index ev_1_n, built again: the parent's new index ev_n may take it (a resolver can \
             keep it)",
            "+ index ev_1_n, built again: the parent's new index ev_n may take it (a resolver can \
             keep it)",
        ] {
            assert!(said.contains(line), "{line}: {said}");
        }
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("kept", &[]);
        let saved = saved(&plan);
        assert_eq!(drops(&saved), ["ev_1_r"], "{said}");
        assert_eq!(
            decided(&saved),
            expected(&[
                ("ev_1/ev_m", &[], &["ev_1_e"]),
                ("ev_1/ev_n", &[], &["ev_1_n"]),
                ("ev_1/ev_o", &[], &["ev_1_e", "ev_1_o"]),
                ("ev_1/ev_r", &[], &["ev_1_e"]),
            ]),
            "{said}"
        );
        assert!(
            said.contains("leaving app.ev_1's index ev_1_n beside the copy"),
            "{said}"
        );
        // The respelled one is the engine's equal, so it is rebuilt, and no
        // resolver is said to keep it.
        assert!(
            said.contains(
                "- index ev_1_r, built again: the parent's new index ev_o or ev_r may take it  ["
            ),
            "{said}"
        );
        assert!(!said.contains("a resolver can keep it"), "{said}");
        succeeds(p.apply(&plan, &[]));
        unchanged(&p, &held);
        assert_ne!(p.oid("ev_1_r"), rebuilt);
        standing(
            &p,
            &[
                ("ev_1_n", ""),
                ("ev_1_e", ""),
                ("ev_1_o", ""),
                ("ev_1_r", ""),
            ],
        );
        for parent in ["ev_n", "ev_m", "ev_o", "ev_r"] {
            assert_eq!(clones(&p, parent), "1", "{parent}");
        }
        p.in_sync();
    });
}

/// A parent that already has an index, two partitions holding different lists,
/// an index moved to the parent, and an older declared own beside a newer
/// retiring equal one.
const MOVED: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (k integer NOT NULL, m integer, n integer) PARTITION BY RANGE (k); \
    CREATE INDEX ev_k ON app.ev (k); \
    CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
    CREATE TABLE app.ev_2 PARTITION OF app.ev FOR VALUES FROM (10) TO (20); \
    CREATE INDEX ev_1_m ON app.ev_1 (m); \
    CREATE INDEX ev_1_a ON app.ev_1 (n); \
    CREATE INDEX ev_1_b ON app.ev_1 (n); \
    CREATE INDEX ev_2_m ON app.ev_2 (m); \
    CREATE INDEX ev_2_n ON app.ev_2 (n) WHERE m > 5; \
    INSERT INTO app.ev VALUES (1, 1, 1), (11, 2, 2)";

#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_resolved_parent_index_takes_a_retiring_own_and_the_loop_leaves_a_declared_one() {
    engines(|server, image| {
        let p = Project::new(server, "adds-moved-1671", MOVED);
        let held = oids(&p, &["ev_1_m", "ev_1_b", "ev_2_m", "ev_2_n"]);
        let declared = p.oid("ev_1_a");
        for name in ["ev_1_m", "ev_1_b", "ev_2_m"] {
            undeclare(&p, name);
        }
        parent_gains(
            &p,
            "indexes",
            "  ev_m:\n    columns: [m]\n  ev_n:\n    columns: [\"n\"]\n",
        );
        let (without, _) = p.plan("without", &[]);
        assert_eq!(
            drops(&saved(&without)),
            ["ev_1_a", "ev_1_b", "ev_1_m", "ev_2_m", "ev_2_n"]
        );
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("taken", &[]);
        let saved = saved(&plan);
        // The older declared one is taken first, so it is rebuilt, and the
        // newer retiring one becomes the copy.
        assert_eq!(drops(&saved), ["ev_1_a"], "{said}");
        assert_eq!(
            decided(&saved),
            expected(&[
                ("ev_1/ev_m", &["ev_1_m"], &[]),
                ("ev_1/ev_n", &["ev_1_b"], &[]),
                ("ev_2/ev_m", &["ev_2_m"], &[]),
                ("ev_2/ev_n", &[], &["ev_2_n"]),
            ]),
            "{said}"
        );
        assert!(
            said.contains("keeping app.ev_1's index ev_1_m as the copy"),
            "{said}"
        );
        succeeds(p.apply(&plan, &[]));
        unchanged(&p, &held);
        assert_ne!(p.oid("ev_1_a"), declared);
        standing(
            &p,
            &[
                ("ev_1_m", "ev_m"),
                ("ev_1_b", "ev_n"),
                ("ev_2_m", "ev_m"),
                ("ev_2_n", ""),
                ("ev_1_a", ""),
            ],
        );
        p.in_sync();
    });
}

/// An unqualified call in the parent's definition binds under the parent's path,
/// so an own calling another schema's function of that name is not equal to it.
const ELSEWHERE: &str = "CREATE SCHEMA app; CREATE SCHEMA other; \
    CREATE FUNCTION app.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 + 1; \
    CREATE FUNCTION other.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 + 2; \
    CREATE TABLE app.ev (k integer NOT NULL, m integer) PARTITION BY RANGE (k); \
    CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
    CREATE INDEX ev_1_f ON app.ev_1 (other.f(m)); \
    INSERT INTO app.ev VALUES (1, 1)";

#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_parents_call_binds_under_its_own_path_and_leaves_another_schemas_beside() {
    engines(|server, image| {
        let p = Project::new(server, "adds-path-1671", ELSEWHERE);
        let held = oids(&p, &["ev_1_f"]);
        parent_gains(
            &p,
            "indexes",
            "  ev_f:\n    keys:\n      - expression: f(m)\n",
        );
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("beside", &[]);
        let saved = saved(&plan);
        assert_eq!(drops(&saved), Vec::<String>::new(), "{said}");
        assert_eq!(
            decided(&saved),
            expected(&[("ev_1/ev_f", &[], &["ev_1_f"])]),
            "{said}"
        );
        succeeds(p.apply(&plan, &[]));
        unchanged(&p, &held);
        standing(&p, &[("ev_1_f", "")]);
        assert_eq!(clones(&p, "ev_f"), "1");
        p.in_sync();
    });
}

const TWO: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.ev (k integer NOT NULL, m integer) PARTITION BY RANGE (k); \
    CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
    CREATE INDEX ev_1_m ON app.ev_1 (m) WHERE m > 5; \
    CREATE INDEX ev_1_old ON app.ev_1 (m); \
    INSERT INTO app.ev VALUES (1, 1)";

/// A column added to the parent in the same plan is in the rehearsal's shells,
/// so it changes nothing about how the indexes are handled; any other change to
/// a parent column gets no answer, and the report says so.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn an_added_parent_column_is_rehearsed_and_a_retyped_one_is_not() {
    engines(|server, image| {
        let p = Project::new(server, "adds-column-1671", TWO);
        let held = oids(&p, &["ev_1_m", "ev_1_old"]);
        undeclare(&p, "ev_1_old");
        edit(&p, |tree| {
            tree.replacen("columns:\n", "columns:\n  extra:\n    type: integer\n", 1)
        });
        parent_gains(
            &p,
            "indexes",
            "  ev_m:\n    columns: [m]\n  ev_extra:\n    columns: [extra]\n",
        );
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("added", &[]);
        let added = saved(&plan);
        assert_eq!(drops(&added), Vec::<String>::new(), "{said}");
        assert_eq!(
            decided(&added),
            expected(&[("ev_1/ev_m", &["ev_1_old"], &["ev_1_m"])]),
            "{said}"
        );
        succeeds(p.apply(&plan, &[]));
        unchanged(&p, &held);
        standing(&p, &[("ev_1_m", ""), ("ev_1_old", "ev_m")]);
        assert_eq!(clones(&p, "ev_extra"), "1");
        p.in_sync();

        let p = Project::new(server, "adds-retype-1671", TWO);
        retype(&p, "m", "bigint");
        parent_gains(&p, "indexes", "  ev_m:\n    columns: [m]\n");
        let (without, _) = p.plan("without", &[]);
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("retyped", &[]);
        assert_eq!(saved(&plan).changes, saved(&without).changes);
        assert!(
            said.contains("not rehearsed on app.ev_1: this plan also changes the parent's columns"),
            "{said}"
        );
        succeeds(p.apply(&plan, &[]));
        p.in_sync();
    });
}

/// A staged plan holds one change: with every own left to the addition it is the
/// addition alone, which a staged apply holds to the record and runs.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_staged_plan_can_hold_a_parents_addition_that_drops_nothing() {
    engines(|server, image| {
        let p = Project::new(
            server,
            "adds-held-1671",
            "CREATE SCHEMA app; \
             CREATE TABLE app.ev (k integer NOT NULL, m integer) PARTITION BY RANGE (k); \
             CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
             CREATE INDEX ev_1_m ON app.ev_1 (m); \
             CREATE INDEX ev_1_n ON app.ev_1 (m DESC); \
             INSERT INTO app.ev VALUES (1, 1)",
        );
        undeclare(&p, "ev_1_m");
        parent_gains(&p, "indexes", "  ev_m:\n    columns: [m]\n");
        succeeds(p.d.run(&["plan"]));
        p.d.commit();
        let refused = p.run(&["plan", "--env", "prod", "--staged"]);
        assert_ne!(code(&refused), 0, "{}", stdout(&refused));
        assert!(
            stderr(&refused).contains("this plan has"),
            "{}",
            stderr(&refused)
        );
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("staged", &["--staged"]);
        assert_eq!(
            decided(&saved(&plan)),
            expected(&[("ev_1/ev_m", &["ev_1_m"], &["ev_1_n"])]),
            "{said}"
        );
        let held = oids(&p, &["ev_1_m", "ev_1_n"]);
        succeeds(p.apply(&plan, &["--staged"]));
        unchanged(&p, &held);
        standing(&p, &[("ev_1_m", "ev_m"), ("ev_1_n", "")]);
        p.in_sync();
    });
}

/// A case with no answer plans as without a resolver, and says why: a column
/// renamed elsewhere, and a function the plan creates, which the scratch does not
/// hold.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn without_an_answer_a_parents_addition_plans_as_without_a_resolver_and_says_why() {
    engines(|server, image| {
        let setup = format!("{TWO}; CREATE TABLE app.other (a integer)");
        let renamed = Project::new(server, "adds-unsettled-1671", &setup);
        let file = renamed.d.dir.join("schema/app.other.yml");
        let tree = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, tree.replace("  a:", "  b:")).unwrap();
        succeeds(renamed.d.run(&["rename", "app.other.a", "b"]));
        parent_gains(&renamed, "indexes", "  ev_m:\n    columns: [m]\n");

        let created = Project::new(server, "adds-function-1671", TWO);
        std::fs::write(
            created.d.dir.join("schema/app.g%28integer%29.function.yml"),
            "function: app.g(integer)\npublic_execute: true\n\ndefinition: |-\n  \
             (x integer) RETURNS integer LANGUAGE sql IMMUTABLE SET search_path = pg_catalog \
             AS $$ SELECT x $$\n",
        )
        .unwrap();
        parent_gains(
            &created,
            "indexes",
            "  ev_m:\n    keys:\n      - expression: app.g(m)\n",
        );

        for (p, allow, why) in [
            (renamed, "destructive,constraint,rename", Skipped::Unsettles),
            (created, "destructive,constraint", Skipped::Scratch),
        ] {
            let (without, _) = p.plan("without", &[]);
            p.select(Resolver::Docker(image));
            let (plan, said) = p.plan("unanswered", &[]);
            assert_eq!(
                saved(&plan).changes,
                saved(&without).changes,
                "{why:?}: {said}"
            );
            let line = format!("not rehearsed on app.ev_1: {}", why.why());
            assert!(said.contains(&line), "{why:?}: {said}");
            assert!(!said.contains("a resolver can keep it"), "{why:?}: {said}");
            succeeds(p.apply(&plan, &["--allow", allow]));
            p.in_sync();
        }
    });
}

/// A retiring own a foreign key was bound to, and one under a name the
/// declarations give to something else, are dropped whatever the engine says;
/// one the engine would adopt for a standing key is refused with a resolver as
/// without one.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn a_referenced_or_claimed_own_is_not_left_to_a_parents_addition() {
    engines(|server, image| {
        let p = Project::new(
            server,
            "adds-claimed-1671",
            "CREATE SCHEMA app; \
             CREATE TABLE app.ev (k integer NOT NULL, m integer) PARTITION BY RANGE (k); \
             CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
             CREATE INDEX ev_1_m ON app.ev_1 (m); \
             CREATE TABLE app.other (a integer); \
             INSERT INTO app.ev VALUES (1, 1)",
        );
        undeclare(&p, "ev_1_m");
        parent_gains(&p, "indexes", "  ev_m:\n    columns: [m]\n");
        let file = p.d.dir.join("schema/app.other.yml");
        let tree = std::fs::read_to_string(&file).unwrap();
        std::fs::write(
            &file,
            format!("{tree}\nindexes:\n  ev_1_m:\n    columns: [a]\n"),
        )
        .unwrap();
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("claimed", &[]);
        assert_eq!(drops(&saved(&plan)), ["ev_1_m"], "{said}");
        assert_eq!(decided(&saved(&plan)), expected(&[]), "{said}");
        succeeds(p.apply(&plan, &[]));
        p.in_sync();

        // A standing referenced own the engine would adopt stays refused.
        let p = Project::new(
            server,
            "adds-referenced-1671",
            "CREATE SCHEMA app; \
             CREATE TABLE app.ev (k integer NOT NULL, m integer) PARTITION BY RANGE (k); \
             CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
             CREATE UNIQUE INDEX ev_1_u ON app.ev_1 (k); \
             CREATE TABLE app.r (k integer); \
             ALTER TABLE app.r ADD CONSTRAINT r_fk FOREIGN KEY (k) REFERENCES app.ev_1 (k)",
        );
        parent_gains(
            &p,
            "indexes",
            "  ev_u:\n    columns: [k]\n    unique: true\n",
        );
        for resolver in [Resolver::None, Resolver::Docker(image)] {
            p.select(resolver);
            let refused = p.d.run(&["plan"]);
            assert_ne!(code(&refused), 0, "{}", stdout(&refused));
            assert!(stderr(&refused).contains("r_fk"), "{}", stderr(&refused));
        }
    });
}

/// A key an unmanaged table holds into a partition is nothing the differ sees.
/// Without a resolver the own it stands on is rebuilt, which the key refuses. An
/// own the engine would not take is left standing with a resolver, so that plan
/// is applied, and the key stays where it was (DEC-1671.3).
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn an_own_an_unmanaged_key_references_is_left_standing() {
    engines(|server, image| {
        let p = Project::new(
            server,
            "adds-unmanaged-1671",
            "CREATE SCHEMA app; \
             CREATE TABLE app.ev (k integer NOT NULL, m integer) PARTITION BY RANGE (k); \
             CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
             CREATE UNIQUE INDEX ev_1_u ON app.ev_1 (k); \
             INSERT INTO app.ev VALUES (1, 1)",
        );
        // Made after the project adopted the database, which declares none of it.
        on_server(
            p.connection(),
            "CREATE SCHEMA elsewhere; \
             CREATE TABLE elsewhere.r (k integer CONSTRAINT r_fk REFERENCES app.ev_1 (k)); \
             INSERT INTO elsewhere.r VALUES (1)",
        );
        let held = oids(&p, &["ev_1_u"]);
        // The engine binds no key's index to one that holds more columns.
        parent_gains(
            &p,
            "indexes",
            "  ev_u:\n    columns: [k]\n    include: [m]\n    unique: true\n",
        );
        succeeds(p.d.run(&["plan"]));
        p.d.commit();
        let refused = p.run(&["plan", "--env", "prod"]);
        assert_ne!(code(&refused), 0, "{}", stdout(&refused));
        assert!(stderr(&refused).contains("r_fk"), "{}", stderr(&refused));
        p.select(Resolver::Docker(image));
        let (plan, said) = p.plan("kept", &[]);
        assert_eq!(drops(&saved(&plan)), Vec::<String>::new(), "{said}");
        assert_eq!(
            decided(&saved(&plan)),
            expected(&[("ev_1/ev_u", &[], &["ev_1_u"])]),
            "{said}"
        );
        succeeds(p.apply(&plan, &[]));
        unchanged(&p, &held);
        standing(&p, &[("ev_1_u", "")]);
        assert_eq!(
            text_of(
                p.connection(),
                "SELECT conindid::regclass::text FROM pg_constraint WHERE conname = 'r_fk'"
            ),
            "app.ev_1_u"
        );
        p.in_sync();
    });
}

/// An invalid own is taken by the addition and marks the parent's index invalid,
/// which a copy on an empty table does not show, so the rehearsal gives no
/// answer for it.
#[test]
#[ignore = "needs PostgreSQL 18 and 16 and a Docker daemon; set PBPS_TEST_PG_DB, PBPS_TEST_PG_OLD_DB and their scratch images"]
fn an_invalid_own_gets_no_answer_from_the_rehearsal() {
    engines(|server, _| {
        let p = Project::new(server, "adds-invalid-1671", TWO);
        let scratch = OwnDatabase::new(server, "adds-shells-1671");
        on_server(scratch.connection(), "CREATE SCHEMA app");
        let ask = || rehearsed(p.connection(), scratch.connection());
        let answered = ask().expect("an answer");
        assert_eq!(
            answered.adopted.keys().map(|n| n.raw()).collect::<Vec<_>>(),
            ["ev_1_old"]
        );
        assert_eq!(
            answered.beside.iter().map(|n| n.raw()).collect::<Vec<_>>(),
            ["ev_1_m"]
        );
        on_server(
            p.connection(),
            "UPDATE pg_index SET indisvalid = false \
             WHERE indexrelid = 'app.ev_1_old'::regclass",
        );
        assert_eq!(ask(), Err(Skipped::InvalidIndex));
    });
}

/// The scratch's answer for a plain index `ev_m` on `m` added over `app.ev_1`,
/// asked directly.
fn rehearsed(target: &str, scratch: &str) -> Result<pbps_model::adoption::AdditionAnswer, Skipped> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let connect = |to| pbps_db::Conn::connect(pbps_db::Driver::Postgres, to);
            let name = |table| TableName::new("app", table);
            let mut conn = connect(target).await.unwrap();
            let wanted = [name("ev"), name("ev_1")].into();
            let (_, relations) = pbps_pg::catalog::introspect_wanting(&mut conn, &wanted)
                .await
                .unwrap();
            let held = |table| relations[&name(table)].as_ref().unwrap();
            let question = pbps_diff::attach::AdditionQuestion {
                uid: "t_aaaaaa".parse().unwrap(),
                parent_uid: "t_bbbbbb".parse().unwrap(),
                table: name("ev_1"),
                parent: name("ev"),
                changes: vec![pbps_model::Change::Index(
                    pbps_model::IndexChange::AddIndex {
                        table: name("ev"),
                        name: "ev_m".into(),
                        index: Box::new(pbps_model::Index {
                            columns: vec![pbps_model::IndexColumn::column("m")],
                            include: Vec::new(),
                            unique: false,
                            filter: None,
                            method: Default::default(),
                            storage_parameters: Default::default(),
                            filegroup: None,
                        }),
                        clustered: false,
                        rehearsal: None,
                    },
                )],
                declared: ["ev_1_m".into()].into(),
                retiring: ["ev_1_old".into()].into(),
                dropped: Default::default(),
            };
            let mut conn = connect(scratch).await.unwrap();
            pbps_pg::adoption::rehearse_addition(
                &mut conn,
                &pbps_pg::Postgres::new(),
                &question,
                held("ev"),
                held("ev_1"),
            )
            .await
            .unwrap()
        })
}

/// On 18, the engine's own equality of exclusions: a declared one with no
/// predicate is left beside, and the retiring equal one becomes the copy. A
/// declared one the engine would take is refused as the parent's again, with or
/// without a resolver (DEC-2448.7).
#[test]
#[ignore = "needs PostgreSQL 18 and a Docker daemon; set PBPS_TEST_PG_DB and its scratch image"]
fn a_resolved_parent_exclusion_leaves_rebuilds_and_takes_as_the_engine_does_on_18() {
    let read = |variable| std::env::var(variable).expect(variable);
    let own = |name: &str, predicate: &str| {
        format!(
            "ALTER TABLE app.ev_1 ADD CONSTRAINT {name} EXCLUDE USING btree \
             (k WITH =, v WITH =){predicate}; "
        )
    };
    let setup = format!(
        "CREATE SCHEMA app; \
         CREATE TABLE app.ev (k integer NOT NULL, v integer NOT NULL) PARTITION BY RANGE (k); \
         CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
         {}{}INSERT INTO app.ev VALUES (1, 1)",
        own("ev_1_diff", ""),
        own("ev_1_gone", " WHERE (v > 0)"),
    );
    let p = Project::new(&read("PBPS_TEST_PG_DB"), "adds-exclusion-1671", &setup);
    let held = oids(&p, &["ev_1_diff", "ev_1_gone"]);
    undeclare(&p, "ev_1_gone");
    succeeds(p.d.run(&[
        "drop-exclusion",
        "app.ev_1",
        "ev_1_gone",
        "--reason",
        "the parent's holds it",
    ]));
    parent_gains(
        &p,
        "exclude",
        "  ev_x:\n    method: btree\n    keys:\n      - column: k\n        operator: \"=\"\n      \
         - column: v\n        operator: \"=\"\n    where: \"(v > 0)\"\n",
    );
    let (without, said) = p.plan("without", &[]);
    assert_eq!(taken_off(&saved(&without)), ["ev_1_diff", "ev_1_gone"]);
    assert!(
        said.contains("the parent's new exclusion constraint ev_x may take it"),
        "{said}"
    );
    p.select(Resolver::Docker(&read("PBPS_TEST_PG_SCRATCH_IMAGE")));
    let (plan, said) = p.plan("kept", &[]);
    let saved = saved(&plan);
    assert_eq!(taken_off(&saved), Vec::<String>::new(), "{said}");
    assert_eq!(
        decided(&saved),
        expected(&[("ev_1/ev_x", &["ev_1_gone"], &["ev_1_diff"])]),
        "{said}"
    );
    succeeds(p.apply(&plan, &[]));
    unchanged(&p, &held);
    standing(&p, &[("ev_1_diff", ""), ("ev_1_gone", "ev_x")]);
    p.in_sync();
}
