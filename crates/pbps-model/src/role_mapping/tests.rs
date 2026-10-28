use super::*;
use crate::change::PlannedChange;
use crate::role::Role;
use crate::schema::Table;
use crate::uid::UidKind;
use crate::{GrantTarget, Permission, TableName, Uid};

fn name(raw: &str) -> RoleName {
    raw.into()
}

fn table() -> TableName {
    TableName {
        schema: "app".into(),
        name: "t".into(),
    }
}

fn policy(roles: &[&str]) -> RowPolicy {
    RowPolicy {
        to: PolicyRoles::Roles(roles.iter().map(|r| name(r)).collect()),
        using: Some("true".into()),
        ..RowPolicy::default()
    }
}

fn planned(change: Change) -> PlannedChange {
    PlannedChange {
        drop_guard: None,
        change,
        risks: Default::default(),
        strategy: Default::default(),
        findings: Vec::new(),
        provenance: Default::default(),
    }
}

fn mapping(names: &[&str]) -> RoleMapping {
    let names: Vec<_> = names.iter().map(|n| name(n)).collect();
    RoleMapping::generate(&names, "pbps_rehearsal", "tok", |role| {
        role.raw().starts_with("pg_")
    })
}

/// Every name the plan or the base mentions gets a name of the run's, numbered
/// in name order so the same names always map the same way, and the engine's
/// own roles keep theirs.
#[test]
fn a_mapping_numbers_the_names_and_keeps_the_engines_own() {
    let map = mapping(&["reader", "pg_read_all_data", "analyst"]);
    assert_eq!(map.name(&name("analyst")).raw(), "pbps_rehearsal_0_tok");
    assert_eq!(map.name(&name("reader")).raw(), "pbps_rehearsal_1_tok");
    assert_eq!(
        map.name(&name("pg_read_all_data")).raw(),
        "pg_read_all_data"
    );
    assert_eq!(
        map.created().iter().map(RoleName::raw).collect::<Vec<_>>(),
        ["pbps_rehearsal_0_tok", "pbps_rehearsal_1_tok"]
    );
    // Negative: a name nobody gave it is its own, and a mapping of nothing
    // creates nothing.
    assert_eq!(map.name(&name("stranger")).raw(), "stranger");
    assert!(mapping(&["pg_monitor"]).is_empty());
    assert!(mapping(&["pg_monitor"]).created().is_empty());
}

#[test]
fn a_schema_is_mapped_in_its_roles_and_in_the_roles_its_policies_apply_to() {
    let mut schema = Schema::default();
    schema.roles.insert(name("reader"), Role::default());
    let mut t = Table::default();
    t.row_policies
        .insert("p".into(), policy(&["reader", "auditor"]));
    t.row_policies
        .insert("everyone".into(), RowPolicy::default());
    schema.tables.insert(table(), t);
    let map = mapping(&["auditor", "reader"]);
    let mapped = map.schema(&schema);
    assert_eq!(
        mapped.named_roles(),
        map.created().iter().cloned().collect::<BTreeSet<_>>()
    );
    // The rest of the policy is what it was.
    let before = &schema.tables[&table()].row_policies["p"];
    let after = &mapped.tables[&table()].row_policies["p"];
    assert_eq!(after.using, before.using);
    assert_eq!(after.command, before.command);
    // Negative: a policy for everyone names no role, and stays that.
    assert_eq!(
        mapped.tables[&table()].row_policies["everyone"].to,
        PolicyRoles::Public
    );
    assert_eq!(schema.named_roles().len(), 2, "the input is untouched");
}

#[test]
fn identity_follows_the_role_to_its_run_name() {
    let mut ids = IdsFile::default();
    ids.roles
        .insert(Uid::generate(UidKind::Role), name("reader"));
    ids.roles
        .insert(Uid::generate(UidKind::Role), name("stranger"));
    let map = mapping(&["reader"]);
    let mapped = map.ids(&ids);
    assert!(mapped.role_uid(&name("pbps_rehearsal_0_tok")).is_some());
    assert!(mapped.role_uid(&name("reader")).is_none());
    // Negative: a role the mapping does not know keeps its identity as it was.
    assert!(mapped.role_uid(&name("stranger")).is_some());
}

