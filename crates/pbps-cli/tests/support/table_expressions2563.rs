//! Live acceptance of the admission predicate at the expressions a plan gives a
//! table: CHECK, DEFAULT and generated columns (#2563, DEC-2563.4).
use super::*;

const BASE: &str = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  email: {type: text}\nprimary_key: {name: pk_t, columns: [id]}\n";

#[derive(Clone, Copy, Debug)]
enum Kind {
    Check,
    Default,
    Generated,
    Create,
}
const KINDS: [Kind; 4] = [Kind::Check, Kind::Default, Kind::Generated, Kind::Create];

impl Kind {
    /// Declares the table as it is after the change.
    fn declare(self, d: &Demo) {
        match self {
            Kind::Check => d.table(&format!("{BASE}checks:\n  ck: 'app.ok(email)'\n")),
            Kind::Default => d.table(&BASE.replace(
                "email: {type: text}",
                "email: {type: text, default: 'app.mk()'}",
            )),
            Kind::Generated => d.table(&BASE.replace(
                "email: {type: text}",
                "email: {type: text}\n  g: {type: boolean, generated: {expression: 'app.ok(email)', stored: true}}",
            )),
            Kind::Create => std::fs::write(
                d.dir.join("schema/app.u.yml"),
                "table: app.u\ncolumns:\n  id: {type: bigint, nullable: false}\n  label: {type: text}\nprimary_key: {name: pk_u, columns: [id]}\nchecks:\n  ck_u: 'app.ok(label)'\n",
            )
            .unwrap(),
        }
    }

    /// Whether the change is in the database.
    fn applied(self, connection: &str) -> bool {
        scalar(
            connection,
            match self {
                Kind::Check => {
                    "SELECT count(*) FROM pg_constraint WHERE conname = 'ck' AND conrelid = 'app.t'::regclass"
                }
                Kind::Default => {
                    "SELECT count(*) FROM pg_attrdef WHERE adrelid = 'app.t'::regclass"
                }
                Kind::Generated => {
                    "SELECT count(*) FROM information_schema.columns WHERE table_schema = 'app' AND table_name = 't' AND column_name = 'g'"
                }
                Kind::Create => "SELECT count(*) FROM pg_class WHERE oid = to_regclass('app.u')",
            },
        ) == 1
    }

    /// The routine the declaration calls, and the line that fixes it.
    fn function(self) -> &'static str {
        match self {
            Kind::Default => "app.mk()",
            Kind::Check | Kind::Generated | Kind::Create => "app.ok(pg_catalog.text)",
        }
    }
    fn fix(self) -> String {
        format!(
            "ALTER FUNCTION {} SET search_path = pg_catalog, pg_temp",
            self.function()
        )
    }

    /// Whether applying it over a stored row runs the routine.
    fn evaluates_over_a_row(self) -> bool {
        matches!(self, Kind::Check | Kind::Generated)
    }

    /// The same routine, declared in the project and so created by the plan.
    fn managed(self, path: bool) -> String {
        let set = if path {
            "SET search_path = pg_catalog "
        } else {
            ""
        };
        match self {
            Kind::Default => format!(
                "function: app.mk()\ndefinition: '() RETURNS text LANGUAGE plpgsql {set}AS $$BEGIN RETURN ''x''; END$$'\n"
            ),
            Kind::Check | Kind::Generated | Kind::Create => format!(
                "function: app.ok(text)\ndefinition: '(text) RETURNS boolean LANGUAGE plpgsql IMMUTABLE {set}AS $$BEGIN RETURN true; END$$'\n"
            ),
        }
    }
}

fn functions(path: bool, owner: Option<&str>, body: &str) -> String {
    let set = if path {
        "SET search_path = pg_catalog "
    } else {
        ""
    };
    let mut sql = format!(
        "CREATE FUNCTION app.ok(text) RETURNS boolean LANGUAGE plpgsql IMMUTABLE {set}AS $$BEGIN {body} RETURN true; END$$; \
         CREATE FUNCTION app.mk() RETURNS text LANGUAGE plpgsql {set}AS $$BEGIN {body} RETURN 'x'; END$$;"
    );
    if let Some(owner) = owner {
        sql.push_str(&format!(
            " GRANT USAGE ON SCHEMA app TO {owner}; ALTER FUNCTION app.ok(text) OWNER TO {owner}; \
             ALTER FUNCTION app.mk() OWNER TO {owner};"
        ));
    }
    sql
}

