//! Standing transitions must change actual access, not only catalog metadata.
use super::row_security_roundtrip::{Outcome, Roles, assert_noop, attempt, versions};
use super::row_security_transitions::{ALLOW, adopted};
use super::*;
use pbps_db::{Conn, Driver};
use pbps_model::{PolicyCommand, PolicyRoles, RowPolicy, RowSecurity};
use std::collections::BTreeMap;

// Statement order is SELECT, then (INSERT, UPDATE, tenant move, negative UPDATE,
// negative INSERT) for tenant 1 and tenant 2. None means SQLSTATE 42501;
// Some(&[]) means a successful statement returned no visible row.
type Cells = [Option<&'static [i64]>; 11];
const ALL: Cells = [
    Some(&[1, 2, 3]),
    Some(&[101]),
    Some(&[1]),
    Some(&[1]),
    Some(&[1]),
    Some(&[102]),
    Some(&[101]),
    Some(&[2]),
    Some(&[2]),
    Some(&[2]),
    Some(&[102]),
];
const DENY: Cells = [
    Some(&[]),
    None,
    Some(&[]),
    Some(&[]),
    Some(&[]),
    None,
    None,
    Some(&[]),
    Some(&[]),
    Some(&[]),
    None,
];
const TENANT_1: Cells = [
    Some(&[1]),
    Some(&[101]),
    Some(&[1]),
    None,
    None,
    None,
    None,
    Some(&[]),
    Some(&[]),
    Some(&[]),
    None,
];
const TENANT_2: Cells = [
    Some(&[2]),
    None,
    Some(&[]),
    Some(&[]),
    Some(&[]),
    None,
    Some(&[101]),
    Some(&[2]),
    None,
    None,
    None,
];
const EXTRA_READ_1: Cells = [
    Some(&[1, 3]),
    Some(&[101]),
    Some(&[1]),
    None,
    None,
    None,
    None,
    Some(&[]),
    Some(&[]),
    Some(&[]),
    None,
];
const LOOSE_1: Cells = [
    Some(&[1, 3]),
    Some(&[101]),
    Some(&[1]),
    None,
    Some(&[1]),
    Some(&[102]),
    None,
    Some(&[]),
    Some(&[]),
    Some(&[]),
    None,
];
const LOOSE_2: Cells = [
    Some(&[2]),
    None,
    Some(&[]),
    Some(&[]),
    Some(&[]),
    None,
    Some(&[101]),
    Some(&[2]),
    None,
    Some(&[2]),
    Some(&[102]),
];
const READ_ONLY_1: Cells = [
    Some(&[1]),
    None,
    Some(&[]),
    Some(&[]),
    Some(&[]),
    None,
    None,
    Some(&[]),
    Some(&[]),
    Some(&[]),
    None,
];
const READ_ONLY_2: Cells = [
    Some(&[2]),
    None,
    Some(&[]),
    Some(&[]),
    Some(&[]),
    None,
    None,
    Some(&[]),
    Some(&[]),
    Some(&[]),
    None,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Actor {
    Owner,
    Inherit,
    NoInherit,
    Reader,
}

fn tenant(_: Actor, tenant: Option<i32>) -> Cells {
    match tenant {
        Some(1) => TENANT_1,
        Some(2) => TENANT_2,
        _ => DENY,
    }
}
fn extra_read(_: Actor, tenant: Option<i32>) -> Cells {
    match tenant {
        Some(1) => EXTRA_READ_1,
        Some(2) => TENANT_2,
        _ => DENY,
    }
}
fn loose(_: Actor, tenant: Option<i32>) -> Cells {
    match tenant {
        Some(1) => LOOSE_1,
        Some(2) => LOOSE_2,
        _ => DENY,
    }
}
fn read_only(_: Actor, tenant: Option<i32>) -> Cells {
    match tenant {
        Some(1) => READ_ONLY_1,
        Some(2) => READ_ONLY_2,
        _ => DENY,
    }
}

const TENANT_SQL: &str = "CREATE POLICY gate ON app.t USING (tenant=current_setting('app.tenant',true)::integer AND payload>=0) WITH CHECK (tenant=current_setting('app.tenant',true)::integer AND payload>=0);";
const TENANT_YAML: &str = "  gate:\n    using: \"tenant=current_setting('app.tenant',true)::integer AND payload>=0\"\n    with_check: \"tenant=current_setting('app.tenant',true)::integer AND payload>=0\"\n";
const BASE_SQL: &str = "CREATE POLICY base ON app.t USING(true) WITH CHECK(true);";
const TENANT_PREDICATE: &str = "tenant=current_setting('app.tenant',true)::integer AND payload>=0";

struct Case {
    name: &'static str,
    initial: String,
    edit: fn(&mut BTreeMap<String, RowPolicy>, &str, &str),
    flags: [RowSecurity; 2],
    expected: [fn(Actor, Option<i32>) -> Cells; 2],
}

fn table(policies: &str, flags: RowSecurity) -> String {
    format!(
        "table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n  tenant: {{type: integer}}\n  payload: {{type: integer}}\nprimary_key: [id]\nrow_security: {{enabled: {}, forced: {}}}\nrow_policies:\n{policies}",
        flags.enabled, flags.forced
    )
}

/// All three matrix tests use this driver. Expectations are literal cells;
/// only the common disabled/owner control bypasses a case's filtered cells.
fn matrix(cases: &[Case]) {
    versions(|base, _| {
        let mut roles = Roles::new(base);
        let owner = roles.create("1173d_owner", "LOGIN PASSWORD 'trigger-test'");
        let inherited = roles.create("1173d_inherit", "LOGIN PASSWORD 'trigger-test' INHERIT");
        let noinherit = roles.create("1173d_noinherit", "LOGIN PASSWORD 'trigger-test' NOINHERIT");
        let reader = roles.create("1173d_reader", "LOGIN PASSWORD 'trigger-test'");
        on_server(
            base,
            &format!("GRANT {owner} TO {inherited}; GRANT {owner} TO {noinherit}"),
        );
        for (role, usage) in [(&inherited, 1), (&noinherit, 0)] {
            assert_eq!(
                scalar(
                    base,
                    &format!("SELECT pg_has_role('{role}','{owner}','MEMBER')::int::bigint")
                ),
                1
            );
            assert_eq!(
                scalar(
                    base,
                    &format!("SELECT pg_has_role('{role}','{owner}','USAGE')::int::bigint")
                ),
                usage
            );
        }
        for case in cases {
            let started = std::time::Instant::now();
            let own = OwnDatabase::new(base, "rls_1173d_matrix");
            let connection = own.connection();
            on_server(
                connection,
                &format!(
                    "CREATE SCHEMA app; CREATE TABLE app.t(id integer PRIMARY KEY,tenant integer,payload integer); INSERT INTO app.t VALUES (1,1,10),(2,2,20),(3,1,-1); {} ALTER TABLE app.t {} ROW LEVEL SECURITY; ALTER TABLE app.t {} ROW LEVEL SECURITY; ALTER TABLE app.t OWNER TO {owner}; GRANT USAGE ON SCHEMA app TO {owner},{reader},{inherited},{noinherit}; GRANT SELECT,INSERT,UPDATE ON app.t TO {reader},{noinherit}",
                    case.initial,
                    if case.flags[0].enabled {
                        "ENABLE"
                    } else {
                        "DISABLE"
                    },
                    if case.flags[0].forced {
                        "FORCE"
                    } else {
                        "NO FORCE"
                    }
                ),
            );
            let d = adopted(connection, "rls-1173d-matrix");
            let opening = pbps_load::load_table_file(&d.dir.join("schema/app.t.yml")).unwrap();
            for phase in 0..2 {
                if phase == 1 {
                    // Preserve the pulled spelling of every untouched field:
                    // respelling a predicate would itself request replacement.
                    let mut target = opening.table.clone();
                    target.row_security = case.flags[1];
                    (case.edit)(&mut target.row_policies, &owner, &reader);
                    std::fs::write(
                        d.dir.join("schema/app.t.yml"),
                        pbps_load::fmt::render(
                            &opening.name,
                            &target,
                            &opening.intents,
                            opening.strategy.as_ref(),
                        ),
                    )
                    .unwrap();
                    let plan = connected_artifact(&d, connection, false);
                    let saved: pbps_model::SavedPlan =
                        serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
                    let actual: Vec<String> = saved
                        .changes
                        .changes
                        .iter()
                        .map(|step| {
                            serde_json::to_value(&step.change).unwrap()["op"]
                                .as_str()
                                .unwrap()
                                .to_owned()
                        })
                        .collect();
                    let expected: &[&str] = if case.name.starts_with("replace-") {
                        &["drop_policy", "create_policy"]
                    } else if case.name.starts_with("create-") {
                        &["create_policy"]
                    } else if case.name.starts_with("drop-") {
                        &["drop_policy"]
                    } else {
                        &["set_row_security"]
                    };
                    assert_eq!(
                        actual, expected,
                        "{} must isolate its transition",
                        case.name
                    );
                    succeeds(approved_apply(&d, connection, &plan, ALLOW));
                }
                let flags = case.flags[phase];
                for (actor, role) in [
                    (Actor::Owner, &owner),
                    (Actor::Inherit, &inherited),
                    (Actor::NoInherit, &noinherit),
                    (Actor::Reader, &reader),
                ] {
                    for setting in [Some(1), None, Some(2), Some(99)] {
                        let bypass = !flags.enabled
                            || (!flags.forced && matches!(actor, Actor::Owner | Actor::Inherit));
                        let expected = if bypass {
                            ALL
                        } else {
                            (case.expected[phase])(actor, setting)
                        };
                        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                            let mut conn=Conn::connect(Driver::Postgres,&as_role(connection,role)).await.unwrap();
                            let identity=conn.query("SELECT current_user::text, rolsuper OR rolbypassrls FROM pg_roles WHERE rolname=current_user").await.unwrap();
                            assert_eq!(identity[0].try_get_at::<&str>(0).unwrap(),Some(role.as_str()));
                            assert_eq!(identity[0].try_get_at::<bool>(1).unwrap(),Some(false));
                            conn.execute("BEGIN").await.unwrap();
                            if let Some(value)=setting {
                                conn.execute(&format!("SET LOCAL app.tenant='{value}'")).await.unwrap();
                            } else {
                                assert_eq!(conn.query("SELECT current_setting('app.tenant',true) IS NULL").await.unwrap()[0].try_get_at::<bool>(0).unwrap(),Some(true));
                            }
                            let mut statements=vec!["SELECT id::bigint FROM app.t ORDER BY id".to_owned()];
                            for t in [1,2] {
                                statements.extend([
                                    format!("INSERT INTO app.t VALUES(101,{t},10) RETURNING id::bigint"),
                                    format!("UPDATE app.t SET payload=11 WHERE id={t} RETURNING id::bigint"),
                                    format!("UPDATE app.t SET tenant={} WHERE id={t} RETURNING id::bigint",3-t),
                                    format!("UPDATE app.t SET payload=-1 WHERE id={t} RETURNING id::bigint"),
                                    format!("INSERT INTO app.t VALUES(102,{t},-1) RETURNING id::bigint"),
                                ]);
                            }
                            assert_eq!(statements.len(), expected.len(), "every statement needs an expected cell");
                            for (sql,cell) in statements.iter().zip(expected) {
                                let want=cell.map_or(Outcome::Denied,|ids|Outcome::Rows(ids.to_vec()));
                                assert_eq!(attempt(&mut conn,sql).await,want,"case={}, phase={phase}, actor={actor:?}, tenant={setting:?}, SQL={sql}",case.name);
                            }
                            conn.execute("ROLLBACK").await.unwrap();
                        });
                    }
                }
            }
            assert_noop(&d, connection);
            eprintln!(
                "1173d matrix {} {:?}->{:?}: {:?}",
                case.name,
                case.flags[0],
                case.flags[1],
                started.elapsed()
            );
        }
    });
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_d_flag_transitions_enforce_owner_and_member_controls() {
    let mut cases = Vec::new();
    for other in [false, true] {
        for (name, from, to) in [("enable", false, true), ("disable", true, false)] {
            cases.push(Case {
                name,
                initial: TENANT_SQL.into(),
                edit: |_, _, _| {},
                flags: [
                    RowSecurity {
                        enabled: from,
                        forced: other,
                    },
                    RowSecurity {
                        enabled: to,
                        forced: other,
                    },
                ],
                expected: [tenant, tenant],
            });
        }
        for (name, from, to) in [("force", false, true), ("no-force", true, false)] {
            cases.push(Case {
                name,
                initial: TENANT_SQL.into(),
                edit: |_, _, _| {},
                flags: [
                    RowSecurity {
                        enabled: other,
                        forced: from,
                    },
                    RowSecurity {
                        enabled: other,
                        forced: to,
                    },
                ],
                expected: [tenant, tenant],
            });
        }
    }
    matrix(&cases);
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_d_policy_creations_and_drops_enforce_access() {
    let mut cases = Vec::new();
    for forced in [false, true] {
        let flags = [RowSecurity {
            enabled: true,
            forced,
        }; 2];
        let broad_sql =
            format!("{TENANT_SQL} CREATE POLICY bonus ON app.t USING(true) WITH CHECK(true);");
        let restricted_sql = format!(
            "{BASE_SQL} {}",
            TENANT_SQL.replace("ON app.t USING", "ON app.t AS RESTRICTIVE USING")
        );
        cases.extend([
            Case {
                name: "create-permissive",
                initial: TENANT_SQL.into(),
                flags,
                edit: |policies, _, _| {
                    policies.insert(
                        "bonus".into(),
                        RowPolicy {
                            using: Some("true".into()),
                            with_check: Some("true".into()),
                            ..Default::default()
                        },
                    );
                },
                expected: [tenant, |_, _| ALL],
            },
            Case {
                name: "drop-permissive",
                initial: broad_sql,
                flags,
                edit: |policies, _, _| {
                    policies.remove("bonus");
                },
                expected: [|_, _| ALL, tenant],
            },
            Case {
                name: "create-restrictive",
                initial: BASE_SQL.into(),
                flags,
                edit: |policies, _, _| {
                    policies.insert(
                        "gate".into(),
                        RowPolicy {
                            restrictive: true,
                            using: Some(TENANT_PREDICATE.into()),
                            with_check: Some(TENANT_PREDICATE.into()),
                            ..Default::default()
                        },
                    );
                },
                expected: [|_, _| ALL, tenant],
            },
            Case {
                name: "drop-restrictive",
                initial: restricted_sql,
                flags,
                edit: |policies, _, _| {
                    policies.remove("gate");
                },
                expected: [tenant, |_, _| ALL],
            },
        ]);
    }
    matrix(&cases);
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_d_replacements_enforce_each_changed_policy_field() {
    let mut cases = Vec::new();
    for forced in [false, true] {
        let flags = [RowSecurity {
            enabled: true,
            forced,
        }; 2];
        let read_sql = "CREATE POLICY gate ON app.t USING(tenant=current_setting('app.tenant',true)::integer) WITH CHECK(tenant=current_setting('app.tenant',true)::integer AND payload>=0);";
        cases.extend([
            Case {
                name: "replace-command", initial: TENANT_SQL.into(), flags,
                edit: |policies, _, _| {
                    let gate = policies.get_mut("gate").unwrap();
                    gate.command = PolicyCommand::Select;
                    gate.with_check = None;
                },
                expected: [tenant, read_only],
            },
            Case {
                name: "replace-mode", initial: format!("{BASE_SQL}{TENANT_SQL}"), flags,
                edit: |policies, _, _| policies.get_mut("gate").unwrap().restrictive = true,
                expected: [|_, _| ALL, tenant],
            },
            Case {
                name: "replace-roles", initial: TENANT_SQL.into(), flags,
                edit: |policies, owner, reader| policies.get_mut("gate").unwrap().to = PolicyRoles::Roles([owner.into(), reader.into()].into()),
                expected: [tenant, |actor, t| if actor == Actor::NoInherit { DENY } else { tenant(actor, t) }],
            },
            Case {
                name: "replace-using", initial: read_sql.into(), flags,
                edit: |policies, _, _| policies.get_mut("gate").unwrap().using = Some(TENANT_PREDICATE.into()),
                expected: [extra_read, tenant],
            },
            Case {
                name: "replace-with-check", initial: "CREATE POLICY gate ON app.t USING(tenant=current_setting('app.tenant',true)::integer);".into(), flags,
                edit: |policies, _, _| policies.get_mut("gate").unwrap().with_check = Some(TENANT_PREDICATE.into()),
                expected: [loose, extra_read],
            },
        ]);
    }
    matrix(&cases);
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_d_unreadable_policy_catalog_refuses_plan_and_saved_apply_before_ddl() {
    versions(|base, _| {
        let mut roles = Roles::new(base);
        let owner = roles.create("1173d_hidden", "LOGIN PASSWORD 'trigger-test'");
        let own = OwnDatabase::new(base, "rls_1173d_hidden");
        let connection = own.connection();
        on_server(
            connection,
            &format!(
                "CREATE SCHEMA app; CREATE TABLE app.t(id integer PRIMARY KEY,tenant integer,payload integer); {TENANT_SQL} ALTER TABLE app.t ENABLE ROW LEVEL SECURITY; ALTER TABLE app.t OWNER TO {owner}; GRANT USAGE ON SCHEMA app TO {owner}"
            ),
        );
        let d = adopted(connection, "rls-1173d-hidden");
        on_server(
            connection,
            &format!(
                "GRANT SELECT,INSERT,DELETE ON public.__pbps_state,public.__pbps_lock TO {owner}"
            ),
        );
        let login = as_role(connection, &owner);
        std::fs::write(
            d.dir.join("schema/app.t.yml"),
            table(
                &TENANT_YAML.replace("payload>=0", "payload>0"),
                RowSecurity {
                    enabled: true,
                    forced: false,
                },
            ),
        )
        .unwrap();
        let plan = connected_artifact(&d, &login, false);
        let before = latest_snapshot(connection);
        let artifact = d.dir.join("preserve.json");
        std::fs::write(&artifact, "previous artifact").unwrap();
        on_server(
            connection,
            &format!(
                "REVOKE SELECT ON pg_catalog.pg_policy FROM PUBLIC; GRANT SELECT(polrelid) ON pg_catalog.pg_policy TO {owner}"
            ),
        );
        for output in [
            d.run(&["plan", "--db", &login, "--out", artifact.to_str().unwrap()]),
            approved_apply(&d, &login, &plan, ALLOW),
        ] {
            assert_ne!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
            assert!(
                stderr(&output).contains("pg_policy")
                    && stderr(&output).contains("permission denied"),
                "{}{}",
                stdout(&output),
                stderr(&output)
            );
            assert!(
                !stdout(&output).contains("Applying "),
                "{}",
                stdout(&output)
            );
        }
        assert_eq!(
            std::fs::read_to_string(artifact).unwrap(),
            "previous artifact"
        );
        let after = latest_snapshot(connection);
        assert_eq!(after.schema, before.schema);
        assert_eq!(after.ids, before.ids);
        assert_eq!(after.declared, before.declared);
        // A new failed-attempt entry is allowed; an unreadable catalog is never
        // recorded as empty policy state or as a successful migration.
        assert_ne!(after.kind, pbps_model::StateKind::Apply);
        on_server(connection, "GRANT SELECT ON pg_catalog.pg_policy TO PUBLIC");
        assert_eq!(latest_snapshot(connection).schema, before.schema);
        succeeds(approved_apply(&d, &login, &plan, ALLOW));
        assert_noop(&d, &login);
    });
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_d_closing_guard_refuses_policy_and_flag_interference_and_rolls_back() {
    versions(|base, _| {
        for (name, tag, interference) in [
            (
                "command",
                "CREATE POLICY",
                "DROP POLICY gate ON app.t; CREATE POLICY gate ON app.t FOR UPDATE USING(id>0) WITH CHECK(id>0)",
            ),
            (
                "mode",
                "CREATE POLICY",
                "DROP POLICY gate ON app.t; CREATE POLICY gate ON app.t AS RESTRICTIVE USING(id>0) WITH CHECK(id>0)",
            ),
            (
                "roles",
                "CREATE POLICY",
                "ALTER POLICY gate ON app.t TO CURRENT_USER",
            ),
            (
                "using",
                "CREATE POLICY",
                "DROP POLICY gate ON app.t; CREATE POLICY gate ON app.t WITH CHECK(id>0)",
            ),
            (
                "with-check",
                "CREATE POLICY",
                "DROP POLICY gate ON app.t; CREATE POLICY gate ON app.t USING(id>0)",
            ),
            (
                "kept",
                "CREATE POLICY",
                "ALTER POLICY kept ON app.t USING(true)",
            ),
            (
                "enabled",
                "ALTER TABLE",
                "ALTER TABLE app.t DISABLE ROW LEVEL SECURITY",
            ),
            (
                "forced",
                "ALTER TABLE",
                "ALTER TABLE app.t NO FORCE ROW LEVEL SECURITY",
            ),
        ] {
            let own = OwnDatabase::new(base, "rls_1173d_closing");
            let connection = own.connection();
            on_server(
                connection,
                "CREATE SCHEMA app; CREATE TABLE app.t(id integer,n integer); CREATE POLICY kept ON app.t USING(id>=0); CREATE POLICY gate ON app.t USING(false); ALTER TABLE app.t ENABLE ROW LEVEL SECURITY",
            );
            let d = adopted(connection, "rls-1173d-closing");
            std::fs::write(d.dir.join("schema/app.t.yml"),"table: app.t\ncolumns:\n  id: {type: integer}\n  n: {type: integer}\nrow_security: {enabled: true, forced: true}\nrow_policies:\n  kept: {using: '(id >= 0)'}\n  gate: {using: 'id>0', with_check: 'id>0'}\n").unwrap();
            let plan = connected_artifact(&d, connection, false);
            let before = latest_snapshot(connection);
            // The one-shot session flag prevents the fixture's own DDL from
            // re-entering the hook. Its private schema is outside declarations.
            on_server(
                connection,
                &format!(
                    "CREATE SCHEMA rls_d_fixture; CREATE FUNCTION rls_d_fixture.interfere() RETURNS event_trigger LANGUAGE plpgsql AS $hook$ BEGIN IF current_setting('rls_d_fixture.fired',true) IS DISTINCT FROM 'yes' AND (TG_TAG <> 'ALTER TABLE' OR (SELECT relrowsecurity AND relforcerowsecurity FROM pg_class WHERE oid='app.t'::regclass)) THEN PERFORM set_config('rls_d_fixture.fired','yes',true); {interference}; END IF; END $hook$; CREATE EVENT TRIGGER rls_d_interfere ON ddl_command_end WHEN TAG IN ('{tag}') EXECUTE FUNCTION rls_d_fixture.interfere()"
                ),
            );
            let out = approved_apply(&d, connection, &plan, ALLOW);
            assert_ne!(code(&out), 0, "{name}: {}{}", stdout(&out), stderr(&out));
            assert!(
                stderr(&out).contains("app.t")
                    && (stderr(&out).contains("row policy")
                        || stderr(&out).contains("row-security")),
                "{name}: {}{}",
                stdout(&out),
                stderr(&out)
            );
            let after = latest_snapshot(connection);
            assert_eq!(after.kind, pbps_model::StateKind::Failed, "{name}");
            assert_eq!(after.schema, before.schema, "{name}");
            assert_eq!(after.ids, before.ids, "{name}");
            assert_eq!(after.declared, before.declared, "{name}");
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_policy WHERE polrelid='app.t'::regclass AND polname='gate' AND pg_get_expr(polqual,polrelid)='false'"
                ),
                1,
                "{name}"
            );
            assert_eq!(
                scalar(
                    connection,
                    "SELECT relrowsecurity::int::bigint+2*relforcerowsecurity::int::bigint FROM pg_class WHERE oid='app.t'::regclass"
                ),
                1,
                "{name}"
            );
            on_server(
                connection,
                "DROP EVENT TRIGGER rls_d_interfere; DROP FUNCTION rls_d_fixture.interfere(); DROP SCHEMA rls_d_fixture",
            );
            succeeds(approved_apply(&d, connection, &plan, ALLOW));
            assert_noop(&d, connection);
        }
    });
}
