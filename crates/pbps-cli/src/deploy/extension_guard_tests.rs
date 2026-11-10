use super::*;

/// What is sealed is what the rehearsal read, and a package whose members
/// the declarations also manage as their own is refused before it is
/// sealed, never sealed with a manifest the engine will not honour
/// (DEC-2447.6, DEC-2447.7). A create the rehearsal read nothing for is
/// refused too, not sealed empty.
#[test]
fn a_manifest_is_sealed_only_for_a_create_the_rehearsal_read_and_nobody_else_owns() {
    let definition = pbps_model::Extension {
        schema: "ext".into(),
        version: "1.5".into(),
    };
    let uid: pbps_model::Uid = "e_aaaaaa".parse().unwrap();
    let changes = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateExtension {
                uid: uid.clone(),
                name: "cube".into(),
                definition: definition.clone(),
                requires: Default::default(),
            },
        )],
    };
    let ids = IdsFile {
        extensions: [(uid, "cube".into())].into(),
        ..Default::default()
    };
    let read = |member: &str| -> crate::rehearsal::connected::Manifests {
        [(
            "cube".into(),
            pbps_model::ExtensionProvider {
                definition: definition.clone(),
                requires: Default::default(),
                members: [pbps_model::ProviderMember {
                    class: "table".into(),
                    names: vec!["ext".into(), member.into()],
                    args: Vec::new(),
                }]
                .into(),
            },
        )]
        .into()
    };
    let mut declared = Schema::default();
    declared
        .tables
        .insert(TableName::new("app", "ordinary"), Default::default());
    let pins = extension_pins(&changes, read("cube_t"), &declared, &ids).unwrap();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].manifest.members.len(), 1);
    // The declarations manage a table the package would also own.
    declared
        .tables
        .insert(TableName::new("ext", "cube_t"), Default::default());
    let error = extension_pins(&changes, read("cube_t"), &declared, &ids)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("`cube`")
            && error.contains("table ext.cube_t")
            && error.contains("two owners"),
        "{error}"
    );
    // Nothing read for a create is a refusal and not an empty manifest.
    let error = extension_pins(&changes, Default::default(), &Schema::default(), &ids)
        .unwrap_err()
        .to_string();
    assert!(error.contains("read no members"), "{error}");
}

/// A plan that installs a package lets exactly that package appear, in
/// exactly the pinned definition, and fails the close if it did not
/// (DEC-2447.6). Without this a create that never took effect, or took
/// effect as another version, would be recorded as applied.
/// The package a plan drops stands as pinned until its statement and is
/// gone from the close on; a drop that did not take effect fails the
/// closing read, and no other package may move (DEC-2447.8).
#[test]
fn a_dropped_package_stands_as_pinned_until_its_statement_and_is_gone_at_the_close() {
    let pinned = pbps_model::Extension {
        schema: "ext".into(),
        version: "1.5".into(),
    };
    let changes = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::DropExtension {
                uid: "e_aaaaaa".parse().unwrap(),
                name: "cube".into(),
                definition: pinned.clone(),
                requirers: Default::default(),
                releases: Default::default(),
                index_releases: Default::default(),
            },
        )],
    };
    let mut before = Schema::default();
    for (name, version) in [("cube", "1.5"), ("standing", "1.0")] {
        before.extensions.insert(
            name.into(),
            pbps_model::Extension {
                schema: "ext".into(),
                version: version.into(),
            },
        );
    }
    let ids = guard::fixture_ids(&changes, &before);
    let plan = guard::Plan {
        changes: &changes,
        ids: &ids,
    };
    let staged = guard::Staged::new(plan).unwrap();
    let after_with = |extension: Option<pbps_model::Extension>| {
        let mut after = before.clone();
        after.extensions.remove("cube");
        after
            .extensions
            .extend(extension.map(|e| ("cube".into(), e)));
        after
    };
    let check = |read: guard::Read<'_>, after: &Schema| {
        refuse_pinned_movement(&pbps_pg::Postgres::new(), read, &before, after, "fixture")
            .map_err(|e| e.to_string())
    };
    let closing = || guard::Read::transactional(plan);
    let midway = || {
        staged.read(StagedRead::Checkpoint {
            completed: 0,
            total: 1,
        })
    };
    // Gone at the close and at a checkpoint after the statement; standing
    // as pinned at a checkpoint before it.
    check(closing(), &after_with(None)).unwrap();
    check(midway(), &after_with(None)).unwrap();
    check(midway(), &after_with(Some(pinned.clone()))).unwrap();
    // Still there at the close: the drop did not take effect.
    let error = check(closing(), &after_with(Some(pinned.clone()))).unwrap_err();
    assert!(
        error.contains("`cube`") && error.contains("promised"),
        "{error}"
    );
    // Another version or schema standing is refused wherever it is read.
    for other in [
        pbps_model::Extension {
            version: "1.6".into(),
            ..pinned.clone()
        },
        pbps_model::Extension {
            schema: "other".into(),
            ..pinned.clone()
        },
    ] {
        for read in [closing(), midway()] {
            let error = check(read, &after_with(Some(other.clone()))).unwrap_err();
            assert!(error.contains("promised"), "{error}");
        }
    }
    // The drop of one package excuses no other moving.
    let mut moved = after_with(None);
    moved.extensions.get_mut("standing").unwrap().version = "2.0".into();
    assert!(check(closing(), &moved).unwrap_err().contains("`standing`"));
    let mut appeared = after_with(None);
    appeared
        .extensions
        .insert("unplanned".into(), pinned.clone());
    assert!(
        check(closing(), &appeared)
            .unwrap_err()
            .contains("`unplanned`")
    );
}

