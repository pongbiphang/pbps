//! An operator class a package provides, named by an index key: read by the pull
//! when the package is managed, left out with the command that opts it in when it
//! is not, planned against a pinned provider, carried through a move of the
//! package without a rebuild, and listed when the package is dropped (#2447,
//! DEC-2447.14). Every case runs on both pinned engines, against the packages the
//! images ship (`pg_trgm`, `hstore`), on a confined scratch of the target's own
//! cluster; the move runs on the server scratch as E's does.

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
    CREATE EXTENSION pg_trgm SCHEMA ext; CREATE EXTENSION hstore SCHEMA ext";

/// A table of the target's own, before anything is pulled, with the classes the
/// cases name.
const DOCS: &str = "CREATE TABLE app.docs (id integer PRIMARY KEY, body text, n integer, \
    h ext.hstore)";

/// A class that only shares a package's schema and shape: no `e` edge to it.
const LOOKALIKE: &str = "CREATE OPERATOR CLASS ext.fake_trgm_ops FOR TYPE text USING gin AS \
    OPERATOR 1 ext.% (text, text), FUNCTION 1 btint4cmp(int4, int4), \
    FUNCTION 2 ext.gin_extract_value_trgm(text, internal), \
    FUNCTION 3 ext.gin_extract_query_trgm(text, internal, int2, internal, internal, internal, internal), \
    FUNCTION 4 ext.gin_trgm_consistent(internal, int2, text, int4, internal, internal, internal, internal), \
    FUNCTION 6 ext.gin_trgm_triconsistent(internal, int2, text, int4, internal, internal, internal)";

fn fixture_on(server: &str, slug: &str, kind: Kind, externals: &'static str) -> Fixture {
    Fixture::new(
        server,
        slug,
        kind,
        &Options {
            externals: Some(externals),
            ..Options::default()
        },
    )
}

fn fixture(server: &str, slug: &str) -> Fixture {
    fixture_on(server, slug, Kind::Supplied, PACKAGES)
}

fn pulled(f: &Fixture, packages: &[&str]) -> Output {
    let mut args = vec!["pull", "--force", "--db", f.target()];
    for name in packages {
        args.extend(["--extension", name]);
    }
    succeeds(f.d.run(&args))
}

/// The packages named, pulled, written and recorded the way an operator adopts
/// them, with `app.docs` on the target.
fn adopted(f: &Fixture, setup: &str, packages: &[&str]) {
    on_server(f.target(), setup);
    pulled(f, packages);
    f.d.commit();
    succeeds(f.d.run(&[
        "baseline",
        "--db",
        f.target(),
        "--reason",
        "adopt the packages",
    ]));
}

fn minted(f: &Fixture) {
    succeeds(f.d.run(&["plan"]));
    f.d.commit();
}

fn table_path(f: &Fixture) -> std::path::PathBuf {
    f.d.dir.join("schema/app.docs.yml")
}

fn table_file(f: &Fixture) -> String {
    std::fs::read_to_string(table_path(f)).unwrap_or_default()
}

fn saved(f: &Fixture) -> pbps_model::SavedPlan {
    serde_json::from_slice(&std::fs::read(f.artifact()).unwrap()).unwrap()
}

fn no_changes(out: &Output) {
    assert_eq!(code(out), 0, "{}", said(out));
    assert!(stdout(out).contains("No changes"), "{}", said(out));
}

/// The table as pulled, with one more index declared.
/// The index the apply test declares first.
const KEPT: &str = "  docs_trgm:\n    method: gin\n    keys:\n      - {column: body, opclass: ext.gin_trgm_ops}\n  \
    docs_expr:\n    method: gin\n    keys:\n      - {expression: \"lower(body)\", opclass: ext.gin_trgm_ops}\n";

fn with_index(base: &str, index: &str) -> String {
    format!("{}indexes:\n{index}", base.trim_end().to_owned() + "\n")
}

