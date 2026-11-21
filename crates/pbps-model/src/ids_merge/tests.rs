use super::*;
use crate::ids_tree::parse;
use crate::ids_tree::tests::tree;

fn uid(s: &str) -> Uid {
    s.parse().unwrap()
}

fn tomb(was: &str) -> Tombstone {
    Tombstone {
        was: was.into(),
        dropped_at: "2026-01-01".into(),
        reason: "r".into(),
        operator: "o".into(),
        require: None,
    }
}

fn live(u: &str, name: &str) -> (Uid, Rec) {
    (uid(u), Rec::Live(name.into()))
}

fn file(table: &str, columns: Vec<(Uid, Rec)>) -> TreeFile {
    TreeFile {
        uid: uid("t_aaaaaa"),
        body: Body::Table(TableBody {
            table: table.into(),
            columns,
            ..Default::default()
        }),
        dropped: None,
    }
}

fn cols(f: &TreeFile) -> Vec<(String, Rec)> {
    match &f.body {
        Body::Table(t) => t
            .columns
            .iter()
            .map(|(u, r)| (u.to_string(), r.clone()))
            .collect(),
        Body::Role(_) | Body::Type(_) | Body::Search(_) | Body::Extension(_) => unreachable!(),
    }
}

fn yaml(name: &str, columns: &[&str]) -> YamlView {
    YamlView {
        name: Some(name.into()),
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
    }
}

fn base() -> TreeFile {
    file(
        "dbo.t",
        vec![live("c_aaaaa1", "id"), live("c_aaaaa2", "name")],
    )
}

#[test]
fn a_change_on_one_side_is_taken_over_the_side_that_left_it() {
    let mut ours = base();
    let Body::Table(t) = &mut ours.body else {
        unreachable!()
    };
    t.columns[1].1 = Rec::Live("full_name".into());
    let merged = merge(
        Some(&base()),
        Some(&ours),
        Some(&base()),
        &yaml("dbo.t", &["id", "full_name"]),
    );
    assert_eq!(merged.unwrap().unwrap(), ours);
    let merged = merge(
        Some(&base()),
        Some(&base()),
        Some(&ours),
        &yaml("dbo.t", &["id", "full_name"]),
    );
    assert_eq!(merged.unwrap().unwrap(), ours);
}

#[test]
fn the_same_change_on_both_sides_is_taken_once() {
    let mut both = base();
    let Body::Table(t) = &mut both.body else {
        unreachable!()
    };
    t.columns[1].1 = Rec::Dead(tomb("dbo.t.name"));
    let merged = merge(
        Some(&base()),
        Some(&both),
        Some(&both),
        &YamlView::default(),
    );
    assert_eq!(merged.unwrap().unwrap(), both);
}

#[test]
fn two_different_names_are_decided_by_the_name_the_resolved_yaml_declares() {
    let rename = |to: &str| {
        let mut f = base();
        let Body::Table(t) = &mut f.body else {
            unreachable!()
        };
        t.columns[1].1 = Rec::Live(to.into());
        f
    };
    let (ours, theirs) = (rename("full_name"), rename("display_name"));
    let view = yaml("dbo.t", &["id", "display_name"]);
    let merged = merge(Some(&base()), Some(&ours), Some(&theirs), &view)
        .unwrap()
        .unwrap();
    assert_eq!(cols(&merged)[1].1, Rec::Live("display_name".into()));
}

#[test]
fn two_different_names_the_yaml_cannot_decide_are_refused_with_both_sides() {
    let rename = |to: &str| {
        let mut f = base();
        let Body::Table(t) = &mut f.body else {
            unreachable!()
        };
        t.columns[1].1 = Rec::Live(to.into());
        f
    };
    let (ours, theirs) = (rename("full_name"), rename("display_name"));
    // Neither name, and both names, are no answer.
    for names in [
        &["id", "other"][..],
        &["id", "full_name", "display_name"][..],
    ] {
        let view = yaml("dbo.t", names);
        let conflicts = merge(Some(&base()), Some(&ours), Some(&theirs), &view).unwrap_err();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        let c = &conflicts[0];
        assert_eq!(c.uid, uid("c_aaaaa2"));
        assert_eq!(c.section, "columns");
        assert!(c.ours.contains("full_name") && c.theirs.contains("display_name"));
        assert!(c.remedy.contains("pbps rename"));
    }
}

