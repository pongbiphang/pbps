use pbps_model::{
    operation::{OwnerError, Owners, PlanOwners, Spelling},
    plan_names::{At, PlanNames, PlanNamesError},
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
        Change::Type(TypeChange::CreateType {
            uid: a.clone(),
            name: old.clone(),
            definition: definition(),
        }),
        Change::Type(TypeChange::RenameType {
            uid: a.clone(),
            from: old.clone(),
            to: new.clone(),
            definition: definition(),
        }),
        Change::Type(TypeChange::DropType {
            uid: a.clone(),
            name: new.clone(),
            definition: definition(),
        }),
        Change::Type(TypeChange::CreateType {
            uid: b.clone(),
            name: new.clone(),
            definition: definition(),
        }),
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
    let changes = vec![PlannedChange::new(Change::Type(TypeChange::CreateType {
        uid: uid.clone(),
        name: name.clone(),
        definition: definition(),
    }))];
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

#[test]
fn a_type_history_refuses_a_second_holder_with_a_readable_address() {
    let old: Uid = "y_aaaaaa".parse().unwrap();
    let new: Uid = "y_bbbbbb".parse().unwrap();
    let name = TypeRef::new("app", "line\nbreak").unwrap();
    let create = PlannedChange::new(Change::Type(TypeChange::CreateType {
        uid: new.clone(),
        name: name.clone(),
        definition: definition(),
    }));
    let drop = PlannedChange::new(Change::Type(TypeChange::DropType {
        uid: old.clone(),
        name: name.clone(),
        definition: definition(),
    }));
    let before = IdsFile {
        types: [(old.clone(), name.clone())].into(),
        ..Default::default()
    };
    let after = IdsFile {
        types: [(new.clone(), name.clone())].into(),
        ..Default::default()
    };
    let boundary = Spelling::At(At::After(0));
    let overlap = [create.clone(), drop.clone()];
    let owners = PlanOwners::new(PlanNames::of(&overlap, &after).unwrap(), &before, &after);
    let error = owners.managed_type(&name, boundary).unwrap_err();
    let address = name.shown();
    assert!(matches!(error, OwnerError::Names { .. }));
    assert_eq!(
        error.to_string(),
        format!(
            "cannot bind {address} after change 0: type address {address} is held by both {old} and {new} at boundary 1"
        )
    );

    let handover = [drop, create];
    let owners = PlanOwners::new(PlanNames::of(&handover, &after).unwrap(), &before, &after);
    assert!(matches!(
        owners.managed_type(&name, boundary),
        Err(OwnerError::Vacant { .. })
    ));
    assert_eq!(
        owners
            .managed_type(&name, Spelling::At(At::After(1)))
            .unwrap(),
        new
    );
}

const RAW_ADDRESS: &str = "line\nbreak\\path";
const SHOWN_ADDRESS: &str = r"line\nbreak\\path";

fn rename_table(uid: &Uid, from: TableName, to: TableName) -> PlannedChange {
    PlannedChange::new(Change::Table(TableChange::RenameTable {
        uid: uid.clone(),
        from,
        to,
        defaults: vec![],
    }))
}

fn assert_collision(error: PlanNamesError, kind: &str, address: &str, old: &Uid, new: &Uid) {
    assert!(
        matches!(&error, PlanNamesError::Collision { namespace, .. } if *namespace == kind),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        format!("{kind} address {address} is held by both {old} and {new} at boundary 1")
    );
}

#[test]
fn a_table_history_collision_shows_the_qualified_address_once() {
    let old: Uid = "t_aaaaaa".parse().unwrap();
    let new: Uid = "t_bbbbbb".parse().unwrap();
    let name = TableName::new("app", RAW_ADDRESS);
    let released = TableName::new("app", "released");
    let changes = [
        rename_table(&new, TableName::new("app", "incoming"), name.clone()),
        rename_table(&old, name.clone(), released.clone()),
    ];
    let after = IdsFile {
        tables: [(new.clone(), name), (old.clone(), released)].into(),
        ..Default::default()
    };
    assert_collision(
        PlanNames::of(&changes, &after).unwrap_err(),
        "table",
        &format!("app.{SHOWN_ADDRESS}"),
        &old,
        &new,
    );
}

#[test]
fn a_column_history_collision_names_the_column_and_owning_table_uid() {
    let table_uid: Uid = "t_aaaaaa".parse().unwrap();
    let old: Uid = "c_aaaaaa".parse().unwrap();
    let new: Uid = "c_bbbbbb".parse().unwrap();
    let table = TableName::new("app", "parent");
    let column = table.column(RAW_ADDRESS);
    let changes = [
        PlannedChange::new(Change::Column(ColumnChange::AddColumn {
            uid: new.clone(),
            table: table.clone(),
            name: column.name.clone(),
            column: Box::new(Column::new("int".parse().unwrap())),
        })),
        PlannedChange::new(Change::Column(ColumnChange::DropColumn {
            uid: old.clone(),
            column: column.clone(),
        })),
    ];
    let after = IdsFile {
        tables: [(table_uid.clone(), table)].into(),
        columns: [(new.clone(), column)].into(),
        ..Default::default()
    };
    assert_collision(
        PlanNames::of(&changes, &after).unwrap_err(),
        "column",
        &format!("{SHOWN_ADDRESS} on table {table_uid}"),
        &old,
        &new,
    );
}

#[test]
fn a_role_history_collision_shows_the_address_once() {
    let old: Uid = "r_aaaaaa".parse().unwrap();
    let new: Uid = "r_bbbbbb".parse().unwrap();
    let name = RoleName::from(RAW_ADDRESS);
    let changes = [
        PlannedChange::new(Change::Role(RoleChange::CreateRole {
            uid: new.clone(),
            name: name.clone(),
        })),
        PlannedChange::new(Change::Role(RoleChange::DropRole {
            uid: old.clone(),
            name: name.clone(),
            members: vec![],
        })),
    ];
    let after = IdsFile {
        roles: [(new.clone(), name)].into(),
        ..Default::default()
    };
    assert_collision(
        PlanNames::of(&changes, &after).unwrap_err(),
        "role",
        SHOWN_ADDRESS,
        &old,
        &new,
    );
}

#[test]
fn a_history_source_mismatch_shows_the_expected_address_once() {
    let uid: Uid = "t_aaaaaa".parse().unwrap();
    let old = TableName::new("app", RAW_ADDRESS);
    let new = TableName::new("app", "destination");
    let changes = [
        rename_table(&uid, old.clone(), TableName::new("app", "intermediate")),
        rename_table(&uid, old, new.clone()),
    ];
    let after = IdsFile {
        tables: [(uid.clone(), new)].into(),
        ..Default::default()
    };
    let error = PlanNames::of(&changes, &after).unwrap_err();
    assert!(matches!(error, PlanNamesError::Source { step: 1, .. }));
    assert_eq!(
        error.to_string(),
        format!("table identity {uid} does not hold app.{SHOWN_ADDRESS} before change 1")
    );
}