/// With its package managed, an index on a provider's class is pulled as written,
/// baselined and planned as no change; with the package not managed it is left
/// out and the command is named; a lookalike, and a class with parameters, are
/// left out whatever the package.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_provider_class_is_read_when_its_package_is_managed_and_left_out_by_name_when_not() {
    servers(|server, slug| {
        let f = fixture(server, &format!("classes2447-pull-{slug}"));
        adopted(
            &f,
            &format!(
                "{DOCS}; CREATE INDEX docs_trgm ON app.docs USING gin (body ext.gin_trgm_ops); \
                 CREATE INDEX docs_fuzzy ON app.docs USING gist (lower(body) ext.gist_trgm_ops); \
                 CREATE INDEX docs_expr ON app.docs USING gin (lower(body) ext.gin_trgm_ops)"
            ),
            &["pg_trgm", "hstore"],
        );
        let table = table_file(&f);
        // The GIN expression key is read back, so a declaration of it is a no-op plan.
        assert!(
            table.contains("docs_expr") && table.contains("lower(body)"),
            "{table}"
        );
        for held in ["ext.gin_trgm_ops", "ext.gist_trgm_ops"] {
            assert!(table.contains(held), "{held}:\n{table}");
        }
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        succeeds(f.d.run(&["verify", "--db", f.target()]));

        // Negatives: a class nobody owns, and a class with parameters, are named.
        let g = fixture(server, &format!("classes2447-left-{slug}"));
        on_server(
            g.target(),
            &format!(
                "{DOCS}; {LOOKALIKE}; \
                 CREATE INDEX docs_fake ON app.docs USING gin (body ext.fake_trgm_ops); \
                 CREATE INDEX docs_param ON app.docs USING gist (body ext.gist_trgm_ops (siglen=32)); \
                 CREATE INDEX docs_trgm ON app.docs USING gin (body ext.gin_trgm_ops); \
                 CREATE INDEX docs_plain_expr ON app.docs USING gin ((body::jsonb))"
            ),
        );
        let out = pulled(&g, &["pg_trgm", "hstore"]);
        let notes = said(&out);
        assert!(
            notes.contains("docs_fake") && notes.contains("which no package owns"),
            "{notes}"
        );
        assert!(
            notes.contains("docs_param") && notes.contains("operator class parameters"),
            "{notes}"
        );
        let table = table_file(&g);
        assert!(
            table.contains("docs_trgm") && table.contains("ext.gin_trgm_ops"),
            "{table}"
        );
        assert!(
            !table.contains("docs_fake") && !table.contains("docs_param"),
            "{table}"
        );

        // Negative: with only `hstore` managed the class's package is not, and the
        // command that opts it in is named.
        let h = fixture(server, &format!("classes2447-unmanaged-{slug}"));
        on_server(
            h.target(),
            &format!(
                "{DOCS}; CREATE INDEX docs_trgm ON app.docs USING gin (body ext.gin_trgm_ops)"
            ),
        );
        let notes = said(&pulled(&h, &["hstore"]));
        assert!(
            notes.contains("docs_trgm") && notes.contains("pbps pull --extension pg_trgm"),
            "{notes}"
        );
        assert!(!table_file(&h).contains("docs_trgm"));
    });
}

/// A new index on a provider's class is planned against a pinned provider,
/// applied, read back and planned again as no change; the pin names the package,
/// and the apply refuses when the class's family gains a member after the plan.
/// Each declaration the engine or the model would refuse is refused by name at
/// plan.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_new_index_on_a_provider_class_is_pinned_applied_and_refused_when_the_class_changes() {
    servers(|server, slug| {
        let f = fixture(server, &format!("classes2447-add-{slug}"));
        adopted(&f, DOCS, &["pg_trgm", "hstore"]);
        let base = table_file(&f);
        let declare = |index: &str| {
            std::fs::write(table_path(&f), with_index(&base, index)).unwrap();
        };
        declare(KEPT);
        minted(&f);
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let plan = saved(&f);
        assert_eq!(plan.version, pbps_model::plan::CURRENT_VERSION);
        let pin = plan
            .provider_pins
            .iter()
            .find(|p| p.name == "gin_trgm_ops")
            .unwrap_or_else(|| panic!("{:#?}", plan.provider_pins));
        assert_eq!(pin.kind, pbps_model::ProviderKind::Class);
        assert_eq!((pin.schema.as_str(), pin.method.as_str()), ("ext", "gin"));
        assert!(!pin.planned);
        assert_eq!(
            pin.provider
                .as_ref()
                .expect("the provider is pinned")
                .extension,
            "pg_trgm"
        );

        // The family gains a member after the plan: the digest holds it.
        on_server(
            f.target(),
            "ALTER OPERATOR FAMILY ext.gin_trgm_ops USING gin ADD OPERATOR 20 ext.<% (text, text)",
        );
        let refused = f.apply("destructive");
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("changed since planning"),
            "{}",
            said(&refused)
        );
        on_server(
            f.target(),
            "ALTER OPERATOR FAMILY ext.gin_trgm_ops USING gin DROP OPERATOR 20 (text, text)",
        );

        succeeds(f.plan(&[]));
        succeeds(f.apply("destructive"));
        // Both the column key and the expression key are built on the class, and the
        // expression reads back byte for byte: the next plan is no change.
        assert_eq!(
            scalar(
                f.target(),
                "SELECT count(*) FROM pg_index i JOIN pg_opclass oc ON oc.oid = i.indclass[0] \
                 WHERE i.indexrelid IN ('app.docs_trgm'::regclass, 'app.docs_expr'::regclass) \
                 AND oc.opcname = 'gin_trgm_ops'"
            ),
            2
        );
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        succeeds(f.d.run(&["verify", "--db", f.target()]));

        // Negatives, each refused at plan and by name, writing nothing.
        on_server(f.target(), LOOKALIKE);
        for (what, index, expected) in [
            (
                "a lookalike",
                "  docs_x:\n    method: gin\n    keys:\n      - {column: body, opclass: ext.fake_trgm_ops}\n",
                "not a member of any extension",
            ),
            (
                "a class for another type",
                "  docs_x:\n    method: gin\n    keys:\n      - {column: n, opclass: ext.gin_trgm_ops}\n",
                "is for `text`",
            ),
            (
                "the default class spelled out",
                "  docs_x:\n    method: gin\n    keys:\n      - {column: h, opclass: ext.gin_hstore_ops}\n",
                "leave the class out",
            ),
            (
                "a class under a method it is not for",
                "  docs_x:\n    method: gist\n    keys:\n      - {column: body, opclass: ext.gin_trgm_ops}\n",
                "gin_trgm_ops",
            ),
            (
                "a GIN expression key under no class",
                "  docs_x:\n    method: gin\n    keys:\n      - {expression: \"lower(body)\"}\n",
                "over the expression",
            ),
            (
                "a class of a schema no declared package stands in",
                "  docs_x:\n    method: gin\n    keys:\n      - {column: body, opclass: other.gin_trgm_ops}\n",
                "declares no extension in schema",
            ),
        ] {
            // The applied index stays declared, so only the new one is in question.
            std::fs::write(table_path(&f), with_index(&base, &format!("{KEPT}{index}"))).unwrap();
            let out = f.plan(&[]);
            assert_ne!(code(&out), 0, "{what}: {}", said(&out));
            assert!(said(&out).contains(expected), "{what}: {}", said(&out));
        }
    });
}

