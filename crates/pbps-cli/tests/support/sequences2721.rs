//! Standalone sequences through the tool (#2721 PR A, DEC-2721.1): pulled into declaration
//! files, pulled again with no difference and an empty follow-up plan, advanced with `nextval`
//! without a plan or a drift, what is not held left out and named, and every difference refused
//! by name because nothing plans them yet. Each case runs on both pinned engines.

use super::rehearsal2379::{Fixture, Kind, Options};
use super::{code, on_server, stderr, stdout, succeeds};
use std::collections::BTreeMap;
use std::process::Output;

fn servers(mut check: impl FnMut(&str, &str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).unwrap(), variable);
    }
}

fn said(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

fn fixture(server: &str, slug: &str) -> Fixture {
    Fixture::new(server, slug, Kind::Supplied, &Options::default())
}

/// What a user made: an integer sequence with every option, a plain one, one with a step
/// beyond its type's span, and a table that calls none of them (a default that calls one is a
/// limitation of its own, DEC-2721.1).
const MADE: &str = "\
    CREATE SEQUENCE app.order_no AS integer START 100 INCREMENT 5 MINVALUE 100 MAXVALUE 999999 CYCLE CACHE 20; \
    CREATE SEQUENCE app.plain; \
    CREATE SEQUENCE app.stride AS smallint INCREMENT BY 40000; \
    CREATE TABLE app.orders (id integer PRIMARY KEY, note text)";

/// The sequence files under `schema/`, by name, with their text.
fn sequence_files(f: &Fixture) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(f.d.dir.join("schema")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.ends_with(".sequence.yml") {
            out.insert(name, std::fs::read_to_string(&path).unwrap());
        }
    }
    out
}

fn no_changes(out: &Output) {
    assert_eq!(code(out), 0, "{}", said(out));
    assert!(stdout(out).contains("No changes"), "{}", said(out));
}

fn pull(f: &Fixture) -> Output {
    succeeds(f.d.run(&["pull", "--force", "--db", f.target()]))
}

fn adopt(f: &Fixture) {
    f.d.commit();
    succeeds(f.d.run(&[
        "baseline",
        "--db",
        f.target(),
        "--reason",
        "adopt the sequences",
    ]));
}

/// A database with standalone sequences is pulled into one file each, with the table's default
/// carried and no limitation about it; pulled again the files are the same bytes; the plan is
/// empty and the recorded state verifies. **Advancing a sequence with `nextval` and `setval`
/// then produces no plan and no drift**, and a sequence made after the baseline is not either.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_database_with_standalone_sequences_pulls_again_with_no_difference_and_advancing_one_plans_nothing()
 {
    servers(|server, slug| {
        let f = fixture(server, &format!("sequences2721-roundtrip-{slug}"));
        on_server(f.target(), MADE);
        let out = pull(&f);
        assert!(
            !said(&out).contains("standalone sequence"),
            "a held sequence is not a census notice: {}",
            said(&out)
        );
        let files = sequence_files(&f);
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "app.order_no.sequence.yml",
                "app.plain.sequence.yml",
                "app.stride.sequence.yml"
            ],
            "the sequences the user made, a step beyond the type's span included, and none a column owns"
        );
        assert_eq!(
            files["app.order_no.sequence.yml"],
            "sequence: app.order_no\ntype: integer\nstart: 100\nincrement: 5\nmin: 100\n\
             max: 999999\ncycle: true\ncache: 20\n"
        );
        assert_eq!(
            files["app.plain.sequence.yml"],
            "sequence: app.plain\ntype: bigint\nstart: 1\nincrement: 1\n"
        );
        // What the pull wrote validates: no pulled sequence is dropped on the way.
        succeeds(f.d.run(&["validate"]));
        assert!(!said(&out).contains("was left out"), "{}", said(&out));
        // The same bytes the second time.
        pull(&f);
        assert_eq!(sequence_files(&f), files);
        adopt(&f);
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        // Advancing the sequences is not a change: not for the plan, not for drift.
        on_server(
            f.target(),
            "SELECT nextval('app.order_no'); SELECT nextval('app.order_no'); \
             SELECT setval('app.plain', 4000); INSERT INTO app.orders (id, note) VALUES (1, 'x')",
        );
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        // A sequence made outside the declarations after the baseline is not drift either
        // (DEC-2721.1): verify and the plan are as before.
        on_server(f.target(), "CREATE SEQUENCE app.made_later");
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
    });
}

