//! The resolver's ordinary PostgreSQL plan cannot carry cluster identity
//! changes while roles stay externally managed (DEC-1527.1).
use pbps_dialect::Dialect;
use pbps_model::{Change, GrantTarget, Hints, IdsFile, Permission, Role, Schema};
use pbps_pg::Postgres;

fn declared(name: Option<&str>, granted: bool) -> (Schema, IdsFile) {
    let mut schema = Schema::default();
    let mut ids = IdsFile::default();
    if let Some(name) = name {
        let mut role = Role::default();
        if granted {
            role.grants.insert(
                GrantTarget::Schema("app".into()),
                [Permission::Usage].into(),
            );
        }
        schema.roles.insert(name.into(), role);
        ids.roles.insert("r_aaaaaa".parse().unwrap(), name.into());
    }
    (schema, ids)
}

fn ordinary(base: &(Schema, IdsFile), desired: &(Schema, IdsFile)) -> pbps_diff::order::DiffPlan {
    pbps_diff::resolver::ordinary(
        pbps_diff::Side {
            schema: &base.0,
            ids: &base.1,
        },
        pbps_diff::Side {
            schema: &desired.0,
            ids: &desired.1,
        },
        &Postgres::new(),
        &Hints::default(),
    )
    .unwrap()
}

#[test]
fn ordinary_postgres_plans_never_manage_role_identities() {
    assert!(!Postgres::new().manages_roles());
    for (before, after) in [
        (None, Some("reader")),
        (Some("reader"), None),
        (Some("reader"), Some("renamed")),
    ] {
        let plan = ordinary(&declared(before, false), &declared(after, false));
        assert!(
            plan.changes.iter().all(|p| !matches!(
                p.change,
                Change::CreateRole { .. } | Change::DropRole { .. } | Change::RenameRole { .. }
            )),
            "{before:?} -> {after:?}: {plan:?}"
        );
        assert!(plan.changes.is_empty(), "{before:?} -> {after:?}: {plan:?}");
    }
}

#[test]
fn external_role_adoption_and_removal_still_plan_database_grants() {
    // Negative control: identity elision must not swallow the database-local
    // authorization changes that PostgreSQL does manage.
    let empty = declared(None, false);
    let reader = declared(Some("reader"), true);
    let added = ordinary(&empty, &reader);
    assert!(
        matches!(added.changes.as_slice(), [p] if matches!(
            &p.change,
            Change::Grant { role, target: GrantTarget::Schema(schema), permissions }
            if role.raw() == "reader" && schema == "app" && permissions.contains(&Permission::Usage)
        )),
        "{added:?}"
    );
    let removed = ordinary(&reader, &empty);
    assert!(
        matches!(removed.changes.as_slice(), [p] if matches!(
            &p.change,
            Change::Revoke { role, target: GrantTarget::Schema(schema), permissions }
            if role.raw() == "reader" && schema == "app" && permissions.contains(&Permission::Usage)
        )),
        "{removed:?}"
    );
    // An externally completed rename keeps the same UID and its ACLs; it
    // neither schedules identity DDL nor gratuitously regrants permissions.
    assert!(
        ordinary(&reader, &declared(Some("renamed"), true))
            .changes
            .is_empty()
    );
}
