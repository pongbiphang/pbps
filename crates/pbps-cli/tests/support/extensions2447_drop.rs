//! A recorded package is dropped by an explicit, gated `DROP EXTENSION RESTRICT`:
//! a human records why (`pbps drop-extension`), the plan seals the package's
//! manifest and what the drop reaches, `apply` vets both before the statement
//! and proves it afterwards (#2447, DEC-2447.8). Every case runs on both pinned
//! engines. The packages are the SQL-only ones in `scripts/fixtures/pg-history695`.

use super::{
    Demo, OwnDatabase, approved_apply, code, json_output, on_server, stderr, stdout, succeeds,
    text_of,
};
use std::path::Path;

fn servers(mut check: impl FnMut(&str, &str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).unwrap(), variable);
    }
}

fn said(output: &std::process::Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

fn file(d: &Demo, name: &str) -> std::path::PathBuf {
    d.dir.join(format!("schema/extensions/{name}.yml"))
}

/// The packages `names` adopted into a project the way an operator does: pulled
/// by name, committed, and baselined.
fn adopted(connection: &str, slug: &str, names: &[&str]) -> Demo {
    let d = Demo::new(slug);
    let mut args = vec!["pull", "--db", connection];
    for name in names {
        args.extend(["--extension", name]);
    }
    succeeds(d.run(&args));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "adopt the packages",
    ]));
    d
}

fn present(connection: &str, names: &str) -> String {
    text_of(
        connection,
        &format!(
            "SELECT coalesce(string_agg(extname, ',' ORDER BY extname), '') FROM pg_extension WHERE extname IN ({names})"
        ),
    )
}

fn ledger(connection: &str) -> String {
    text_of(connection, "SELECT count(*)::text FROM public.__pbps_state")
}

fn plan_to(d: &Demo, connection: &str, path: &Path) -> std::process::Output {
    d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()])
}

