use super::{refusal::*, *};
use pbps_db::{DbError, DriverDiagnostic, ErrorPosition};
use pbps_model::{ObjectName, diagnostic::Declaration};

fn error(position: Option<ErrorPosition>) -> DbError {
    DbError::Driver {
        message: "prose names app.decoy; do not read it".into(),
        code: Some("42P01".into()),
        diagnostic: Some(Box::new(DriverDiagnostic {
            primary: "prose names app.decoy; do not read it".into(),
            schema: None,
            table: None,
            column: None,
            datatype: None,
            constraint: None,
            position,
        })),
    }
}
#[test]
fn unicode_positions_and_inert_controls_keep_the_caret_on_the_character() {
    let sql = "SELECT '漢é',\tapp.missing";
    let byte = sql.find("app.missing").unwrap();
    let p = sql[..byte].chars().count() as u32 + 1;
    assert_ne!(p as usize, byte + 1);
    let e = error(Some(ErrorPosition::Original(p)));
    let (text, at) = location(&e, sql, &[]).unwrap();
    assert_eq!(at, byte);
    let (line, width) = excerpt(text, at);
    assert_eq!(line, "SELECT '漢é',\\tapp.missing");
    assert_eq!(width, 15);
    assert_eq!(
        positioned_name(text, at),
        Some(ObjectName::new("app", "missing"))
    );
    assert!(location(&error(Some(ErrorPosition::Original(0))), sql, &[]).is_none());
    assert!(location(&error(Some(ErrorPosition::Original(999))), sql, &[]).is_none());
}
#[test]
fn internal_positions_require_an_owned_query_and_never_point_into_outer_sql() {
    let sql = "ALTER TABLE app.t ADD COLUMN x int DEFAULT app.hidden()";
    let query = "SELECT app.absent";
    let e = error(Some(ErrorPosition::Internal {
        position: 8,
        query: query.into(),
    }));
    assert!(location(&e, sql, &[]).is_none());
    let owned = vec![format!("() RETURNS int LANGUAGE sql AS $${query}$$")];
    let (text, byte) = location(&e, sql, &owned).unwrap();
    assert_eq!(text, query);
    assert_eq!(
        positioned_name(text, byte),
        Some(ObjectName::new("app", "absent"))
    );
    let debug = format!("{e:?}");
    assert!(!debug.contains(query));
    assert!(debug.contains("[redacted]"));
}
#[test]
fn names_come_from_the_position_and_unsupported_tokens_stay_unknown() {
    assert_eq!(
        positioned_name("'app.real'::regclass", 0),
        Some(ObjectName::new("app", "real"))
    );
    assert_eq!(
        positioned_name("\"App\".\"G\"(1)", 0),
        Some(ObjectName::new("App", "G"))
    );
    assert_eq!(
        positioned_name("APP.G(1)", 0),
        Some(ObjectName::new("app", "g"))
    );
    for text in ["%TYPE", "g(1)", "'app.g\\x'", "\"unterminated"] {
        assert!(positioned_name(text, 0).is_none(), "{text}");
    }
    assert!(location(&error(None), "SELECT app.real()", &[]).is_none());
    assert!(positioned_name("app.real", 1).is_none());
    assert!(positioned_name("'app.real''suffix'", 0).is_none());
}
fn refusal_fixture() -> Refusal {
    let owner = Declaration::Module("app.v".parse().unwrap());
    Refusal {
        version: 160015,
        committed: vec![],
        stalled: true,
        overtakes: vec![],
        pending: vec![Pending {
            declaration: "view app.v".into(),
            subject: trial_tests::module("app.v", ModuleKind::View, "SELECT 1"),
            owner: Some(owner),
            dependency: None,
            last: Some(Diagnosis {
                error: std::sync::Arc::new(error(None)),
                statement: 0,
                generation: 0,
                excerpt: None,
                reference: None,
                candidates: vec![],
                declared: vec![],
                created: vec![],
                overtaken: vec![],
            }),
            recovery: None,
            blocked: vec![],
            why: vec!["no incoming edge".into()],
        }],
    }
}

