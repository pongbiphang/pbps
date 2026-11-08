use super::{ArtifactView, PlanView, admit, seal, singleton};
use pbps_dialect::MinimalDialect;
use pbps_model::{
    Change, ChangeSet, IdsFile, PlanBaseline, PlanMode, PlanOrigin, PlannedChange, SavedPlan,
};
use std::path::Path;

fn wire() -> SavedPlan {
    SavedPlan::new(
        PlanOrigin::Database,
        "minimal",
        "2026-10-22T00:00:00Z",
        PlanBaseline {
            description: "target".into(),
            checksum: "baseline".into(),
            database_collation: None,
            time_zone: None,
        },
        ChangeSet {
            changes: vec![PlannedChange::new(Change::CreateRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: "reader".into(),
            })],
        },
        IdsFile::default(),
    )
}

fn refusal(plan: SavedPlan, approved: &str) -> String {
    admit(plan, &MinimalDialect, Path::new("plan.json"), approved)
        .unwrap_err()
        .to_string()
}

#[test]
fn admission_accepts_a_matching_plan_without_changing_its_order_or_checksum() {
    for mode in [PlanMode::Transactional, PlanMode::Staged] {
        let mut plan = wire();
        plan.mode = mode;
        let checksum = plan.wire_fingerprint();
        let admitted = admit(
            plan.clone(),
            &MinimalDialect,
            Path::new("plan.json"),
            &checksum,
        )
        .unwrap();
        let executable: &dyn PlanView<MinimalDialect> = &admitted;
        assert_eq!(executable.mode(), mode);
        assert_eq!(admitted.wire(), &plan);
        assert_eq!(admitted.checksum(), checksum);
    }
}

#[test]
fn admission_refuses_an_unapproved_checksum_before_preview_or_dialect_checks() {
    let mut plan = wire();
    let checksum = plan.wire_fingerprint();
    let error = refusal(plan.clone(), "approved");
    assert!(error.contains("approved checksum: approved"));
    assert!(error.contains(&format!("plan checksum now: {checksum}")));
    plan.origin = PlanOrigin::Preview;
    plan.dialect = "another".into();
    let checksum = plan.wire_fingerprint();
    assert_eq!(
        refusal(plan, "approved"),
        format!(
            "`plan.json` no longer matches the artifact approved at the deployment gate.\n\
             approved checksum: approved\n\
             plan checksum now: {checksum}\n\
             Do not approve the new value implicitly: retrieve the reviewed artifact or run \
             `pbps explain --plan ...` and send it through approval again."
        )
    );
}

#[test]
fn admission_refuses_a_preview_before_dialect_or_payload_checks() {
    let mut plan = wire();
    plan.origin = PlanOrigin::Preview;
    let expected = "`plan.json` is an offline preview, not an applyable plan.\n\
                    Recompute it against the target with `pbps plan --db ... --out plan.json`.";
    assert_eq!(refusal(plan.clone(), &plan.wire_fingerprint()), expected);
    plan.dialect = "another".into();
    plan.mode = PlanMode::Staged;
    plan.changes.changes.clear();
    let checksum = plan.wire_fingerprint();
    assert_eq!(refusal(plan, &checksum), expected);
}

#[test]
fn admission_refuses_another_dialect_before_payload_checks_and_escapes_its_name() {
    let mut plan = wire();
    plan.dialect = "another\nengine".into();
    let expected = "`plan.json` was computed for another\\nengine and this project is minimal";
    assert_eq!(refusal(plan.clone(), &plan.wire_fingerprint()), expected);
    plan.mode = PlanMode::Staged;
    plan.changes.changes.clear();
    let checksum = plan.wire_fingerprint();
    assert_eq!(refusal(plan, &checksum), expected);
}

#[test]
fn admission_refuses_an_old_version_before_checksum_checks() {
    let mut plan = wire();
    plan.version = 0;
    assert_eq!(
        refusal(plan, "unapproved"),
        format!(
            "`plan.json` is a version 0 plan and this tool understands version {}.\n\
             Recompute it with `pbps plan --db ... --out ...` and review the new artifact for approval.",
            pbps_model::plan::CURRENT_VERSION
        )
    );
}

