use pbps_model::{
    operation::{OwnerError, Owners, PlanOwners, Spelling},
    plan_names::{At, PlanNames},
    *,
};
fn definition() -> Box<ManagedType> {
    Box::new(ManagedType::Domain {
        base: "integer".parse().unwrap(),
        default: None,
        not_null: false,
        checks: Default::default(),
        collation: None,
    })
}
#[test]
fn intermediate_type_owners_preserve_uids_through_create_rename_drop_and_reuse() {
    let a: Uid = "y_aaaaaa".parse().unwrap();
    let b: Uid = "y_bbbbbb".parse().unwrap();
    let old: TypeRef = "app.d".parse().unwrap();
    let new: TypeRef = "app.renamed".parse().unwrap();
    let changes: Vec<_> = vec![
        Change::CreateType {
            uid: a.clone(),
            name: old.clone(),
            definition: definition(),
        },
        Change::RenameType {
            uid: a.clone(),
            from: old.clone(),
            to: new.clone(),
            definition: definition(),
        },
        Change::DropType {
            uid: a.clone(),
            name: new.clone(),
            definition: definition(),
        },
        Change::CreateType {
            uid: b.clone(),
            name: new.clone(),
            definition: definition(),
        },
    ]
    .into_iter()
    .map(PlannedChange::new)
    .collect();
    let before = IdsFile::default();
    let after = IdsFile {
        types: [(b.clone(), new.clone())].into(),
        ..Default::default()
    };
    let owners = PlanOwners::new(PlanNames::of(&changes, &after).unwrap(), &before, &after);
    assert!(matches!(
        owners.managed_type(&old, Spelling::At(At::Before(0))),
        Err(OwnerError::Vacant { .. })
    ));
    assert_eq!(
        owners
            .managed_type(&old, Spelling::At(At::After(0)))
            .unwrap(),
        a
    );
    assert!(matches!(
        owners.managed_type(&old, Spelling::At(At::After(1))),
        Err(OwnerError::Vacant { .. })
    ));
    assert_eq!(
        owners
            .managed_type(&new, Spelling::At(At::After(1)))
            .unwrap(),
        a
    );
    assert!(matches!(
        owners.managed_type(&new, Spelling::At(At::After(2))),
        Err(OwnerError::Vacant { .. })
    ));
    assert_eq!(
        owners
            .managed_type(&new, Spelling::At(At::After(3)))
            .unwrap(),
        b
    );
    assert!(matches!(
        owners.managed_type(&new, Spelling::At(At::After(4))),
        Err(OwnerError::Outside { .. })
    ));
    assert!(matches!(
        owners.managed_type(&"app.unknown".parse().unwrap(), Spelling::At(At::End)),
        Err(OwnerError::Unmanaged { .. })
    ));
}
#[test]
fn unreadable_type_history_is_not_an_unknown_or_vacant_name() {
    let uid: Uid = "y_aaaaaa".parse().unwrap();
    let name: TypeRef = "app.d".parse().unwrap();
    let changes = vec![PlannedChange::new(Change::CreateType {
        uid: uid.clone(),
        name: name.clone(),
        definition: definition(),
    })];
    let ids = IdsFile::default();
    // Unrelated legacy readers need no intermediate type proof. Asking for
    // that proof must report the inconsistent final mapping explicitly.
    let names = PlanNames::of(&changes, &ids).unwrap();
    let owners = PlanOwners::new(names, &ids, &ids);
    assert!(matches!(
        owners.managed_type(&name, Spelling::At(At::After(0))),
        Err(OwnerError::Names { .. })
    ));
}
