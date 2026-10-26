use pbps_model::operation::*;
use pbps_model::plan_names::{At, PlanNames};
use pbps_model::*;
#[path = "support/rebuild.rs"]
mod fixture;

#[test]
fn rebuild_addresses_keep_source_and_reserved_owners_distinct() {
    let (root, base, desired, addresses) = fixture::approval();
    let owners = PlanOwners::new(PlanNames::of(&[], &base).unwrap(), &base, &desired)
        .with_rebuild_addresses(&root, &desired, &addresses)
        .unwrap();
    for (identities, spelling, prefix) in [
        (&root.intent.source, Spelling::Base, "old_"),
        (&root.intent.replacement, Spelling::Final, "tmp_"),
    ] {
        for (uid, name) in &identities.tables {
            assert_eq!(&owners.table(name, spelling).unwrap(), uid);
            let alias = TableName::new("app", format!("{prefix}{}", name.name));
            assert_eq!(&owners.table(&alias, Spelling::At(At::Start)).unwrap(), uid);
        }
        for (uid, name) in &identities.columns {
            let alias =
                TableName::new("app", format!("{prefix}{}", name.table.name)).column(&name.name);
            assert_eq!(
                &owners.column(&alias, Spelling::At(At::Start)).unwrap(),
                uid
            );
        }
    }
    let outside = Spelling::At(At::Before(0)); // The indexed logical plan is empty.
    let alias = TableName::new("app", "tmp_p");
    assert!(matches!(
        owners.table(&alias, outside),
        Err(OwnerError::Outside { .. })
    ));
    assert!(matches!(
        owners.column(&alias.column("k"), outside),
        Err(OwnerError::Outside { .. })
    ));
    for (name, wrong) in [("tmp_p", Spelling::Base), ("old_p", Spelling::Final)] {
        let address = TableName::new("app", name);
        assert!(owners.table(&address, wrong).is_err());
        assert!(owners.column(&address.column("k"), wrong).is_err());
    }
    assert!(
        owners
            .table(&root.table, Spelling::At(At::Start))
            .unwrap_err()
            .to_string()
            .contains("explicit address")
    );
    assert!(
        owners
            .column(
                &TableName::new("app", "tmp_p1").column("k"),
                Spelling::Final
            )
            .is_err(),
        "inherited columns have no fresh UID"
    );
}

#[test]
fn rebuild_addresses_refuse_stale_foreign_duplicate_and_occupied_bindings() {
    let (root, base, desired, addresses) = fixture::approval();
    let attempt = |root: &RebuildPartitionedTable, a: &[IntermediateAddress]| {
        PlanOwners::new(PlanNames::of(&[], &base).unwrap(), &base, &desired)
            .with_rebuild_addresses(root, &desired, a)
            .err()
            .unwrap()
            .to_string()
    };
    let mut stale = root.clone();
    stale.intent.target_checksum = "other".into();
    assert!(attempt(&stale, &addresses).contains("stale"));
    let mut bad = addresses.clone();
    bad[0].uid = "t_cccccc".parse().unwrap();
    assert!(attempt(&root, &bad).contains("reserved"));
    let mut bad = addresses.clone();
    bad.push(bad[0].clone());
    assert!(attempt(&root, &bad).contains("duplicated"));
    let mut bad = addresses;
    bad[0].address = root.table.clone();
    assert!(attempt(&root, &bad).contains("already managed"));
}

#[test]
fn rebuild_copy_and_analyze_have_step_local_effects_and_no_name_claims() {
    let (root, base, desired, addresses) = fixture::approval();
    let owners = PlanOwners::new(PlanNames::of(&[], &base).unwrap(), &base, &desired)
        .with_rebuild_addresses(&root, &desired, &addresses)
        .unwrap();
    let schema = Schema::default();
    let before = Snapshot {
        schema: &schema,
        ids: &base,
    };
    let after = Snapshot {
        schema: &schema,
        ids: &desired,
    };
    let changes = fixture::changes()
        .into_iter()
        .map(PlannedChange::new)
        .collect::<Vec<_>>();
    let ops = Operations::of_plan(&changes, ModuleEdits::InPlace);
    assert!(
        effects(&ops.as_slice()[0], &owners, before, after)
            .unwrap_err()
            .to_string()
            .contains("app.p")
    );
    let copy = effects(&ops.as_slice()[1], &owners, before, after).unwrap();
    let analyze = effects(&ops.as_slice()[2], &owners, before, after).unwrap();
    assert!(copy.creates.is_empty() && copy.removes.is_empty());
    assert_eq!(
        copy.rewrites.len(),
        3,
        "parent, leaf and projected column values"
    );
    assert_eq!(
        copy.requires
            .iter()
            .filter(|r| matches!(r, Requirement::Reads(_)))
            .count(),
        3
    );
    for key in analyze.requires.iter().filter_map(|r| {
        if let Requirement::Reads(k) = r {
            Some(k)
        } else {
            None
        }
    }) {
        assert!(copy.rewrites.contains(&Resource::Column(key.clone())));
    }
    assert_eq!(
        analyze
            .requires
            .iter()
            .filter(|r| matches!(r, Requirement::Reads(_)))
            .count(),
        2
    );
    assert!(
        analyze.creates.is_empty() && analyze.removes.is_empty() && analyze.rewrites.is_empty()
    );
    for op in ops.iter().skip(1) {
        let slots = names(op, &owners, before).unwrap();
        assert!(slots.claims.is_empty() && slots.releases.is_empty());
    }
    let mut bad = changes;
    let Change::CopyTableRows(c) = &mut bad[1].change else {
        panic!()
    };
    c.target.uid = c.source.uid.clone();
    let ops = Operations::of_plan(&bad, ModuleEdits::InPlace);
    assert!(
        effects(&ops.as_slice()[1], &owners, before, after)
            .unwrap_err()
            .to_string()
            .contains("recorded UID")
    );
}