/// What a cell holds: the database, the project over it, and the plan's path.
struct Cell {
    _own: OwnDatabase,
    d: Demo,
    connection: String,
    plan: std::path::PathBuf,
}

struct Setup<'a> {
    /// The routine exists in the database (otherwise the plan creates it).
    in_database: bool,
    path: bool,
    untrusted_owner: bool,
    pin: bool,
    body: &'a str,
    row: bool,
}

fn cell(version: i32, url: &str, kind: Kind, tag: &str, setup: &Setup<'_>) -> Cell {
    let slug = format!("tx2563-{tag}-{kind:?}-{version}").to_lowercase();
    let own = OwnDatabase::new(url, &slug);
    let connection = own.connection().to_owned();
    let d = bootstrapped_demo(&connection, &slug, BASE);
    d.keyed(&connection);
    if setup.pin {
        let config = d.dir.join("pbps.yml");
        let current = std::fs::read_to_string(&config).unwrap();
        std::fs::write(&config, format!("{current}pin_type_routines: true\n")).unwrap();
    }
    let stranger = format!(
        "pbps_tx2563_{version}_{}_{tag}_{kind:?}",
        std::process::id()
    )
    .to_lowercase();
    if setup.untrusted_owner {
        on_server(url, &format!("CREATE ROLE {stranger}"));
    }
    if setup.in_database {
        on_server(
            &connection,
            &functions(
                setup.path,
                setup.untrusted_owner.then_some(stranger.as_str()),
                setup.body,
            ),
        );
    } else {
        std::fs::write(d.dir.join("schema/app.fn.yml"), kind.managed(setup.path)).unwrap();
    }
    if setup.row {
        on_server(&connection, "INSERT INTO app.t VALUES (1, 'a')");
    }
    kind.declare(&d);
    let plan = d.dir.join("tx-plan.json");
    Cell {
        _own: own,
        d,
        connection,
        plan,
    }
}

impl Cell {
    fn plan(&self) -> Output {
        succeeds(self.d.run(&["plan"]));
        self.d.commit();
        self.d.run(&[
            "plan",
            "--db",
            &self.connection,
            "--out",
            self.plan.to_str().unwrap(),
        ])
    }
    fn apply(&self) -> Output {
        approved_apply(
            &self.d,
            &self.connection,
            &self.plan,
            &["--allow", "destructive,narrowing,constraint"],
        )
    }
    fn saved(&self) -> pbps_model::SavedPlan {
        serde_json::from_slice(&std::fs::read(&self.plan).unwrap()).unwrap()
    }
}

fn text(out: &Output) -> String {
    format!("{}{}", stdout(out), stderr(out))
}

