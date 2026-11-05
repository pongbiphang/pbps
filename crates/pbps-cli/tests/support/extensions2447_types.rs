//! A column of a type a package provides: read by the pull when the package is
//! managed, left out with the command that opts it in when it is not, planned
//! against a pinned provider, carried through a move of the package, and
//! brought by a package the same plan creates (#2447, DEC-2447.12). Every case
//! runs on both pinned engines, against the packages the images ship (`citext`,
//! `hstore`, `ltree`), on a confined scratch of the target's own cluster.

use super::rehearsal2379::{Fixture, Kind, Options};
use super::{code, on_server, scalar, stderr, stdout, succeeds, text_of};
use std::process::Output;

fn servers(mut check: impl FnMut(&str, &str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).unwrap(), variable);
    }
}

fn said(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

/// The packages' own text, run on the target and on the scratch.
const PACKAGES: &str = "CREATE SCHEMA IF NOT EXISTS ext; \
    CREATE EXTENSION citext SCHEMA ext; CREATE EXTENSION hstore SCHEMA ext; \
    CREATE EXTENSION ltree SCHEMA ext";

/// A table of the target's own, before anything is pulled.
const WANTS: &str = "CREATE TABLE app.wants (id integer PRIMARY KEY, \
    c ext.citext, ca ext.citext[], h ext.hstore, l ext.ltree)";

fn fixture(server: &str, slug: &str) -> Fixture {
    fixture_on(server, slug, Kind::Supplied)
}

/// [`fixture`] on another kind of scratch. A package the engine marks `trusted`
/// is installed by its bootstrap superuser, so its members belong to one, and a
/// move needs the role that owns them: the move case runs where the scratch
/// role is a superuser too, as the target's own is (DEC-2447.11).
fn fixture_on(server: &str, slug: &str, kind: Kind) -> Fixture {
    Fixture::new(
        server,
        slug,
        kind,
        &Options {
            externals: Some(PACKAGES),
            ..Options::default()
        },
    )
}

/// The packages named, pulled, written and recorded the way an operator adopts
/// them, with `app.wants` on the target.
fn adopted(f: &Fixture, packages: &[&str]) {
    on_server(f.target(), WANTS);
    let mut args = vec!["pull", "--force", "--db", f.target()];
    for name in packages {
        args.extend(["--extension", name]);
    }
    succeeds(f.d.run(&args));
    f.d.commit();
    succeeds(f.d.run(&[
        "baseline",
        "--db",
        f.target(),
        "--reason",
        "adopt the packages",
    ]));
}

/// An offline plan mints the identities a new declaration needs, and the
/// project commits them: a connected plan computes against what reviewers read.
fn minted(f: &Fixture) {
    succeeds(f.d.run(&["plan"]));
    f.d.commit();
}

fn table_file(f: &Fixture) -> String {
    std::fs::read_to_string(f.d.dir.join("schema/app.wants.yml")).unwrap_or_default()
}

fn saved(f: &Fixture) -> pbps_model::SavedPlan {
    serde_json::from_slice(&std::fs::read(f.artifact()).unwrap()).unwrap()
}

fn no_changes(out: &Output) {
    assert_eq!(code(out), 0, "{}", said(out));
    assert!(stdout(out).contains("No changes"), "{}", said(out));
}

/// With its package managed, a table with a column, an array and a default of
/// a provided type is pulled as written, baselined, and planned as no change;
/// without it the table is left out and the note names the command.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_provided_type_is_read_when_its_package_is_managed_and_left_out_with_the_command_when_not() {
    servers(|server, slug| {
        let f = fixture(server, &format!("types2447-pull-{slug}"));
        adopted(&f, &["citext", "hstore", "ltree"]);
        let table = table_file(&f);
        for held in ["ext.citext", "ext.hstore", "ext.ltree"] {
            assert!(table.contains(held), "{held}:\n{table}");
        }
        assert!(table.contains("ext.citext[]"), "the array form:\n{table}");
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        succeeds(f.d.run(&["verify", "--db", f.target()]));

        // Negative: pulled with `hstore` and `ltree` only, the table whose
        // column is `ext.citext` is left out, and the command is named.
        let g = fixture(server, &format!("types2447-left-{slug}"));
        on_server(g.target(), WANTS);
        let out = succeeds(g.d.run(&[
            "pull",
            "--force",
            "--db",
            g.target(),
            "--extension",
            "hstore",
            "--extension",
            "ltree",
        ]));
        let said = said(&out);
        assert!(
            said.contains("`app.wants` was left out")
                && said.contains("pbps pull --extension citext"),
            "{said}"
        );
        assert!(!g.d.dir.join("schema/app.wants.yml").exists());
    });
}

