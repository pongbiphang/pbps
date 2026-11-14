//! An exclusion constraint that excludes with an operator a package provides (#2448 PR C1b,
//! DEC-2447.14): pinned at plan like a class, refused when the operator is no package's or has
//! changed since the plan, and written `OPERATOR(schema.symbol)` with the operator's own
//! schema. Every case runs on both pinned engines, against `pg_trgm` (whose `%` is a
//! commutative boolean operator of its GiST class) on a confined scratch.

use super::rehearsal2379::{Fixture, Kind, Options};
use super::{Pg, code, on_server, scalar, stderr, stdout, succeeds};
use std::process::Output;

fn said(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

const PACKAGES: &str = "CREATE SCHEMA IF NOT EXISTS ext; CREATE EXTENSION pg_trgm SCHEMA ext";

fn fixture(server: &str, slug: &str) -> Fixture {
    Fixture::new(
        server,
        slug,
        Kind::Supplied,
        &Options {
            externals: Some(PACKAGES),
            ..Options::default()
        },
    )
}

/// The table as pulled, the package opted in and recorded the way an operator adopts it.
fn adopted(f: &Fixture) {
    on_server(
        f.target(),
        "CREATE TABLE app.docs (id integer PRIMARY KEY, body text)",
    );
    succeeds(f.d.run(&[
        "pull",
        "--force",
        "--db",
        f.target(),
        "--extension",
        "pg_trgm",
    ]));
    f.d.commit();
    succeeds(f.d.run(&[
        "baseline",
        "--db",
        f.target(),
        "--reason",
        "adopt the package",
    ]));
}

fn table_path(f: &Fixture) -> std::path::PathBuf {
    f.d.dir.join("schema/app.docs.yml")
}

/// The pulled table with one exclusion constraint declared on `body`.
fn with_exclusion(f: &Fixture, operator: &str) -> String {
    let base = std::fs::read_to_string(table_path(f)).unwrap();
    format!(
        "{}\nexclude:\n  no_similar:\n    method: gist\n    keys:\n      - {{column: body, \
         opclass: ext.gist_trgm_ops, operator: \"{operator}\"}}\n",
        base.trim_end()
    )
}

fn saved(f: &Fixture) -> pbps_model::SavedPlan {
    serde_json::from_slice(&std::fs::read(f.artifact()).unwrap()).unwrap()
}

fn constraints(f: &Fixture) -> i64 {
    scalar(
        f.target(),
        "SELECT count(*) FROM pg_constraint WHERE conname = 'no_similar' AND contype = 'x'",
    )
}

/// A new exclusion on a package's operator is pinned (the class and the operator), applied
/// behind the `constraint` risk and enforces; a change to the operator after the plan is
/// refused by name and nothing is written; the next plan is empty. Negatives: an operator no
/// package owns, though it stands in the package's schema, is refused at plan by name; the
/// engine's own operator needs no pin.
fn an_exclusion_on_a_package_operator_is_pinned_applied_and_refused_when_the_operator_changes(
    pg: Pg,
) {
    pg.run(|server, slug| {
        let f = fixture(server, &format!("exclude2448-operator-{slug}"));
        adopted(&f);
        std::fs::write(table_path(&f), with_exclusion(&f, "ext.%")).unwrap();
        f.d.commit();
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let plan = saved(&f);
        let operator = plan
            .provider_pins
            .iter()
            .find(|p| p.kind == pbps_model::ProviderKind::Operator)
            .unwrap_or_else(|| panic!("{:#?}", plan.provider_pins));
        assert_eq!(
            (operator.schema.as_str(), operator.name.as_str()),
            ("ext", "%")
        );
        assert!(!operator.planned);
        assert_eq!(operator.provider.as_ref().unwrap().extension, "pg_trgm");
        assert!(
            plan.provider_pins
                .iter()
                .any(|p| p.kind == pbps_model::ProviderKind::Class && p.name == "gist_trgm_ops"),
            "the key's class is pinned beside its operator"
        );
        assert!(
            plan.changes
                .changes
                .iter()
                .any(|c| matches!(c.change, pbps_model::Change::SetExclusion { .. }))
        );

        // The operator becomes a member of another family after the plan: the digest holds it.
        on_server(
            f.target(),
            "CREATE OPERATOR FAMILY ext.pbps_other USING gist; \
             ALTER OPERATOR FAMILY ext.pbps_other USING gist ADD OPERATOR 1 ext.% (text, text)",
        );
        let refused = f.apply("constraint");
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("changed since planning"),
            "{}",
            said(&refused)
        );
        assert_eq!(constraints(&f), 0, "a refused apply writes nothing");
        on_server(f.target(), "DROP OPERATOR FAMILY ext.pbps_other USING gist");

        succeeds(f.plan(&[]));
        succeeds(f.apply("constraint"));
        assert_eq!(constraints(&f), 1);
        // It enforces: a second similar row is refused by the engine with its own message.
        on_server(f.target(), "INSERT INTO app.docs VALUES (1, 'postgresql')");
        let clash =
            super::try_on_server(f.target(), "INSERT INTO app.docs VALUES (2, 'postgresql')");
        assert!(clash.is_err(), "the exclusion refuses a similar row");
        let next = succeeds(f.d.run(&["plan", "--db", f.target()]));
        assert!(stdout(&next).contains("No changes"), "{}", said(&next));
        succeeds(f.d.run(&["verify", "--db", f.target()]));

        // Negative: an operator that only shares the package's schema is no package's.
        let g = fixture(server, &format!("exclude2448-lookalike-{slug}"));
        adopted(&g);
        on_server(
            g.target(),
            "CREATE OPERATOR ext.### (LEFTARG = text, RIGHTARG = text, FUNCTION = texteq, \
             COMMUTATOR = OPERATOR(ext.###))",
        );
        std::fs::write(table_path(&g), with_exclusion(&g, "ext.###")).unwrap();
        g.d.commit();
        let out = g.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("not a member of any extension"),
            "{}",
            said(&out)
        );
        assert!(!g.artifact().exists(), "a refused plan writes no file");
    });
}

on_both_servers! {
    an_exclusion_on_a_package_operator_is_pinned_applied_and_refused_when_the_operator_changes => an_exclusion_on_a_package_operator_is_pinned_applied_and_refused_when_the_operator_changes,
}