/// What the model does not hold is left out and named, never read as less than it is: an
/// unlogged standalone sequence is named by the census at pull, and a default over it is
/// still a limitation. A sequence a `serial` owns, or `OWNED BY` a column, is not a standalone
/// one: it is no file of its own.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn what_the_model_does_not_hold_is_left_out_and_named_and_an_owned_sequence_is_not_standalone() {
    servers(|server, slug| {
        let f = fixture(server, &format!("sequences2721-left-{slug}"));
        on_server(
            f.target(),
            // Names the fixture's own tables (`t`, `country`, `f`, `v`) do not take.
            "CREATE UNLOGGED SEQUENCE app.lonely; CREATE SEQUENCE app.plain; \
             CREATE TABLE app.lonely_user (id integer DEFAULT nextval('app.lonely')); \
             CREATE TABLE app.serial_user (id serial); \
             CREATE TABLE app.owner_tbl (id integer); \
             CREATE SEQUENCE app.owned_seq OWNED BY app.owner_tbl.id",
        );
        let out = pull(&f);
        let notes = said(&out);
        assert!(
            notes.contains("standalone sequence `app.lonely`"),
            "the unheld one is named by the census: {notes}"
        );
        assert!(
            notes.contains("defaults from the sequence") && notes.contains("app.lonely"),
            "a default over a sequence the model cannot hold is still named: {notes}"
        );
        let files = sequence_files(&f);
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        assert_eq!(names, ["app.plain.sequence.yml"], "{notes}");
        // Negative: the owned ones are no standalone sequence and are not named as one.
        assert!(
            !notes.contains("standalone sequence `app.serial_user_id_seq`")
                && !notes.contains("standalone sequence `app.owned_seq`"),
            "{notes}"
        );
        let serial = std::fs::read_to_string(f.d.dir.join("schema/app.serial_user.yml")).unwrap();
        assert!(serial.contains("serial"), "{serial}");
    });
}

/// Nothing plans a sequence yet, so a declaration that differs from the target is refused by
/// name, whichever way it differs; a sequence only the target has is left alone.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn every_difference_is_refused_by_name_and_an_undeclared_sequence_is_left_alone() {
    servers(|server, slug| {
        let f = fixture(server, &format!("sequences2721-refused-{slug}"));
        on_server(f.target(), MADE);
        pull(&f);
        adopt(&f);
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        let dir = f.d.dir.join("schema");
        // A sequence the target lacks: refused as arriving with a later part.
        std::fs::write(
            dir.join("app.fresh.sequence.yml"),
            "sequence: app.fresh\ntype: bigint\nstart: 1\nincrement: 1\n",
        )
        .unwrap();
        let out = f.d.run(&["plan", "--db", f.target()]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("sequence `app.fresh`") && said(&out).contains("creating one"),
            "{}",
            said(&out)
        );
        std::fs::remove_file(dir.join("app.fresh.sequence.yml")).unwrap();
        // A sequence the target holds differently, said attribute by attribute.
        let path = dir.join("app.order_no.sequence.yml");
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            original
                .replace("increment: 5", "increment: 7")
                .replace("cycle: true\n", ""),
        )
        .unwrap();
        let out = f.d.run(&["plan", "--db", f.target()]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        let text = said(&out);
        assert!(
            text.contains("sequence `app.order_no`")
                && text.contains("increment: declared 7, target has 5")
                && text.contains("cycle: declared false, target has true")
                && !text.contains("type:"),
            "{text}"
        );
        std::fs::write(&path, &original).unwrap();
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        // Negative: a sequence only the target has is not this project's.
        std::fs::remove_file(dir.join("app.plain.sequence.yml")).unwrap();
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
    });
}

