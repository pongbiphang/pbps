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

/// What a user made: an integer sequence with every option, a plain one, and a table whose
/// primary key defaults from the first.
const MADE: &str = "\
    CREATE SEQUENCE app.order_no AS integer START 100 INCREMENT 5 MINVALUE 100 MAXVALUE 999999 CYCLE CACHE 20; \
    CREATE SEQUENCE app.plain; \
    CREATE TABLE app.orders (id integer PRIMARY KEY DEFAULT nextval('app.order_no'), note text)";

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
            !said(&out).contains("which it does not own")
                && !said(&out).contains("standalone sequence"),
            "a held sequence is neither a limitation nor a census notice: {}",
            said(&out)
        );
        let files = sequence_files(&f);
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            ["app.order_no.sequence.yml", "app.plain.sequence.yml"],
            "the sequences the user made, and none a column owns"
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
        let table = std::fs::read_to_string(f.d.dir.join("schema/app.orders.yml")).unwrap();
        assert!(table.contains("nextval("), "{table}");
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
             SELECT setval('app.plain', 4000); INSERT INTO app.orders (note) VALUES ('x')",
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