/// A declared column of a type whose package the project does not manage is
/// refused at plan, by name, with the command; a base type no package owns is
/// refused as before.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_type_of_an_unmanaged_package_or_of_no_package_is_refused_by_name() {
    servers(|server, slug| {
        let f = fixture(server, &format!("types2447-refuse-{slug}"));
        adopted(&f, &["hstore"]);
        std::fs::write(
            f.d.dir.join("schema/app.extra.yml"),
            "table: app.extra\ncolumns:\n  id: {type: integer, nullable: false}\n  \
             c: {type: ext.citext}\nprimary_key: {name: pk_extra, columns: [id]}\n",
        )
        .unwrap();
        minted(&f);
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        let said_unmanaged = said(&out);
        assert!(
            said_unmanaged.contains("citext")
                && said_unmanaged.contains("does not manage")
                && said_unmanaged.contains("pbps pull --extension citext"),
            "{said_unmanaged}"
        );

        // A base type built from ordinary support functions has no package.
        on_server(
            f.target(),
            "CREATE TYPE public.mytxt; \
             CREATE FUNCTION public.mytxt_in(cstring) RETURNS public.mytxt \
               LANGUAGE internal IMMUTABLE STRICT AS 'textin'; \
             CREATE FUNCTION public.mytxt_out(public.mytxt) RETURNS cstring \
               LANGUAGE internal IMMUTABLE STRICT AS 'textout'; \
             CREATE TYPE public.mytxt (INPUT = public.mytxt_in, OUTPUT = public.mytxt_out, LIKE = text)",
        );
        std::fs::write(
            f.d.dir.join("schema/app.extra.yml"),
            "table: app.extra\ncolumns:\n  id: {type: integer, nullable: false}\n  \
             c: {type: public.mytxt}\nprimary_key: {name: pk_extra, columns: [id]}\n",
        )
        .unwrap();
        f.d.commit();
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(said(&out).contains("no extension owns"), "{}", said(&out));
    });
}

/// A new column of a provided type is planned against a pinned provider,
/// applied, read back and planned again as no change; the pin names the package
/// and its digest moves when the type's own properties change before apply.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_new_column_of_a_provided_type_is_pinned_applied_and_refused_when_the_type_changes() {
    servers(|server, slug| {
        let f = fixture(server, &format!("types2447-add-{slug}"));
        adopted(&f, &["citext", "hstore", "ltree"]);
        let path = f.d.dir.join("schema/app.wants.yml");
        let table = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            table.replace("primary_key", "  c2: {type: ext.citext}\nprimary_key"),
        )
        .unwrap();
        minted(&f);
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let plan = saved(&f);
        assert_eq!(plan.version, pbps_model::plan::CURRENT_VERSION);
        let pin = plan
            .type_pins
            .iter()
            .find(|p| p.name == "citext")
            .unwrap_or_else(|| panic!("{:#?}", plan.type_pins));
        assert_eq!(pin.kind, "provided");
        let provider = pin.provider.as_ref().expect("the provider is pinned");
        assert_eq!(provider.extension, "citext");

        // The type changes after the plan: the digest holds its support functions.
        on_server(f.target(), "ALTER TYPE ext.citext SET (RECEIVE = NONE)");
        let refused = f.apply("destructive");
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("changed since planning"),
            "{}",
            said(&refused)
        );
        on_server(
            f.target(),
            "ALTER TYPE ext.citext SET (RECEIVE = ext.citextrecv)",
        );

        succeeds(f.plan(&[]));
        succeeds(f.apply("destructive"));
        assert_eq!(
            scalar(
                f.target(),
                "SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.wants'::regclass \
                 AND attname = 'c2' AND atttypid = 'ext.citext'::regtype"
            ),
            1
        );
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        succeeds(f.d.run(&["verify", "--db", f.target()]));
    });
}