/// The guard that refuses to record over a different state compares the managed projection
/// `verify` does (DEC-2721.1): a sequence nobody declared or recorded, made after the
/// baseline, is not a difference, so `snapshot` records without `--force`; a recorded sequence
/// that was changed is one, and `snapshot` refuses.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_sequence_nobody_recorded_is_not_a_difference_for_snapshot_and_a_changed_recorded_one_is() {
    servers(|server, slug| {
        let f = fixture(server, &format!("sequences2721-snapshot-{slug}"));
        on_server(f.target(), MADE);
        pull(&f);
        adopt(&f);
        on_server(f.target(), "CREATE SEQUENCE app.made_later START 50");
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        succeeds(f.d.run(&["snapshot", "--db", f.target()]));
        // Negative: a sequence the record holds, changed by hand, is a difference.
        on_server(f.target(), "ALTER SEQUENCE app.order_no INCREMENT BY 7");
        let out = f.d.run(&["snapshot", "--db", f.target()]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("differs from the state recorded"),
            "{}",
            said(&out)
        );
        succeeds(f.d.run(&["snapshot", "--db", f.target(), "--force"]));
    });
}

/// A table whose default calls a standalone sequence keeps the limitation it had before the
/// sequence was held (DEC-2721.1): the pull declares the sequence and carries the table with
/// the limitation named, and the table cannot be recorded, because nothing yet creates the
/// sequence before the table that calls it. A rehearsal that built it from nothing would have
/// refused a plan that was valid.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_table_defaulting_from_a_standalone_sequence_keeps_its_limitation_and_cannot_be_baselined() {
    servers(|server, slug| {
        let f = fixture(server, &format!("sequences2721-default-{slug}"));
        on_server(
            f.target(),
            "CREATE SEQUENCE app.order_no; \
             CREATE TABLE app.caller (id integer DEFAULT nextval('app.order_no'), note text)",
        );
        let out = pull(&f);
        let notes = said(&out);
        assert!(
            notes.contains("defaults from the sequence") && notes.contains("app.order_no"),
            "the limitation is named: {notes}"
        );
        // The sequence itself is declared, and the table is carried with its default.
        assert!(sequence_files(&f).contains_key("app.order_no.sequence.yml"));
        let table = std::fs::read_to_string(f.d.dir.join("schema/app.caller.yml")).unwrap();
        assert!(table.contains("nextval("), "{table}");
        f.d.commit();
        let out = f.d.run(&[
            "baseline",
            "--db",
            f.target(),
            "--reason",
            "adopt a default over a sequence",
        ]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("defaults from the sequence"),
            "{}",
            said(&out)
        );
        // Negative: the sequence beside a table that does not call it records and plans.
        let g = fixture(server, &format!("sequences2721-default-none-{slug}"));
        on_server(
            g.target(),
            "CREATE SEQUENCE app.order_no; CREATE TABLE app.caller (id integer, note text)",
        );
        pull(&g);
        adopt(&g);
        no_changes(&succeeds(g.d.run(&["plan", "--db", g.target()])));
    });
}

/// A sequence the target holds, beside a plan that goes through the rehearsal, does not make
/// the plan unanswerable (DEC-2721.1): the scratch is built without what no plan can create.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_rehearsed_plan_beside_a_held_sequence_converges_on_the_scratch() {
    servers(|server, slug| {
        let f = Fixture::new(
            server,
            &format!("sequences2721-rehearse-{slug}"),
            Kind::Supplied,
            &Options {
                target_only: Some(
                    "CREATE SEQUENCE app.held_beside; CREATE SEQUENCE app.held_too START 9",
                ),
                ..Options::default()
            },
        );
        f.revise_row();
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let text = stdout(&out);
        assert!(text.contains("Rehearsal: converged on scratch"), "{text}");
        assert!(
            !said(&out).contains("could not be completed"),
            "{}",
            said(&out)
        );
    });
}