fn saved(path: &Path) -> pbps_model::SavedPlan {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn ids(d: &Demo) -> pbps_model::IdsFile {
    serde_json::from_slice(&std::fs::read(d.dir.join("schema.ids.json")).unwrap()).unwrap()
}

/// What a reviewer needs to know: removing the declaration alone is not a
/// drop, because the identity file would have forgotten the package without
/// anyone saying why.
fn retire(d: &Demo, name: &str, reason: &str) {
    std::fs::remove_file(file(d, name)).unwrap();
    succeeds(d.run(&["drop-extension", name, "--reason", reason]));
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_recorded_package_is_dropped_with_its_member_table_by_a_pinned_gated_plan() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("drop2447-ok-{slug}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA ext; CREATE EXTENSION tabled2447 SCHEMA ext; \
             INSERT INTO ext.tabled2447_t VALUES (1, 'a'), (2, 'b')",
        );
        let d = adopted(connection, &format!("drop2447-ok-{slug}"), &["tabled2447"]);
        let uid = ids(&d).extension_uid(&"tabled2447".into()).unwrap().clone();
        std::fs::remove_file(file(&d, "tabled2447")).unwrap();
        d.commit();

        // Without a recorded reason the removal is a blocker, naming the
        // command, and writes no plan.
        let path = d.dir.join("plan.json");
        let entries = ledger(connection);
        let refused = plan_to(&d, connection, &path);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("pbps drop-extension tabled2447 --reason")
                && said(&refused).contains("must record why"),
            "{}",
            said(&refused)
        );
        assert!(!path.exists());
        assert_eq!(ledger(connection), entries);

        succeeds(d.run(&[
            "drop-extension",
            "tabled2447",
            "--reason",
            "retired by #2447 test",
        ]));
        let recorded = ids(&d);
        let tombstone = recorded
            .tombstones
            .get(&uid)
            .expect("a tombstone for the uid");
        assert_eq!(tombstone.was, "tabled2447");
        assert_eq!(tombstone.reason, "retired by #2447 test");
        assert!(recorded.extension_uid(&"tabled2447".into()).is_none());
        d.commit();

        let planned = plan_to(&d, connection, &path);
        assert_eq!(code(&planned), 0, "{}", said(&planned));
        let plan = saved(&path);
        assert_eq!(plan.version, pbps_model::plan::CURRENT_VERSION);
        let [only] = plan.changes.changes.as_slice() else {
            panic!("{:#?}", plan.changes)
        };
        assert!(matches!(
            &only.change,
            pbps_model::Change::DropExtension { uid: u, name, .. } if *u == uid && name.raw() == "tabled2447"
        ));
        assert!(
            only.risks.contains(&pbps_model::RiskClass::Destructive),
            "{:?}",
            only.risks
        );
        // Sealed with the manifest the target holds, member table included, and
        // nothing outside it.
        let [pin] = plan.extension_drops.as_slice() else {
            panic!("{:?}", plan.extension_drops)
        };
        assert!(
            pin.manifest
                .members
                .iter()
                .any(|m| m.class == "table" && m.names == ["ext", "tabled2447_t"]),
            "{:?}",
            pin.manifest
        );
        assert!(pin.outside.is_empty());
        assert!(plan.extension_pins.is_empty());
        assert!(
            plan.ids.tombstones.contains_key(&uid)
                && plan.ids.extension_uid(&"tabled2447".into()).is_none()
        );
        assert!(
            stdout(&planned).contains("drop extension tabled2447"),
            "{}",
            stdout(&planned)
        );

        // Destructive is a gate of its own: without it nothing runs.
        let gated = approved_apply(&d, connection, &path, &[]);
        assert_ne!(code(&gated), 0);
        assert!(said(&gated).contains("destructive"), "{}", said(&gated));
        assert_eq!(present(connection, "'tabled2447'"), "tabled2447");
        assert_eq!(
            text_of(connection, "SELECT count(*)::text FROM ext.tabled2447_t"),
            "2"
        );

        let applied = approved_apply(&d, connection, &path, &["--allow", "destructive"]);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(present(connection, "'tabled2447'"), "");
        assert_eq!(
            text_of(
                connection,
                "SELECT coalesce(to_regclass('ext.tabled2447_t')::text, 'gone')"
            ),
            "gone"
        );
        assert_ne!(ledger(connection), entries, "the apply was recorded");
        let next = json_output(succeeds(
            d.run(&["plan", "--db", connection, "--format", "json"]),
        ));
        assert_eq!(next["data"]["changes"], 0, "{next}");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_drop_that_would_reach_what_the_plan_does_not_remove_is_refused_by_name() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("drop2447-reach-{slug}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA ext; CREATE EXTENSION zzz_base SCHEMA ext",
        );
        let d = adopted(connection, &format!("drop2447-reach-{slug}"), &["zzz_base"]);
        // After the adoption, so no declaration holds any of them: a function
        // that depends on the package by a mark, and a view that uses a member.
        on_server(
            connection,
            "CREATE SCHEMA app; \
             CREATE FUNCTION app.marked() RETURNS int LANGUAGE sql AS 'SELECT 1'; \
             ALTER FUNCTION app.marked() DEPENDS ON EXTENSION zzz_base; \
             CREATE VIEW app.over AS SELECT ext.zzz_base_one() AS one",
        );
        retire(&d, "zzz_base", "not needed any more");
        d.commit();
        let path = d.dir.join("plan.json");
        let entries = ledger(connection);
        let out = plan_to(&d, connection, &path);
        assert_ne!(code(&out), 0, "{}", said(&out));
        let message = said(&out);
        assert!(
            message.contains("zzz_base")
                && message.contains("function app.marked")
                && message.contains("the engine would delete it too")
                && message.contains("rule app.over._RETURN")
                && message.contains("the engine would refuse"),
            "{message}"
        );
        assert!(!path.exists(), "a refused drop wrote a partial plan");
        assert_eq!(ledger(connection), entries);
        assert_eq!(present(connection, "'zzz_base'"), "zzz_base");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_managed_module_that_hangs_off_the_package_is_dropped_first_by_the_same_plan() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("drop2447-carry-{slug}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA ext; CREATE SCHEMA app; CREATE EXTENSION zzz_base SCHEMA ext; \
             CREATE FUNCTION app.marked() RETURNS int LANGUAGE sql AS 'SELECT 1'; \
             ALTER FUNCTION app.marked() DEPENDS ON EXTENSION zzz_base",
        );
        let d = adopted(connection, &format!("drop2447-carry-{slug}"), &["zzz_base"]);
        // Both the package and the function it marks are managed now. Dropping
        // the package alone would delete the function behind the plan's back.
        let module = std::fs::read_dir(d.dir.join("schema"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.to_string_lossy().contains("marked"))
            .expect("the pulled function declaration");
        retire(&d, "zzz_base", "the package goes");
        d.commit();
        let path = d.dir.join("plan.json");
        let alone = plan_to(&d, connection, &path);
        assert_ne!(code(&alone), 0, "{}", said(&alone));
        assert!(
            said(&alone).contains("function app.marked"),
            "{}",
            said(&alone)
        );
        assert!(!path.exists());

        // The function's own removal in the same plan carries it.
        std::fs::remove_file(&module).unwrap();
        d.commit();
        let planned = plan_to(&d, connection, &path);
        assert_eq!(code(&planned), 0, "{}", said(&planned));
        let plan = saved(&path);
        let kinds: Vec<&str> = plan
            .changes
            .changes
            .iter()
            .map(|p| {
                if matches!(&p.change, pbps_model::Change::DropModule { .. }) {
                    "module"
                } else if let pbps_model::Change::DropExtension { releases, .. } = &p.change {
                    assert_eq!(releases.len(), 1, "{releases:?}");
                    "package"
                } else {
                    panic!("{:?}", p.change)
                }
            })
            .collect();
        assert_eq!(kinds, ["module", "package"], "the module goes first");
        let [pin] = plan.extension_drops.as_slice() else {
            panic!("{:?}", plan.extension_drops)
        };
        assert_eq!(pin.outside.len(), 1, "{:?}", pin.outside);
        let applied = approved_apply(&d, connection, &path, &["--allow", "destructive"]);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(present(connection, "'zzz_base'"), "");
        assert_eq!(
            text_of(
                connection,
                "SELECT coalesce(to_regprocedure('app.marked()')::text, 'gone')"
            ),
            "gone"
        );
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_package_another_package_requires_is_dropped_only_with_it_and_after_it() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("drop2447-req-{slug}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA ext; CREATE EXTENSION zzz_base SCHEMA ext; CREATE EXTENSION aaa_dep SCHEMA ext",
        );
        let d = adopted(
            connection,
            &format!("drop2447-req-{slug}"),
            &["aaa_dep", "zzz_base"],
        );
        retire(&d, "zzz_base", "retire the base");
        d.commit();
        let path = d.dir.join("plan.json");
        let alone = plan_to(&d, connection, &path);
        assert_ne!(code(&alone), 0, "{}", said(&alone));
        assert!(
            said(&alone).contains("package `aaa_dep` requires it and stays"),
            "{}",
            said(&alone)
        );
        assert!(!path.exists());

        retire(&d, "aaa_dep", "retire the dependent too");
        d.commit();
        let planned = plan_to(&d, connection, &path);
        assert_eq!(code(&planned), 0, "{}", said(&planned));
        let plan = saved(&path);
        // The requirer first, though `zzz_base` sorts after `aaa_dep` by name:
        // only the edge can put them in this order.
        let order: Vec<_> = plan
            .changes
            .changes
            .iter()
            .filter_map(|p| {
                if let pbps_model::Change::DropExtension {
                    name, requirers, ..
                } = &p.change
                {
                    Some((name.raw().to_owned(), requirers.len()))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            order,
            [("aaa_dep".to_owned(), 0), ("zzz_base".to_owned(), 1)]
        );
        assert_eq!(plan.extension_drops.len(), 2);
        let applied = approved_apply(&d, connection, &path, &["--allow", "destructive"]);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(present(connection, "'aaa_dep', 'zzz_base'"), "");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_membership_that_changed_since_the_plan_stops_the_apply_before_anything_is_dropped() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("drop2447-stale-{slug}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA ext; CREATE SCHEMA app; CREATE EXTENSION tabled2447 SCHEMA ext",
        );
        let d = adopted(
            connection,
            &format!("drop2447-stale-{slug}"),
            &["tabled2447"],
        );
        retire(&d, "tabled2447", "retire it");
        d.commit();
        let path = d.dir.join("plan.json");
        succeeds(plan_to(&d, connection, &path));
        // A table added to the package after the plan was made: its rows would
        // go with it, and nobody approved that.
        on_server(
            connection,
            "CREATE TABLE app.later (id int); INSERT INTO app.later VALUES (1); \
             ALTER EXTENSION tabled2447 ADD TABLE app.later",
        );
        let entries = ledger(connection);
        let out = approved_apply(&d, connection, &path, &["--allow", "destructive"]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("members changed since it was planned")
                && said(&out).contains("app.later")
                && said(&out).contains("nothing was dropped"),
            "{}",
            said(&out)
        );
        assert_eq!(present(connection, "'tabled2447'"), "tabled2447");
        assert_eq!(
            text_of(connection, "SELECT count(*)::text FROM app.later"),
            "1"
        );
        // The refused attempt is a failure entry, not the drop recorded.
        let latest = super::latest_snapshot(connection);
        assert_eq!(latest.kind, pbps_model::StateKind::Failed);
        assert!(
            latest.schema.extensions.contains_key("tabled2447"),
            "{:?}",
            latest.schema.extensions
        );
        let _ = entries;
        // The plan is rewritten over what the package holds now, and then the
        // member table is in the sealed manifest and the drop is approved.
        succeeds(plan_to(&d, connection, &path));
        assert!(
            saved(&path).extension_drops[0]
                .manifest
                .members
                .iter()
                .any(|m| m.names == ["app", "later"])
        );
        let applied = approved_apply(&d, connection, &path, &["--allow", "destructive"]);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(present(connection, "'tabled2447'"), "");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_drop_is_planned_only_against_a_database_and_never_by_removing_a_declaration() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("drop2447-off-{slug}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA ext; CREATE EXTENSION zzz_base SCHEMA ext",
        );
        let d = adopted(connection, &format!("drop2447-off-{slug}"), &["zzz_base"]);
        let since = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&d.dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        retire(&d, "zzz_base", "retire it");
        d.commit();
        // Offline there is nothing to read what the drop reaches from: compared
        // from a commit, the drop is a change that only a database can plan.
        let offline = d.run(&[
            "plan",
            "--no-dev",
            "--since",
            &since,
            "--out",
            d.dir.join("offline.json").to_str().unwrap(),
        ]);
        let message = said(&offline);
        assert!(
            code(&offline) != 0
                && message.contains("DROP EXTENSION")
                && message.contains("plan --db"),
            "{message}"
        );
        assert!(!d.dir.join("offline.json").exists());
        // A removal that nobody recorded: not even the identity file may
        // forget it, so the next plan names it again.
        let d = adopted(connection, &format!("drop2447-off2-{slug}"), &["zzz_base"]);
        std::fs::remove_file(file(&d, "zzz_base")).unwrap();
        d.commit();
        for _ in 0..2 {
            let out = d.run(&["plan", "--db", connection]);
            assert_ne!(code(&out), 0, "{}", said(&out));
            assert!(
                said(&out).contains("pbps drop-extension zzz_base"),
                "{}",
                said(&out)
            );
        }
        assert_eq!(present(connection, "'zzz_base'"), "zzz_base");
    });
}

/// A staged apply runs the drop in a transaction of its own, vets the package
/// there before the statement, and confirms there that it is gone: a manifest
/// that is not what the package holds stops it before anything is dropped, and
/// the plan as written drops the package and records it in its identities.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_staged_apply_vets_and_confirms_the_drop_in_its_own_transaction() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("drop2447-staged-{slug}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA ext; CREATE EXTENSION tabled2447 SCHEMA ext",
        );
        let d = adopted(
            connection,
            &format!("drop2447-staged-{slug}"),
            &["tabled2447"],
        );
        retire(&d, "tabled2447", "retire it, staged");
        d.commit();
        let path = d.dir.join("plan.json");
        succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--staged",
            "--out",
            path.to_str().unwrap(),
        ]));
        let original = std::fs::read_to_string(&path).unwrap();
        let staged = || {
            approved_apply(
                &d,
                connection,
                &path,
                &["--allow", "destructive", "--staged"],
            )
        };
        // A member the package does not hold: the statement's own transaction
        // refuses it before the drop.
        let mut edited: serde_json::Value = serde_json::from_str(&original).unwrap();
        edited["extension_drops"][0]["manifest"]["members"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"class": "function", "names": ["ext", "forged"], "args": []}));
        std::fs::write(&path, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        let out = staged();
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("members changed since it was planned")
                && said(&out).contains("forged"),
            "{}",
            said(&out)
        );
        assert_eq!(present(connection, "'tabled2447'"), "tabled2447");
        // The plan as written drops it, and the next plan has nothing to do.
        std::fs::write(&path, &original).unwrap();
        let out = staged();
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert_eq!(present(connection, "'tabled2447'"), "");
        let next = json_output(succeeds(
            d.run(&["plan", "--db", connection, "--format", "json"]),
        ));
        assert_eq!(next["data"]["changes"], 0, "{next}");
    });
}

