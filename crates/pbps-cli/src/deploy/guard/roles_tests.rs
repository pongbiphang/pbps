use super::{Plan, Read, Staged};
use crate::deploy::{StagedRead, refuse_pinned_movement};
use pbps_model::{
    Change, ChangeSet, GrantTarget, IdsFile, Permission, PlannedChange, Role, Schema,
};

fn schema(roles: &[(&str, &[Permission])]) -> Schema {
    Schema {
        roles: roles
            .iter()
            .map(|(name, permissions)| {
                let grants = if permissions.is_empty() {
                    Default::default()
                } else {
                    [(
                        GrantTarget::Schema("dbo".into()),
                        permissions.iter().copied().collect(),
                    )]
                    .into()
                };
                (
                    name.to_string(),
                    Role {
                        description: None,
                        grants,
                    },
                )
            })
            .collect(),
        ..Schema::default()
    }
}

fn changes(steps: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes: steps.into_iter().map(PlannedChange::new).collect(),
    }
}

fn rename(uid: &str, from: &str, to: &str) -> Change {
    Change::RenameRole {
        uid: uid.parse().unwrap(),
        from: from.into(),
        to: to.into(),
    }
}

fn drop_role(uid: &str, name: &str) -> Change {
    Change::DropRole {
        uid: uid.parse().unwrap(),
        name: name.into(),
        members: vec![],
    }
}

fn ids(roles: &[(&str, &str)]) -> IdsFile {
    IdsFile {
        roles: roles
            .iter()
            .map(|(uid, name)| (uid.parse().unwrap(), name.to_string()))
            .collect(),
        ..IdsFile::default()
    }
}

fn compare(cs: &ChangeSet, ids: &IdsFile, before: &Schema, after: &Schema) -> Result<(), String> {
    refuse_pinned_movement(
        &pbps_mssql::Mssql,
        Read::transactional(Plan { changes: cs, ids }),
        before,
        after,
        "test",
    )
    .map_err(|e| format!("{e:#}"))
}

#[test]
fn a_role_handoff_compares_only_the_surviving_identitys_grants() {
    use Permission::{Insert, Select, Update};
    let cs = changes(vec![
        drop_role("r_aaaaaa", "a"),
        rename("r_bbbbbb", "b", "a"),
    ]);
    let ids = ids(&[("r_bbbbbb", "a")]);
    let before = schema(&[("a", &[Insert]), ("b", &[Select])]);
    compare(&cs, &ids, &before, &schema(&[("a", &[Select])])).unwrap();
    for held in [&[][..], &[Select, Update][..], &[Insert][..]] {
        let error = compare(&cs, &ids, &before, &schema(&[("a", held)])).unwrap_err();
        assert!(
            error.contains("role a does not hold on schema::dbo"),
            "{error}"
        );
    }
}

#[test]
fn a_replacement_role_starts_without_the_retired_holders_grants() {
    use Permission::{Insert, Select};
    let cs = changes(vec![
        drop_role("r_aaaaaa", "a"),
        Change::CreateRole {
            uid: "r_bbbbbb".parse().unwrap(),
            name: "a".into(),
        },
        Change::Grant {
            role: "a".into(),
            target: GrantTarget::Schema("dbo".into()),
            permissions: [Select].into(),
        },
    ]);
    let ids = ids(&[("r_bbbbbb", "a")]);
    let before = schema(&[("a", &[Insert])]);
    compare(&cs, &ids, &before, &schema(&[("a", &[Select])])).unwrap();
    for held in [&[][..], &[Select, Insert][..]] {
        assert!(compare(&cs, &ids, &before, &schema(&[("a", held)])).is_err());
    }
}