#[test]
fn admission_still_refuses_invalid_payloads_after_the_outer_checks_pass() {
    let mut plan = wire().staged();
    plan.changes.changes.clear();
    let checksum = plan.wire_fingerprint();
    assert!(
        refusal(plan, &checksum)
            .contains("a staged plan must contain exactly one logical change; this artifact has 0")
    );
}

#[test]
fn sealing_refuses_a_mode_mismatch_in_both_directions_by_name() {
    for mode in [PlanMode::Transactional, PlanMode::Staged] {
        let mut plan = wire();
        let ordered = singleton(plan.changes.changes[0].clone(), mode, &MinimalDialect);
        plan.mode = match mode {
            PlanMode::Transactional => PlanMode::Staged,
            PlanMode::Staged => PlanMode::Transactional,
        };
        let expected = format!(
            "the saved plan mode {} does not match the ordered plan mode {mode}",
            plan.mode,
        );
        assert_eq!(seal(ordered, plan).unwrap_err().to_string(), expected);
    }
}

#[test]
fn sealing_still_refuses_payload_edits_with_a_matching_mode() {
    let mut plan = wire();
    let ordered = singleton(plan.changes.changes[0].clone(), plan.mode, &MinimalDialect);
    plan.changes.changes.clear();
    assert_eq!(
        seal(ordered, plan).unwrap_err().to_string(),
        "the resolver ordering proof does not describe this change sequence"
    );
}

#[test]
fn sealing_refuses_relabeling_the_ordered_engine_and_escapes_the_wire_name() {
    let mut plan = wire();
    let ordered = singleton(plan.changes.changes[0].clone(), plan.mode, &MinimalDialect);
    plan.dialect = "another\nengine".into();
    assert_eq!(
        seal(ordered, plan).unwrap_err().to_string(),
        "the saved plan dialect another\\nengine does not match the ordered plan dialect minimal"
    );
}

#[test]
fn rebuild_parts_without_an_intent_record_are_still_refused_at_admission() {
    let changes: Vec<Change> = serde_json::from_str(include_str!(
        "../../../pbps-model/tests/fixtures/rebuild_operations.json"
    ))
    .unwrap();
    use super::rebuild_tests::fixture as recipe_fixture;
    for change in changes
        .into_iter()
        .chain([Change::BuildPartitionedIndex(Box::new(
            recipe_fixture::index_recipe(),
        ))])
    {
        let name = change.table().unwrap().shown().to_string();
        let mut plan = wire();
        plan.changes = ChangeSet {
            changes: vec![PlannedChange::new(change)],
        };
        let error = refusal(plan.clone(), &plan.wire_fingerprint());
        assert!(
            error.contains("cannot be admitted") && error.contains(&name),
            "{error}"
        );
        assert!(refusal(plan, "wrong checksum").contains("approved checksum"));
    }
}

fn rebuild_step(uid: &str, tmp: &str, step: pbps_model::EnumRebuildStep) -> (IdsFile, Change) {
    let uid: pbps_model::Uid = uid.parse().unwrap();
    let name: pbps_model::TypeRef = "app.e".parse().unwrap();
    let ids = IdsFile {
        types: [("y_000001".parse().unwrap(), name.clone())].into(),
        ..Default::default()
    };
    let change = Change::EnumRebuild {
        uid,
        name,
        tmp: tmp.parse().unwrap(),
        step,
    };
    (ids, change)
}