/// A project that rehearses still plans a drop: the rehearsal replays the plan
/// on the scratch, which holds the package through its externals file, and
/// converges. The drop's own evidence is the target's, not the scratch's.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_drop_is_rehearsed_when_the_project_rehearses_and_pins_the_targets_manifest() {
    use super::rehearsal2379::{Fixture, Kind, Options};
    servers(|server, slug| {
        let f = Fixture::new(
            server,
            &format!("drop2447-reh-{slug}"),
            Kind::Supplied,
            &Options {
                externals: Some("CREATE SCHEMA ext; CREATE EXTENSION zzz_base SCHEMA ext"),
                ..Default::default()
            },
        );
        succeeds(f.d.run(&[
            "pull",
            "--force",
            "--db",
            f.target(),
            "--extension",
            "zzz_base",
        ]));
        f.d.commit();
        succeeds(f.d.run(&[
            "baseline",
            "--db",
            f.target(),
            "--reason",
            "adopt the package",
        ]));
        retire(&f.d, "zzz_base", "retire it, rehearsed");
        f.d.commit();
        let planned = f.plan(&[]);
        assert_eq!(code(&planned), 0, "{}", said(&planned));
        assert!(
            stdout(&planned).contains("Rehearsal: converged"),
            "{}",
            stdout(&planned)
        );
        let plan = saved(&f.artifact());
        assert_eq!(plan.extension_drops.len(), 1);
        assert!(plan.extension_pins.is_empty());
        let applied = f.apply("destructive");
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(present(f.target(), "'zzz_base'"), "");
    });
}

