use pbps_model::ids::IdsError;
use pbps_model::operation::{OwnerError, Owners, PlanOwners, Spelling};
use pbps_model::plan_names::{At, Holder, PlanNames};
use pbps_model::*;

const NAME: &str = "role\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";
const SHOWN: &str = r"role\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";

fn uid(value: &str) -> Uid {
    value.parse().unwrap()
}

#[test]
fn role_names_keep_raw_identity_serde_schema_and_string_debug() {
    let name = RoleName::from(NAME);
    let lookalike = RoleName::from(SHOWN);
    assert_ne!(name, lookalike);
    assert_eq!(name.raw(), NAME);
    assert_eq!(name.shown().to_string(), SHOWN);
    assert_eq!(format!("{name:?}"), format!("{NAME:?}"));
    assert_eq!(name.clone().into_raw(), NAME);
    assert_eq!(
        serde_json::to_string(&name).unwrap(),
        serde_json::to_string(NAME).unwrap()
    );
    assert_eq!(
        serde_json::from_str::<RoleName>(&serde_json::to_string(NAME).unwrap()).unwrap(),
        name
    );
    assert_eq!(
        schemars::schema_for!(RoleName),
        schemars::schema_for!(String)
    );
    let schema = Schema {
        roles: [
            (name.clone(), Role::default()),
            (lookalike.clone(), Role::default()),
        ]
        .into(),
        ..Default::default()
    };
    assert_eq!(schema.roles.len(), 2);
    assert!(schema.roles.contains_key(NAME));
    assert!(schema.roles.contains_key(SHOWN));
    let ids = IdsFile {
        roles: [
            (uid("r_aaaaaa"), name.clone()),
            (uid("r_bbbbbb"), lookalike.clone()),
        ]
        .into(),
        ..Default::default()
    };
    assert!(ids.validate().is_ok());
    let mut duplicate = ids.clone();
    duplicate.roles.insert(uid("r_cccccc"), name.clone());
    let error = duplicate.validate().unwrap_err();
    assert!(matches!(&error, IdsError::DuplicateName { name, .. } if name == NAME));
    assert_eq!(
        error.to_string(),
        format!(
            "r_aaaaaa and r_cccccc both point at the name `{SHOWN}`; decide which uid survives and delete the other entry from the identity file"
        )
    );
    let missing = ids.check_held_by(&Schema::default()).unwrap_err();
    assert!(matches!(&missing, IdsError::NotHeld { name, .. } if name == NAME));
    assert_eq!(
        missing.to_string(),
        format!("r_aaaaaa points at `{SHOWN}`, but the snapshot schema does not hold it")
    );
    assert_eq!(ids.role_uid(&name), Some(&uid("r_aaaaaa")));
    assert_eq!(ids.index().role_uid(&lookalike), Some(&uid("r_bbbbbb")));
    let changes = [];
    let history = PlanNames::of(&changes, &ids).unwrap();
    assert_eq!(
        history.role_holder(&name, At::Start),
        Holder::Tracked(&uid("r_aaaaaa"))
    );
    assert_eq!(
        history.role_holder(&lookalike, At::End),
        Holder::Tracked(&uid("r_bbbbbb"))
    );
    assert_eq!(history.role_at(&uid("r_aaaaaa"), At::End), Some(&name));
    let mut renamed = ids.clone();
    renamed.rename_role(&name, &RoleName::from("new"));
    assert_eq!(renamed.role_uid(&lookalike), Some(&uid("r_bbbbbb")));
    assert_eq!(renamed.role_uid(&name), None);
    assert_eq!(renamed.without_roles(&[lookalike]).roles.len(), 1);
    assert_eq!(RoleName::from("名前_ünï").shown().to_string(), "名前_ünï");
}

#[test]
fn role_subject_keys_and_grantee_identity_keep_original_spelling() {
    let name = RoleName::from(NAME);
    let target = GrantTarget::Schema("app".into());
    let changes = [
        Change::CreateRole {
            uid: uid("r_aaaaaa"),
            name: name.clone(),
        },
        Change::DropRole {
            uid: uid("r_aaaaaa"),
            name: name.clone(),
            members: Vec::new(),
        },
        Change::RenameRole {
            uid: uid("r_aaaaaa"),
            from: name.clone(),
            to: "new".into(),
        },
        Change::Grant {
            role: name.clone(),
            target: target.clone(),
            permissions: [Permission::Usage].into(),
        },
        Change::Revoke {
            role: name.clone(),
            target,
            permissions: [Permission::Usage].into(),
        },
    ];
    for change in changes {
        assert_eq!(change.subject(), format!("role {NAME}"));
        assert_ne!(change.subject(), format!("role {SHOWN}"));
        assert_eq!(change.roles().next(), Some(&name));
    }
    assert_ne!(
        pbps_model::operation::Grantee::Role("PUBLIC".into()),
        pbps_model::operation::Grantee::Public
    );
    assert_eq!(
        serde_json::to_string(&PolicyRoles::Roles([name].into())).unwrap(),
        format!("{{\"roles\":[{}]}}", serde_json::to_string(NAME).unwrap())
    );
    assert_ne!(
        PolicyRoles::Public,
        PolicyRoles::Roles(["PUBLIC".into()].into())
    );
}

