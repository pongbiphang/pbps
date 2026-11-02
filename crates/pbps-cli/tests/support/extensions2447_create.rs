//! A declared package is created by a pinned `CREATE EXTENSION`: planned with
//! a rehearsal, sealed with the members the install script made, gated, and
//! vetted by `apply` in the transaction that installs it (#2447, DEC-2447.6).
//! Every case runs on both pinned engines, on a confined account's database on
//! the target's own cluster, which is the scratch of the same major version.
//!
//! The packages are the SQL-only ones in `scripts/fixtures/pg-history695`:
//! `aaa_dep` requires `zzz_base`, which sorts after it by name, so only the
//! prerequisite edge can put the two in order.

use super::rehearsal2379::{Fixture, Kind, Options};
use super::{code, json_output, on_server, plan_checksum, stderr, stdout, succeeds, text_of};
use std::process::Output;

fn servers(mut check: impl FnMut(&str, &str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).unwrap(), variable);
    }
}

fn said(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

fn fixture(server: &str, slug: &str, options: &Options<'_>) -> Fixture {
    let f = Fixture::new(server, slug, Kind::Supplied, options);
    on_server(f.target(), "CREATE SCHEMA ext");
    f
}

/// Declares `name` at `version` in schema `ext` and lets an offline plan mint
/// its identity, which is how a first declaration gets one.
fn declare(f: &Fixture, name: &str, version: &str) {
    let dir = f.d.dir.join("schema/extensions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.yml")),
        format!("extension: {name}\nschema: ext\nversion: \"{version}\"\n"),
    )
    .unwrap();
}