/// A role that does not own the package is refused at planning, naming the owner
/// and the engine's own code, and no plan is written (DEC-2447.8).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_role_that_does_not_own_the_package_is_refused_before_a_plan_exists() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("drop2447-owner-{slug}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA ext; CREATE EXTENSION zzz_base SCHEMA ext",
        );
        let d = adopted(connection, &format!("drop2447-owner-{slug}"), &["zzz_base"]);
        retire(&d, "zzz_base", "retire it, as a stranger");
        d.commit();
        let role = format!("pbps_stranger_{}_{}", std::process::id(), slug.len());
        on_server(
            admin,
            &format!(
                "DROP ROLE IF EXISTS {role}; CREATE ROLE {role} LOGIN PASSWORD 'stranger' NOSUPERUSER"
            ),
        );
        on_server(
            connection,
            &format!(
                "GRANT USAGE ON SCHEMA public, ext TO {role}; GRANT SELECT ON ALL TABLES IN SCHEMA public TO {role}"
            ),
        );
        let stranger = connection
            .replace("user=postgres", &format!("user={role}"))
            .replace("password=Pbps!Test12345", "password=stranger");
        assert_ne!(stranger, connection);
        let path = d.dir.join("plan.json");
        let out = plan_to(&d, &stranger, &path);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("`zzz_base` is owned by `postgres`")
                && said(&out).contains("42501")
                && said(&out).contains(&role),
            "{}",
            said(&out)
        );
        assert!(!path.exists());
        assert_eq!(present(connection, "'zzz_base'"), "zzz_base");
        // Negative: the owner plans the same drop.
        let ok = plan_to(&d, connection, &path);
        assert_eq!(code(&ok), 0, "{}", said(&ok));
        on_server(connection, &format!("DROP OWNED BY {role}"));
        on_server(admin, &format!("DROP ROLE {role}"));
    });
}