#[test]
fn explicit_index_recipes_own_all_clones_and_refuse_ambiguous_owners() {
    let (root, base, desired, addresses) = fixture::approval();
    let owners = PlanOwners::new(PlanNames::of(&[], &base).unwrap(), &base, &desired)
        .with_rebuild_addresses(&root, &desired, &addresses)
        .unwrap();
    let before_schema = Schema {
        tables: root.before,
        ..Default::default()
    };
    let after_schema = Schema {
        tables: root.after,
        ..Default::default()
    };
    let before = Snapshot {
        schema: &before_schema,
        ids: &base,
    };
    let after = Snapshot {
        schema: &after_schema,
        ids: &desired,
    };
    let mut recipe = fixture::index_recipe();
    for definition in [
        recipe.definition.clone(),
        PartitionIndexDefinition::Unique(UniqueConstraint {
            columns: vec!["k".into()],
            storage_parameters: Default::default(),
        }),
        PartitionIndexDefinition::PrimaryKey(PrimaryKey {
            name: Some(recipe.name.clone()),
            columns: vec!["k".into()],
            storage_parameters: Default::default(),
        }),
    ] {
        recipe.definition = definition;
        let ops = Operations::expanded(
            1,
            [(
                Origin::Restoration(Source::Change(0)),
                PlannedChange::new(Change::BuildPartitionedIndex(Box::new(recipe.clone()))),
            )],
            ModuleEdits::InPlace,
        );
        let op = &ops.as_slice()[0];
        let fx = effects(op, &owners, before, after).unwrap();
        assert!(fx.creates.contains(&Resource::Object(
            op.key(&owners).unwrap(),
            Incarnation::Final
        )));
        let named = names(op, &owners, before).unwrap();
        assert_eq!(named.claims.len(), 1 + recipe.clones.len());
        assert!(
            named
                .claims
                .iter()
                .all(|n| n.holder == op.key(&owners).unwrap())
        );
        assert_eq!(
            fx.requires
                .iter()
                .filter(|r| matches!(r, Requirement::Reads(_)))
                .count(),
            1 + recipe.clones.len()
        );
    }
    let mut reused = recipe.clone();
    reused.clones[0].name = reused.name.clone();
    assert!(reused.validate().is_err());
    reused.clones[0].leaf.name.schema = "other".into();
    assert!(reused.validate().is_ok());
    let mut bad = recipe.clone();
    bad.clones.push(bad.clones[0].clone());
    assert!(bad.validate().is_err());
    if let PartitionIndexDefinition::PrimaryKey(key) = &mut recipe.definition {
        key.name = None;
    }
    assert!(recipe.validate().is_err());
}

#[test]
fn a_composed_rebuilds_base_spelling_is_what_the_environment_holds() {
    let (mut root, _, desired, _) = fixture::approval();
    // An older link whose replacement is this link's source: the environment
    // still holds the older source.
    let fresh = |kind| Uid::generate(kind);
    let older_source = RebuildIdentities {
        tables: root
            .intent
            .source
            .tables
            .values()
            .map(|n| (fresh(UidKind::Table), n.clone()))
            .collect(),
        columns: root
            .intent
            .source
            .columns
            .values()
            .map(|c| (fresh(UidKind::Column), c.clone()))
            .collect(),
    };
    let mut older = root.intent.clone();
    older.source = older_source.clone();
    older.replacement = root.intent.source.clone();
    older.types = Default::default();
    root.earlier.push(older);
    assert_eq!(root.source(), &older_source);
    let held = older_source.as_ids();
    let addresses: Vec<_> = older_source
        .tables
        .iter()
        .map(|(uid, name)| IntermediateAddress {
            uid: uid.clone(),
            address: TableName::new("app", format!("old_{}", name.name)),
        })
        .collect();
    let owners = PlanOwners::new(PlanNames::of(&[], &held).unwrap(), &held, &desired)
        .with_rebuild_addresses(&root, &desired, &addresses)
        .unwrap();
    for (uid, name) in &older_source.tables {
        assert_eq!(&owners.table(name, Spelling::Base).unwrap(), uid);
    }
    // The ids of the link the files record are not what this environment holds.
    let intermediate = root.intent.source.as_ids();
    assert!(
        PlanOwners::new(
            PlanNames::of(&[], &intermediate).unwrap(),
            &intermediate,
            &desired
        )
        .with_rebuild_addresses(&root, &desired, &[])
        .is_err()
    );
}