#[test]
fn a_missing_source_index_has_no_fictional_path_or_unsupported_fix() {
    let owner = Declaration::Module("app.v".parse().unwrap());
    let refusal = refusal_fixture();
    let rendered = refusal.to_string();
    assert!(rendered.contains("source file unavailable"));
    assert!(rendered.contains("42P01 prose names app.decoy; do not read it"));
    assert!(!rendered.contains("positioned name"));
    assert!(!rendered.contains("fix:"));
    assert!(!rendered.contains(".yaml"));
    assert!(
        refusal
            .render(|d| (d == &owner).then(|| "actual/unusual.yml".into()))
            .contains("actual/unusual.yml")
    );
}

#[test]
fn hard_blocked_steps_refer_to_the_predecessor_without_cycle_advice() {
    let mut report = refusal_fixture();
    let mut blocked = report.pending[0].clone();
    blocked.declaration = "view app.blocked".into();
    blocked.subject = trial_tests::module("app.blocked", ModuleKind::View, "SELECT 1");
    blocked.owner = Some(Declaration::Module("app.blocked".parse().unwrap()));
    blocked.last = None;
    blocked.blocked = vec!["view app.v".into()];
    report.pending.push(blocked);
    let rendered = report.to_string();
    let (refused, blocked) = rendered
        .split_once("scratch could not create view app.blocked")
        .unwrap();
    assert!(refused.contains("change the declaration: remove a creation-time dependency"));
    assert!(!refused.contains("never tried"));
    assert!(blocked.contains("not attempted: waiting for hard predecessor view app.v"));
    assert!(blocked.contains(
        "resolve the refusal shown above for view app.v; this declaration was never tried"
    ));
    assert!(!blocked.contains("change the declaration:"));
    assert!(!blocked.contains("cycle"));
    assert!(!blocked.contains("late-bound"));

    // An immediate engine refusal has no non-progressing set to diagnose.
    report.pending.truncate(1);
    report.stalled = false;
    let immediate = report.to_string();
    assert!(immediate.contains("correct the engine error shown above before retrying"));
    assert!(!immediate.contains("cycle"));
    assert!(!immediate.contains("never tried"));
}

#[test]
fn created_context_labels_committed_and_rolled_back_successes_separately() {
    for (committed, local, expected) in [
        (
            vec![],
            vec!["view app.created"],
            "committed earlier ranges: none; earlier in this range (rolled back): view app.created",
        ),
        (
            vec!["type app.mood"],
            vec![],
            "committed earlier ranges: type app.mood; earlier in this range (rolled back): none",
        ),
        (
            vec!["type app.mood"],
            vec!["view app.created"],
            "committed earlier ranges: type app.mood; earlier in this range (rolled back): view app.created",
        ),
    ] {
        let mut report = refusal_fixture();
        report.committed = committed.into_iter().map(str::to_owned).collect();
        report.pending[0].last.as_mut().unwrap().created =
            local.into_iter().map(str::to_owned).collect();
        let rendered = report.to_string();
        let line = rendered
            .lines()
            .find(|line| line.contains("created before this attempt:"))
            .unwrap();
        assert_eq!(line, format!("  created before this attempt: {expected}"));
        assert!(!line.contains("(committed earlier range)"));
        assert!(!line.contains("(created earlier in this range"));
        assert_eq!(line.matches('(').count(), 1);
        assert_eq!(line.matches(')').count(), 1);
    }
}

