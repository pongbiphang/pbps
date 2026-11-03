//! A package is updated by an explicit, gated `ALTER EXTENSION ... UPDATE TO`:
//! the plan rehearses the real update on a scratch that holds the version the
//! target holds, seals the manifest before and after and the versions the engine
//! walks, and `apply` vets both in the statement's own transaction and proves
//! nothing that depended on the package was lost (#2447, DEC-2447.10). Every
//! case runs on both pinned engines. The packages are the SQL-only ones in
//! `scripts/fixtures/pg-history695`: `upd2447` adds a member and replaces one,
//! `updcascade2447` drops a member `CASCADE`, `updreq2447` goes to a version that
//! requires `zzz_base`.

use super::rehearsal2379::{Fixture, Kind, Options};
use super::{approved_apply, code, json_output, on_server, stderr, stdout, succeeds, text_of};
use std::process::Output;

fn servers(mut check: impl FnMut(&str, &str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).unwrap(), variable);
    }
}

fn said(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

/// A project that holds `name` at `version` on its target and on its scratch
/// (the externals file installs it on both), adopted the way an operator does.
fn adopted(server: &str, slug: &str, name: &str, version: &str, options: &Options<'_>) -> Fixture {
    let externals = format!(
        "CREATE SCHEMA IF NOT EXISTS ext; CREATE EXTENSION {name} SCHEMA ext VERSION '{version}'"
    );
    let f = Fixture::new(
        server,
        slug,
        Kind::Supplied,
        &Options {
            externals: Some(&externals),
            ..options.clone()
        },
    );
    succeeds(f.d.run(&["pull", "--force", "--db", f.target(), "--extension", name]));
    f.d.commit();
    succeeds(f.d.run(&[
        "baseline",
        "--db",
        f.target(),
        "--reason",
        "adopt the package",
    ]));
    f
}

/// Writes the declaration of `name` at `version` in `schema`, uncommitted.
fn write_declaration(f: &Fixture, name: &str, schema: &str, version: &str) {
    std::fs::create_dir_all(f.d.dir.join("schema/extensions")).unwrap();
    std::fs::write(
        f.d.dir.join(format!("schema/extensions/{name}.yml")),
        format!("extension: {name}\nschema: {schema}\nversion: \"{version}\"\n"),
    )
    .unwrap();
}

/// [`write_declaration`], committed.
fn declare(f: &Fixture, name: &str, schema: &str, version: &str) {
    write_declaration(f, name, schema, version);
    f.d.commit();
}

/// The newest ledger entry is a failure, and still holds the package as it was.
fn failed_leaving(f: &Fixture, name: &str, version: &str) {
    let latest = super::latest_snapshot(f.target());
    assert_eq!(latest.kind, pbps_model::StateKind::Failed);
    assert_eq!(latest.schema.extensions[name].version, version);
}

fn saved(f: &Fixture) -> pbps_model::SavedPlan {
    serde_json::from_slice(&std::fs::read(f.artifact()).unwrap()).unwrap()
}

fn held(f: &Fixture, names: &str) -> String {
    text_of(
        f.target(),
        &format!(
            "SELECT coalesce(string_agg(extname || '@' || n.nspname || '@' || extversion, ',' ORDER BY extname), '') \
             FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace WHERE extname IN ({names})"
        ),
    )
}

fn entries(f: &Fixture) -> String {
    text_of(f.target(), "SELECT count(*)::text FROM public.__pbps_state")
}

const ALLOW: &str = "extension-code,destructive";

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_update_is_rehearsed_sealed_gated_applied_and_the_next_plan_waits_for_the_externals_file() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("update2447-ok-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        declare(&f, "upd2447", "ext", "1.1");
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert!(
            stdout(&out).contains("Rehearsal: converged"),
            "{}",
            stdout(&out)
        );
        let plan = saved(&f);
        assert_eq!(plan.version, pbps_model::plan::CURRENT_VERSION);
        let [only] = plan.changes.changes.as_slice() else {
            panic!("{:#?}", plan.changes)
        };
        let pbps_model::Change::UpdateExtension { name, from, to, .. } = &only.change else {
            panic!("{:#?}", only.change)
        };
        assert_eq!(
            (name.raw(), from.version.as_str(), to.version.as_str()),
            ("upd2447", "1.0", "1.1")
        );
        // Gated twice over: package code runs, and nothing proves it additive.
        assert!(only.risks.contains(&pbps_model::RiskClass::ExtensionCode));
        assert!(only.risks.contains(&pbps_model::RiskClass::Destructive));
        // Sealed with the members before and after, and the versions walked.
        let [pin] = plan.extension_changes.as_slice() else {
            panic!("{:#?}", plan.extension_changes)
        };
        assert_eq!(pin.path, ["1.0", "1.1"]);
        let functions = |m: &pbps_model::ExtensionProvider| -> Vec<String> {
            m.members
                .iter()
                .filter(|m| m.class == "function")
                .map(|m| m.names.last().unwrap().clone())
                .collect()
        };
        assert_eq!(functions(&pin.before), ["upd2447_one"]);
        assert_eq!(functions(&pin.after), ["upd2447_one", "upd2447_two"]);
        // The class is gated: one missing risk is named, and nothing ran.
        let refused = f.apply("destructive");
        assert_ne!(code(&refused), 0);
        assert!(
            said(&refused).contains("extension-code"),
            "{}",
            said(&refused)
        );
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
        let before = entries(&f);
        let applied = f.apply(ALLOW);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.1");
        assert_eq!(
            text_of(f.target(), "SELECT ext.upd2447_one()::text"),
            "11",
            "the update script ran"
        );
        assert_eq!(text_of(f.target(), "SELECT ext.upd2447_two()::text"), "2");
        assert_ne!(entries(&f), before, "the apply was recorded");
        // Until the externals file is raised, every plan fails the exact scratch
        // check by name, and says what to do.
        let waiting = f.plan(&[]);
        assert_ne!(code(&waiting), 0, "{}", said(&waiting));
        let message = said(&waiting);
        assert!(
            message.contains("upd2447")
                && message.contains("`1.1`")
                && message.contains("externals file"),
            "{message}"
        );
        std::fs::write(
            f.d.dir.join("externals.sql"),
            "CREATE SCHEMA ext; CREATE EXTENSION upd2447 SCHEMA ext VERSION '1.1';\n",
        )
        .unwrap();
        f.d.commit();
        let next = json_output(f.plan(&["--format", "json"]));
        assert_eq!(next["data"]["changes"], 0, "{next}");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_update_offline_or_without_a_rehearsal_is_refused_by_name_and_writes_nothing() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("update2447-no-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        let before = entries(&f);
        // Offline: no facts, no rehearsal, nothing to pin. Compared with the
        // committed revision, so the new version is not committed yet.
        write_declaration(&f, "upd2447", "ext", "1.1");
        let offline = f.d.run(&["plan"]);
        assert_ne!(code(&offline), 0, "{}", said(&offline));
        for fact in ["upd2447", "from `1.0` to `1.1`", "only against a database"] {
            assert!(said(&offline).contains(fact), "{}", said(&offline));
        }
        f.d.commit();
        // Connected, with the rehearsal turned off and with no scratch selected.
        let off = f.plan(&["--no-rehearse"]);
        assert_ne!(code(&off), 0, "{}", said(&off));
        assert!(
            said(&off).contains("`upd2447` is updated by this plan")
                && said(&off).contains("the rehearsal is turned off"),
            "{}",
            said(&off)
        );
        let unselected = adopted(
            server,
            &format!("update2447-no-{slug}-unselected"),
            "upd2447",
            "1.0",
            &Options {
                unselected: true,
                ..Default::default()
            },
        );
        declare(&unselected, "upd2447", "ext", "1.1");
        let none = unselected.plan(&[]);
        assert_ne!(code(&none), 0, "{}", said(&none));
        assert!(
            said(&none).contains("no scratch is available"),
            "{}",
            said(&none)
        );
        assert!(!f.artifact().exists() && !unselected.artifact().exists());
        assert_eq!(entries(&f), before);
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_update_with_no_path_or_to_an_unlisted_version_is_refused_by_name_before_a_plan_exists() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("update2447-path-{slug}"),
            "upd2447",
            "1.1",
            &Options::default(),
        );
        let before = entries(&f);
        // 1.1 to 1.0 has no update script: the engine would refuse it, and
        // pbps never guesses a downgrade or drops and recreates the package.
        declare(&f, "upd2447", "ext", "1.0");
        let down = f.plan(&[]);
        assert_ne!(code(&down), 0, "{}", said(&down));
        assert!(
            said(&down).contains("no update path from version `1.1` to `1.0`")
                && said(&down).contains("upd2447"),
            "{}",
            said(&down)
        );
        // A version the package does not list is named with the ones it does.
        declare(&f, "upd2447", "ext", "9.9");
        let unlisted = f.plan(&[]);
        assert_ne!(code(&unlisted), 0, "{}", said(&unlisted));
        assert!(
            said(&unlisted).contains("`9.9` is not installable")
                && said(&unlisted).contains("available versions: `1.0`, `1.1`"),
            "{}",
            said(&unlisted)
        );
        assert!(!f.artifact().exists());
        assert_eq!(entries(&f), before);
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.1");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_update_that_deletes_a_dependent_is_rolled_back_and_records_nothing() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("update2447-cascade-{slug}"),
            "updcascade2447",
            "1.0",
            &Options::default(),
        );
        declare(&f, "updcascade2447", "ext", "1.1");
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        // After the plan, something outside the project comes to depend on the
        // member the update script drops with CASCADE, which the engine allows.
        on_server(
            f.target(),
            "CREATE TABLE public.dep2447 (x int DEFAULT ext.updcascade2447_gone())",
        );
        let applied = f.apply(ALLOW);
        assert_ne!(code(&applied), 0, "{}", said(&applied));
        assert!(
            said(&applied).contains("depended on extension `updcascade2447`")
                && said(&applied).contains("rolled back"),
            "{}",
            said(&applied)
        );
        assert_eq!(held(&f, "'updcascade2447'"), "updcascade2447@ext@1.0");
        assert_eq!(
            text_of(
                f.target(),
                "SELECT count(*)::text FROM pg_attrdef WHERE adrelid = 'public.dep2447'::regclass"
            ),
            "1",
            "the dependent is still there"
        );
        failed_leaving(&f, "updcascade2447", "1.0");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_update_is_refused_when_the_packages_members_changed_after_the_plan() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("update2447-stale-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        declare(&f, "upd2447", "ext", "1.1");
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        on_server(
            f.target(),
            "CREATE FUNCTION public.later2447() RETURNS int LANGUAGE sql AS 'SELECT 1'; \
             ALTER EXTENSION upd2447 ADD FUNCTION public.later2447()",
        );
        let applied = f.apply(ALLOW);
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
fn an_update_beside_a_resolver_bound_change_is_refused_by_name_and_writes_nothing() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("update2447-both-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
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
        declare(&f, "upd2447", "ext", "1.1");
        let before = entries(&f);
        let planned = f.plan(&[]);
        assert_ne!(code(&planned), 0, "{}", said(&planned));
        let message = said(&planned);
        assert!(
            message.contains("this plan also updates extension `upd2447`")
                && message.contains("Plan the update alone first")
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
fn a_version_and_a_schema_change_in_one_plan_is_refused_with_the_two_plan_remedy() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("update2447-move-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        declare(&f, "upd2447", "moved", "1.1");
        let both = f.plan(&[]);
        assert_ne!(code(&both), 0, "{}", said(&both));
        assert!(
            said(&both).contains("together with SET SCHEMA")
                && said(&both).contains("update the package in one plan and move it in the next"),
            "{}",
            said(&both)
        );
        // The schema alone keeps its refusal; the version alone is the update.
        declare(&f, "upd2447", "moved", "1.0");
        let moved = f.plan(&[]);
        assert_ne!(code(&moved), 0, "{}", said(&moved));
        assert!(said(&moved).contains("SET SCHEMA"), "{}", said(&moved));
        assert!(!f.artifact().exists());
        declare(&f, "upd2447", "ext", "1.1");
        let update = f.plan(&[]);
        assert_eq!(code(&update), 0, "{}", said(&update));
        assert_eq!(saved(&f).extension_changes.len(), 1);
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_update_waits_for_the_package_its_target_version_requires() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("update2447-req-{slug}"),
            "updreq2447",
            "1.0",
            &Options::default(),
        );
        declare(&f, "updreq2447", "ext", "1.1");
        // The target version requires a package nobody declared or installed.
        let missing = f.plan(&[]);
        assert_ne!(code(&missing), 0, "{}", said(&missing));
        assert!(
            said(&missing).contains("requires extension `zzz_base`")
                && said(&missing).contains("never installs a prerequisite by default"),
            "{}",
            said(&missing)
        );
        // Declared, it is created first, by an edge, and the update follows.
        std::fs::write(
            f.d.dir.join("schema/extensions/zzz_base.yml"),
            "extension: zzz_base\nschema: ext\nversion: \"1.0\"\n",
        )
        .unwrap();
        succeeds(f.d.run(&["plan", "--no-dev"]));
        f.d.commit();
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let plan = saved(&f);
        let order: Vec<_> = plan
            .changes
            .changes
            .iter()
            .filter_map(|p| {
                if let pbps_model::Change::CreateExtension { name, .. } = &p.change {
                    Some(format!("create {}", name.shown()))
                } else if let pbps_model::Change::UpdateExtension { name, requires, .. } = &p.change
                {
                    Some(format!(
                        "update {} ({} required)",
                        name.shown(),
                        requires.len()
                    ))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            order,
            ["create zzz_base", "update updreq2447 (1 required)"],
            "{order:?}"
        );
        let applied = f.apply(ALLOW);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(
            held(&f, "'updreq2447', 'zzz_base'"),
            "updreq2447@ext@1.1,zzz_base@ext@1.0"
        );
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_staged_apply_vets_and_confirms_the_update_in_its_own_transaction() {
    servers(|server, slug| {
        let f = adopted(
            server,
            &format!("update2447-staged-{slug}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        declare(&f, "upd2447", "ext", "1.1");
        let out = f.plan(&["--staged"]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let original = std::fs::read_to_string(f.artifact()).unwrap();
        let staged = || {
            approved_apply(
                &f.d,
                f.target(),
                &f.artifact(),
                &["--allow", ALLOW, "--staged"],
            )
        };
        // A member the package does not hold: the statement's own transaction
        // refuses it before the update.
        let mut edited: serde_json::Value = serde_json::from_str(&original).unwrap();
        edited["extension_changes"][0]["before"]["members"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"class": "function", "names": ["ext", "forged"], "args": []}));
        std::fs::write(f.artifact(), serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        let out = staged();
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("members changed since it was planned")
                && said(&out).contains("forged"),
            "{}",
            said(&out)
        );
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
        // What the rehearsal read is held to as well: a result the update does
        // not leave rolls it back after the statement.
        let mut edited: serde_json::Value = serde_json::from_str(&original).unwrap();
        edited["extension_changes"][0]["after"]["members"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"class": "function", "names": ["ext", "forged"], "args": []}));
        std::fs::write(f.artifact(), serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        let out = staged();
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("does not match what was approved")
                && said(&out).contains("forged"),
            "{}",
            said(&out)
        );
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
        // The plan as written updates it.
        std::fs::write(f.artifact(), &original).unwrap();
        let out = staged();
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.1");
    });
}

/// Hides the one update script of `upd2447` on a container, and puts it back
/// even if an assertion fails: the package files changing between a plan and an
/// apply is the one thing the opening check can see that the baseline cannot.
struct HiddenUpdateScript {
    container: String,
}
impl HiddenUpdateScript {
    const SCRIPT: &'static str = "upd2447--1.0--1.1.sql";
    fn run(container: &str, from: &str, to: &str) {
        succeeds(
            std::process::Command::new("docker")
                .args([
                    "exec",
                    container,
                    "sh",
                    "-c",
                    &format!(
                        "set -eu; d=$(pg_config --sharedir)/extension; mv \"$d/{from}\" \"$d/{to}\""
                    ),
                ])
                .output()
                .unwrap(),
        );
    }
    fn new(container: String) -> Self {
        Self::run(
            &container,
            Self::SCRIPT,
            &format!("{}.disabled", Self::SCRIPT),
        );
        Self { container }
    }
}
impl Drop for HiddenUpdateScript {
    fn drop(&mut self) {
        Self::run(
            &self.container,
            &format!("{}.disabled", Self::SCRIPT),
            Self::SCRIPT,
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_update_is_refused_at_the_opening_when_the_engine_lost_its_path_since_the_plan() {
    servers(|server, variable| {
        let f = adopted(
            server,
            &format!("update2447-path-lost-{variable}"),
            "upd2447",
            "1.0",
            &Options::default(),
        );
        declare(&f, "upd2447", "ext", "1.1");
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let hidden =
            HiddenUpdateScript::new(std::env::var(variable.replace("_DB", "_CONTAINER")).unwrap());
        let applied = f.apply(ALLOW);
        drop(hidden);
        assert_ne!(code(&applied), 0, "{}", said(&applied));
        assert!(
            said(&applied).contains("nothing was applied")
                && said(&applied).contains("has no update path from version `1.0` to `1.1`"),
            "{}",
            said(&applied)
        );
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
        failed_leaving(&f, "upd2447", "1.0");
        // Restored, the same plan applies.
        let applied = f.apply(ALLOW);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_update_that_adds_a_member_an_object_already_holds_is_refused_by_the_rehearsal_naming_the_statement()
 {
    servers(|server, slug| {
        // The project manages `ext.upd2447_two()`, a function of its own that
        // version 1.1 of the package also brings: one name, two owners.
        let f = adopted(
            server,
            &format!("update2447-owner-{slug}"),
            "upd2447",
            "1.0",
            &Options {
                target_only: Some(
                    "CREATE FUNCTION ext.upd2447_two() RETURNS integer LANGUAGE sql IMMUTABLE RETURN 0",
                ),
                ..Default::default()
            },
        );
        assert!(
            f.d.dir
                .join("schema/ext.upd2447_two%28%29.function.yml")
                .exists(),
            "the pull took the function in as a managed one"
        );
        declare(&f, "upd2447", "ext", "1.1");
        let before = entries(&f);
        let planned = f.plan(&[]);
        assert_ne!(code(&planned), 0, "{}", said(&planned));
        assert!(
            said(&planned).contains("ALTER EXTENSION \"upd2447\" UPDATE TO '1.1'")
                && said(&planned).contains("already exists"),
            "{}",
            said(&planned)
        );
        assert!(!f.artifact().exists());
        assert_eq!(entries(&f), before);
        assert_eq!(held(&f, "'upd2447'"), "upd2447@ext@1.0");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_real_packages_update_retires_and_adds_members_and_leaves_what_depends_on_it_standing() {
    servers(|server, slug| {
        // hstore 1.4 to 1.8 is a real path (four scripts): three members added,
        // two retired (measured), with a table, a GIN index and a view over
        // them that the project does not manage.
        let f = adopted(
            server,
            &format!("update2447-hstore-{slug}"),
            "hstore",
            "1.4",
            &Options {
                target_only: Some(
                    "CREATE TABLE public.h2447 (h ext.hstore); \
                     CREATE INDEX h2447_i ON public.h2447 USING gin (h ext.gin_hstore_ops); \
                     CREATE VIEW public.h2447_v AS SELECT ext.akeys(h) FROM public.h2447",
                ),
                ..Default::default()
            },
        );
        declare(&f, "hstore", "ext", "1.8");
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let plan = saved(&f);
        let [pin] = plan.extension_changes.as_slice() else {
            panic!("{:#?}", plan.extension_changes)
        };
        assert_eq!(pin.path, ["1.4", "1.5", "1.6", "1.7", "1.8"]);
        let added = pin.after.members.difference(&pin.before.members).count();
        let retired = pin.before.members.difference(&pin.after.members).count();
        assert!(added > 0 && retired > 0, "added {added}, retired {retired}");
        let applied = f.apply(ALLOW);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(held(&f, "'hstore'"), "hstore@ext@1.8");
        assert_eq!(
            text_of(
                f.target(),
                "SELECT count(*)::text FROM pg_class WHERE relname IN ('h2447', 'h2447_i', 'h2447_v')"
            ),
            "3"
        );
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_update_waits_for_a_package_only_a_version_on_its_path_requires() {
    servers(|server, slug| {
        // 1.0 to 1.2 goes through 1.1, whose control file requires `zzz_base`;
        // the final version needs nothing (measured: the engine reads each).
        let f = adopted(
            server,
            &format!("update2447-mid-{slug}"),
            "updmid2447",
            "1.0",
            &Options::default(),
        );
        declare(&f, "updmid2447", "ext", "1.2");
        // Neither declared nor installed: refused by name, nothing planned.
        let missing = f.plan(&[]);
        assert_ne!(code(&missing), 0, "{}", said(&missing));
        assert!(
            said(&missing).contains("`updmid2447`")
                && said(&missing).contains("requires extension `zzz_base`"),
            "{}",
            said(&missing)
        );
        assert!(!f.artifact().exists());
        // Declared, it is created first and the update waits for it.
        std::fs::write(
            f.d.dir.join("schema/extensions/zzz_base.yml"),
            "extension: zzz_base\nschema: ext\nversion: \"1.0\"\n",
        )
        .unwrap();
        succeeds(f.d.run(&["plan", "--no-dev"]));
        f.d.commit();
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let plan = saved(&f);
        let order: Vec<_> = plan
            .changes
            .changes
            .iter()
            .filter_map(|p| {
                if let pbps_model::Change::CreateExtension { name, .. } = &p.change {
                    Some(format!("create {}", name.shown()))
                } else if let pbps_model::Change::UpdateExtension { name, requires, .. } = &p.change
                {
                    Some(format!(
                        "update {} ({} required)",
                        name.shown(),
                        requires.len()
                    ))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            order,
            ["create zzz_base", "update updmid2447 (1 required)"],
            "{order:?}"
        );
        assert_eq!(plan.extension_changes[0].path, ["1.0", "1.1", "1.2"]);
        let applied = f.apply(ALLOW);
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(
            held(&f, "'updmid2447', 'zzz_base'"),
            "updmid2447@ext@1.2,zzz_base@ext@1.0"
        );
    });
}