/// The swap drops one type and renames another to its name, so an edited plan
/// must not be able to aim it at a type that is not the replacement pbps made
/// (DEC-1635.7).
#[test]
fn an_edited_rebuild_cannot_point_its_swap_at_another_type() {
    use pbps_model::{EnumRebuildStep, ManagedType};
    let swap = || EnumRebuildStep::Swap {
        definition: Box::new(ManagedType::Enum { values: vec![] }),
        comment: None,
        depth: 0,
        over: None,
    };
    let (ids, change) = rebuild_step("y_000001", "app.pbps_rebuild_y_000001", swap());
    assert!(super::saved::rebuild_is_well_formed(&ids, &change));
    for (uid, tmp) in [
        ("y_000001", "app.users"),
        ("y_000001", "other.pbps_rebuild_y_000001"),
        ("y_000002", "app.pbps_rebuild_y_000001"),
    ] {
        let (ids, change) = rebuild_step(uid, tmp, swap());
        assert!(
            !super::saved::rebuild_is_well_formed(&ids, &change),
            "{uid} {tmp}"
        );
    }
    // A retype names a column, not another kind of object.
    let retype = |column: &str| EnumRebuildStep::Retype {
        column_uid: column.parse().unwrap(),
        column: "app.t.m".parse().unwrap(),
        to: pbps_model::ColumnType::reference("app.e".parse().unwrap()),
        array: false,
        values: Box::new(pbps_model::ValueChange::Relabel { removed: vec![] }),
    };
    let (ids, change) = rebuild_step("y_000001", "app.pbps_rebuild_y_000001", retype("c_aaaaaa"));
    assert!(super::saved::rebuild_is_well_formed(&ids, &change));
    let (ids, change) = rebuild_step("y_000001", "app.pbps_rebuild_y_000001", retype("t_aaaaaa"));
    assert!(!super::saved::rebuild_is_well_formed(&ids, &change));
}

fn package_plan() -> SavedPlan {
    use pbps_model::{Extension, ExtensionPin, ExtensionProvider};
    let definition = Extension {
        schema: "ext".into(),
        version: "1.5".into(),
    };
    let mut plan = wire();
    plan.changes.changes = vec![
        PlannedChange::new(Change::CreateExtension {
            uid: "e_000001".parse().unwrap(),
            name: "cube".into(),
            definition: definition.clone(),
            requires: Default::default(),
        }),
        PlannedChange::new(Change::CreateExtension {
            uid: "e_000002".parse().unwrap(),
            name: "earthdistance".into(),
            definition: definition.clone(),
            requires: ["e_000001".parse().unwrap()].into(),
        }),
    ];
    plan.ids.extensions = [
        ("e_000001".parse().unwrap(), "cube".into()),
        ("e_000002".parse().unwrap(), "earthdistance".into()),
    ]
    .into();
    plan.extension_pins = ["e_000001", "e_000002"]
        .iter()
        .zip(["cube", "earthdistance"])
        .map(|(uid, name)| ExtensionPin {
            uid: uid.parse().unwrap(),
            name: name.into(),
            definition: definition.clone(),
            manifest: ExtensionProvider {
                definition: definition.clone(),
                requires: Default::default(),
                members: Default::default(),
            },
        })
        .collect();
    plan
}