#[test]
fn a_column_added_on_each_side_is_kept_in_the_order_the_yaml_gives() {
    let add = |u: &str, n: &str| {
        let mut f = base();
        let Body::Table(t) = &mut f.body else {
            unreachable!()
        };
        t.columns.push(live(u, n));
        f
    };
    let (ours, theirs) = (add("c_aaaaa3", "email"), add("c_aaaaa4", "phone"));
    let view = yaml("dbo.t", &["id", "phone", "name", "email"]);
    let merged = merge(Some(&base()), Some(&ours), Some(&theirs), &view)
        .unwrap()
        .unwrap();
    let names: Vec<_> = cols(&merged)
        .into_iter()
        .map(|(_, r)| match r {
            Rec::Live(n) => n,
            Rec::Dead(_) => unreachable!(),
        })
        .collect();
    assert_eq!(names, ["id", "phone", "name", "email"]);
}

#[test]
fn a_drop_on_one_side_wins_over_an_untouched_record_but_not_over_a_rename() {
    let mut dropped = base();
    let Body::Table(t) = &mut dropped.body else {
        unreachable!()
    };
    t.columns[1].1 = Rec::Dead(tomb("dbo.t.name"));
    let merged = merge(
        Some(&base()),
        Some(&dropped),
        Some(&base()),
        &YamlView::default(),
    );
    assert_eq!(merged.unwrap().unwrap(), dropped);
    let mut renamed = base();
    let Body::Table(t) = &mut renamed.body else {
        unreachable!()
    };
    t.columns[1].1 = Rec::Live("full_name".into());
    let view = yaml("dbo.t", &["id", "full_name"]);
    let conflicts = merge(Some(&base()), Some(&dropped), Some(&renamed), &view).unwrap_err();
    assert_eq!(conflicts[0].uid, uid("c_aaaaa2"));
}

#[test]
fn a_record_one_side_deleted_and_the_other_left_alone_is_gone() {
    let mut ours = base();
    let Body::Table(t) = &mut ours.body else {
        unreachable!()
    };
    t.columns.pop();
    let merged = merge(
        Some(&base()),
        Some(&ours),
        Some(&base()),
        &YamlView::default(),
    );
    assert_eq!(cols(&merged.unwrap().unwrap()).len(), 1);
}

#[test]
fn a_renamed_table_header_follows_one_side_or_the_yaml() {
    let (ours, theirs) = (file("dbo.a", base_cols()), file("dbo.b", base_cols()));
    let base = file("dbo.t", base_cols());
    let ok = merge(Some(&base), Some(&ours), Some(&base), &YamlView::default());
    assert_eq!(ok.unwrap().unwrap(), ours);
    let view = yaml("dbo.b", &["id", "name"]);
    let ok = merge(Some(&base), Some(&ours), Some(&theirs), &view)
        .unwrap()
        .unwrap();
    assert_eq!(ok, theirs);
    let conflicts = merge(
        Some(&base),
        Some(&ours),
        Some(&theirs),
        &YamlView::default(),
    )
    .unwrap_err();
    assert_eq!(conflicts[0].section, "name");
}

fn base_cols() -> Vec<(Uid, Rec)> {
    vec![live("c_aaaaa1", "id"), live("c_aaaaa2", "name")]
}

#[test]
fn a_file_deleted_on_one_side_is_deleted_only_if_the_other_side_left_it_alone() {
    let none = YamlView::default();
    assert_eq!(merge(Some(&base()), None, Some(&base()), &none), Ok(None));
    assert_eq!(merge(Some(&base()), Some(&base()), None, &none), Ok(None));
    let mut changed = base();
    let Body::Table(t) = &mut changed.body else {
        unreachable!()
    };
    t.columns.pop();
    let conflicts = merge(Some(&base()), None, Some(&changed), &none).unwrap_err();
    assert_eq!(conflicts[0].section, "file");
    assert_eq!(conflicts[0].ours, "(deleted)");
    // Added on one side only: kept.
    assert_eq!(merge(None, Some(&base()), None, &none), Ok(Some(base())));
    assert_eq!(merge(None, None, None, &none), Ok(None));
}