fn each(f: impl Fn(i32, &str, Kind)) {
    for (version, url) in [(16, old_server()), (18, server())] {
        for kind in KINDS {
            f(version, &url, kind);
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_trusted_owners_routine_with_a_fixed_path_is_admitted_in_a_table_expression() {
    each(|version, url, kind| {
        let c = cell(
            version,
            url,
            kind,
            "ok",
            &Setup {
                in_database: true,
                path: true,
                untrusted_owner: false,
                pin: false,
                body: "",
                row: false,
            },
        );
        let planned = c.plan();
        assert_eq!(
            code(&planned),
            0,
            "PG{version} {kind:?}: {}",
            text(&planned)
        );
        let applied = c.apply();
        assert_eq!(
            code(&applied),
            0,
            "PG{version} {kind:?}: {}",
            text(&applied)
        );
        assert!(kind.applied(&c.connection), "PG{version} {kind:?}");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_routine_without_a_fixed_path_is_refused_with_the_fix_before_its_statement() {
    each(|version, url, kind| {
        let c = cell(
            version,
            url,
            kind,
            "nopath",
            &Setup {
                in_database: true,
                path: false,
                untrusted_owner: false,
                pin: false,
                body: "",
                row: false,
            },
        );
        assert_eq!(
            code(&c.plan()),
            0,
            "PG{version} {kind:?}: the refusal waits for apply"
        );
        let applied = c.apply();
        let shown = text(&applied);
        assert_ne!(code(&applied), 0, "PG{version} {kind:?}: {shown}");
        assert!(shown.contains(&kind.fix()), "PG{version} {kind:?}: {shown}");
        assert!(
            shown.contains("nothing has been changed"),
            "PG{version} {kind:?}: {shown}"
        );
        assert!(
            !kind.applied(&c.connection),
            "PG{version} {kind:?}: nothing was applied"
        );
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_untrusted_owners_routine_is_refused_without_the_review() {
    each(|version, url, kind| {
        let c = cell(
            version,
            url,
            kind,
            "stranger",
            &Setup {
                in_database: true,
                path: true,
                untrusted_owner: true,
                pin: false,
                body: "",
                row: false,
            },
        );
        assert_eq!(code(&c.plan()), 0, "PG{version} {kind:?}");
        let applied = c.apply();
        let shown = text(&applied);
        assert_ne!(code(&applied), 0, "PG{version} {kind:?}: {shown}");
        for needle in [
            "`trusted_roles`",
            "`pin_type_routines: true`",
            "bring the routine into the project",
        ] {
            assert!(
                shown.contains(needle),
                "PG{version} {kind:?} {needle}: {shown}"
            );
        }
        assert!(!kind.applied(&c.connection), "PG{version} {kind:?}");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_reviewed_routine_is_shown_pinned_and_never_evaluated_at_plan() {
    each(|version, url, kind| {
        // The routine raises when it runs, and a row is there for it to run on.
        let raising = "RAISE EXCEPTION 'reviewed code ran' USING ERRCODE = 'PB001';";
        let c = cell(
            version,
            url,
            kind,
            "review",
            &Setup {
                in_database: true,
                path: true,
                untrusted_owner: true,
                pin: true,
                body: raising,
                row: true,
            },
        );
        let planned = c.plan();
        assert_eq!(
            code(&planned),
            0,
            "PG{version} {kind:?}: the plan evaluated it: {}",
            text(&planned)
        );
        let saved = c.saved();
        assert_eq!(saved.type_routines.len(), 1, "PG{version} {kind:?}");
        assert!(saved.type_routine_pins.is_some(), "PG{version} {kind:?}");
        // After the approval apply runs it, and a row that raises takes the plan back.
        let applied = c.apply();
        if kind.evaluates_over_a_row() {
            assert_ne!(
                code(&applied),
                0,
                "PG{version} {kind:?}: {}",
                text(&applied)
            );
            assert!(!kind.applied(&c.connection), "PG{version} {kind:?}");
        } else {
            assert_eq!(
                code(&applied),
                0,
                "PG{version} {kind:?}: {}",
                text(&applied)
            );
        }
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_reviewed_routine_that_is_harmless_applies_after_the_approval() {
    each(|version, url, kind| {
        let c = cell(
            version,
            url,
            kind,
            "reviewed",
            &Setup {
                in_database: true,
                path: true,
                untrusted_owner: true,
                pin: true,
                body: "",
                row: true,
            },
        );
        assert_eq!(code(&c.plan()), 0, "PG{version} {kind:?}");
        assert_eq!(c.saved().type_routines.len(), 1, "PG{version} {kind:?}");
        let applied = c.apply();
        assert_eq!(
            code(&applied),
            0,
            "PG{version} {kind:?}: {}",
            text(&applied)
        );
        assert!(kind.applied(&c.connection), "PG{version} {kind:?}");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_routine_the_same_plan_creates_is_noted_at_plan_and_judged_at_apply() {
    each(|version, url, kind| {
        for path in [true, false] {
            let c = cell(
                version,
                url,
                kind,
                if path { "same" } else { "samenopath" },
                &Setup {
                    in_database: false,
                    path,
                    untrusted_owner: false,
                    pin: false,
                    body: "",
                    row: false,
                },
            );
            let planned = c.plan();
            assert_eq!(
                code(&planned),
                0,
                "PG{version} {kind:?} path={path}: {}",
                text(&planned)
            );
            assert!(
                text(&planned).contains("judged at apply only"),
                "PG{version} {kind:?} path={path}: {}",
                text(&planned)
            );
            let applied = c.apply();
            let shown = text(&applied);
            if path {
                assert_eq!(code(&applied), 0, "PG{version} {kind:?}: {shown}");
                assert!(kind.applied(&c.connection), "PG{version} {kind:?}");
            } else {
                assert_ne!(code(&applied), 0, "PG{version} {kind:?}: {shown}");
                assert!(
                    shown.contains("SET search_path = pg_catalog, pg_temp"),
                    "PG{version} {kind:?}: {shown}"
                );
                assert!(!kind.applied(&c.connection), "PG{version} {kind:?}");
            }
        }
    });
}