#[test]
fn an_update_that_would_leave_a_managed_name_or_another_version_is_refused_before_it_is_sealed() {
    let at = |version: &str| pbps_model::Extension {
        schema: "ext".into(),
        version: version.into(),
    };
    let member = |name: &str| pbps_model::ProviderMember {
        class: "function".into(),
        names: vec!["ext".into(), name.into()],
        args: Vec::new(),
    };
    let after = pbps_model::ExtensionProvider {
        definition: at("1.1"),
        requires: Default::default(),
        members: [member("pkg_one"), member("pkg_two")].into(),
    };
    let mut declared = Schema::default();
    declared.modules.insert(
        "ext.pkg_two()".parse().unwrap(),
        pbps_model::Module {
            kind: pbps_model::ModuleKind::Function,
            description: None,
            definition: "() RETURNS int LANGUAGE sql RETURN 2".into(),
            options: Default::default(),
        },
    );
    let name: pbps_model::ObjectLocalName = "pkg".into();
    let error = refuse_two_owners(&name, &after, &at("1.1"), &declared)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("`pkg`") && error.contains("pkg_two") && error.contains("two owners"),
        "{error}"
    );
    // Negatives: an unrelated declaration is fine, and so is the same
    // manifest when nothing manages its members; another version is not.
    refuse_two_owners(&name, &after, &at("1.1"), &Schema::default()).unwrap();
    assert!(refuse_two_owners(&name, &after, &at("1.2"), &Schema::default()).is_err());
}

#[test]
fn an_updated_package_stands_at_its_old_or_new_version_and_at_the_close_at_the_new_one() {
    let at = |version: &str| pbps_model::Extension {
        schema: "ext".into(),
        version: version.into(),
    };
    let changes = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::UpdateExtension {
                uid: "e_aaaaaa".parse().unwrap(),
                name: "cube".into(),
                from: at("1.4"),
                to: at("1.5"),
                requires: Default::default(),
                releases: Default::default(),
            },
        )],
    };
    let mut before = Schema::default();
    before.extensions.insert("cube".into(), at("1.4"));
    before.extensions.insert("standing".into(), at("1.0"));
    let ids = guard::fixture_ids(&changes, &before);
    let plan = guard::Plan {
        changes: &changes,
        ids: &ids,
    };
    let staged = guard::Staged::new(plan).unwrap();
    let held = |version: &str| {
        let mut after = before.clone();
        after.extensions.insert("cube".into(), at(version));
        after
    };
    let check = |read: guard::Read<'_>, after: &Schema| {
        refuse_pinned_movement(&pbps_pg::Postgres::new(), read, &before, after, "fixture")
            .map_err(|e| e.to_string())
    };
    let closing = || guard::Read::transactional(plan);
    let midway = || {
        staged.read(StagedRead::Checkpoint {
            completed: 0,
            total: 1,
        })
    };
    // At the new version, at the close and at a checkpoint; the old one
    // only while the plan runs.
    check(closing(), &held("1.5")).unwrap();
    check(midway(), &held("1.5")).unwrap();
    check(midway(), &held("1.4")).unwrap();
    let error = check(closing(), &held("1.4")).unwrap_err();
    assert!(
        error.contains("`cube`") && error.contains("promised"),
        "{error}"
    );
    // A third version is refused wherever it is read, and so is a schema
    // change, and a package that left.
    for other in [
        at("1.6"),
        pbps_model::Extension {
            schema: "other".into(),
            ..at("1.5")
        },
    ] {
        for read in [closing(), midway()] {
            let mut after = before.clone();
            after.extensions.insert("cube".into(), other.clone());
            assert!(check(read, &after).unwrap_err().contains("promised"));
        }
    }
    let mut gone = before.clone();
    gone.extensions.remove("cube");
    assert!(check(midway(), &gone).unwrap_err().contains("promised"));
    // The update excuses no other package moving.
    let mut moved = held("1.5");
    moved.extensions.get_mut("standing").unwrap().version = "2.0".into();
    assert!(check(closing(), &moved).unwrap_err().contains("`standing`"));
}