#[test]
fn two_identities_at_one_path_are_refused() {
    let mut other = base();
    other.uid = uid("t_bbbbbb");
    let conflicts = merge(None, Some(&base()), Some(&other), &YamlView::default()).unwrap_err();
    assert_eq!(conflicts[0].section, "uid");
    // A table and a role at one path are no better.
    let role = TreeFile {
        uid: uid("t_aaaaaa"),
        body: Body::Role("r".into()),
        dropped: None,
    };
    assert!(merge(None, Some(&base()), Some(&role), &YamlView::default()).is_err());
}

#[test]
fn a_dropped_header_changed_on_both_sides_differently_is_refused() {
    let drop_with = |why: &str| {
        let mut f = base();
        f.dropped = Some(Tombstone {
            reason: why.into(),
            ..tomb("dbo.t")
        });
        f
    };
    let conflicts = merge(
        Some(&base()),
        Some(&drop_with("a")),
        Some(&drop_with("b")),
        &YamlView::default(),
    )
    .unwrap_err();
    assert_eq!(conflicts[0].section, "dropped");
}

#[test]
fn merging_a_file_with_itself_or_with_its_base_changes_nothing() {
    for (path, text) in tree() {
        let f = parse(&path, &text).unwrap();
        let none = YamlView::default();
        assert_eq!(
            merge(Some(&f), Some(&f), Some(&f), &none).unwrap().unwrap(),
            f,
            "{path}"
        );
        assert_eq!(
            merge(None, Some(&f), Some(&f), &none).unwrap().unwrap(),
            f,
            "{path}"
        );
    }
}

fn retirement(reason: &str) -> crate::ExclusionRetirement {
    crate::ExclusionRetirement {
        was: "{}".into(),
        reason: reason.into(),
        operator: "o".into(),
        dropped_at: "2026-11-15".into(),
    }
}

fn with_exclusions(mut f: TreeFile, records: &[(&str, &str)]) -> TreeFile {
    let Body::Table(t) = &mut f.body else {
        unreachable!()
    };
    t.exclusions = records
        .iter()
        .map(|(name, reason)| ((*name).to_owned(), retirement(reason)))
        .collect();
    f
}

/// DEC-2448.4: a retirement is a record of its own in the table's file, so two
/// branches retiring different constraints both keep theirs, and two retiring
/// one with different reasons conflict under the table's uid.
#[test]
fn retirements_of_different_constraints_merge_and_the_same_one_differently_conflicts() {
    let none = YamlView::default();
    let base = base();
    let ours = with_exclusions(base.clone(), &[("a", "r")]);
    let theirs = with_exclusions(base.clone(), &[("b", "r")]);
    let merged = merge(Some(&base), Some(&ours), Some(&theirs), &none)
        .unwrap()
        .unwrap();
    let Body::Table(t) = &merged.body else {
        unreachable!()
    };
    let names: Vec<_> = t.exclusions.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    let theirs = with_exclusions(base.clone(), &[("a", "another reason")]);
    let conflicts = merge(Some(&base), Some(&ours), Some(&theirs), &none).unwrap_err();
    assert_eq!(conflicts[0].section, "exclusions");
    assert_eq!(conflicts[0].uid, uid("t_aaaaaa"));
    // A record one side deleted and the other left alone is gone.
    let merged = merge(Some(&ours), Some(&base), Some(&ours), &none)
        .unwrap()
        .unwrap();
    let Body::Table(t) = &merged.body else {
        unreachable!()
    };
    assert!(t.exclusions.is_empty());
}