/// Every change that names a role comes out naming the run's role, whichever
/// way it names it: as the subject, in a rename, among a dropped role's members
/// and inside the policy it creates or drops.
#[test]
fn a_change_names_only_the_runs_roles_afterwards() {
    let target: GrantTarget = "schema::app".parse().unwrap();
    let permissions = BTreeSet::from([Permission::Usage]);
    let changes = ChangeSet {
        changes: vec![
            planned(Change::Grant {
                role: name("reader"),
                target: target.clone(),
                permissions: permissions.clone(),
            }),
            planned(Change::Revoke {
                role: name("writer"),
                target,
                permissions,
            }),
            planned(Change::RenameRole {
                uid: Uid::generate(UidKind::Role),
                from: name("old"),
                to: name("new"),
            }),
            planned(Change::DropRole {
                uid: Uid::generate(UidKind::Role),
                name: name("gone"),
                members: vec![name("member")],
            }),
            planned(Change::CreatePolicy {
                table: table(),
                name: "p".into(),
                policy: policy(&["reader", "auditor"]),
                origin: crate::change::RowSecurityOrigin::Created,
            }),
            planned(Change::DropPolicy {
                table: table(),
                name: "q".into(),
                policy: policy(&["writer"]),
            }),
        ],
    };
    let named: BTreeSet<_> = changes
        .changes
        .iter()
        .flat_map(|c| c.change.named_roles())
        .collect();
    assert_eq!(named.len(), 7);
    let map = RoleMapping::generate(named.iter(), "pbps_rehearsal", "tok", |_| false);
    let mapped = map.changes(&changes);
    for planned in &mapped.changes {
        for role in planned.change.named_roles() {
            assert!(
                role.raw().starts_with("pbps_rehearsal_") && role.raw().ends_with("_tok"),
                "{role:?} in {:?}",
                planned.change
            );
        }
    }
    assert_eq!(
        mapped
            .changes
            .iter()
            .flat_map(|c| c.change.named_roles())
            .count(),
        9,
        "no change lost or gained a role"
    );
    // Negative: a change that names no role is what it was.
    let plain = ChangeSet {
        changes: vec![planned(Change::DropTable {
            uid: Uid::generate(UidKind::Table),
            name: table(),
            detach_from: None,
        })],
    };
    assert_eq!(map.changes(&plain), plain);
}

/// A mapping that changes nothing leaves every name, schema, identity and
/// change as it was, and still says which names the run has to make.
#[test]
fn an_unchanged_mapping_is_the_identity_and_lists_what_to_make() {
    let map = RoleMapping::unchanged([name("reader"), name("writer")]);
    assert_eq!(map.name(&name("reader")).raw(), "reader");
    assert_eq!(
        map.created().iter().map(RoleName::raw).collect::<Vec<_>>(),
        ["reader", "writer"]
    );
    let mut schema = Schema::default();
    schema.roles.insert(name("reader"), Role::default());
    let mut t = Table::default();
    t.row_policies.insert("p".into(), policy(&["reader"]));
    schema.tables.insert(table(), t);
    assert_eq!(map.schema(&schema), schema);
    let mut ids = IdsFile::default();
    ids.roles
        .insert(Uid::generate(UidKind::Role), name("reader"));
    assert_eq!(map.ids(&ids), ids);
    let changes = ChangeSet {
        changes: vec![planned(Change::CreateRole {
            uid: Uid::generate(UidKind::Role),
            name: name("reader"),
        })],
    };
    assert_eq!(map.changes(&changes), changes);
    // Negative: the default mapping makes nothing.
    assert!(RoleMapping::default().created().is_empty());
}