#[test]
fn an_updated_and_moved_package_passes_through_each_definition_and_ends_at_the_last() {
    let at = |schema: &str, version: &str| pbps_model::Extension {
        schema: schema.into(),
        version: version.into(),
    };
    let uid: pbps_model::Uid = "e_aaaaaa".parse().unwrap();
    let steps = |with_update: bool| pbps_model::ChangeSet {
        changes: with_update
            .then(|| pbps_model::Change::UpdateExtension {
                uid: uid.clone(),
                name: "cube".into(),
                from: at("ext", "1.4"),
                to: at("ext", "1.5"),
                requires: Default::default(),
                releases: Default::default(),
            })
            .into_iter()
            .chain([pbps_model::Change::SetExtensionSchema {
                uid: uid.clone(),
                name: "cube".into(),
                from: at("ext", if with_update { "1.5" } else { "1.4" }),
                to: at("app", if with_update { "1.5" } else { "1.4" }),
                releases: Default::default(),
            }])
            .map(pbps_model::PlannedChange::new)
            .collect(),
    };
    let mut before = Schema::default();
    before.extensions.insert("cube".into(), at("ext", "1.4"));
    for with_update in [true, false] {
        let changes = steps(with_update);
        let ids = guard::fixture_ids(&changes, &before);
        let plan = guard::Plan {
            changes: &changes,
            ids: &ids,
        };
        // A staged guard reads one logical change, so a midway read is made
        // for the move alone; the chain is read at its close.
        let staged = (!with_update).then(|| guard::Staged::new(plan).unwrap());
        let end = at("app", if with_update { "1.5" } else { "1.4" });
        let held = |definition: &pbps_model::Extension| {
            let mut after = before.clone();
            after.extensions.insert("cube".into(), definition.clone());
            after
        };
        let check = |read: guard::Read<'_>, after: &Schema| {
            refuse_pinned_movement(&pbps_pg::Postgres::new(), read, &before, after, "fixture")
                .map_err(|e| e.to_string())
        };
        let reads = || {
            let mut reads = vec![guard::Read::transactional(plan)];
            if let Some(staged) = &staged {
                reads.push(staged.read(StagedRead::Checkpoint {
                    completed: 0,
                    total: 1,
                }));
            }
            reads
        };
        // The end is accepted at the close; a definition on the way only while
        // the plan runs.
        for read in reads() {
            check(read, &held(&end)).unwrap();
        }
        for passing in [at("ext", "1.4"), at("ext", "1.5")] {
            if !(with_update || passing.version == "1.4") {
                continue;
            }
            let error = check(guard::Read::transactional(plan), &held(&passing)).unwrap_err();
            assert!(error.contains("`cube`"), "{error}");
            if let Some(staged) = &staged {
                check(
                    staged.read(StagedRead::Checkpoint {
                        completed: 0,
                        total: 1,
                    }),
                    &held(&passing),
                )
                .unwrap();
            }
        }
        // Negatives: a definition the chain never passes through is refused
        // wherever it is read.
        for other in [at("elsewhere", "1.4"), at("app", "9.9")] {
            for read in reads() {
                let error = check(read, &held(&other)).unwrap_err();
                assert!(error.contains("promised"), "{error}");
            }
        }
    }
}