/// A move of the package carries its columns: the type keeps its OID, so the
/// column is not retyped, and the declarations that spell the schema are
/// re-planned as the text changes they are.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_move_of_the_package_carries_its_columns_without_retyping_them() {
    servers(|server, slug| {
        // The rehearsal of a move runs as the scratch's role, and the members of
        // a trusted package belong to a superuser: the server scratch (a
        // superuser, PostgreSQL 18 only here) rehearses it, and on a confined
        // one the plan is refused by name on both engines (below).
        if slug != "PBPS_TEST_PG_DB" {
            return;
        }
        let f = fixture_on(server, &format!("types2447-move-{slug}"), Kind::Server);
        adopted(&f, &["citext", "hstore", "ltree"]);
        let before = scalar(
            f.target(),
            "SELECT oid::int8 FROM pg_type WHERE oid = 'ext.citext'::regtype",
        );
        on_server(f.target(), "CREATE SCHEMA moved");
        // The package, and every declaration that spells its schema.
        let spelled = |text: String| text.replace("ext.citext", "moved.citext");
        let path = f.d.dir.join("schema/app.wants.yml");
        std::fs::write(&path, spelled(std::fs::read_to_string(&path).unwrap())).unwrap();
        let declaration = f.d.dir.join("schema/extensions/citext.yml");
        let text = std::fs::read_to_string(&declaration).unwrap();
        std::fs::write(&declaration, text.replace("schema: ext", "schema: moved")).unwrap();
        f.d.commit();
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let plan = saved(&f);
        let kinds: Vec<String> = plan
            .changes
            .changes
            .iter()
            .map(|c| format!("{:?}", c.change))
            .collect();
        assert!(
            kinds.iter().any(|k| k.starts_with("SetExtensionSchema")),
            "{kinds:#?}"
        );
        assert!(
            !kinds.iter().any(|k| k.starts_with("AlterColumnType")),
            "the column is carried, never retyped: {kinds:#?}"
        );
        succeeds(f.apply("rename,extension-code,destructive"));
        assert_eq!(
            scalar(
                f.target(),
                "SELECT oid::int8 FROM pg_type WHERE oid = 'moved.citext'::regtype"
            ),
            before,
            "the type keeps its OID through the move"
        );
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        succeeds(f.d.run(&["verify", "--db", f.target()]));
    });
}

/// The move of a trusted package on a confined scratch is refused by the
/// rehearsal, by name: the members belong to the bootstrap superuser and the
/// scratch's role cannot move them, so its success on a superuser is no proof
/// for it (measured on 16.15 and 18.6, DEC-2447.11).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_move_the_confined_scratch_cannot_perform_is_refused_by_the_rehearsal() {
    servers(|server, slug| {
        let f = fixture(server, &format!("types2447-confined-{slug}"));
        adopted(&f, &["citext", "hstore", "ltree"]);
        on_server(f.target(), "CREATE SCHEMA moved");
        let declaration = f.d.dir.join("schema/extensions/citext.yml");
        let text = std::fs::read_to_string(&declaration).unwrap();
        std::fs::write(&declaration, text.replace("schema: ext", "schema: moved")).unwrap();
        let path = f.d.dir.join("schema/app.wants.yml");
        let table = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, table.replace("ext.citext", "moved.citext")).unwrap();
        f.d.commit();
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("must be owner of type"),
            "{}",
            said(&out)
        );
        assert_eq!(
            text_of(
                f.target(),
                "SELECT extnamespace::regnamespace::text FROM pg_extension WHERE extname = 'citext'"
            ),
            "ext"
        );
    });
}

/// A package the plan creates and a table of its type, in one plan: the create
/// first, the type taken as the package's after the rehearsal, a placeholder
/// pin, and an apply that brings both; a type the package does not list is
/// refused at plan.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_package_and_its_consumer_are_planned_together_and_the_type_is_proved_after_the_rehearsal() {
    servers(|server, slug| {
        let f = Fixture::new(
            server,
            &format!("types2447-create-{slug}"),
            Kind::Supplied,
            &Options::default(),
        );
        on_server(f.target(), "CREATE SCHEMA ext");
        // The resolver cannot describe a package the target does not hold yet,
        // so a plan that creates one takes no resolver-bound change beside it:
        // the project keeps no default, check, filtered index, view or routine.
        for file in ["app.v.view.yml", "app.f%28integer%29.function.yml"] {
            std::fs::remove_file(f.d.dir.join("schema").join(file)).unwrap();
        }
        std::fs::write(
            f.d.dir.join("schema/app.t.yml"),
            "table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  \
             amount: {type: numeric, nullable: false}\n  note: {type: text}\n\
             primary_key: {name: pk_t, columns: [id]}\nindexes:\n  ix:\n    columns: [note]\n",
        )
        .unwrap();
        let out = f.d.run(&["plan"]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        f.d.commit();
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        succeeds(f.apply("destructive,grant-widen"));
        let dir = f.d.dir.join("schema/extensions");
        std::fs::create_dir_all(&dir).unwrap();
        let version = text_of(
            f.target(),
            "SELECT default_version FROM pg_available_extensions WHERE name = 'citext'",
        );
        std::fs::write(
            dir.join("citext.yml"),
            format!("extension: citext\nschema: ext\nversion: \"{version}\"\n"),
        )
        .unwrap();
        std::fs::write(
            f.d.dir.join("schema/app.mail.yml"),
            "table: app.mail\ncolumns:\n  id: {type: integer, nullable: false}\n  \
             who: {type: ext.citext}\nprimary_key: {name: pk_mail, columns: [id]}\n",
        )
        .unwrap();
        succeeds(f.d.run(&["plan"]));
        f.d.commit();
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let plan = saved(&f);
        let at = |needle: &str| {
            plan.changes
                .changes
                .iter()
                .position(|c| format!("{:?}", c.change).starts_with(needle))
                .unwrap_or_else(|| panic!("`{needle}` is not in the plan"))
        };
        assert!(
            at("CreateExtension") < at("CreateTable"),
            "the package is first"
        );
        let pin = plan
            .type_pins
            .iter()
            .find(|p| p.name == "citext")
            .unwrap_or_else(|| panic!("{:#?}", plan.type_pins));
        assert_eq!(pin.kind, "planned");
        succeeds(f.apply("extension-code,destructive"));
        assert_eq!(
            scalar(
                f.target(),
                "SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.mail'::regclass \
                 AND attname = 'who' AND atttypid = 'ext.citext'::regtype"
            ),
            1
        );
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));

        // Negative: a column of a type the package does not list.
        std::fs::write(
            f.d.dir.join("schema/app.mail2.yml"),
            "table: app.mail2\ncolumns:\n  id: {type: integer, nullable: false}\n  \
             who: {type: ext.nothing}\nprimary_key: {name: pk_mail2, columns: [id]}\n",
        )
        .unwrap();
        f.d.commit();
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
    });
}

