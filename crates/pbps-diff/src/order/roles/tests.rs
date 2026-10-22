use super::*;
use pbps_model::{PlannedChange, TableName, Uid, UidKind};

fn drop(name: &str, members: &[&str], salt: u32) -> PlannedChange {
    PlannedChange::new(Change::DropRole {
        uid: Uid::derived(UidKind::Role, name, salt),
        name: name.into(),
        members: members.iter().map(|m| (*m).into()).collect(),
    })
}

// The mixed-slot fixture deliberately includes changes irrelevant to role names.
#[allow(clippy::wildcard_enum_match_arm)]
fn names(changes: &ChangeSet) -> Vec<&str> {
    changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::DropRole { name, .. } => Some(name.raw()),
            _ => None,
        })
        .collect()
}

fn same_as_before(input: ChangeSet) -> ChangeSet {
    let mut expected = input.clone();
    legacy::legacy_order(&mut expected);
    let actual = role_drops(crate::order::propose(input)).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(legacy::checksum(&actual), legacy::checksum(&expected));
    actual.changes().clone()
}

#[test]
fn role_depths_and_peer_order_match_the_legacy_sort_across_multiple_branches() {
    for salt in 0..32 {
        let roles = vec![
            drop("shared", &[], salt),
            drop("leaf", &["some_user"], salt),
            drop("branch", &["shared"], salt),
            drop("unrelated", &[], salt),
            drop("first_root", &["leaf"], salt),
            drop("second_root", &["branch", "shared"], salt),
        ];
        // Each starting position gets a turn at the front. Reversing the
        // rest also changes the stable order of peers without changing depths.
        for reverse in [false, true] {
            for offset in 0..roles.len() {
                let mut changes = roles.clone();
                if reverse {
                    changes.reverse();
                }
                changes.rotate_left(offset);
                let input = ChangeSet { changes };
                let before = names(&input);
                let expected: Vec<_> = [
                    &["unrelated", "first_root", "second_root"][..],
                    &["leaf", "branch"][..],
                    &["shared"][..],
                ]
                .iter()
                .flat_map(|level| before.iter().copied().filter(|name| level.contains(name)))
                .collect();
                let actual = same_as_before(input.clone());
                assert_eq!(names(&actual), expected);
            }
        }
    }
}

#[test]
fn role_drops_leave_every_non_role_slot_and_payload_unchanged() {
    let other = |name: &str| {
        PlannedChange::new(Change::DropTable {
            uid: Uid::derived(UidKind::Table, name, 0),
            name: TableName::new("dbo", name),
            detach_from: None,
        })
    };
    let plan = ChangeSet {
        changes: vec![
            other("before"),
            drop("member", &[], 0),
            other("between"),
            drop("holder", &["member"], 0),
            other("after"),
        ],
    };
    let actual = same_as_before(plan.clone());
    assert_eq!(names(&actual), ["holder", "member"]);
    for i in [0, 2, 4] {
        assert_eq!(actual.changes[i], plan.changes[i]);
    }
}

#[test]
fn roles_with_no_dropped_members_keep_the_entire_plan_unchanged() {
    for changes in [
        vec![],
        vec![drop("only", &["some_user"], 0)],
        vec![drop("z", &["some_user"], 0), drop("a", &[], 0)],
    ] {
        let input = ChangeSet { changes };
        assert_eq!(same_as_before(input.clone()), input);
    }
}

#[test]
fn a_role_membership_cycle_is_refused_by_name() {
    let input = ChangeSet {
        changes: vec![drop("a", &["b"], 0), drop("b", &["a"], 0)],
    };
    let why = role_drops(crate::order::propose(input)).unwrap_err();
    assert!(
        why.contains("`role a`") && why.contains("`role b`") && why.contains("authorization"),
        "{why}"
    );
}

#[test]
fn a_same_depth_role_cannot_cross_its_earlier_peer() {
    let drops = ChangeSet {
        changes: vec![drop("a", &[], 0), drop("b", &[], 0)],
    };
    let mut schedule = constraints(&drops);
    schedule.edge(1, 0, Source::Catalog, OrderReason::Authorization);
    let why = schedule.settle_raw(drops).unwrap_err();
    assert!(matches!(why, super::super::Refusal::Cycle { .. }), "{why}");
}

#[test]
fn role_membership_order_uses_raw_names_instead_of_shown_lookalikes() {
    let raw = "role\nline\\\u{202e}名前";
    let lookalike = pbps_model::RoleName::from(raw).shown().to_string();
    let input = ChangeSet {
        changes: vec![drop(raw, &[], 0), drop(&lookalike, &[raw], 0)],
    };
    assert_eq!(names(&same_as_before(input)), [lookalike.as_str(), raw]);
    let independent = ChangeSet {
        changes: vec![drop(raw, &[], 0), drop(&lookalike, &["absent"], 0)],
    };
    assert_eq!(same_as_before(independent.clone()), independent);
}