/// A package is not updated in the plan that uses its class: the class's digest
/// would move under the plan that approved it. The update goes in a plan of its
/// own, which is not refused.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_package_update_beside_a_use_of_its_class_is_refused_by_name() {
    servers(|server, slug| {
        // The oldest version the image offers, so that an update has somewhere to go.
        let f = fixture_on(
            server,
            &format!("classes2447-update-{slug}"),
            Kind::Supplied,
            "CREATE SCHEMA IF NOT EXISTS ext; \
             DO $$ DECLARE v text; BEGIN \
               SELECT version INTO v FROM pg_available_extension_versions \
                WHERE name = 'pg_trgm' AND version <> (SELECT default_version \
                  FROM pg_available_extensions WHERE name = 'pg_trgm') \
                ORDER BY version LIMIT 1; \
               EXECUTE format('CREATE EXTENSION pg_trgm VERSION %L SCHEMA ext', v); \
             END $$",
        );
        adopted(
            &f,
            DOCS.replace(", h ext.hstore", "").as_str(),
            &["pg_trgm"],
        );
        let base = table_file(&f);
        let declaration = f.d.dir.join("schema/extensions/pg_trgm.yml");
        let text = std::fs::read_to_string(&declaration).unwrap();
        let current = text_of(
            f.target(),
            "SELECT extversion FROM pg_extension WHERE extname = 'pg_trgm'",
        );
        let newest = text_of(
            f.target(),
            "SELECT default_version FROM pg_available_extensions WHERE name = 'pg_trgm'",
        );
        assert_ne!(current, newest, "the image offers an update");
        std::fs::write(&declaration, text.replace(&current, &newest)).unwrap();
        std::fs::write(
            table_path(&f),
            with_index(
                &base,
                "  docs_trgm:\n    method: gin\n    keys:\n      - {column: body, opclass: ext.gin_trgm_ops}\n",
            ),
        )
        .unwrap();
        f.d.commit();
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        // Refused by name, and naming the index. The version check fires first in the
        // plan's own order (the project records a version the target does not hold); the
        // rule that a package is not updated beside a use of its class is the kernel's and is
        // proved by its unit test, because this path never reaches it.
        let text = said(&out);
        assert!(
            text.contains("bring them together before planning")
                && text.contains("gin_trgm_ops")
                && text.contains("docs_trgm"),
            "{text}"
        );
        // Negative: the same update without the use is planned.
        std::fs::write(table_path(&f), &base).unwrap();
        f.d.commit();
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
    });
}