/// Each create is sealed with exactly one manifest that says the same thing
/// the change does, so no edited part of a plan can send a statement to the
/// vet with another package's evidence (DEC-2447.6).
#[test]
fn every_create_has_exactly_its_own_pin_and_every_pin_its_create() {
    let check = |plan: &SavedPlan| super::saved::package_pins_match_their_creates(plan);
    assert!(check(&package_plan()).is_ok());
    type Edit = (&'static str, fn(&mut SavedPlan));
    let edits: [Edit; 9] = [
        ("a create with no pin", |p| {
            p.extension_pins.pop();
        }),
        ("a pin with no create", |p| {
            p.changes.changes.pop();
        }),
        ("a pin twice", |p| {
            let again = p.extension_pins[0].clone();
            p.extension_pins.push(again);
        }),
        ("another version in the pin", |p| {
            p.extension_pins[0].definition.version = "1.6".into();
        }),
        ("another schema in the manifest", |p| {
            p.extension_pins[1].manifest.definition.schema = "other".into();
        }),
        ("another name in the pin", |p| {
            p.extension_pins[0].name = "hstore".into();
        }),
        ("a uid the ids do not hold", |p| {
            p.ids.extensions.remove(&"e_000001".parse().unwrap());
        }),
        ("an edge to a package the plan does not create", |p| {
            if let Change::CreateExtension { requires, .. } = &mut p.changes.changes[1].change {
                requires.insert("e_000009".parse().unwrap());
            }
        }),
        ("an edge to itself", |p| {
            if let Change::CreateExtension { requires, .. } = &mut p.changes.changes[0].change {
                requires.insert("e_000001".parse().unwrap());
            }
        }),
    ];
    for (what, edit) in edits {
        let mut plan = package_plan();
        edit(&mut plan);
        let error = check(&plan).expect_err(what).to_string();
        assert!(
            error.contains("extension evidence disagrees"),
            "{what}: {error}"
        );
    }
}

/// The check is part of admission, not only a function that exists: an edited
/// plan whose create lost its pin is refused by `admit`, before a statement,
/// and the plan as written gets past it.
#[test]
fn admission_refuses_a_create_whose_pin_was_removed() {
    let mut plan = package_plan();
    plan.extension_pins.pop();
    let checksum = plan.wire_fingerprint();
    let error = refusal(plan, &checksum);
    assert!(error.contains("extension evidence disagrees"), "{error}");
    let whole = package_plan();
    let checksum = whole.wire_fingerprint();
    assert!(admit(whole, &MinimalDialect, Path::new("plan.json"), &checksum).is_ok());
}

/// A plan that drops `cube` and `earthdistance` (which requires it): each drop
/// is sealed with one manifest, the plan's identities tombstone both with a
/// reason, and the requirer is dropped by the same plan.
fn drop_plan() -> SavedPlan {
    let definition = pbps_model::Extension {
        schema: "ext".into(),
        version: "1.0".into(),
    };
    let mut plan = wire();
    plan.changes.changes = vec![
        PlannedChange::new(Change::DropExtension {
            uid: "e_000002".parse().unwrap(),
            name: "earthdistance".into(),
            definition: definition.clone(),
            requirers: Default::default(),
            releases: Default::default(),
            index_releases: Default::default(),
        }),
        PlannedChange::new(Change::DropExtension {
            uid: "e_000001".parse().unwrap(),
            name: "cube".into(),
            definition: definition.clone(),
            requirers: ["e_000002".parse().unwrap()].into(),
            releases: Default::default(),
            index_releases: Default::default(),
        }),
    ];
    plan.ids.tombstones = [("e_000001", "cube"), ("e_000002", "earthdistance")]
        .into_iter()
        .map(|(uid, name)| {
            (
                uid.parse().unwrap(),
                pbps_model::Tombstone {
                    require: None,
                    was: name.into(),
                    dropped_at: "2026-11-02".into(),
                    reason: "retired".into(),
                    operator: "test".into(),
                },
            )
        })
        .collect();
    plan.extension_drops = ["e_000001", "e_000002"]
        .iter()
        .zip(["cube", "earthdistance"])
        .map(|(uid, name)| pbps_model::ExtensionDropPin {
            uid: uid.parse().unwrap(),
            name: name.into(),
            definition: definition.clone(),
            manifest: pbps_model::ExtensionProvider {
                definition: definition.clone(),
                requires: Default::default(),
                members: Default::default(),
            },
            outside: Default::default(),
        })
        .collect();
    plan
}

/// Each drop is sealed with exactly one manifest that says the same thing the
/// change does, recorded by a tombstone with a reason and no live entry, and a
/// requirer it waits for is dropped by the plan too (DEC-2447.8).
#[test]
fn every_drop_has_exactly_its_own_pin_a_tombstone_and_the_requirers_it_waits_for() {
    let check = |plan: &SavedPlan| super::saved::package_pins_match_their_drops(plan);
    assert!(check(&drop_plan()).is_ok());
    type Edit = (&'static str, fn(&mut SavedPlan));
    let edits: [Edit; 9] = [
        ("a drop with no pin", |p| {
            p.extension_drops.pop();
        }),
        ("a pin with no drop", |p| {
            p.changes.changes.pop();
        }),
        ("a pin twice", |p| {
            let again = p.extension_drops[0].clone();
            p.extension_drops.push(again);
        }),
        ("another version in the pin", |p| {
            p.extension_drops[0].definition.version = "1.6".into();
        }),
        ("another schema in the manifest", |p| {
            p.extension_drops[1].manifest.definition.schema = "other".into();
        }),
        ("no tombstone", |p| {
            p.ids.tombstones.remove(&"e_000001".parse().unwrap());
        }),
        ("a tombstone with a blank reason", |p| {
            p.ids
                .tombstones
                .get_mut(&"e_000001".parse().unwrap())
                .unwrap()
                .reason = "  ".into();
        }),
        ("the package still live in the ids", |p| {
            p.ids
                .extensions
                .insert("e_000001".parse().unwrap(), "cube".into());
        }),
        ("a requirer the plan does not drop", |p| {
            if let Change::DropExtension { requirers, .. } = &mut p.changes.changes[1].change {
                requirers.insert("e_000009".parse().unwrap());
            }
        }),
    ];
    for (what, edit) in edits {
        let mut plan = drop_plan();
        edit(&mut plan);
        let error = check(&plan).expect_err(what).to_string();
        assert!(
            error.contains("extension evidence disagrees"),
            "{what}: {error}"
        );
    }
}

/// The check is part of admission: an edited plan whose drop lost its pin is
/// refused by `admit`, and the plan as written gets past it.
#[test]
fn admission_refuses_a_drop_whose_pin_was_removed() {
    let mut plan = drop_plan();
    plan.extension_drops.pop();
    let checksum = plan.wire_fingerprint();
    let error = refusal(plan, &checksum);
    assert!(error.contains("extension evidence disagrees"), "{error}");
    let whole = drop_plan();
    let checksum = whole.wire_fingerprint();
    assert!(admit(whole, &MinimalDialect, Path::new("plan.json"), &checksum).is_ok());
}

/// A plan that creates `cube` and updates `earthdistance`, whose target version
/// requires it: the update is sealed with one pin whose versions, manifests and
/// path agree with the change, and the package it requires is created by the
/// same plan.
fn update_plan() -> SavedPlan {
    let at = |version: &str| pbps_model::Extension {
        schema: "ext".into(),
        version: version.into(),
    };
    let manifest = |version: &str| pbps_model::ExtensionProvider {
        definition: at(version),
        requires: ["cube".to_owned()].into(),
        members: Default::default(),
    };
    let mut plan = wire();
    plan.changes.changes = vec![
        PlannedChange::new(Change::CreateExtension {
            uid: "e_000001".parse().unwrap(),
            name: "cube".into(),
            definition: at("1.5"),
            requires: Default::default(),
        }),
        PlannedChange::new(Change::UpdateExtension {
            uid: "e_000002".parse().unwrap(),
            name: "earthdistance".into(),
            from: at("1.1"),
            to: at("1.3"),
            requires: ["e_000001".parse().unwrap()].into(),
            releases: Default::default(),
        }),
    ];
    plan.ids.extensions = [("e_000001", "cube"), ("e_000002", "earthdistance")]
        .into_iter()
        .map(|(uid, name)| (uid.parse().unwrap(), name.into()))
        .collect();
    plan.extension_pins = vec![pbps_model::ExtensionPin {
        uid: "e_000001".parse().unwrap(),
        name: "cube".into(),
        definition: at("1.5"),
        manifest: pbps_model::ExtensionProvider {
            definition: at("1.5"),
            requires: Default::default(),
            members: Default::default(),
        },
    }];
    plan.extension_changes = vec![pbps_model::ExtensionChangePin {
        uid: "e_000002".parse().unwrap(),
        name: "earthdistance".into(),
        from: at("1.1"),
        to: at("1.3"),
        path: ["1.1", "1.2", "1.3"].map(str::to_owned).to_vec(),
        before: manifest("1.1"),
        after: manifest("1.3"),
    }];
    plan
}

/// Each update is sealed with exactly one pin that says the same thing the
/// change does, over the versions the engine walks, and a package it requires
/// is created by the plan (DEC-2447.10).
#[test]
fn every_update_has_exactly_its_own_pin_a_path_and_the_packages_it_requires() {
    let check = |plan: &SavedPlan| super::saved::package_pins_match_their_updates(plan);
    assert!(check(&update_plan()).is_ok());
    type Edit = (&'static str, fn(&mut SavedPlan));
    let edits: [Edit; 16] = [
        ("an update with no pin", |p| {
            p.extension_changes.pop();
        }),
        ("a pin with no update", |p| {
            p.changes.changes.pop();
        }),
        ("a pin twice", |p| {
            let again = p.extension_changes[0].clone();
            p.extension_changes.push(again);
        }),
        ("another version to go to", |p| {
            p.extension_changes[0].to.version = "1.2".into();
        }),
        ("another version to come from", |p| {
            p.extension_changes[0].from.version = "1.0".into();
        }),
        ("a manifest of another version before", |p| {
            p.extension_changes[0].before.definition.version = "1.0".into();
        }),
        ("a manifest of another version after", |p| {
            p.extension_changes[0].after.definition.schema = "other".into();
        }),
        ("a path that starts elsewhere", |p| {
            p.extension_changes[0].path[0] = "1.0".into();
        }),
        ("a path that ends elsewhere", |p| {
            p.extension_changes[0].path.pop();
        }),
        ("an empty path", |p| {
            p.extension_changes[0].path.clear();
        }),
        ("another identity for the package", |p| {
            p.ids
                .extensions
                .insert("e_000002".parse().unwrap(), "other".into());
        }),
        ("another schema for the whole update", |p| {
            if let Change::UpdateExtension { to, .. } = &mut p.changes.changes[1].change {
                to.schema = "other".into();
            }
            p.extension_changes[0].to.schema = "other".into();
            p.extension_changes[0].after.definition.schema = "other".into();
        }),
        ("a pin that goes where the change does not", |p| {
            p.extension_changes[0].to.version = "1.2".into();
            p.extension_changes[0].after.definition.version = "1.2".into();
            p.extension_changes[0].path[2] = "1.2".into();
        }),
        ("a pin that comes from where the change does not", |p| {
            p.extension_changes[0].from.version = "1.0".into();
            p.extension_changes[0].before.definition.version = "1.0".into();
            p.extension_changes[0].path[0] = "1.0".into();
        }),
        ("a pin for another package", |p| {
            p.extension_changes[0].name = "other".into();
        }),
        ("a required package the plan does not create", |p| {
            p.changes.changes.remove(0);
            p.extension_pins.clear();
        }),
    ];
    for (what, edit) in edits {
        let mut plan = update_plan();
        edit(&mut plan);
        let error = check(&plan).expect_err(what).to_string();
        assert!(
            error.contains("extension evidence disagrees"),
            "{what}: {error}"
        );
    }
}

/// The check is part of admission: an edited plan whose update lost its pin is
/// refused by `admit`, and the plan as written gets past it.
#[test]
fn admission_refuses_an_update_whose_pin_was_removed() {
    let mut plan = update_plan();
    plan.extension_changes.pop();
    let checksum = plan.wire_fingerprint();
    let error = refusal(plan, &checksum);
    assert!(error.contains("extension evidence disagrees"), "{error}");
    let whole = update_plan();
    let checksum = whole.wire_fingerprint();
    assert!(admit(whole, &MinimalDialect, Path::new("plan.json"), &checksum).is_ok());
}

/// A plan that moves `earthdistance` from `ext` to `app` (and, when `with_update`
/// is set, updates it from 1.1 to 1.3 first): the one pin covers the whole chain
/// (DEC-2447.11).
fn move_plan(with_update: bool) -> SavedPlan {
    let at = |schema: &str, version: &str| pbps_model::Extension {
        schema: schema.into(),
        version: version.into(),
    };
    let manifest = |schema: &str, version: &str| pbps_model::ExtensionProvider {
        definition: at(schema, version),
        requires: Default::default(),
        members: Default::default(),
    };
    let uid: pbps_model::Uid = "e_000002".parse().unwrap();
    let (first, last) = if with_update {
        ("1.1", "1.3")
    } else {
        ("1.3", "1.3")
    };
    let mut plan = wire();
    plan.changes.changes = Vec::new();
    if with_update {
        plan.changes
            .changes
            .push(PlannedChange::new(Change::UpdateExtension {
                uid: uid.clone(),
                name: "earthdistance".into(),
                from: at("ext", first),
                to: at("ext", last),
                requires: Default::default(),
                releases: Default::default(),
            }));
    }
    plan.changes
        .changes
        .push(PlannedChange::new(Change::SetExtensionSchema {
            uid: uid.clone(),
            name: "earthdistance".into(),
            from: at("ext", last),
            to: at("app", last),
            releases: Default::default(),
        }));
    plan.ids.extensions = [(uid.clone(), "earthdistance".into())].into();
    plan.extension_changes = vec![pbps_model::ExtensionChangePin {
        uid,
        name: "earthdistance".into(),
        from: at("ext", first),
        to: at("app", last),
        path: if with_update {
            ["1.1", "1.2", "1.3"].map(str::to_owned).to_vec()
        } else {
            Vec::new()
        },
        before: manifest("ext", first),
        after: manifest("app", last),
    }];
    plan
}

/// A move is sealed with one pin whose path is empty, and one that follows an
/// update is the same pin spanning both statements; every part that drifts from
/// the chain is refused (DEC-2447.11).
#[test]
fn every_move_has_one_pin_over_its_whole_chain_and_each_drift_is_refused() {
    let check = |plan: &SavedPlan| super::saved::package_pins_match_their_updates(plan);
    assert!(check(&move_plan(false)).is_ok());
    assert!(check(&move_plan(true)).is_ok());
    type Edit = (&'static str, bool, fn(&mut SavedPlan));
    let edits: [Edit; 11] = [
        ("a move with no pin", false, |p| {
            p.extension_changes.pop();
        }),
        ("a pin that goes to another schema", false, |p| {
            p.extension_changes[0].to.schema = "other".into();
            p.extension_changes[0].after.definition.schema = "other".into();
        }),
        ("a pin that starts in another schema", false, |p| {
            p.extension_changes[0].from.schema = "other".into();
            p.extension_changes[0].before.definition.schema = "other".into();
        }),
        ("a path on a move alone", false, |p| {
            p.extension_changes[0].path = vec!["1.3".into()];
        }),
        ("a move that changes the version", false, |p| {
            if let Change::SetExtensionSchema { to, .. } = &mut p.changes.changes[0].change {
                to.version = "1.4".into();
            }
        }),
        ("a move that stays in its schema", false, |p| {
            if let Change::SetExtensionSchema { to, .. } = &mut p.changes.changes[0].change {
                to.schema = "ext".into();
            }
        }),
        ("a move twice", false, |p| {
            let again = p.changes.changes[0].clone();
            p.changes.changes.push(again);
        }),
        ("an update the pin path does not follow", true, |p| {
            p.extension_changes[0].path.pop();
        }),
        (
            "a move that starts elsewhere than the update ends",
            true,
            |p| {
                if let Change::SetExtensionSchema { from, .. } = &mut p.changes.changes[1].change {
                    from.version = "1.2".into();
                }
            },
        ),
        ("a pin that ignores the move", true, |p| {
            p.extension_changes[0].to.schema = "ext".into();
            p.extension_changes[0].after.definition.schema = "ext".into();
        }),
        ("a pin that ignores the update", true, |p| {
            p.extension_changes[0].from.version = "1.3".into();
            p.extension_changes[0].before.definition.version = "1.3".into();
        }),
    ];
    for (what, with_update, edit) in edits {
        let mut plan = move_plan(with_update);
        edit(&mut plan);
        let error = check(&plan).expect_err(what).to_string();
        assert!(
            error.contains("extension evidence disagrees"),
            "{what}: {error}"
        );
    }
}