/// A package drop beside a change that needs the resolver is refused by name
/// before the resolver runs, with the two-plan remedy, and writes nothing; the
/// same drop alone, with the same resolver selected, plans. Measured before the
/// guard: the combination failed late with "resolver evidence omits a required
/// prerequisite", an internal error, on 16 and 18 (DEC-2447.8).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_package_drop_beside_a_resolver_bound_change_is_refused_by_name_and_writes_nothing() {
    use super::rehearsal2379::{Fixture, Kind, Options};
    let fixture = |admin: &str, slug: &str| {
        let f = Fixture::new(
            admin,
            &format!("drop2447-{slug}"),
            Kind::Supplied,
            &Options {
                externals: Some("CREATE SCHEMA ext; CREATE EXTENSION zzz_base SCHEMA ext"),
                ..Default::default()
            },
        );
        succeeds(f.d.run(&[
            "pull",
            "--force",
            "--db",
            f.target(),
            "--extension",
            "zzz_base",
        ]));
        f.d.commit();
        succeeds(f.d.run(&[
            "baseline",
            "--db",
            f.target(),
            "--reason",
            "adopt the package",
        ]));
        f
    };
    servers(|admin, slug| {
        let f = fixture(admin, &format!("both-{slug}"));
        // A new column on a table that a kept view reads: the resolver's
        // question. It gets its identity from an offline plan first.
        let table = std::fs::read_to_string(f.d.dir.join("schema/app.t.yml")).unwrap();
        std::fs::write(
            f.d.dir.join("schema/app.t.yml"),
            table.replace(
                "  note:\n    type: text\n",
                "  note:\n    type: text\n  extra:\n    type: text\n",
            ),
        )
        .unwrap();
        succeeds(f.d.run(&["plan"]));
        f.d.commit();
        retire(&f.d, "zzz_base", "retire it beside a column");
        f.d.commit();
        let entries = ledger(f.target());
        let planned = f.plan(&[]);
        assert_ne!(code(&planned), 0, "{}", said(&planned));
        let message = said(&planned);
        assert!(
            message.contains("this plan also drops extension `zzz_base`")
                && message.contains("Plan the drop alone first")
                && !message.contains("this is a bug in pbps"),
            "{message}"
        );
        assert!(!f.artifact().exists(), "a refused plan wrote a file");
        assert_eq!(ledger(f.target()), entries);
        assert_eq!(present(f.target(), "'zzz_base'"), "zzz_base");

        // Negative: the drop alone, with the resolver selected, plans.
        let alone = fixture(admin, &format!("alone-{slug}"));
        retire(&alone.d, "zzz_base", "retire it alone");
        alone.d.commit();
        let ok = alone.plan(&[]);
        assert_eq!(code(&ok), 0, "{}", said(&ok));
        assert_eq!(saved(&alone.artifact()).extension_drops.len(), 1);
    });
}