fn minted(f: &Fixture) {
    succeeds(f.d.run(&["plan"]));
    f.d.commit();
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

const BOTH: &str = "'aaa_dep', 'zzz_base'";

fn ledger_entries(f: &Fixture) -> String {
    text_of(f.target(), "SELECT count(*)::text FROM public.__pbps_state")
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_declared_package_is_created_after_its_prerequisite_pinned_gated_and_recorded() {
    servers(|server, slug| {
        let f = fixture(
            server,
            &format!("create2447-ok-{slug}"),
            &Options::default(),
        );
        declare(&f, "aaa_dep", "1.0");
        declare(&f, "zzz_base", "1.0");
        minted(&f);
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert!(
            stdout(&out).contains("Rehearsal: converged"),
            "{}",
            stdout(&out)
        );
        let plan = saved(&f);
        assert_eq!(plan.version, pbps_model::plan::CURRENT_VERSION);
        // The prerequisite first, by an edge: `aaa_dep` sorts ahead by name.
        let order: Vec<_> = plan
            .changes
            .changes
            .iter()
            .filter_map(|p| {
                if let pbps_model::Change::CreateExtension { name, requires, .. } = &p.change {
                    Some((name.raw().to_owned(), requires.len(), p.risks.clone()))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            order.iter().map(|o| o.0.as_str()).collect::<Vec<_>>(),
            ["zzz_base", "aaa_dep"],
            "{order:?}"
        );
        assert_eq!((order[0].1, order[1].1), (0, 1));
        assert!(
            order
                .iter()
                .all(|o| o.2.contains(&pbps_model::RiskClass::ExtensionCode))
        );
        // Both are sealed with the members the scratch held after the replay.
        assert_eq!(plan.extension_pins.len(), 2);
        let manifest = |name: &str| {
            plan.extension_pins
                .iter()
                .find(|p| p.name.raw() == name)
                .unwrap()
        };
        assert!(
            manifest("zzz_base")
                .manifest
                .members
                .iter()
                .any(|m| m.class == "function" && m.names.last().unwrap() == "zzz_base_one"),
            "{:?}",
            manifest("zzz_base").manifest
        );
        assert_eq!(
            manifest("aaa_dep").manifest.requires,
            ["zzz_base".to_owned()].into()
        );
        // The class is gated: the risk is named, and nothing was installed.
        let refused = f.apply("destructive");
        assert_ne!(code(&refused), 0);
        assert!(
            said(&refused).contains("extension-code"),
            "{}",
            said(&refused)
        );
        assert_eq!(held(&f, BOTH), "");
        let before = ledger_entries(&f);
        let applied = f.apply("extension-code");
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        assert_eq!(held(&f, BOTH), "aaa_dep@ext@1.0,zzz_base@ext@1.0");
        assert_eq!(
            text_of(f.target(), "SELECT ext.aaa_dep_two()::text"),
            "2",
            "the package's own code ran"
        );
        assert_ne!(ledger_entries(&f), before, "the apply was recorded");
        // The next plan has nothing to do: the packages are held, recorded and
        // identified.
        // (Not rehearsed: a scratch that stands for a target holding managed
        // packages has them from its externals file, as before this change.)
        let next = json_output(f.plan(&["--no-rehearse", "--format", "json"]));
        assert_eq!(next["data"]["changes"], 0, "{next}");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_create_without_a_rehearsal_or_a_database_is_refused_by_name_and_writes_nothing() {
    servers(|server, slug| {
        let f = fixture(
            server,
            &format!("create2447-no-{slug}"),
            &Options::default(),
        );
        declare(&f, "zzz_base", "1.0");
        minted(&f);
        let before = ledger_entries(&f);
        // Turned off, and with no scratch entry selected.
        for (label, extra, options, reason) in [
            (
                "off",
                vec!["--no-rehearse"],
                None,
                "the rehearsal is turned off",
            ),
            (
                "unselected",
                vec![],
                Some(Options {
                    unselected: true,
                    ..Default::default()
                }),
                "no scratch is available",
            ),
        ] {
            let other;
            let used = match &options {
                Some(options) => {
                    other = fixture(server, &format!("create2447-no-{slug}-{label}"), options);
                    declare(&other, "zzz_base", "1.0");
                    minted(&other);
                    &other
                }
                None => &f,
            };
            let out = used.plan(&extra);
            assert_ne!(code(&out), 0, "{label}: {}", said(&out));
            let message = said(&out);
            // The refusal before the plan is built, not the later one that finds
            // no manifest to pin.
            assert!(
                message.contains("`zzz_base`") && message.contains(reason),
                "{label}: {message}"
            );
            assert!(
                !used.artifact().exists(),
                "{label}: a refused plan wrote a file"
            );
        }
        // A preview with no database has no scratch to read the members from:
        // it shows the create but cannot be applied, and a script or a dev
        // rehearsal of it refuses by name.
        let script = f.d.dir.join("plan.sql");
        let root = std::process::Command::new("git")
            .arg("-C")
            .arg(&f.d.dir)
            .args(["rev-list", "--max-parents=0", "HEAD"])
            .output()
            .unwrap();
        let root = String::from_utf8(root.stdout).unwrap();
        let since = ["--since", root.trim()];
        let out = f.d.run(&[
            "plan",
            "--sql",
            script.to_str().unwrap(),
            since[0],
            since[1],
        ]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(said(&out).contains("`zzz_base`"), "{}", said(&out));
        assert!(!script.exists());
        // A dev rehearsal of it refuses by name before any engine is asked.
        let dev =
            f.d.run(&["plan", since[0], since[1], "--dev", "docker://unused"]);
        assert_ne!(code(&dev), 0, "{}", said(&dev));
        assert!(
            said(&dev).contains("`zzz_base`")
                && said(&dev).contains("planned only against a database"),
            "{}",
            said(&dev)
        );
        let preview = succeeds(f.d.run(&["plan", since[0], since[1]]));
        assert!(
            stdout(&preview).contains("create extension zzz_base"),
            "{}",
            stdout(&preview)
        );
        assert_eq!(ledger_entries(&f), before);
        assert_eq!(held(&f, BOTH), "");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn what_the_target_cannot_install_is_named_before_a_plan_exists() {
    servers(|server, slug| {
        // A version the server does not list names the ones it does.
        let f = fixture(
            server,
            &format!("create2447-version-{slug}"),
            &Options::default(),
        );
        declare(&f, "zzz_base", "9.9");
        minted(&f);
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        let message = said(&out);
        assert!(
            message.contains("`zzz_base`")
                && message.contains("`9.9`")
                && message.contains("available versions: `1.0`"),
            "{message}"
        );
        assert!(!f.artifact().exists());
        // A prerequisite the declarations neither pin nor the target holds.
        let f = fixture(
            server,
            &format!("create2447-needs-{slug}"),
            &Options::default(),
        );
        declare(&f, "aaa_dep", "1.0");
        minted(&f);
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("requires extension `zzz_base`"),
            "{}",
            said(&out)
        );
        // A schema the target does not have.
        let f = fixture(
            server,
            &format!("create2447-schema-{slug}"),
            &Options::default(),
        );
        std::fs::create_dir_all(f.d.dir.join("schema/extensions")).unwrap();
        std::fs::write(
            f.d.dir.join("schema/extensions/zzz_base.yml"),
            "extension: zzz_base\nschema: nowhere\nversion: \"1.0\"\n",
        )
        .unwrap();
        minted(&f);
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("schema `nowhere`") && said(&out).contains("does not have"),
            "{}",
            said(&out)
        );
        // A package that is already installed is never adopted by a create.
        let f = fixture(
            server,
            &format!("create2447-held-{slug}"),
            &Options::default(),
        );
        on_server(f.target(), "CREATE EXTENSION zzz_base SCHEMA ext");
        declare(&f, "zzz_base", "1.0");
        minted(&f);
        let out = f.plan(&[]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(said(&out).contains("already installed"), "{}", said(&out));
        // Negative: the same declaration, with the schema and a free name, plans.
        let f = fixture(
            server,
            &format!("create2447-fine-{slug}"),
            &Options::default(),
        );
        declare(&f, "zzz_base", "1.0");
        minted(&f);
        succeeds(f.plan(&[]));
    });
}

/// The members are sealed and the engine is held to them in the transaction
/// that installs the package. A plan whose manifest differs from what the
/// script makes (here one a hand edit gave a member the script does not make,
/// with the checksum approved over the edited file) is rolled back whole.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_install_that_differs_from_its_sealed_manifest_is_rolled_back_and_not_recorded() {
    servers(|server, slug| {
        let f = fixture(
            server,
            &format!("create2447-vet-{slug}"),
            &Options::default(),
        );
        declare(&f, "zzz_base", "1.0");
        minted(&f);
        succeeds(f.plan(&[]));
        let original = std::fs::read_to_string(f.artifact()).unwrap();
        let mut edited: serde_json::Value = serde_json::from_str(&original).unwrap();
        let members = edited["extension_pins"][0]["manifest"]["members"]
            .as_array_mut()
            .unwrap();
        members.push(serde_json::json!({
            "class": "function",
            "names": ["ext", "zzz_base_two"],
            "args": []
        }));
        std::fs::write(f.artifact(), serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        let before = ledger_entries(&f);
        let out = f.apply("extension-code");
        assert_ne!(code(&out), 0, "{}", said(&out));
        let message = said(&out);
        assert!(
            message.contains("`zzz_base`")
                && message.contains("does not match what was approved")
                && message.contains("zzz_base_two"),
            "{message}"
        );
        assert!(message.contains("rolled back"), "{message}");
        assert_eq!(held(&f, BOTH), "", "the install was taken back");
        // The failed attempt is recorded as a failure and not as the package.
        assert_eq!(
            text_of(
                f.target(),
                "SELECT count(*)::text FROM pg_proc WHERE proname = 'zzz_base_one'"
            ),
            "0"
        );
        let _ = before;
        // Negative: the plan as written installs.
        std::fs::write(f.artifact(), &original).unwrap();
        assert_eq!(plan_checksum(&f.artifact()), {
            let raw: pbps_model::SavedPlan = serde_json::from_str(&original).unwrap();
            raw.wire_fingerprint()
        });
        succeeds(f.apply("extension-code"));
        assert_eq!(held(&f, "'zzz_base'"), "zzz_base@ext@1.0");
    });
}

/// A name the target holds and pbps does not manage is a collision the scratch
/// cannot see: the engine refuses it inside the apply's transaction, and the
/// message says which statement.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_member_name_an_unmanaged_object_holds_fails_the_install_and_rolls_back() {
    servers(|server, slug| {
        let f = fixture(
            server,
            &format!("create2447-clash-{slug}"),
            &Options::default(),
        );
        declare(&f, "zzz_base", "1.0");
        minted(&f);
        succeeds(f.plan(&[]));
        // Made after the plan, so the plan is the one the target allowed.
        on_server(
            f.target(),
            "CREATE FUNCTION ext.zzz_base_one() RETURNS integer LANGUAGE sql AS 'SELECT 7'",
        );
        let out = f.apply("extension-code");
        assert_ne!(code(&out), 0, "{}", said(&out));
        let message = said(&out);
        assert!(
            message.contains("CREATE EXTENSION \"zzz_base\"") && message.contains("already exists"),
            "{message}"
        );
        assert_eq!(held(&f, "'zzz_base'"), "");
        assert_eq!(
            text_of(f.target(), "SELECT ext.zzz_base_one()::text"),
            "7",
            "the other object stands, untouched"
        );
    });
}

/// The plan is not the permission: what the target allowed when it was made is
/// asked again under the lock, before the first statement, with the planner's
/// own sentences. A package that appeared since is refused and nothing is
/// written.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_apply_asks_the_target_again_and_installs_nothing_when_it_changed() {
    servers(|server, slug| {
        let f = fixture(
            server,
            &format!("create2447-again-{slug}"),
            &Options::default(),
        );
        declare(&f, "zzz_base", "1.0");
        minted(&f);
        succeeds(f.plan(&[]));
        let before = ledger_entries(&f);
        // Another session installs the package between plan and apply.
        on_server(f.target(), "CREATE EXTENSION zzz_base SCHEMA ext");
        let out = f.apply("extension-code");
        assert_ne!(code(&out), 0, "{}", said(&out));
        let message = said(&out);
        assert!(
            message.contains("nothing was applied")
                && message.contains("`zzz_base`")
                && message.contains("already installed"),
            "{message}"
        );
        assert_eq!(held(&f, "'zzz_base'"), "zzz_base@ext@1.0");
        // The refused attempt is a failure entry that records no package.
        let latest = super::latest_snapshot(f.target());
        assert_eq!(latest.kind, pbps_model::StateKind::Failed);
        assert!(latest.schema.extensions.is_empty() && latest.ids.extensions.is_empty());
        let _ = before;
        // The schema going away is refused the same way.
        let g = fixture(
            server,
            &format!("create2447-gone-{slug}"),
            &Options::default(),
        );
        declare(&g, "zzz_base", "1.0");
        minted(&g);
        succeeds(g.plan(&[]));
        on_server(g.target(), "DROP SCHEMA ext");
        let out = g.apply("extension-code");
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("schema `ext`") && said(&out).contains("does not have"),
            "{}",
            said(&out)
        );
    });
}

/// A staged apply runs the statement in a transaction of its own and vets it
/// there, so a package that differs from its manifest is taken back by that
/// statement's own rollback, and the plan as written installs and is
/// checkpointed with the package in its identities.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_staged_apply_vets_the_install_in_its_own_transaction() {
    let staged = |f: &Fixture| {
        let checksum = plan_checksum(&f.artifact());
        std::process::Command::new(super::BIN)
            .arg("--project")
            .arg(&f.d.dir)
            .args(["apply", "--env", "prod", "--plan"])
            .arg(f.artifact())
            .args([
                "--checksum",
                &checksum,
                "--allow",
                "extension-code",
                "--staged",
            ])
            .env("PBPS_REH_TARGET", f.target())
            .output()
            .unwrap()
    };
    servers(|server, slug| {
        let f = fixture(
            server,
            &format!("create2447-staged-{slug}"),
            &Options::default(),
        );
        declare(&f, "zzz_base", "1.0");
        minted(&f);
        let planned = f.plan(&["--staged"]);
        assert_eq!(code(&planned), 0, "{}", said(&planned));
        let original = std::fs::read_to_string(f.artifact()).unwrap();
        // Another member than the script makes: the statement's own
        // transaction takes the install back, and nothing is recorded.
        let mut edited: serde_json::Value = serde_json::from_str(&original).unwrap();
        edited["extension_pins"][0]["manifest"]["members"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"class": "function", "names": ["ext", "zzz_base_two"], "args": []}));
        std::fs::write(f.artifact(), serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        let out = staged(&f);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("does not match what was approved"),
            "{}",
            said(&out)
        );
        assert_eq!(held(&f, "'zzz_base'"), "");
        // The plan as written installs, and the next plan has nothing to do.
        std::fs::write(f.artifact(), &original).unwrap();
        let out = staged(&f);
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert_eq!(held(&f, "'zzz_base'"), "zzz_base@ext@1.0");
        let next = json_output(f.plan(&["--no-rehearse", "--format", "json"]));
        assert_eq!(next["data"]["changes"], 0, "{next}");
    });
}

/// A preview is never applyable. The saved plan of an offline `plan` holds the
/// create with no pin; `apply --plan` refuses it as a preview, and one edited to
/// claim a database origin (with the checksum approved over the edit) is
/// refused at admission for the create with no pin. Neither writes anything.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_offline_saved_plan_holding_a_create_is_refused_by_apply_and_writes_nothing() {
    servers(|server, slug| {
        let f = fixture(
            server,
            &format!("create2447-offline-{slug}"),
            &Options::default(),
        );
        declare(&f, "zzz_base", "1.0");
        minted(&f);
        let root = std::process::Command::new("git")
            .arg("-C")
            .arg(&f.d.dir)
            .args(["rev-list", "--max-parents=0", "HEAD"])
            .output()
            .unwrap();
        let root = String::from_utf8(root.stdout).unwrap();
        let out = f.d.run(&[
            "plan",
            "--since",
            root.trim(),
            "--out",
            f.artifact().to_str().unwrap(),
        ]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let plan = saved(&f);
        assert_eq!(plan.origin, pbps_model::PlanOrigin::Preview);
        assert!(plan.extension_pins.is_empty());
        assert!(
            plan.changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::CreateExtension { .. }))
        );
        let before = ledger_entries(&f);
        let untouched = |f: &Fixture| {
            assert_eq!(held(f, BOTH), "");
            assert_eq!(ledger_entries(f), before);
        };
        let out = f.apply("extension-code");
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(said(&out).contains("offline preview"), "{}", said(&out));
        untouched(&f);
        // The same file made to claim a database origin: the create has no pin.
        let mut edited: serde_json::Value =
            serde_json::from_slice(&std::fs::read(f.artifact()).unwrap()).unwrap();
        edited["origin"] = "database".into();
        std::fs::write(f.artifact(), serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        let out = f.apply("extension-code");
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("extension evidence disagrees"),
            "{}",
            said(&out)
        );
        untouched(&f);
    });
}

/// A plan that creates a package and also changes names a kept surface could
/// bind (here a new table, beside a kept view) needs the resolver's answer, and
/// the resolver cannot describe a package the target lacks. It used to fail
/// late with "a bug in pbps"; it is refused by name before the resolver runs
/// (DEC-2447.6). The two plans taken in order work.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn create_and_a_binding_change_in_one_plan_is_refused_by_name() {
    servers(|server, slug| {
        let f = fixture(
            server,
            &format!("create2447-both-{slug}"),
            &Options::default(),
        );
        declare(&f, "zzz_base", "1.0");
        std::fs::write(
            f.d.dir.join("schema/app.t2.yml"),
            "table: app.t2\ncolumns:\n  id: {type: integer, nullable: false}\nprimary_key: {name: pk_t2, columns: [id]}\n",
        )
        .unwrap();
        minted(&f);
        let before = ledger_entries(&f);
        let out = f.plan(&[]);
        assert_eq!(code(&out), 2, "{}", said(&out));
        let message = said(&out);
        assert!(
            message.contains("`zzz_base`")
                && message.contains("Plan the package alone first")
                && message.contains("externals file"),
            "{message}"
        );
        assert!(
            !message.contains("bug in pbps"),
            "the refusal came from the internal error: {message}"
        );
        assert!(!f.artifact().exists(), "a refused plan wrote a file");
        assert_eq!(ledger_entries(&f), before);
        assert_eq!(held(&f, BOTH), "");

        // The remedy, in order: the package alone, applied; then the scratch
        // holds it through its externals file and the rest plans.
        let g = fixture(
            server,
            &format!("create2447-order-{slug}"),
            &Options::default(),
        );
        declare(&g, "zzz_base", "1.0");
        minted(&g);
        let out = g.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let out = g.apply("extension-code");
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert_eq!(held(&g, "'zzz_base'"), "zzz_base@ext@1.0");
        std::fs::write(
            g.d.dir.join("externals.sql"),
            "CREATE SCHEMA ext; CREATE EXTENSION zzz_base SCHEMA ext VERSION '1.0';\n",
        )
        .unwrap();
        let config = std::fs::read_to_string(g.d.dir.join("pbps.yml")).unwrap();
        std::fs::write(
            g.d.dir.join("pbps.yml"),
            config.replace(
                "    url_env: PBPS_REH_SCRATCH\n",
                "    url_env: PBPS_REH_SCRATCH\n    externals: externals.sql\n",
            ),
        )
        .unwrap();
        std::fs::write(
            g.d.dir.join("schema/app.t2.yml"),
            "table: app.t2\ncolumns:\n  id: {type: integer, nullable: false}\nprimary_key: {name: pk_t2, columns: [id]}\n",
        )
        .unwrap();
        minted(&g);
        let out = g.plan(&[]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        let plan = saved(&g);
        assert!(plan.extension_pins.is_empty());
        assert!(matches!(
            plan.analysis,
            pbps_model::resolver::PlanAnalysis::Resolved(_)
        ));
    });
}
