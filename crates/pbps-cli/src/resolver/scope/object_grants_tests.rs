use super::*;
use pbps_model::{ObjectName, PlannedChange, Role, RoleName, Uid, UidKind};

fn environment_key() -> EnvironmentFingerprintKey {
    let path = std::env::temp_dir().join(format!("pbps1454-key-{}", rand::random::<u64>()));
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all(pbps_db::fingerprint::FingerprintKey::generate().as_bytes())
        .unwrap();
    let key = EnvironmentFingerprintKey::from_file(&path);
    drop(file);
    std::fs::remove_file(path).unwrap();
    key.unwrap()
}

fn object(name: &str) -> GrantTarget {
    GrantTarget::Object(ObjectName::new("app", name))
}

fn read(target: &GrantTarget, permissions: &[Permission]) -> Pulled {
    let mut read = Pulled::default();
    read.owners.insert(target.clone(), "owner".into());
    read.schema.roles.insert(
        "reader".into(),
        Role {
            grants: BTreeMap::from([(target.clone(), permissions.iter().copied().collect())]),
            ..Role::default()
        },
    );
    read
}

fn write(target: GrantTarget, permission: Permission, grant: bool) -> Change {
    let role = "reader".into();
    let permissions = BTreeSet::from([permission]);
    if grant {
        Change::Grant {
            role,
            target,
            permissions,
        }
    } else {
        Change::Revoke {
            role,
            target,
            permissions,
        }
    }
}

fn changes(changes: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    }
}

#[test]
fn a_selected_grant_moves_its_answer_and_refuses_an_independent_revoke() {
    let key = environment_key();
    let target = object("t");
    let before = read(&target, &[]);
    let changes = changes(vec![write(target.clone(), Permission::Select, true)]);
    let sealed = seal(&key, &changes, &IdsFile::default(), Some(&before)).unwrap();
    assert_ne!(sealed[0].before.fingerprint, sealed[0].after.fingerprint);
    assert!(
        recheck(&key, &changes, &sealed, &before, End::Before)
            .unwrap()
            .1
            .is_empty()
    );
    let after = read(&target, &[Permission::Select]);
    assert!(
        recheck(&key, &changes, &sealed, &after, End::After)
            .unwrap()
            .1
            .is_empty()
    );
    assert_eq!(
        recheck(&key, &changes, &sealed, &before, End::After)
            .unwrap()
            .1
            .len(),
        1
    );
    let unrelated = read(&target, &[Permission::Select, Permission::Update]);
    assert!(
        recheck(&key, &changes, &sealed, &unrelated, End::After)
            .unwrap()
            .1
            .is_empty()
    );
    let mut other_role = after;
    other_role
        .schema
        .roles
        .insert("outsider".into(), Role::default());
    assert!(
        recheck(&key, &changes, &sealed, &other_role, End::After)
            .unwrap()
            .1
            .is_empty()
    );
}

#[test]
fn repeated_writes_use_the_final_permission_without_importing_untouched_acl() {
    let key = environment_key();
    let target = object("t");
    let before = read(&target, &[Permission::Update]);
    let changes = changes(vec![
        write(target.clone(), Permission::Select, true),
        write(target.clone(), Permission::Select, false),
        write(target.clone(), Permission::Delete, true),
    ]);
    let sealed = seal(&key, &changes, &IdsFile::default(), Some(&before)).unwrap();
    let good = read(&target, &[Permission::Update, Permission::Delete]);
    assert!(
        recheck(&key, &changes, &sealed, &good, End::After)
            .unwrap()
            .1
            .is_empty()
    );
    let wrong = read(&target, &[Permission::Select, Permission::Delete]);
    assert_eq!(
        recheck(&key, &changes, &sealed, &wrong, End::After)
            .unwrap()
            .1
            .len(),
        2
    );
}