fn with_partition_exclusions(mut f: TreeFile, child: &str, records: &[(&str, &str)]) -> TreeFile {
    let Body::Table(t) = &mut f.body else {
        unreachable!()
    };
    t.partition_exclusions = vec![(
        uid(child),
        records
            .iter()
            .map(|(name, reason)| ((*name).to_owned(), retirement(reason)))
            .collect(),
    )];
    f
}

/// DEC-2448.7: a partition child's retirements sit in its parent's file by the child's uid, so
/// two branches retiring a constraint of two different children both keep theirs, and two
/// retiring one of the same child with different reasons conflict under the child's uid.
/// Negative: a record one side deleted and the other left alone is gone.
#[test]
fn retirements_of_two_partitions_merge_and_one_partition_retired_two_ways_conflicts() {
    let none = YamlView::default();
    let base = base();
    let ours = with_partition_exclusions(base.clone(), "t_bbbbb1", &[("a", "r")]);
    let theirs = with_partition_exclusions(base.clone(), "t_bbbbb2", &[("a", "r")]);
    let merged = merge(Some(&base), Some(&ours), Some(&theirs), &none)
        .unwrap()
        .unwrap();
    let Body::Table(t) = &merged.body else {
        unreachable!()
    };
    let children: Vec<_> = t
        .partition_exclusions
        .iter()
        .map(|(u, _)| u.to_string())
        .collect();
    assert_eq!(children, ["t_bbbbb1", "t_bbbbb2"]);
    let theirs = with_partition_exclusions(base.clone(), "t_bbbbb1", &[("a", "another reason")]);
    let conflicts = merge(Some(&base), Some(&ours), Some(&theirs), &none).unwrap_err();
    assert_eq!(conflicts[0].section, "partition_exclusions");
    assert_eq!(conflicts[0].uid, uid("t_bbbbb1"));
    let merged = merge(Some(&ours), Some(&base), Some(&ours), &none)
        .unwrap()
        .unwrap();
    let Body::Table(t) = &merged.body else {
        unreachable!()
    };
    assert!(t.partition_exclusions.is_empty());
}

/// DEC-2448.7: the record is one per (child, constraint name), as a table's own are per
/// name, so two branches retiring different constraints of one child both keep theirs.
/// Negative: the same constraint of the same child retired two ways conflicts, named by
/// the child, and a record one side deleted while the other kept a sibling stays gone.
#[test]
fn retirements_of_different_constraints_of_one_partition_both_survive_the_merge() {
    let none = YamlView::default();
    let base = base();
    let ours = with_partition_exclusions(base.clone(), "t_bbbbb1", &[("a", "r")]);
    let theirs = with_partition_exclusions(base.clone(), "t_bbbbb1", &[("b", "r")]);
    let merged = merge(Some(&base), Some(&ours), Some(&theirs), &none)
        .unwrap()
        .unwrap();
    let Body::Table(t) = &merged.body else {
        unreachable!()
    };
    assert_eq!(t.partition_exclusions.len(), 1);
    assert_eq!(t.partition_exclusions[0].0, uid("t_bbbbb1"));
    let names: Vec<_> = t.partition_exclusions[0]
        .1
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(names, ["a", "b"]);
    // The same (child, name) with different records conflicts under the child.
    let theirs = with_partition_exclusions(base.clone(), "t_bbbbb1", &[("a", "other"), ("b", "r")]);
    let conflicts = merge(Some(&base), Some(&ours), Some(&theirs), &none).unwrap_err();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].section, "partition_exclusions");
    assert_eq!(conflicts[0].uid, uid("t_bbbbb1"));
    // One side deleted `a` from the base that held both; the other kept both.
    let both = with_partition_exclusions(base.clone(), "t_bbbbb1", &[("a", "r"), ("b", "r")]);
    let only_b = with_partition_exclusions(base.clone(), "t_bbbbb1", &[("b", "r")]);
    let merged = merge(Some(&both), Some(&only_b), Some(&both), &none)
        .unwrap()
        .unwrap();
    let Body::Table(t) = &merged.body else {
        unreachable!()
    };
    let names: Vec<_> = t.partition_exclusions[0]
        .1
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(names, ["b"]);
}