#[test]
fn a_new_role_at_a_vacated_name_is_not_forwarded_through_the_old_rename() {
    use Permission::{Insert, Select};
    let cs = changes(vec![
        rename("r_aaaaaa", "a", "b"),
        Change::CreateRole {
            uid: "r_bbbbbb".parse().unwrap(),
            name: "a".into(),
        },
        Change::Grant {
            role: "a".into(),
            target: GrantTarget::Schema("dbo".into()),
            permissions: [Insert].into(),
        },
    ]);
    let ids = ids(&[("r_aaaaaa", "b"), ("r_bbbbbb", "a")]);
    let before = schema(&[("a", &[Select])]);
    let after = schema(&[("a", &[Insert]), ("b", &[Select])]);
    compare(&cs, &ids, &before, &after).unwrap();
    for name in ["a", "b"] {
        let mut robbed = after.clone();
        robbed.roles.get_mut(name).unwrap().grants.clear();
        let error = compare(&cs, &ids, &before, &robbed).unwrap_err();
        assert!(
            error.contains(&format!("role {name} does not hold")),
            "{error}"
        );
    }
}

#[test]
fn role_grants_follow_repeated_moves_and_temporary_names() {
    use Permission::{Insert, Select};
    for (cs, final_ids, before, after) in [
        (
            changes(vec![
                rename("r_aaaaaa", "a", "tmp"),
                rename("r_aaaaaa", "tmp", "b"),
            ]),
            ids(&[("r_aaaaaa", "b")]),
            schema(&[("a", &[Select])]),
            schema(&[("b", &[Select])]),
        ),
        (
            changes(vec![
                rename("r_aaaaaa", "a", "tmp"),
                rename("r_bbbbbb", "b", "a"),
                rename("r_aaaaaa", "tmp", "b"),
            ]),
            ids(&[("r_aaaaaa", "b"), ("r_bbbbbb", "a")]),
            schema(&[("a", &[Select]), ("b", &[Insert])]),
            schema(&[("b", &[Select]), ("a", &[Insert])]),
        ),
    ] {
        compare(&cs, &final_ids, &before, &after).unwrap();
        let mut robbed = after.clone();
        robbed.roles.get_mut("b").unwrap().grants.clear();
        assert!(compare(&cs, &final_ids, &before, &robbed).is_err());
    }
}

#[test]
fn staged_role_reads_keep_grants_at_the_observed_boundary() {
    use Permission::{Insert, Select};
    let rename = changes(vec![rename("r_aaaaaa", "a", "b")]);
    let final_ids = ids(&[("r_aaaaaa", "b")]);
    let staged = Staged::new(Plan {
        changes: &rename,
        ids: &final_ids,
    })
    .unwrap();
    let before = schema(&[("a", &[Select])]);
    let after = schema(&[("b", &[Select])]);
    for baseline in [&before, &after] {
        for (held, accepted) in [
            (&[Select][..], true),
            (&[][..], false),
            (&[Select, Insert][..], false),
        ] {
            let result = refuse_pinned_movement(
                &pbps_mssql::Mssql,
                staged.read(StagedRead::Closing { total: 1 }),
                baseline,
                &schema(&[("b", held)]),
                "test",
            );
            assert_eq!(result.is_ok(), accepted, "{result:?}");
        }
    }
    // Membership removal precedes DROP ROLE; retiring the planned UID must
    // not hide permission movement while the role is still observed.
    let mut step = drop_role("r_aaaaaa", "a");
    if let Change::DropRole { members, .. } = &mut step {
        members.push("member".into());
    }
    let dropped = changes(vec![step]);
    let empty_ids = IdsFile::default();
    let staged = Staged::new(Plan {
        changes: &dropped,
        ids: &empty_ids,
    })
    .unwrap();
    for (held, accepted) in [
        (&[Select][..], true),
        (&[][..], false),
        (&[Select, Insert][..], false),
    ] {
        let result = refuse_pinned_movement(
            &pbps_mssql::Mssql,
            staged.read(StagedRead::Checkpoint {
                completed: 1,
                total: 2,
            }),
            &before,
            &schema(&[("a", held)]),
            "test",
        );
        assert_eq!(result.is_ok(), accepted, "{result:?}");
    }
}

#[test]
fn ambiguous_baseline_roles_do_not_overwrite_each_others_grant_comparison() {
    use Permission::{Insert, Select};
    let cs = changes(vec![rename("r_aaaaaa", "a", "b")]);
    let ids = ids(&[("r_aaaaaa", "b")]);
    let before = schema(&[("a", &[Select]), ("b", &[Insert])]);
    for held in [&[Select][..], &[Insert][..]] {
        assert!(compare(&cs, &ids, &before, &schema(&[("b", held)])).is_err());
    }
}