#[test]
fn role_history_collision_keeps_the_previous_string_debug_message() {
    let name = RoleName::from("role\\path\nline");
    let changes = [
        PlannedChange::new(Change::CreateRole {
            uid: uid("r_bbbbbb"),
            name: name.clone(),
        }),
        PlannedChange::new(Change::RenameRole {
            uid: uid("r_aaaaaa"),
            from: name.clone(),
            to: "new".into(),
        }),
    ];
    let ids = IdsFile {
        roles: [(uid("r_aaaaaa"), "new".into()), (uid("r_bbbbbb"), name)].into(),
        ..Default::default()
    };
    let error = PlanNames::of(&changes, &ids).unwrap_err();
    assert_eq!(
        error.to_string(),
        r#"role address "role\\\\path\\nline" is held by both r_aaaaaa and r_bbbbbb at boundary 1"#
    );
}

#[test]
fn owner_errors_show_names_once_without_changing_raw_holder_lookups() {
    let table = TableName::new("app", NAME);
    let column = table.column(NAME);
    let role = RoleName::from(NAME);
    let changes = [
        PlannedChange::new(Change::CreateTable {
            uid: uid("t_aaaaaa"),
            name: table.clone(),
            table: Box::new(Table {
                columns: [(NAME.into(), Column::new("int".parse().unwrap()))].into(),
                ..Default::default()
            }),
            beside_default: None,
            moves_at_most: None,
        }),
        PlannedChange::new(Change::CreateRole {
            uid: uid("r_aaaaaa"),
            name: role.clone(),
        }),
    ];
    let ids = IdsFile {
        tables: [(uid("t_aaaaaa"), table.clone())].into(),
        columns: [(uid("c_aaaaaa"), column.clone())].into(),
        roles: [(uid("r_aaaaaa"), role.clone())].into(),
        ..Default::default()
    };
    let empty = IdsFile::default();
    let owners = PlanOwners::new(PlanNames::of(&changes, &ids).unwrap(), &empty, &ids);
    for (error, shown) in [
        (
            owners.table(&table, Spelling::Base).unwrap_err(),
            format!("app.{SHOWN}"),
        ),
        (
            owners.column(&column, Spelling::Base).unwrap_err(),
            format!("app.{SHOWN}.{SHOWN}"),
        ),
        (
            owners.role(&role, Spelling::Base).unwrap_err(),
            SHOWN.to_owned(),
        ),
    ] {
        assert_eq!(
            error.to_string(),
            format!("no managed object is recorded under `{shown}` (Base)")
        );
    }
    for (error, shown) in [
        (
            owners.table(&table, Spelling::At(At::Start)).unwrap_err(),
            format!("app.{SHOWN}"),
        ),
        (
            owners.column(&column, Spelling::At(At::Start)).unwrap_err(),
            format!("app.{SHOWN}.{SHOWN}"),
        ),
        (
            owners.role(&role, Spelling::At(At::Start)).unwrap_err(),
            SHOWN.to_owned(),
        ),
    ] {
        assert_eq!(
            error.to_string(),
            format!("nothing holds `{shown}` at At(Start)")
        );
    }
    assert_eq!(
        owners.table(&table, Spelling::Final).unwrap(),
        uid("t_aaaaaa")
    );
    assert_eq!(
        owners.column(&column, Spelling::Final).unwrap(),
        uid("c_aaaaaa")
    );
    assert_eq!(
        owners.role(&role, Spelling::Final).unwrap(),
        uid("r_aaaaaa")
    );
    assert_eq!(
        owners.role(&role, Spelling::At(At::End)).unwrap(),
        uid("r_aaaaaa")
    );
    let absent = PlanOwners::new(PlanNames::of(&[], &empty).unwrap(), &empty, &empty);
    for spelling in [Spelling::Base, Spelling::Final, Spelling::At(At::Start)] {
        for (error, shown) in [
            (
                absent.table(&table, spelling).unwrap_err(),
                format!("app.{SHOWN}"),
            ),
            (
                absent.column(&column, spelling).unwrap_err(),
                format!("app.{SHOWN}.{SHOWN}"),
            ),
            (absent.role(&role, spelling).unwrap_err(), SHOWN.to_owned()),
        ] {
            assert!(matches!(error, OwnerError::Unmanaged { .. }));
            assert_eq!(
                error.to_string(),
                format!("no managed object is recorded under `{shown}` ({spelling:?})")
            );
        }
    }
    assert!(
        owners
            .role(&RoleName::from(SHOWN), Spelling::Final)
            .is_err()
    );
}