/// A retype to or from a provided type is refused by name, with the three
/// plans that do it.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_retype_to_or_from_a_provided_type_is_refused_by_name() {
    servers(|server, slug| {
        let f = fixture(server, &format!("types2447-retype-{slug}"));
        adopted(&f, &["citext", "hstore", "ltree"]);
        let path = f.d.dir.join("schema/app.wants.yml");
        let table = std::fs::read_to_string(&path).unwrap();
        // `c` was `ext.citext` and becomes `text`.
        let retyped = table.replacen("  c:\n    type: ext.citext\n", "  c:\n    type: text\n", 1);
        assert_ne!(retyped, table, "{table}");
        std::fs::write(&path, retyped).unwrap();
        minted(&f);
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("an extension provides"),
            "{}",
            said(&out)
        );
    });
}

/// What hangs off a table of an unmanaged package's type: a trigger, a view
/// and a foreign key from a table of plain types.
const HANGERS: &str = "CREATE TABLE app.plain (id integer PRIMARY KEY, n text); \
    CREATE FUNCTION app.touch() RETURNS trigger LANGUAGE plpgsql AS \
    $$ BEGIN RETURN NEW; END $$; \
    CREATE TRIGGER wants_touch BEFORE INSERT ON app.wants \
    FOR EACH ROW EXECUTE FUNCTION app.touch(); \
    CREATE VIEW app.wants_view AS SELECT id FROM app.wants; \
    CREATE TABLE app.refers (id integer PRIMARY KEY, \
    wid integer REFERENCES app.wants (id))";

/// `init --from` and `pull` select no package here, so a table of a type a
/// package provides is left out with the command that opts the package in,
/// and what hangs off it (a trigger) goes through the one cleanup of any
/// left-out table, so the adoption succeeds. A table of plain types is kept.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn init_from_and_pull_leave_a_table_of_an_unmanaged_packages_type_out_with_its_dependents() {
    servers(|server, slug| {
        let f = fixture(server, &format!("types2447-init-{slug}"));
        on_server(f.target(), &format!("{WANTS}; {HANGERS}"));
        let dir =
            std::env::temp_dir().join(format!("pbps-init-2447-{slug}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let var = format!("PBPS_INIT_2447_{slug}");
        let out = std::process::Command::new(super::BIN)
            .env(&var, f.target())
            .arg("--project")
            .arg(&dir)
            .args(["init", "--from", "source", "--url-env", &var])
            .args(["--dialect", "postgres"])
            .output()
            .unwrap();
        let init = said(&out);
        assert_eq!(code(&out), 0, "{init}");
        let pulled = succeeds(f.d.run(&["pull", "--force", "--db", f.target()]));
        for (how, said) in [("init", init), ("pull", said(&pulled))] {
            assert!(
                said.contains("`app.wants` was left out")
                    && said.contains("pbps pull --extension citext"),
                "{how}: {said}"
            );
            assert!(
                said.contains("wants_touch"),
                "{how} names the trigger: {said}"
            );
        }
        for root in [&dir, &f.d.dir] {
            assert!(!root.join("schema/app.wants.yml").exists());
            assert!(root.join("schema/app.plain.yml").exists());
            let validate = std::process::Command::new(super::BIN)
                .arg("--project")
                .arg(root)
                .arg("validate")
                .output()
                .unwrap();
            assert_eq!(code(&validate), 0, "{}", said(&validate));
        }
        let _ = std::fs::remove_dir_all(&dir);
    });
}