/// Drift in a recorded sequence is reported by name (DEC-2721.1): one dropped or renamed away
/// out of band, and one altered, each make `verify` say so; a sequence nobody recorded, made
/// beside them, is not looked at.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_recorded_sequence_that_was_dropped_renamed_or_altered_is_drift_by_name_and_an_unrecorded_one_is_not()
 {
    servers(|server, slug| {
        let f = fixture(server, &format!("sequences2721-drift-{slug}"));
        on_server(f.target(), MADE);
        pull(&f);
        adopt(&f);
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        // Negative: a sequence nobody recorded is not drift.
        on_server(f.target(), "CREATE SEQUENCE app.made_later");
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        let drifted = |sql: &str| {
            on_server(f.target(), sql);
            let out = f.d.run(&["verify", "--db", f.target()]);
            assert_eq!(code(&out), 2, "{}", said(&out));
            said(&out)
        };
        let text = drifted("ALTER SEQUENCE app.order_no INCREMENT BY 7");
        assert!(
            text.contains("sequence `app.order_no` changed since it was recorded")
                && text.contains("increment: now 7, recorded 5"),
            "{text}"
        );
        on_server(f.target(), "ALTER SEQUENCE app.order_no INCREMENT BY 5");
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        let text = drifted("DROP SEQUENCE app.plain");
        assert!(
            text.contains("sequence `app.plain` was recorded and is not in the database now"),
            "{text}"
        );
        on_server(f.target(), "CREATE SEQUENCE app.plain");
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        let text = drifted("ALTER SEQUENCE app.stride RENAME TO stride_moved");
        assert!(
            text.contains("sequence `app.stride` was recorded and is not in the database now"),
            "{text}"
        );
        // The renamed one is a sequence nobody recorded: not named on its own.
        assert!(!text.contains("stride_moved"), "{text}");
    });
}

/// Of the sequences, a recording holds only what the declarations name (DEC-2721.1): a helper
/// the target holds and nobody declared, present when a state is recorded by a bootstrap, a
/// baseline or an apply, is not in the record, so dropping it later is no drift of the
/// project's. Negative: a declared and recorded sequence that is dropped still is, by name.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_undeclared_sequence_present_when_a_state_is_recorded_is_not_recorded_and_dropping_it_is_no_drift()
 {
    servers(|server, slug| {
        // Bootstrap: the helper stood before the build.
        let f = Fixture::new(
            server,
            &format!("sequences2721-rec-boot-{slug}"),
            Kind::Supplied,
            &Options {
                externals: Some("CREATE SEQUENCE app.helper"),
                ..Options::default()
            },
        );
        on_server(f.target(), "DROP SEQUENCE app.helper");
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        // Apply: the helper appears after the build, and a plan is applied beside it.
        let a = Fixture::new(
            server,
            &format!("sequences2721-rec-apply-{slug}"),
            Kind::Supplied,
            &Options {
                target_only: Some("CREATE SEQUENCE app.helper"),
                ..Options::default()
            },
        );
        a.revise_row();
        succeeds(a.plan(&[]));
        succeeds(a.apply("data-update"));
        on_server(a.target(), "DROP SEQUENCE app.helper");
        succeeds(a.d.run(&["verify", "--db", a.target()]));
        // Baseline: the helper stands when the state is recorded from the declarations.
        let b = fixture(server, &format!("sequences2721-rec-base-{slug}"));
        on_server(b.target(), MADE);
        pull(&b);
        on_server(b.target(), "CREATE SEQUENCE app.helper");
        adopt(&b);
        on_server(b.target(), "DROP SEQUENCE app.helper");
        succeeds(b.d.run(&["verify", "--db", b.target()]));
        // Snapshot: a second helper stands when the current state is recorded.
        on_server(b.target(), "CREATE SEQUENCE app.helper_two");
        succeeds(b.d.run(&["snapshot", "--db", b.target()]));
        on_server(b.target(), "DROP SEQUENCE app.helper_two");
        succeeds(b.d.run(&["verify", "--db", b.target()]));
        // Negative: a declared and recorded one that is dropped is drift, by name.
        on_server(b.target(), "DROP SEQUENCE app.plain");
        let out = b.d.run(&["verify", "--db", b.target()]);
        assert_eq!(code(&out), 2, "{}", said(&out));
        assert!(
            said(&out).contains("sequence `app.plain` was recorded and is not in the database now"),
            "{}",
            said(&out)
        );
    });
}