/// A move of the package carries the class: it keeps its OID and the index keeps
/// its `indclass`, so the index is not rebuilt, and the declarations that spell
/// the schema are re-planned as the text changes they are.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_move_of_the_package_carries_its_class_without_rebuilding_the_index() {
    servers(|server, slug| {
        // The rehearsal of a move runs as the scratch's role, which is a
        // superuser on the server scratch only (PostgreSQL 18 here), as E's.
        if slug != "PBPS_TEST_PG_DB" {
            return;
        }
        let f = fixture_on(
            server,
            &format!("classes2447-move-{slug}"),
            Kind::Server,
            PACKAGES,
        );
        adopted(
            &f,
            &format!(
                "{DOCS}; CREATE INDEX docs_trgm ON app.docs USING gin (body ext.gin_trgm_ops)"
            ),
            &["pg_trgm", "hstore"],
        );
        let index_oid = scalar(f.target(), "SELECT 'app.docs_trgm'::regclass::oid::int8");
        let class_oid = scalar(
            f.target(),
            "SELECT oc.oid::int8 FROM pg_opclass oc WHERE oc.opcname = 'gin_trgm_ops'",
        );
        on_server(f.target(), "CREATE SCHEMA moved");
        let text = table_file(&f).replace("ext.gin_trgm_ops", "moved.gin_trgm_ops");
        std::fs::write(table_path(&f), text).unwrap();
        let declaration = f.d.dir.join("schema/extensions/pg_trgm.yml");
        let text = std::fs::read_to_string(&declaration).unwrap();
        std::fs::write(&declaration, text.replace("schema: ext", "schema: moved")).unwrap();
        f.d.commit();
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let kinds: Vec<String> = saved(&f)
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
            !kinds
                .iter()
                .any(|k| k.starts_with("DropIndex") || k.starts_with("AddIndex")),
            "the index is carried, never rebuilt: {kinds:#?}"
        );
        succeeds(f.apply("rename,extension-code,destructive"));
        assert_eq!(
            scalar(f.target(), "SELECT 'app.docs_trgm'::regclass::oid::int8"),
            index_oid
        );
        assert_eq!(
            scalar(
                f.target(),
                "SELECT oc.oid::int8 FROM pg_opclass oc JOIN pg_namespace n ON n.oid = oc.opcnamespace \
                 WHERE oc.opcname = 'gin_trgm_ops' AND n.nspname = 'moved'"
            ),
            class_oid,
            "the class keeps its OID through the move"
        );
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        succeeds(f.d.run(&["verify", "--db", f.target()]));
    });
}

/// A package whose class an index uses is not dropped while the index stays
/// declared, and the refusal names the index; dropping the index in the same plan
/// removes it first.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn dropping_a_package_lists_the_index_on_its_class_and_removes_it_first() {
    servers(|server, slug| {
        let f = fixture(server, &format!("classes2447-drop-{slug}"));
        adopted(&f, DOCS, &["pg_trgm", "hstore"]);
        let base = table_file(&f);
        std::fs::write(table_path(&f), with_index(&base, KEPT)).unwrap();
        minted(&f);
        succeeds(f.plan(&[]));
        succeeds(f.apply("destructive"));
        std::fs::remove_file(f.d.dir.join("schema/extensions/pg_trgm.yml")).unwrap();
        succeeds(f.d.run(&[
            "drop-extension",
            "pg_trgm",
            "--reason",
            "not needed any more",
        ]));
        f.d.commit();
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("docs_trgm")
                && said(&out).contains("docs_expr")
                && said(&out).contains("pg_trgm"),
            "{}",
            said(&out)
        );
        // The declaration no longer holds either index (the table file is back to its base,
        // which has none), so the same plan removes both, and the package after them.
        std::fs::write(table_path(&f), &base).unwrap();
        f.d.commit();
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let kinds: Vec<String> = saved(&f)
            .changes
            .changes
            .iter()
            .map(|c| format!("{:?}", c.change))
            .collect();
        let at = |needle: &str| {
            kinds
                .iter()
                .position(|k| k.starts_with("DropIndex") && k.contains(needle))
                .unwrap_or_else(|| panic!("no DropIndex of {needle} in {kinds:#?}"))
        };
        let package = kinds
            .iter()
            .position(|k| k.starts_with("DropExtension"))
            .unwrap_or_else(|| panic!("no DropExtension in {kinds:#?}"));
        assert!(
            at("docs_trgm") < package && at("docs_expr") < package,
            "both indexes go before the package: {kinds:#?}"
        );
        succeeds(f.apply("destructive"));
        assert_eq!(
            scalar(
                f.target(),
                "SELECT count(*) FROM pg_extension WHERE extname = 'pg_trgm'"
            ),
            0
        );
        assert_eq!(
            scalar(
                f.target(),
                "SELECT count(*) FROM pg_class WHERE relname = 'docs_trgm'"
            ),
            0
        );
    });
}