#[test]
fn candidates_exclude_the_refused_declaration_but_keep_other_overloads() {
    let pg = crate::Postgres::new().with_server_version(160015);
    let changes = [
        trial_tests::module(
            "app.f(integer)",
            ModuleKind::Function,
            "(integer) RETURNS integer LANGUAGE plpgsql AS $$DECLARE x app.v.n%TYPE; BEGIN RETURN app.f(1); END$$",
        ),
        trial_tests::module(
            "app.f(text)",
            ModuleKind::Function,
            "(text) RETURNS integer LANGUAGE sql RETURN 1",
        ),
        trial_tests::module("app.v", ModuleKind::View, "SELECT 1 AS n"),
    ];
    let r = trial_tests::build(&pg, &changes, &Default::default(), false);
    let owner = Declaration::Module("app.f(integer)".parse().unwrap());
    let other = Candidate::Declaration(Declaration::Module("app.f(text)".parse().unwrap()));
    let at = r
        .steps
        .iter()
        .position(|step| step.owner.as_ref() == Some(&owner))
        .unwrap();
    let sql = crate::statement::submitted(&r.lowered.groups()[at].statements()[0], 1);
    let byte = sql.rfind("app.f(1)").unwrap();
    for positioned in [false, true] {
        let position =
            positioned.then(|| ErrorPosition::Original(sql[..byte].chars().count() as u32 + 1));
        let mut failure = error(position);
        if let DbError::Driver { code, .. } = &mut failure {
            *code = Some(if positioned { "42883" } else { "42601" }.into());
        }
        let diagnosis = r.diagnosis(
            at,
            0,
            crate::statement::Failure {
                error: failure,
                command: 1,
            },
            &[],
            vec![],
        );
        assert!(
            !diagnosis
                .candidates
                .contains(&Candidate::Declaration(owner.clone()))
        );
        assert!(diagnosis.candidates.contains(&other));
        if !positioned {
            assert!(
                diagnosis
                    .candidates
                    .contains(&Candidate::Declaration(Declaration::Module(
                        "app.v".parse().unwrap()
                    )))
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
#[allow(clippy::wildcard_enum_match_arm)]
async fn true_refusals_render_every_pending_declaration_on_both_majors() {
    use super::trial_tests::{database, module, servers};
    for server in servers() {
        for class in [
            "views",
            "functions",
            "declare_header",
            "header",
            "whole_table",
        ] {
            let (mut admin, mut conn, database, pg) = database(&server).await;
            let mut changes = match class {
                "views" => vec![
                    module("app.v", ModuleKind::View, "SELECT * FROM app.w"),
                    module("app.w", ModuleKind::View, "SELECT * FROM app.v"),
                ],
                "functions" => vec![
                    module(
                        "app.f(integer)",
                        ModuleKind::Function,
                        "(integer) RETURNS integer LANGUAGE sql RETURN app.g($1)",
                    ),
                    module(
                        "app.g(integer)",
                        ModuleKind::Function,
                        "(integer) RETURNS integer LANGUAGE sql RETURN app.f($1)",
                    ),
                ],
                "declare_header" => vec![
                    module("app.v", ModuleKind::View, "SELECT app.f(1) AS n"),
                    module(
                        "app.f(integer)",
                        ModuleKind::Function,
                        "(integer) RETURNS integer LANGUAGE plpgsql AS $$DECLARE x app.v.n%TYPE; BEGIN RETURN 1; END$$",
                    ),
                ],
                "header" => vec![
                    module("app.v", ModuleKind::View, "SELECT (app.f()).n AS n"),
                    module(
                        "app.f()",
                        ModuleKind::Function,
                        "() RETURNS app.v LANGUAGE sql AS $$ SELECT ROW(1)::app.v $$",
                    ),
                ],
                _ => vec![
                    Change::CreateTable {
                        name: ObjectName::new("app", "t"),
                        uid: pbps_model::Uid::generate(pbps_model::UidKind::Table),
                        table: Box::new(super::rule_tests::held_table()),
                        beside_default: None,
                        moves_at_most: None,
                    },
                    module(
                        "app.f(integer)",
                        ModuleKind::Function,
                        "(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT count(*)::int FROM app.t $$",
                    ),
                ],
            };
            changes.insert(0, module("app.created", ModuleKind::View, "SELECT 1 AS n"));
            let mut deps = pbps_model::ModuleDeps::new();
            if class == "functions" {
                changes.push(module("app.blocked", ModuleKind::View, "SELECT 1 AS n"));
                deps.insert(
                    "app.blocked".parse().unwrap(),
                    ["app.f(integer)".parse().unwrap()].into(),
                );
            }
            let mut reconstruction = trial_tests::build(&pg, &changes, &deps, true);
            let report_version = crate::roles::server_version_num(&mut conn).await.unwrap();
            let outcome = reconstruction
                .compile(&pg, &pbps_model::SessionZone::utc(), &mut conn)
                .await;
            admin
                .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
                .await
                .unwrap();
            if class == "whole_table" && report_version >= 180000 {
                assert!(outcome.is_ok(), "{outcome:?}");
                continue;
            }
            let ReconstructError::Refused(report) = outcome.unwrap_err() else {
                panic!("expected typed refusal");
            };
            let files = std::env::temp_dir().join(format!("sources-{database}"));
            std::fs::create_dir(&files).unwrap();
            for (at, change) in changes.iter().enumerate() {
                let yaml = match change {
                    Change::CreateModule { id, module } => pbps_load::render_module(
                        id,
                        module,
                        deps.get(id).unwrap_or(&Default::default()),
                        false,
                    ),
                    Change::CreateTable { name, table, .. } => {
                        pbps_load::render(name, table, &[], None)
                    }
                    _ => unreachable!(),
                };
                std::fs::write(files.join(format!("declaration-{at}.yaml")), yaml).unwrap();
            }
            let loaded = pbps_load::load_schema_dir(&files).unwrap();
            let rendered = report.render(|owner| {
                loaded
                    .sources
                    .get(owner)
                    .map(|path| path.to_string_lossy().into_owned())
            });
            std::fs::remove_dir_all(files).unwrap();
            println!(
                "REFUSAL {class} {}\n{rendered}\nEND REFUSAL",
                report.version
            );
            assert!(rendered.contains("PostgreSQL"));
            assert!(
                report
                    .pending
                    .iter()
                    .filter_map(|p| p.last.as_ref())
                    .all(|last| last.error.diagnostic().is_some())
            );
            if matches!(class, "views" | "functions") {
                assert!(rendered.contains('^'));
            }

            assert!(rendered.contains("earlier in this range (rolled back): view app.created"));
            assert!(!rendered.contains("(created earlier in this range"));
            assert!(rendered.contains("view app.created"));
            assert!(rendered.contains("why this step was ordered here:"));
            assert!(!rendered.contains("fix:"));
            assert!(
                report
                    .pending
                    .iter()
                    .all(|p| rendered.contains(&p.declaration))
            );
            for pending in &report.pending {
                if let Some(last) = &pending.last {
                    assert!(!last.candidates.iter().any(|candidate| matches!(candidate,
                        Candidate::Declaration(owner) if Some(owner) == pending.owner.as_ref()
                    )));
                }
            }
            if class == "functions" {
                assert!(rendered.contains(
                    "not attempted: waiting for hard predecessor function app.f(integer)"
                ));
                assert!(rendered.contains("resolve the refusal shown above for function app.f(integer); this declaration was never tried"));
                assert_eq!(
                    report.pending.iter().filter(|p| p.last.is_some()).count(),
                    2
                );
            }
        }
    }
}

#[test]
fn module_remedies_require_an_exact_provider_and_no_cycle_or_fill() {
    use super::trial_tests::{build, module};
    for (routine, cycle) in [(false, false), (false, true), (true, false)] {
        let pg = crate::Postgres::new().with_server_version(180006);
        let changes = if routine {
            vec![
                module("app.v", ModuleKind::View, "SELECT app.w(1) AS n"),
                module(
                    "app.w(text)",
                    ModuleKind::Function,
                    "(text) RETURNS text LANGUAGE sql RETURN $1",
                ),
            ]
        } else {
            vec![
                module("app.v", ModuleKind::View, "SELECT * FROM app.w"),
                module(
                    "app.w",
                    ModuleKind::View,
                    if cycle {
                        "SELECT * FROM app.v"
                    } else {
                        "SELECT 1 AS n"
                    },
                ),
            ]
        };
        let r = build(&pg, &changes, &Default::default(), true);
        let at = r
            .steps
            .iter()
            .position(|s| s.module.as_ref() == Some(&"app.v".parse().unwrap()))
            .unwrap();
        let sql = crate::statement::submitted(&r.lowered.groups()[at].statements()[0], 1);
        let byte = sql.find("app.w").unwrap();
        let mut e = error(Some(ErrorPosition::Original(
            sql[..byte].chars().count() as u32 + 1,
        )));
        if routine && let DbError::Driver { code, .. } = &mut e {
            *code = Some("42883".into());
        }
        let last = r.diagnosis(
            at,
            0,
            crate::statement::Failure {
                error: e,
                command: 1,
            },
            &[],
            vec![],
        );
        let pending = Pending {
            declaration: r.steps[at].declaration.clone(),
            subject: r.steps[at].subject.clone(),
            owner: r.steps[at].owner.clone(),
            dependency: None,
            last: Some(last),
            recovery: None,
            blocked: vec![],
            why: vec![],
        };
        for fill in [false, true] {
            let mut r = r.clone();
            if fill {
                r.fixture_fill_at(1, Vec::new());
            }
            let ReconstructError::Refused(report) =
                r.refusal(180006, vec![pending.clone()], true, vec![])
            else {
                unreachable!()
            };
            assert_eq!(
                report.pending[0].dependency.is_some(),
                !routine && !fill && !cycle
            );
        }
    }
}