#[test]
fn only_the_pinned_package_may_appear_and_the_close_requires_it() {
    let pinned = pbps_model::Extension {
        schema: "ext".into(),
        version: "1.5".into(),
    };
    let changes = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateExtension {
                uid: "e_aaaaaa".parse().unwrap(),
                name: "cube".into(),
                definition: pbps_model::Extension {
                    schema: "ext".into(),
                    version: "1.5".into(),
                },
                requires: Default::default(),
            },
        )],
    };
    let mut before = Schema::default();
    before.extensions.insert(
        "standing".into(),
        pbps_model::Extension {
            schema: "ext".into(),
            version: "1.0".into(),
        },
    );
    let ids = guard::fixture_ids(&changes, &before);
    let plan = guard::Plan {
        changes: &changes,
        ids: &ids,
    };
    let staged = guard::Staged::new(plan).unwrap();
    let installed = |extension: Option<pbps_model::Extension>| {
        let mut after = before.clone();
        after
            .extensions
            .extend(extension.map(|e| ("cube".into(), e)));
        after
    };
    let check = |read: guard::Read<'_>, after: &Schema| {
        refuse_pinned_movement(&pbps_pg::Postgres::new(), read, &before, after, "fixture")
            .map_err(|e| e.to_string())
    };
    let closing = || guard::Read::transactional(plan);
    let midway = || {
        staged.read(StagedRead::Checkpoint {
            completed: 0,
            total: 1,
        })
    };
    // Installed as pinned, at the close and at a checkpoint after it.
    check(closing(), &installed(Some(pinned.clone()))).unwrap();
    check(midway(), &installed(Some(pinned.clone()))).unwrap();
    // Not installed yet is fine at a checkpoint and refused at the close.
    check(midway(), &installed(None)).unwrap();
    let error = check(closing(), &installed(None)).unwrap_err();
    assert!(
        error.contains("`cube`") && error.contains("promised"),
        "{error}"
    );
    // Another version or schema is refused wherever it is read.
    for other in [
        pbps_model::Extension {
            version: "1.6".into(),
            ..pinned.clone()
        },
        pbps_model::Extension {
            schema: "other".into(),
            ..pinned.clone()
        },
    ] {
        for read in [closing(), midway()] {
            let error = check(read, &installed(Some(other.clone()))).unwrap_err();
            assert!(error.contains("promised"), "{error}");
        }
    }
    // The plan's own create does not excuse any other package moving.
    for moved in ["standing", "unplanned"] {
        let mut after = installed(Some(pinned.clone()));
        match moved {
            "standing" => after
                .extensions
                .get_mut("standing")
                .unwrap()
                .version
                .push('x'),
            _ => {
                after.extensions.insert("unplanned".into(), pinned.clone());
            }
        }
        let error = check(closing(), &after).unwrap_err();
        assert!(error.contains("changed during fixture"), "{moved}: {error}");
    }
}

#[test]
fn exact_extensions_are_required_at_every_read_back() {
    let changes = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "app.new".parse().unwrap(),
                table: Box::default(),
                beside_default: None,
                moves_at_most: None,
            },
        )],
    };
    for (schema, version) in [("ext", "1.4"), ("other", "1.6")] {
        let mut before = Schema::default();
        before.extensions.insert(
            "package".into(),
            pbps_model::Extension {
                schema: schema.into(),
                version: version.into(),
            },
        );
        let mut expected = before.clone();
        expected
            .tables
            .insert("app.new".parse().unwrap(), Default::default());
        let ids = guard::fixture_ids(&changes, &before);
        let plan = guard::Plan {
            changes: &changes,
            ids: &ids,
        };
        let staged = guard::Staged::new(plan).unwrap();
        for read in [
            None,
            Some(StagedRead::Checkpoint {
                completed: 1,
                total: 2,
            }),
            Some(StagedRead::Checkpoint {
                completed: 2,
                total: 2,
            }),
            Some(StagedRead::Closing { total: 2 }),
        ] {
            let check = |after: &Schema| {
                let read = match read {
                    None => guard::Read::transactional(plan),
                    Some(read) => staged.read(read),
                };
                refuse_pinned_movement(&pbps_pg::Postgres::new(), read, &before, after, "fixture")
            };
            // The ordinary change succeeds with the exact managed values
            // it was approved over; no lifecycle exemption is invented.
            check(&expected).unwrap();
            for case in ["schema", "version", "missing", "renamed", "added"] {
                let mut moved = expected.clone();
                match case {
                    "schema" => moved
                        .extensions
                        .get_mut("package")
                        .unwrap()
                        .schema
                        .push('x'),
                    "version" => moved
                        .extensions
                        .get_mut("package")
                        .unwrap()
                        .version
                        .push('x'),
                    "missing" => {
                        moved.extensions.remove("package");
                    }
                    "renamed" => {
                        let definition = moved.extensions.remove("package").unwrap();
                        moved.extensions.insert("renamed".into(), definition);
                    }
                    "added" => {
                        moved
                            .extensions
                            .insert("new".into(), before.extensions["package"].clone());
                    }
                    _ => unreachable!(),
                }
                let error = check(&moved).unwrap_err().to_string();
                assert!(
                    error.contains("extension `") && error.contains("changed during fixture"),
                    "{case}: {error}"
                );
            }
        }
    }
}
