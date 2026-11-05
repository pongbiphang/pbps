//! A package is moved by an explicit, gated `ALTER EXTENSION ... SET SCHEMA`,
//! alone or after the update of the same plan: the plan rehearses the real move
//! on a scratch that holds the package where the target does, seals the manifest
//! before and after, refuses a destination name an unmanaged object holds, and
//! `apply` vets both in the statement's own transaction (#2447, DEC-2447.11).
//! Every case runs on both pinned engines, on the SQL-only packages of
//! `scripts/fixtures/pg-history695`: `upd2447` (versions 1.0 and 1.1) and
//! `relflip2447`, which is relocatable at 1.0 and not at 1.1.

use super::extensions2447_update::{
    adopted, declare, entries, failed_leaving, held, said, saved, servers, write_declaration,
};
use super::rehearsal2379::Options;
use super::{approved_apply, code, json_output, on_server, succeeds, text_of};

const MOVE: &str = "rename";
const BOTH: &str = "extension-code,destructive,rename";

fn functions(m: &pbps_model::ExtensionProvider) -> Vec<String> {
    m.members
        .iter()
        .filter(|m| m.class == "function")
        .map(|m| m.names.join("."))
        .collect()
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_move_is_rehearsed_sealed_gated_applied_and_the_next_plan_waits_for_the_externals_file() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("move2447-ok-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        on_server(f.target(), "CREATE SCHEMA moved");
        declare(&f, "upd2447", "moved", "1.0");
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert!(
            super::stdout(&out).contains("Rehearsal: converged"),
            "{}",
            super::stdout(&out)
        );
        let plan = saved(&f);
        assert_eq!(plan.version, pbps_model::plan::CURRENT_VERSION);
        let [only] = plan.changes.changes.as_slice() else {
            panic!("{:#?}", plan.changes)
        };
        let pbps_model::Change::SetExtensionSchema { name, from, to, .. } = &only.change else {
            panic!("{:#?}", only.change)
        };
        assert_eq!(name.raw(), "upd2447");
        assert_eq!((from.schema.as_str(), to.schema.as_str()), ("ext", "moved"));
        assert_eq!(from.version, to.version);
        // A rename of the package's members, not package code that runs.
        assert!(only.risks.contains(&pbps_model::RiskClass::Rename));
        assert!(!only.risks.contains(&pbps_model::RiskClass::ExtensionCode));
        // Sealed with the members before and after, and no version path.
        let [pin] = plan.extension_changes.as_slice() else {
            panic!("{:#?}", plan.extension_changes)
        };
        assert!(pin.path.is_empty());
        assert_eq!(functions(&pin.before), ["ext.upd2447_one"]);
        assert_eq!(functions(&pin.after), ["moved.upd2447_one"]);
        // The class is gated and nothing ran without it.
        let refused = f.apply("destructive");
        assert_ne!(code(&refused), 0);
        assert!(said(&refused).contains("rename"), "{}", said(&refused));
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
        let before = entries(&f);
        let applied = f.apply(MOVE);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(held(&f, "'upd2447'"), "upd2447@moved@1.0");
        assert_eq!(text_of(f.target(), "SELECT moved.upd2447_one()::text"), "1");
        assert_ne!(entries(&f), before, "the apply was recorded");
        // Until the externals file puts the package where it is now, every plan
        // fails the exact scratch check by name.
        let waiting = f.plan(&[]);
        assert_ne!(code(&waiting), 0, "{}", said(&waiting));
        assert!(
            said(&waiting).contains("upd2447") && said(&waiting).contains("externals file"),
            "{}",
            said(&waiting)
        );
        std::fs::write(
            f.d.dir.join("externals.sql"),
            "CREATE SCHEMA moved; CREATE EXTENSION upd2447 SCHEMA moved VERSION '1.0';\n",
        )
        .unwrap();
        f.d.commit();
        let next = json_output(f.plan(&["--format", "json"]));
        assert_eq!(next["data"]["changes"], 0, "{next}");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_version_and_a_schema_change_in_one_plan_update_first_then_move_under_one_pin() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("move2447-both-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        on_server(f.target(), "CREATE SCHEMA moved");
        declare(&f, "upd2447", "moved", "1.1");
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let plan = saved(&f);
        let kinds: Vec<String> = plan
            .changes
            .changes
            .iter()
            .filter_map(|p| {
                if let pbps_model::Change::UpdateExtension { from, to, .. } = &p.change {
                    Some(format!(
                        "update {} {} to {} {}",
                        from.version, from.schema, to.version, to.schema
                    ))
                } else if let pbps_model::Change::SetExtensionSchema { from, to, .. } = &p.change {
                    Some(format!(
                        "move {} {} to {}",
                        from.version, from.schema, to.schema
                    ))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            kinds,
            ["update 1.0 ext to 1.1 ext", "move 1.1 ext to moved"]
        );
        // One pin spans both statements.
        let [pin] = plan.extension_changes.as_slice() else {
            panic!("{:#?}", plan.extension_changes)
        };
        assert_eq!(pin.path, ["1.0", "1.1"]);
        assert_eq!(
            (pin.from.schema.as_str(), pin.to.schema.as_str()),
            ("ext", "moved")
        );
        assert_eq!(functions(&pin.before), ["ext.upd2447_one"]);
        assert_eq!(
            functions(&pin.after),
            ["moved.upd2447_one", "moved.upd2447_two"]
        );
        // Every class of both is gated.
        let refused = f.apply("extension-code,destructive");
        assert_ne!(code(&refused), 0);
        assert!(said(&refused).contains("rename"), "{}", said(&refused));
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
        let applied = f.apply(BOTH);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(held(&f, "'upd2447'"), "upd2447@moved@1.1");
        assert_eq!(
            text_of(f.target(), "SELECT moved.upd2447_one()::text"),
            "11"
        );
        assert_eq!(text_of(f.target(), "SELECT moved.upd2447_two()::text"), "2");
        // Negative: the update alone stays an update in the package's schema.
        let g = adopted(
            server,
            &format!("move2447-alone-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        declare(&g, "upd2447", "ext", "1.1");
        assert_eq!(code(&g.plan(&[])), 0);
        assert_eq!(saved(&g).changes.changes.len(), 1);
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_staged_apply_vets_and_confirms_the_move_in_its_own_transaction() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("move2447-staged-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        on_server(f.target(), "CREATE SCHEMA moved");
        declare(&f, "upd2447", "moved", "1.0");
        let out = f.plan(&["--staged"]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let original = std::fs::read_to_string(f.artifact()).unwrap();
        let staged = || {
            approved_apply(
                &f.d,
                f.target(),
                &f.artifact(),
                &["--allow", MOVE, "--staged"],
            )
        };
        let tamper = |side: &str, schema: &str| {
            let mut edited: serde_json::Value = serde_json::from_str(&original).unwrap();
            edited["extension_changes"][0][side]["members"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"class": "function", "names": [schema, "forged"], "args": []}));
            std::fs::write(f.artifact(), serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        };
        // A member the package does not hold: refused before the statement.
        tamper("before", "ext");
        let out = staged();
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("members changed since it was planned")
                && said(&out).contains("forged"),
            "{}",
            said(&out)
        );
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
        // A result the move does not leave: rolled back after the statement.
        tamper("after", "moved");
        let out = staged();
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("does not match what was approved")
                && said(&out).contains("forged"),
            "{}",
            said(&out)
        );
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
        std::fs::write(f.artifact(), &original).unwrap();
        let out = staged();
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert_eq!(held(&f, "'upd2447'"), "upd2447@moved@1.0");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_destination_name_an_unmanaged_object_holds_is_refused_until_the_plan_before_it_drops_it() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("move2447-name-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        on_server(
            f.target(),
            "CREATE SCHEMA moved; \
             CREATE FUNCTION moved.upd2447_one() RETURNS int LANGUAGE sql AS 'SELECT 5'",
        );
        declare(&f, "upd2447", "moved", "1.0");
        let before = entries(&f);
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        let message = said(&out);
        assert!(
            message.contains("cannot move to schema `moved`")
                && message.contains("function moved.upd2447_one")
                && message.contains("this plan does not drop"),
            "{message}"
        );
        assert!(!f.artifact().exists(), "a refused move wrote a plan");
        assert_eq!(entries(&f), before);
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");

        // The same function, managed now. In a project with objects the resolver
        // must answer for, the drop and the move in one plan are refused by name
        // (the scratch holds the package where it is now), so the drop is planned
        // first and the move after it, which the refusal above names.
        succeeds(f.d.run(&[
            "pull",
            "--force",
            "--db",
            f.target(),
            "--extension",
            "upd2447",
        ]));
        f.d.commit();
        succeeds(f.d.run(&[
            "baseline",
            "--db",
            f.target(),
            "--reason",
            "manage the function",
        ]));
        let module = std::fs::read_dir(f.d.dir.join("schema"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.to_string_lossy().contains("upd2447_one"))
            .expect("the pulled function declaration");
        std::fs::remove_file(&module).unwrap();
        declare(&f, "upd2447", "moved", "1.0");
        let both = f.plan(&[]);
        assert_ne!(code(&both), 0, "{}", said(&both));
        assert!(
            said(&both).contains("this plan also moves extension `upd2447`"),
            "{}",
            said(&both)
        );
        declare(&f, "upd2447", "ext", "1.0");
        let dropped = f.plan(&[]);
        assert_eq!(code(&dropped), 0, "{}", said(&dropped));
        let applied = f.apply("destructive");
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        declare(&f, "upd2447", "moved", "1.0");
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert!(matches!(
            saved(&f).changes.changes.as_slice(),
            [p] if matches!(p.change, pbps_model::Change::SetExtensionSchema { .. })
        ));
        let applied = f.apply(MOVE);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(held(&f, "'upd2447'"), "upd2447@moved@1.0");
        assert_eq!(text_of(f.target(), "SELECT moved.upd2447_one()::text"), "1");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_move_that_the_package_cannot_make_is_refused_by_name_before_a_plan_exists() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("move2447-fixed-{slug}"),
            "relflip2447",
            "1.0",
            &Options::default(),
        );
        on_server(f.target(), "CREATE SCHEMA moved");
        // 1.1 is not relocatable, so the update cannot be followed by the move.
        declare(&f, "relflip2447", "moved", "1.1");
        let both = f.plan(&[]);
        assert_ne!(code(&both), 0, "{}", said(&both));
        assert!(
            said(&both).contains("cannot be updated and moved in one plan")
                && said(&both).contains("move the package in one plan and update it in the next"),
            "{}",
            said(&both)
        );
        // A destination that is not there.
        declare(&f, "relflip2447", "nosuch", "1.0");
        let missing = f.plan(&[]);
        assert_ne!(code(&missing), 0, "{}", said(&missing));
        assert!(
            said(&missing).contains("`nosuch`") && said(&missing).contains("does not exist"),
            "{}",
            said(&missing)
        );
        assert!(!f.artifact().exists(), "a refused move wrote a plan");
        assert_eq!(held(&f, "'relflip2447'"), "relflip2447@ext@1.0");
        // Negative: at 1.0 it is relocatable, and the plan is made.
        declare(&f, "relflip2447", "moved", "1.0");
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert_eq!(saved(&f).extension_changes.len(), 1);
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_move_is_refused_when_the_packages_members_changed_after_the_plan() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("move2447-stale-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        on_server(f.target(), "CREATE SCHEMA moved");
        declare(&f, "upd2447", "moved", "1.0");
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        on_server(
            f.target(),
            "CREATE FUNCTION public.later2447() RETURNS int LANGUAGE sql AS 'SELECT 1'; \
             ALTER EXTENSION upd2447 ADD FUNCTION public.later2447()",
        );
        let applied = f.apply(MOVE);
        assert_ne!(code(&applied), 0, "{}", said(&applied));
        assert!(
            said(&applied).contains("members changed since it was planned")
                && said(&applied).contains("later2447"),
            "{}",
            said(&applied)
        );
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
        failed_leaving(&f, "upd2447", "1.0");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_move_is_refused_at_the_opening_when_the_destination_is_gone_since_the_plan() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("move2447-open-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        on_server(f.target(), "CREATE SCHEMA moved");
        declare(&f, "upd2447", "moved", "1.0");
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        on_server(f.target(), "DROP SCHEMA moved");
        let refused = f.apply(MOVE);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("nothing was applied")
                && said(&refused).contains("`moved`")
                && said(&refused).contains("does not exist"),
            "{}",
            said(&refused)
        );
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
        // Negative: with the destination back, the same plan applies.
        on_server(f.target(), "CREATE SCHEMA moved");
        let applied = f.apply(MOVE);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(held(&f, "'upd2447'"), "upd2447@moved@1.0");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_move_beside_a_resolver_bound_change_is_refused_by_name_and_writes_nothing() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("move2447-both-resolver-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        on_server(f.target(), "CREATE SCHEMA moved");
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
        declare(&f, "upd2447", "moved", "1.0");
        let before = entries(&f);
        let planned = f.plan(&[]);
        assert_ne!(code(&planned), 0, "{}", said(&planned));
        let message = said(&planned);
        assert!(
            message.contains("this plan also moves extension `upd2447`")
                && message.contains("Plan the update or move alone first")
                && !message.contains("this is a bug in pbps"),
            "{message}"
        );
        assert!(!f.artifact().exists(), "a refused plan wrote a file");
        assert_eq!(entries(&f), before);
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_move_offline_or_without_a_rehearsal_is_refused_by_name_and_writes_nothing() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("move2447-no-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        on_server(f.target(), "CREATE SCHEMA moved");
        let before = entries(&f);
        write_declaration(&f, "upd2447", "moved", "1.0");
        let offline = f.d.run(&["plan"]);
        assert_ne!(code(&offline), 0, "{}", said(&offline));
        for fact in [
            "upd2447",
            "from `ext` to `moved`",
            "only against a database",
        ] {
            assert!(said(&offline).contains(fact), "{}", said(&offline));
        }
        f.d.commit();
        let off = f.plan(&["--no-rehearse"]);
        assert_ne!(code(&off), 0, "{}", said(&off));
        assert!(
            said(&off).contains("`upd2447` is moved by this plan")
                && said(&off).contains("the rehearsal is turned off"),
            "{}",
            said(&off)
        );
        assert!(!f.artifact().exists());
        assert_eq!(entries(&f), before);
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_move_is_rolled_back_when_its_result_is_not_the_sealed_one() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("move2447-after-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        on_server(f.target(), "CREATE SCHEMA moved");
        declare(&f, "upd2447", "moved", "1.0");
        assert_eq!(code(&f.plan(&[])), 0);
        let original = std::fs::read_to_string(f.artifact()).unwrap();
        let mut edited: serde_json::Value = serde_json::from_str(&original).unwrap();
        edited["extension_changes"][0]["after"]["members"]
            .as_array_mut()
            .unwrap()
            .push(
                serde_json::json!({"class": "function", "names": ["moved", "forged"], "args": []}),
            );
        std::fs::write(f.artifact(), serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        let out = approved_apply(&f.d, f.target(), &f.artifact(), &["--allow", MOVE]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("does not match what was approved")
                && said(&out).contains("forged"),
            "{}",
            said(&out)
        );
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
        // Negative: the plan as written moves it.
        std::fs::write(f.artifact(), &original).unwrap();
        let out = approved_apply(&f.d, f.target(), &f.artifact(), &["--allow", MOVE]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert_eq!(held(&f, "'upd2447'"), "upd2447@moved@1.0");
    });
}