#[test]
fn present_empty_absent_and_unreadable_never_collapse() {
    let key = environment_key();
    let target = object("t");
    let present = read(&target, &[]);
    let changes = changes(vec![write(target.clone(), Permission::Select, false)]);
    let sealed = seal(&key, &changes, &IdsFile::default(), Some(&present)).unwrap();
    assert!(
        recheck(&key, &changes, &sealed, &present, End::After)
            .unwrap()
            .1
            .is_empty()
    );
    let mut absent = present.clone();
    absent.owners.remove(&target);
    assert_eq!(
        recheck(&key, &changes, &sealed, &absent, End::After)
            .unwrap()
            .1
            .len(),
        1
    );
    let mut unreadable = present;
    unreadable
        .unexpressible
        .push(pbps_db::catalog::Unexpressible {
            role: "reader".into(),
            target: Some(target),
            what: "GRANT OPTION".into(),
        });
    assert!(recheck(&key, &changes, &sealed, &unreadable, End::After).is_err());
    assert!(seal(&key, &changes, &IdsFile::default(), None).is_err());
}

#[test]
fn fingerprints_bind_the_target_principal_mask_and_key() {
    let key = environment_key();
    let target = object("t");
    let before = read(&target, &[]);
    let changes = changes(vec![write(target, Permission::Select, true)]);
    let sealed = seal(&key, &changes, &IdsFile::default(), Some(&before)).unwrap();
    let other_key = environment_key();
    assert_eq!(
        recheck(&other_key, &changes, &sealed, &before, End::Before)
            .unwrap()
            .1
            .len(),
        1
    );
    let mut altered = sealed.clone();
    altered[0].before.target = GrantTargetState::Present {
        target: object("other"),
    };
    assert_eq!(
        recheck(&key, &changes, &altered, &before, End::Before)
            .unwrap()
            .1
            .len(),
        1
    );
    altered = sealed.clone();
    altered[0].before.grantee = GrantPrincipal::Role {
        name: "outsider".into(),
    };
    assert_eq!(
        recheck(&key, &changes, &altered, &before, End::Before)
            .unwrap()
            .1
            .len(),
        1
    );
    let mut different_mask = changes.clone();
    different_mask.changes[0].change = write(object("t"), Permission::Update, true);
    assert_eq!(
        recheck(&key, &different_mask, &sealed, &before, End::Before)
            .unwrap()
            .1
            .len(),
        1
    );
}

#[test]
fn public_execution_is_separate_from_a_named_public_role_and_other_overloads() {
    let key = environment_key();
    let routine: RoutineId = "app.f(integer)".parse().unwrap();
    let target = GrantTarget::Routine(routine.clone());
    let mut before = read(&target, &[]);
    before
        .schema
        .roles
        .insert(RoleName::from("PUBLIC"), Role::default());
    let mut change = write(target.clone(), Permission::Execute, true);
    if let Change::Grant { role, .. } = &mut change {
        *role = "PUBLIC".into();
    }
    let named = changes(vec![change]);
    let sealed = seal(&key, &named, &IdsFile::default(), Some(&before)).unwrap();
    let mut after = before.clone();
    after
        .public_execute
        .insert(ModuleId::Routine(routine.clone()));
    assert_eq!(
        recheck(&key, &named, &sealed, &after, End::After)
            .unwrap()
            .1
            .len(),
        1
    );
    after
        .schema
        .roles
        .get_mut(&RoleName::from("PUBLIC"))
        .unwrap()
        .grants
        .insert(target, BTreeSet::from([Permission::Execute]));
    assert!(
        recheck(&key, &named, &sealed, &after, End::After)
            .unwrap()
            .1
            .is_empty()
    );
    let public = changes(vec![Change::PublicExecution {
        routine,
        access: pbps_model::PublicAccess::Kept,
        origin: pbps_model::RoutineOrigin::Rebuilt,
    }]);
    let sealed = seal(&key, &public, &IdsFile::default(), Some(&before)).unwrap();
    let mut wrong = before;
    wrong
        .public_execute
        .insert(ModuleId::Routine("app.f(text)".parse().unwrap()));
    assert_eq!(
        recheck(&key, &public, &sealed, &wrong, End::After)
            .unwrap()
            .1
            .len(),
        1
    );
    assert!(
        recheck(&key, &public, &sealed, &after, End::After)
            .unwrap()
            .1
            .is_empty()
    );
}

