//! Standing-policy delivery and its pre-DDL and transactional boundaries.
use super::row_security_roundtrip::{Roles, assert_noop, versions};
use super::*;

pub(super) const ALLOW: &[&str] = &["--allow", "row-security,revoke,destructive,rename,not-null"];

pub(super) fn adopted(connection: &str, slug: &str) -> Demo {
    let d = Demo::new(slug);
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "standing RLS transition fixture",
    ]));
    d
}

fn declaration(policy: Option<&str>, enabled: bool, forced: bool) -> String {
    format!(
        "table: app.t\ncolumns:\n  id: {{type: integer}}\n  n: {{type: integer}}\nrow_security: {{enabled: {enabled}, forced: {forced}}}\nrow_policies:\n  kept: {{using: '(id >= 0)'}}\n{}",
        policy.map_or(String::new(), |p| format!("  gate:\n{p}"))
    )
}

fn write(d: &Demo, text: &str) {
    std::fs::write(d.dir.join("schema/app.t.yml"), text).unwrap();
}

fn apply(d: &Demo, connection: &str) -> pbps_model::SavedPlan {
    let path = connected_artifact(d, connection, false);
    let saved = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    succeeds(approved_apply(d, connection, &path, ALLOW));
    assert_noop(d, connection);
    saved
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_c_standing_transitions_apply_verify_and_replan_empty() {
    versions(|base, _| {
        let mut roles = Roles::new(base);
        let first = roles.create("c_target_a", "");
        let second = roles.create("c_target_b", "");
        let own = OwnDatabase::new(base, "rls_c_transitions");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.t(id integer,n integer); CREATE POLICY kept ON app.t USING(id>=0)",
        );
        let d = adopted(connection, "rls-c-transitions");
        // Creation followed by replacements exercises every command, both modes,
        // named/PUBLIC recipients, text changes and each clause independently.
        for (command, restrictive, targets, using, check) in [
            ("all", false, "public".to_owned(), true, true),
            ("select", true, format!("[{first}]"), true, false),
            ("insert", false, format!("[{second}]"), false, true),
            ("update", true, format!("[{first}, {second}]"), true, false),
            ("update", false, "public".to_owned(), false, true),
            ("delete", true, "public".to_owned(), true, false),
            ("all", false, "public".to_owned(), false, false),
        ] {
            let policy = format!(
                "    command: {command}\n    restrictive: {restrictive}\n    to: {targets}\n{}{}",
                if using { "    using: 'id > 1'\n" } else { "" },
                if check {
                    "    with_check: 'n > 2'\n"
                } else {
                    ""
                }
            );
            write(&d, &declaration(Some(&policy), false, false));
            let saved = apply(&d, connection);
            assert!(
                saved
                    .changes
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, pbps_model::Change::CreatePolicy { .. }))
            );
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_policy WHERE polrelid='app.t'::regclass"
                ),
                2
            );
        }
        write(&d, &declaration(None, false, false));
        let saved = apply(&d, connection);
        assert!(
            matches!(&saved.changes.changes[..],[p] if matches!(p.change,pbps_model::Change::DropPolicy {..}))
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_policy WHERE polrelid='app.t'::regclass"
            ),
            1
        );
        for from in 0..4 {
            for to in 0..4 {
                for bits in [from, to] {
                    write(&d, &declaration(None, bits & 1 != 0, bits & 2 != 0));
                    apply(&d, connection);
                    assert_eq!(
                        scalar(
                            connection,
                            "SELECT relrowsecurity::int::bigint + 2*relforcerowsecurity::int::bigint FROM pg_class WHERE oid='app.t'::regclass"
                        ),
                        bits
                    );
                }
            }
        }
    });
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_c_nonowners_are_refused_at_plan_and_saved_apply_before_writes() {
    versions(|base, _| {
        let mut roles = Roles::new(base);
        let nonowner = roles.create("c_nonowner", "LOGIN PASSWORD 'trigger-test'");
        let inheriting = roles.create("c_inherit", "LOGIN PASSWORD 'trigger-test'");
        let owner = roles.create("c_owner", "");
        on_server(base, &format!("GRANT {owner} TO {inheriting}"));
        let own = OwnDatabase::new(base, "rls_c_owners");
        let connection = own.connection();
        on_server(
            connection,
            &format!(
                "CREATE SCHEMA app; CREATE TABLE app.t(id integer,n integer); CREATE POLICY kept ON app.t USING(id>=0); ALTER TABLE app.t OWNER TO {owner}; GRANT USAGE ON SCHEMA app TO {nonowner},{owner}; GRANT ALL ON app.t TO {nonowner}"
            ),
        );
        let d = adopted(connection, "rls-c-owners");
        on_server(
            connection,
            &format!(
                "GRANT SELECT ON public.__pbps_state,public.__pbps_lock TO {nonowner},{owner}"
            ),
        );
        write(
            &d,
            &declaration(Some("    using: 'id > 0'\n"), false, false),
        );
        let plan = connected_artifact(&d, connection, false);
        let snapshot = latest_snapshot(connection);
        let no = as_role(connection, &nonowner);
        let out = d.run(&["plan", "--db", &no]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("app.t") && stderr(&out).contains("deployer must own"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        let out = approved_apply(&d, &no, &plan, ALLOW);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("app.t") && stderr(&out).contains("deployer must own"),
            "{}",
            stderr(&out)
        );
        assert_eq!(latest_snapshot(connection), snapshot);
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_policy WHERE polrelid='app.t'::regclass"
            ),
            1
        );
        // Inherited owner privileges satisfy the same predicate as ALTER TABLE.
        succeeds(d.run(&["plan", "--db", &as_role(connection, &inheriting)]));
    });
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_c_explicit_policy_removals_release_columns_and_routine_rebuilds() {
    versions(|base, _| {
        let own = OwnDatabase::new(base, "rls_c_dependents");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE FUNCTION app.visible(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$; CREATE TABLE app.t(id integer,n integer); CREATE POLICY kept ON app.t USING(id>=0); CREATE POLICY gate ON app.t USING(app.visible(n)>0)",
        );
        let d = adopted(connection, "rls-c-dependents");
        let original = std::fs::read_to_string(d.dir.join("schema/app.t.yml")).unwrap();
        let ids_path = d.dir.join("schema.ids.json");
        let opening_ids = std::fs::read(&ids_path).unwrap();
        // Retyping and dropping a bound column are refused before any DDL.
        for target in [
            original.replace("\"n\":\n    type: integer", "\"n\":\n    type: bigint"),
            original.replace("  \"n\":\n    type: integer\n", ""),
        ] {
            assert_ne!(target, original, "{original}");
            write(&d, &target);
            if !target.contains("  \"n\":") {
                succeeds(d.run(&["drop", "app.t.n", "--reason", "retire policy-bound column"]));
            }
            let out = d.run(&["plan", "--db", connection]);
            assert_ne!(code(&out), 0);
            assert!(
                stderr(&out).contains("gate")
                    && stderr(&out).contains("explicitly remove or replace"),
                "{}",
                stderr(&out)
            );
            std::fs::write(&ids_path, &opening_ids).unwrap();
        }
        write(&d, &original);
        declare_function(&d, "visible", "SELECT x+1");
        let out = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("gate") && stderr(&out).contains("explicitly remove or replace"),
            "{}",
            stderr(&out)
        );
        // An explicit replacement releases the old dependency and restores only
        // the new declaration after the routine rebuild.
        write(
            &d,
            &declaration(
                Some("    using: 'app.visible(n::integer) > 1'\n"),
                false,
                false,
            )
            .replace("n: {type: integer}", "n: {type: bigint}"),
        );
        let artifact = connected_artifact(&d, connection, false);
        let saved: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
        let mut forged = saved.clone();
        forged.changes.changes.retain(|step| {
            !matches!(
                step.change,
                pbps_model::Change::DropPolicy { .. } | pbps_model::Change::CreatePolicy { .. }
            )
        });
        let pg = pbps_pg::Postgres::new();
        let risks = pbps_diff::order::generated::risks(&forged.changes, &forged.ids, &pg);
        for (step, risks) in forged.changes.changes.iter_mut().zip(risks) {
            step.risks = risks;
        }
        let forged_path = d.dir.join("missing-policy-release.json");
        std::fs::write(&forged_path, serde_json::to_vec_pretty(&forged).unwrap()).unwrap();
        // Pure admission cannot see catalog bindings. Apply must recheck the
        // release even for an internally consistent, checksum-approved plan.
        pbps_diff::order::admit(
            forged.clone(),
            &pg,
            &forged_path,
            &forged.wire_fingerprint(),
        )
        .unwrap();
        let before = latest_snapshot(connection);
        // Progress is printed before admission finishes. A nontransactional
        // sequence records any attempted DDL even if apply rolls it back.
        on_server(
            connection,
            "CREATE SCHEMA rls_d_release; CREATE SEQUENCE rls_d_release.ddl_calls; CREATE FUNCTION rls_d_release.count_ddl() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('rls_d_release.ddl_calls'); END $$; CREATE EVENT TRIGGER rls_d_release_ddl ON ddl_command_end WHEN TAG IN ('DROP FUNCTION','ALTER TABLE') EXECUTE FUNCTION rls_d_release.count_ddl()",
        );
        let out = approved_apply(&d, connection, &forged_path, ALLOW);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("gate") && stderr(&out).contains("explicitly remove or replace"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT is_called::int::bigint FROM rls_d_release.ddl_calls"
            ),
            0,
            "{out:?}"
        );
        let after = latest_snapshot(connection);
        assert_eq!(after.schema, before.schema);
        assert_eq!(after.ids, before.ids);
        succeeds(approved_apply(&d, connection, &artifact, ALLOW));
        assert_eq!(
            scalar(
                connection,
                "SELECT is_called::int::bigint FROM rls_d_release.ddl_calls"
            ),
            1
        );
        on_server(
            connection,
            "DROP EVENT TRIGGER rls_d_release_ddl; DROP FUNCTION rls_d_release.count_ddl(); DROP SEQUENCE rls_d_release.ddl_calls; DROP SCHEMA rls_d_release",
        );
        assert_noop(&d, connection);
        let pos = |pred: fn(&pbps_model::Change) -> bool| {
            saved
                .changes
                .changes
                .iter()
                .position(|p| pred(&p.change))
                .unwrap()
        };
        let drop = pos(|c| matches!(c, pbps_model::Change::DropPolicy { .. }));
        let routine = pos(|c| {
            matches!(
                c,
                pbps_model::Change::DropModule { .. } | pbps_model::Change::AlterModule { .. }
            )
        });
        let restored = pos(|c| {
            matches!(
                c,
                pbps_model::Change::CreateModule { .. } | pbps_model::Change::AlterModule { .. }
            )
        });
        let create = pos(|c| matches!(c, pbps_model::Change::CreatePolicy { .. }));
        let retype = pos(|c| matches!(c, pbps_model::Change::AlterColumnType { .. }));
        assert!(drop < routine && restored < create && drop < retype && retype < create);
        // Removing the policy in the declarations releases its column too.
        write(
            &d,
            &declaration(None, false, false).replace("  n: {type: integer}\n", ""),
        );
        succeeds(d.run(&["drop", "app.t.n", "--reason", "policy released this column"]));
        let saved = apply(&d, connection);
        assert!(matches!(
            &saved.changes.changes[0].change,
            pbps_model::Change::DropPolicy { .. }
        ));
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_attribute WHERE attrelid='app.t'::regclass AND attname='n' AND NOT attisdropped"
            ),
            0
        );
    });
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_c_stale_policies_and_missing_roles_preserve_original_policy() {
    versions(|base, _| {
        let mut roles = Roles::new(base);
        let recipient = roles.create("c_rollback", "");
        let own = OwnDatabase::new(base, "rls_c_rollback");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.t(id integer,n integer); CREATE POLICY kept ON app.t USING(id>=0); CREATE POLICY gate ON app.t USING(false)",
        );
        let d = adopted(connection, "rls-c-rollback");
        write(
            &d,
            &declaration(
                Some(&format!("    to: [{recipient}]\n    using: 'id > 0'\n")),
                false,
                false,
            ),
        );
        let path = connected_artifact(&d, connection, false);
        let snapshot = latest_snapshot(connection);
        on_server(connection, "ALTER POLICY gate ON app.t USING(true)");
        let out = approved_apply(&d, connection, &path, ALLOW);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("is no longer the database this plan was computed against"),
            "{}",
            stderr(&out)
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_policy WHERE polrelid='app.t'::regclass AND pg_get_expr(polqual,polrelid)='true'"
            ),
            1
        );
        assert_eq!(latest_snapshot(connection).schema, snapshot.schema);
        assert_eq!(latest_snapshot(connection).ids, snapshot.ids);
        on_server(connection, "ALTER POLICY gate ON app.t USING(false)");
        on_server(base, &format!("DROP ROLE {recipient}"));
        let out = approved_apply(&d, connection, &path, ALLOW);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("gate") && stderr(&out).contains("missing cluster role"),
            "{}",
            stderr(&out)
        );
        assert_eq!(latest_snapshot(connection).schema, snapshot.schema);
        assert_eq!(latest_snapshot(connection).ids, snapshot.ids);
        on_server(
            base,
            &format!("CREATE ROLE {recipient} NOSUPERUSER NOBYPASSRLS"),
        );
        // The product's early recipient check is no substitute for rollback:
        // invalidate the recipient after DROP POLICY, inside apply's transaction.
        on_server(
            connection,
            &format!(
                "CREATE SCHEMA rls_c_fixture; CREATE FUNCTION rls_c_fixture.lose_recipient() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN EXECUTE 'DROP ROLE {recipient}'; END $$; CREATE EVENT TRIGGER lose_recipient ON ddl_command_end WHEN TAG IN ('DROP POLICY') EXECUTE FUNCTION rls_c_fixture.lose_recipient()"
            ),
        );
        let out = approved_apply(&d, connection, &path, ALLOW);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains(&recipient) && stderr(&out).contains("does not exist"),
            "{}",
            stderr(&out)
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_policy WHERE polrelid='app.t'::regclass AND polname='gate' AND pg_get_expr(polqual,polrelid)='false'"
            ),
            1
        );
        assert_eq!(
            scalar(
                connection,
                &format!("SELECT count(*) FROM pg_roles WHERE rolname='{recipient}'")
            ),
            1
        );
        assert_eq!(latest_snapshot(connection).schema, snapshot.schema);
        assert_eq!(latest_snapshot(connection).ids, snapshot.ids);
        on_server(
            connection,
            "DROP EVENT TRIGGER lose_recipient; DROP FUNCTION rls_c_fixture.lose_recipient(); DROP SCHEMA rls_c_fixture",
        );
        succeeds(approved_apply(&d, connection, &path, ALLOW));
        assert_noop(&d, connection);
    });
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_c_staged_planning_refuses_and_probes_use_forced_owner_preplan_state() {
    versions(|base, _| {
        let mut roles = Roles::new(base);
        let owner = roles.create("c_probe", "LOGIN PASSWORD 'trigger-test'");
        let own = OwnDatabase::new(base, "rls_c_probe");
        let connection = own.connection();
        on_server(
            connection,
            &format!(
                "CREATE SCHEMA app; CREATE TABLE app.t(id integer,n integer); INSERT INTO app.t VALUES(1,NULL); CREATE POLICY kept ON app.t USING(false); ALTER TABLE app.t ENABLE ROW LEVEL SECURITY; ALTER TABLE app.t FORCE ROW LEVEL SECURITY; ALTER TABLE app.t OWNER TO {owner}; GRANT USAGE ON SCHEMA app TO {owner}"
            ),
        );
        let d = adopted(connection, "rls-c-probe");
        on_server(
            connection,
            &format!(
                "GRANT SELECT,INSERT,DELETE ON public.__pbps_state,public.__pbps_lock TO {owner}"
            ),
        );
        // First pin the single-logical-change planner refusal.
        let original = std::fs::read_to_string(d.dir.join("schema/app.t.yml")).unwrap();
        let target = original.replace("forced: true", "forced: false");
        assert_ne!(target, original);
        write(&d, &target);
        let out = d.run(&["plan", "--db", connection, "--staged"]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("row-security changes require a transactional plan"),
            "{}",
            stderr(&out)
        );
        // A later NO FORCE cannot turn a pre-plan filtered count into passed.
        let target = target.replace(
            "\"n\":\n    type: integer",
            "\"n\":\n    type: integer\n    nullable: false",
        );
        write(&d, &target);
        let as_owner = as_role(connection, &owner);
        let path = connected_artifact(&d, &as_owner, false);
        let out = approved_apply(&d, &as_owner, &path, ALLOW);
        assert_ne!(code(&out), 0);
        assert!(
            stdout(&out).contains("Pre-flight: 1 probe(s) could not be checked here"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("returned no readable count"),
            "{}",
            stderr(&out)
        );
        assert!(stderr(&out).contains("null values"), "{}", stderr(&out));
        assert!(
            !stdout(&out).contains("probe(s) passed"),
            "{}",
            stdout(&out)
        );
        assert_eq!(
            scalar(connection, "SELECT count(*) FROM app.t WHERE n IS NULL"),
            1
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT relforcerowsecurity::int::bigint FROM pg_class WHERE oid='app.t'::regclass"
            ),
            1
        );
    });
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_c_saved_policy_drops_pin_opening_definitions_before_ddl() {
    versions(|base, _| {
        for restrictive in [false, true] {
            let own = OwnDatabase::new(base, "rls_c_drop_payload");
            let connection = own.connection();
            on_server(
                connection,
                &format!(
                    "CREATE SCHEMA app; CREATE TABLE app.t(id integer,n integer); CREATE POLICY kept ON app.t USING(id>=0); CREATE POLICY gate ON app.t AS {} FOR SELECT USING(id>0)",
                    if restrictive {
                        "RESTRICTIVE"
                    } else {
                        "PERMISSIVE"
                    }
                ),
            );
            let d = adopted(connection, "rls-c-drop-payload");
            write(&d, &declaration(None, false, false));
            let artifact = connected_artifact(&d, connection, false);
            let original: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
            assert!(
                matches!(&original.changes.changes[..], [step] if matches!(step.change, pbps_model::Change::DropPolicy { .. }))
            );
            let before = latest_snapshot(connection);
            let mut forged = original.clone();
            let pbps_model::Change::DropPolicy { policy, .. } =
                &mut forged.changes.changes[0].change
            else {
                unreachable!()
            };
            policy.restrictive = !restrictive;
            let pg = pbps_pg::Postgres::new();
            let risks = pbps_diff::order::generated::risks(&forged.changes, &forged.ids, &pg);
            for (step, risks) in forged.changes.changes.iter_mut().zip(risks) {
                step.risks = risks;
            }
            let forged_path = d.dir.join("forged-drop.json");
            std::fs::write(&forged_path, serde_json::to_vec_pretty(&forged).unwrap()).unwrap();
            // The edited risk list and approved wire checksum are consistent:
            // only the opening database can disprove the dropped definition.
            pbps_diff::order::admit(
                forged.clone(),
                &pg,
                &forged_path,
                &forged.wire_fingerprint(),
            )
            .unwrap();
            let incorrect_allow = if restrictive {
                "revoke"
            } else {
                "row-security"
            };
            let out = approved_apply(&d, connection, &forged_path, &["--allow", incorrect_allow]);
            assert_ne!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
            let error = stderr(&out);
            assert!(
                error.contains("app.t")
                    && error.contains("gate")
                    && error.contains("dropped definition")
                    && error.contains("baseline"),
                "{error}"
            );
            assert!(
                !stdout(&out).contains("Applying "),
                "the refusal precedes DDL"
            );
            let after = latest_snapshot(connection);
            assert_eq!(after.schema, before.schema);
            assert_eq!(after.ids, before.ids);
            assert_eq!(
                scalar(
                    connection,
                    "SELECT polpermissive::int::bigint FROM pg_policy WHERE polrelid='app.t'::regclass AND polname='gate'"
                ),
                i64::from(!restrictive)
            );
            let correct_allow = if restrictive {
                "row-security"
            } else {
                "revoke"
            };
            succeeds(approved_apply(
                &d,
                connection,
                &artifact,
                &["--allow", correct_allow],
            ));
            assert_noop(&d, connection);
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_policy WHERE polrelid='app.t'::regclass AND polname='gate'"
                ),
                0
            );
        }
    });
}
