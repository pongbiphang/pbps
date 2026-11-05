//! Live acceptance of the admission predicate at the expressions a plan gives a
//! table: CHECK, DEFAULT and generated columns (#2563, DEC-2563.4), and at the
//! index keys and filters, policies and created domains beside them (DEC-2563.5).
use super::*;

const BASE: &str = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  email: {type: text}\nprimary_key: {name: pk_t, columns: [id]}\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Check,
    Default,
    Generated,
    Create,
    IndexKey,
    IndexFilter,
    Policy,
    Domain,
}
const KINDS: [Kind; 8] = [
    Kind::Check,
    Kind::Default,
    Kind::Generated,
    Kind::Create,
    Kind::IndexKey,
    Kind::IndexFilter,
    Kind::Policy,
    Kind::Domain,
];

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
            Kind::IndexKey => d.table(&format!(
                "{BASE}indexes:\n  ix:\n    keys:\n      - expression: 'app.ok(email)'\n"
            )),
            Kind::IndexFilter => d.table(&format!(
                "{BASE}indexes:\n  ix:\n    columns: [id]\n    where: 'app.ok(email)'\n"
            )),
            Kind::Policy => d.table(&format!(
                "{BASE}row_policies:\n  p: {{using: 'app.ok(email)', with_check: 'app.ok(email)'}}\n"
            )),
            Kind::Domain => std::fs::write(
                d.dir.join("schema/app.d.yml"),
                "domain: app.d\ntype: text\nchecks: {c: 'app.ok(VALUE)'}\n",
            )
            .unwrap(),
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
                Kind::IndexKey | Kind::IndexFilter => {
                    "SELECT count(*) FROM pg_class WHERE oid = to_regclass('app.ix')"
                }
                Kind::Policy => {
                    "SELECT count(*) FROM pg_policy WHERE polname = 'p' AND polrelid = 'app.t'::regclass"
                }
                Kind::Domain => "SELECT count(*) FROM pg_type WHERE oid = to_regtype('app.d')",
            },
        ) == 1
    }

    /// The routine the declaration calls, and the line that fixes it.
    fn function(self) -> &'static str {
        match self {
            Kind::Default => "app.mk()",
            Kind::Check
            | Kind::Generated
            | Kind::Create
            | Kind::IndexKey
            | Kind::IndexFilter
            | Kind::Policy
            | Kind::Domain => "app.ok(pg_catalog.text)",
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
        matches!(
            self,
            Kind::Check | Kind::Generated | Kind::IndexKey | Kind::IndexFilter
        )
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
            Kind::Check
            | Kind::Generated
            | Kind::Create
            | Kind::IndexKey
            | Kind::IndexFilter
            | Kind::Policy
            | Kind::Domain => format!(
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
            &["--allow", "destructive,narrowing,constraint,row-security"],
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
            if kind == Kind::Domain {
                // A domain is created before the plan's routines (ORDERING.md,
                // class 7), so the one it calls does not exist yet: judged at
                // its statement, it cannot be bound, and the apply stops there
                // exactly as the engine would stop at the statement itself.
                assert_ne!(code(&applied), 0, "PG{version} {kind:?}: {shown}");
                assert!(
                    shown.contains("could not be bound just before they run"),
                    "PG{version} {kind:?}: {shown}"
                );
                assert!(!kind.applied(&c.connection), "PG{version} {kind:?}");
            } else if path {
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

/// The binding table copies the table's own columns, so a column that already
/// has the name a helper would take must not make a valid change fail to bind
/// (review of #2686). Columns named for the placeholder and for the names the
/// search falls back to are in the table; an unrelated DEFAULT and generated
/// expression change on it, and the plan applies.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_column_named_like_a_binder_helper_does_not_refuse_an_unrelated_expression_change() {
    const T: &str = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  pbps_bound: {type: integer}\n  pbps_bound_1: {type: integer}\n  a: {type: integer, default: '1'}\nGENprimary_key: {name: pk_t, columns: [id]}\n";
    const G: &str = "  g: {type: integer, generated: {expression: 'a + 1', stored: true}}\n";
    for (version, url) in [(16, old_server()), (18, server())] {
        let slug = format!("tx2563-helper-{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection().to_owned();
        // Before 17 a generated expression cannot be changed in place, so the
        // generated column is only here where it can be.
        let generated = version >= 17;
        let from = T.replace("GEN", if generated { G } else { "" });
        let d = bootstrapped_demo(&connection, &slug, &from);
        d.keyed(&connection);
        on_server(&connection, "INSERT INTO app.t (id, a) VALUES (1, 5)");
        d.table(
            &from
                .replace("default: '1'", "default: '2'")
                .replace("a + 1", "a + 2"),
        );
        succeeds(d.run(&["plan"]));
        d.commit();
        let plan = d.dir.join("helper-plan.json");
        let planned = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
        assert_eq!(code(&planned), 0, "PG{version}: {}", text(&planned));
        let applied = approved_apply(
            &d,
            &connection,
            &plan,
            &["--allow", "destructive,narrowing,constraint"],
        );
        assert_eq!(code(&applied), 0, "PG{version}: {}", text(&applied));
        assert_eq!(
            text_of(
                &connection,
                "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef d JOIN pg_attribute a \
                 ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
                 WHERE d.adrelid = 'app.t'::regclass AND a.attname = 'a'"
            ),
            "2",
            "PG{version}"
        );
        if generated {
            assert_eq!(
                text_of(&connection, "SELECT g::text FROM app.t WHERE id = 1"),
                "7",
                "PG{version}"
            );
        }
    }
}

/// A pre-flight probe evaluates the new CHECK over the stored rows, so the
/// routine it calls must have been judged first. When the expression cannot be
/// bound before its statement (here an event trigger refuses the binding
/// table, as a role without TEMP would be refused), "no refusal" is not
/// "admitted": the probe must not run it (review of #2686, round 3).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_check_that_cannot_be_bound_is_not_evaluated_by_the_preflight_probe() {
    for (version, url) in [(16, old_server()), (18, server())] {
        let slug = format!("tx2563-unbound-{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection().to_owned();
        let d = bootstrapped_demo(&connection, &slug, BASE);
        d.keyed(&connection);
        // The probes run in a read-only transaction that is rolled back, so
        // the only trace of a run is the error the routine raises, which the
        // pre-flight reports as why it could not check the rows.
        on_server(
            &connection,
            "CREATE FUNCTION app.ok(x text) RETURNS boolean LANGUAGE plpgsql \
               SET search_path = pg_catalog AS $$ BEGIN \
               RAISE EXCEPTION 'the probe ran the routine'; END $$; \
             INSERT INTO app.t VALUES (1, 'a')",
        );
        Kind::Check.declare(&d);
        on_server(
            &connection,
            "CREATE FUNCTION public.refuse_binding() RETURNS event_trigger LANGUAGE plpgsql \
               SET search_path = pg_catalog AS $$ BEGIN \
               IF pg_catalog.current_query() LIKE '%pbps_table_expressions%' THEN \
                 RAISE EXCEPTION 'the binding table is refused'; END IF; END $$; \
             CREATE EVENT TRIGGER refuse_binding ON ddl_command_start \
               WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION public.refuse_binding()",
        );
        succeeds(d.run(&["plan"]));
        d.commit();
        let plan = d.dir.join("unbound-plan.json");
        let planned = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
        assert_eq!(code(&planned), 0, "PG{version}: {}", text(&planned));
        let applied = approved_apply(
            &d,
            &connection,
            &plan,
            &["--allow", "destructive,narrowing,constraint"],
        );
        // The statement-time check cannot bind it either, so the apply stops.
        assert_ne!(code(&applied), 0, "PG{version}: {}", text(&applied));
        let shown = text(&applied);
        assert!(
            !shown.contains("the probe ran the routine"),
            "PG{version}: the probe ran the routine: {shown}"
        );
        assert!(
            shown.contains("unchecked until source review"),
            "PG{version}: {shown}"
        );
    }
}

const NEW_SITES: [Kind; 4] = [
    Kind::IndexKey,
    Kind::IndexFilter,
    Kind::Policy,
    Kind::Domain,
];

/// An index, a policy or a domain whose code cannot be bound (an event trigger
/// refuses the binding objects, as a role short of TEMP would be refused) is
/// not admitted by that: the plan says it is judged at apply only, and the
/// statement that would run it is refused before it runs (DEC-2563.5).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_index_policy_or_domain_that_cannot_be_bound_is_refused_before_it_runs() {
    for (version, url) in [(16, old_server()), (18, server())] {
        for kind in NEW_SITES {
            let slug = format!("tx2563-nobind-{kind:?}-{version}").to_lowercase();
            let own = OwnDatabase::new(&url, &slug);
            let connection = own.connection().to_owned();
            let d = bootstrapped_demo(&connection, &slug, BASE);
            d.keyed(&connection);
            on_server(
                &connection,
                "CREATE FUNCTION app.ok(x text) RETURNS boolean LANGUAGE plpgsql IMMUTABLE \
                   SET search_path = pg_catalog AS $$ BEGIN \
                   RAISE EXCEPTION 'the routine ran'; END $$; \
                 INSERT INTO app.t VALUES (1, 'a')",
            );
            kind.declare(&d);
            on_server(
                &connection,
                "CREATE FUNCTION public.refuse_binding() RETURNS event_trigger LANGUAGE plpgsql \
                   SET search_path = pg_catalog AS $$ BEGIN \
                   IF pg_catalog.current_query() LIKE '%pbps_table_expressions%' \
                      OR pg_catalog.current_query() LIKE '%pbps_domain_expressions%' THEN \
                     RAISE EXCEPTION 'the binding object is refused'; END IF; END $$; \
                 CREATE EVENT TRIGGER refuse_binding ON ddl_command_start \
                   WHEN TAG IN ('CREATE TABLE', 'CREATE DOMAIN') \
                   EXECUTE FUNCTION public.refuse_binding()",
            );
            succeeds(d.run(&["plan"]));
            d.commit();
            let plan = d.dir.join("nobind-plan.json");
            let planned = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
            assert_eq!(
                code(&planned),
                0,
                "PG{version} {kind:?}: {}",
                text(&planned)
            );
            assert!(
                text(&planned).contains("judged at apply only"),
                "PG{version} {kind:?}: {}",
                text(&planned)
            );
            let applied = approved_apply(
                &d,
                &connection,
                &plan,
                &["--allow", "destructive,narrowing,constraint,row-security"],
            );
            let shown = text(&applied);
            assert_ne!(code(&applied), 0, "PG{version} {kind:?}: {shown}");
            assert!(
                shown.contains("could not be bound just before they run"),
                "PG{version} {kind:?}: {shown}"
            );
            assert!(
                !shown.contains("the routine ran"),
                "PG{version} {kind:?}: the routine ran: {shown}"
            );
            assert!(!kind.applied(&connection), "PG{version} {kind:?}");
        }
    }
}

/// Each expression is the one it was declared as: a text that closes its clause
/// and adds a key, a check or a constraint is refused as more than declared,
/// never judged as if it were the one expression.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_expression_that_closes_its_clause_is_refused_as_more_than_declared() {
    let cases: [(Kind, &str, &str); 3] = [
        (Kind::IndexKey, "app.ok(email)", "lower(email)), (id"),
        (
            Kind::Policy,
            "using: 'app.ok(email)', with_check: 'app.ok(email)'",
            "using: 'true) WITH CHECK (true'",
        ),
        (
            Kind::Domain,
            "app.ok(VALUE)",
            "VALUE IS NOT NULL) CONSTRAINT z CHECK (true",
        ),
    ];
    for (version, url) in [(16, old_server()), (18, server())] {
        for (kind, declared, injected) in cases {
            let c = cell(
                version,
                &url,
                kind,
                "inject",
                &Setup {
                    in_database: true,
                    path: true,
                    untrusted_owner: false,
                    pin: false,
                    body: "",
                    row: false,
                },
            );
            // Declare the injected text where the declared one stood.
            kind.declare(&c.d);
            let file = match kind {
                Kind::Domain => c.d.dir.join("schema/app.d.yml"),
                Kind::Check
                | Kind::Default
                | Kind::Generated
                | Kind::Create
                | Kind::IndexKey
                | Kind::IndexFilter
                | Kind::Policy => c.d.dir.join("schema/app.t.yml"),
            };
            let current = std::fs::read_to_string(&file).unwrap();
            assert!(
                current.contains(declared),
                "PG{version} {kind:?}: {current}"
            );
            std::fs::write(&file, current.replace(declared, injected)).unwrap();
            let planned = c.plan();
            let applied = if code(&planned) == 0 {
                c.apply()
            } else {
                planned
            };
            let shown = text(&applied);
            assert_ne!(code(&applied), 0, "PG{version} {kind:?}: {shown}");
            assert!(
                shown.contains("more than the expressions declared"),
                "PG{version} {kind:?}: {shown}"
            );
            assert!(!kind.applied(&c.connection), "PG{version} {kind:?}");
        }
    }
}

/// The duplicate count of a unique index under a filter evaluates the filter
/// over the stored rows, so the routine it calls must have been judged first.
/// A filter that cannot be bound is not admitted by that: the probe must not run
/// it (the index sibling of the CHECK test above, DEC-2563.5).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_filtered_unique_index_that_cannot_be_bound_is_not_evaluated_by_the_preflight_probe() {
    for (version, url) in [(16, old_server()), (18, server())] {
        let slug = format!("tx2563-uniqueunbound-{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection().to_owned();
        let d = bootstrapped_demo(&connection, &slug, BASE);
        d.keyed(&connection);
        on_server(
            &connection,
            "CREATE FUNCTION app.ok(x text) RETURNS boolean LANGUAGE plpgsql IMMUTABLE \
               SET search_path = pg_catalog AS $$ BEGIN \
               RAISE EXCEPTION 'the probe ran the routine'; END $$; \
             INSERT INTO app.t VALUES (1, 'a')",
        );
        d.table(&format!(
            "{BASE}indexes:\n  ix:\n    columns: [email]\n    unique: true\n    where: 'app.ok(email)'\n"
        ));
        on_server(
            &connection,
            "CREATE FUNCTION public.refuse_binding() RETURNS event_trigger LANGUAGE plpgsql \
               SET search_path = pg_catalog AS $$ BEGIN \
               IF pg_catalog.current_query() LIKE '%pbps_table_expressions%' THEN \
                 RAISE EXCEPTION 'the binding table is refused'; END IF; END $$; \
             CREATE EVENT TRIGGER refuse_binding ON ddl_command_start \
               WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION public.refuse_binding()",
        );
        succeeds(d.run(&["plan"]));
        d.commit();
        let plan = d.dir.join("unique-plan.json");
        let planned = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
        assert_eq!(code(&planned), 0, "PG{version}: {}", text(&planned));
        let applied = approved_apply(
            &d,
            &connection,
            &plan,
            &["--allow", "destructive,narrowing,constraint"],
        );
        let shown = text(&applied);
        assert_ne!(code(&applied), 0, "PG{version}: {shown}");
        assert!(
            !shown.contains("the probe ran the routine"),
            "PG{version}: the probe ran the routine: {shown}"
        );
        assert!(
            shown.contains("unchecked until source review"),
            "PG{version}: {shown}"
        );
    }
}