#[test]
fn tracked_names_use_their_grant_time_holder_and_endpoint_coordinates() {
    let uid = Uid::derived(UidKind::Table, "grant1454", 0);
    let old = ObjectName::new("app", "old");
    let new = ObjectName::new("app", "new");
    let changes = changes(vec![
        Change::RenameTable {
            uid: uid.clone(),
            from: old.clone(),
            to: new.clone(),
            defaults: vec![],
        },
        write(GrantTarget::Object(new.clone()), Permission::Select, true),
    ]);
    let ids = IdsFile {
        tables: BTreeMap::from([(uid, new.clone())]),
        ..IdsFile::default()
    };
    let before = read(&GrantTarget::Object(old.clone()), &[]);
    let key = environment_key();
    let sealed = seal(&key, &changes, &ids, Some(&before)).unwrap();
    assert_eq!(
        sealed[0].before.target,
        GrantTargetState::Present {
            target: GrantTarget::Object(old)
        }
    );
    assert_eq!(
        sealed[0].after.target,
        GrantTargetState::Present {
            target: GrantTarget::Object(new.clone())
        }
    );
    let after = read(&GrantTarget::Object(new), &[Permission::Select]);
    assert!(
        recheck(&key, &changes, &sealed, &after, End::After)
            .unwrap()
            .1
            .is_empty()
    );
    assert!(
        !recheck(&key, &changes, &sealed, &before, End::After)
            .unwrap()
            .1
            .is_empty()
    );
}

#[test]
fn routine_type_moves_preserve_quoted_names_arrays_and_unrelated_arguments() {
    let from: pbps_model::TypeRef = "app.\"old,type\"".parse().unwrap();
    let to: pbps_model::TypeRef = "app.\"new,type\"".parse().unwrap();
    let mut routine: RoutineId = "app.f(app.\"old,type\"[],integer)".parse().unwrap();
    move_type(&mut routine, &from, &to).unwrap();
    assert_eq!(
        routine,
        "app.f(app.\"new,type\"[],integer)".parse().unwrap()
    );
    move_type(&mut routine, &to, &from).unwrap();
    assert_eq!(
        routine,
        "app.f(app.\"old,type\"[],integer)".parse().unwrap()
    );
    let other: pbps_model::TypeRef = "app.other".parse().unwrap();
    let copy = routine.clone();
    move_type(&mut routine, &other, &to).unwrap();
    assert_eq!(routine, copy);
}

#[test]
fn a_bare_execute_target_requires_exactly_one_canonical_overload() {
    let target = object("f");
    let change = changes(vec![write(target.clone(), Permission::Execute, true)]);
    let write = &AuthorizationGrant::writes(&change)[&0];
    let mut read = read(
        &GrantTarget::Routine("app.f(integer)".parse().unwrap()),
        &[],
    );
    assert_eq!(
        canonical_target(&read, &change, 0, write).unwrap(),
        GrantTarget::Routine("app.f(integer)".parse().unwrap())
    );
    read.owners.insert(
        GrantTarget::Routine("app.f(text)".parse().unwrap()),
        "owner".into(),
    );
    assert!(canonical_target(&read, &change, 0, write).is_err());
    read.owners.clear();
    assert!(canonical_target(&read, &change, 0, write).is_err());
}

#[test]
fn an_opening_answer_cannot_be_replayed_as_a_closing_condition() {
    let key = environment_key();
    let target = object("t");
    let before = read(&target, &[]);
    let changes = changes(vec![write(target, Permission::Select, true)]);
    let mut sealed = seal(&key, &changes, &IdsFile::default(), Some(&before)).unwrap();
    // A public opening fingerprint is not authority to replace the sealed
    // closing question, even when the same target and role are present.
    let copied_aggregate = crate::resolver::scope::authorization_digest(
        &key,
        b"unchanged deployer context",
        &[sealed[0].before.fingerprint.clone()],
    );
    sealed[0].after = sealed[0].before.clone();
    let (fresh, changed) = recheck(&key, &changes, &sealed, &before, End::After).unwrap();
    assert_ne!(
        crate::resolver::scope::authorization_digest(&key, b"unchanged deployer context", &fresh),
        copied_aggregate,
        "opening answers cannot certify closing, even with a copied aggregate",
    );
    assert_eq!(changed.len(), 1, "opening answers cannot certify closing",);
}
