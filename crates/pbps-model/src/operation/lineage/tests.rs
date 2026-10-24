use super::*;
use crate::{Column, IdsFile, PartitionBound, PartitionBy, PartitionOf, Schema, Table};

type Cols = &'static [(&'static str, &'static str)];
/// How a child stands at one end of a row.
#[derive(Clone, Copy)]
enum Shape {
    Gone,
    Member,
    OfOther,
    Alone(Cols),
}
use Shape::*;

/// (parent column, child, end, physical, slot): a physical column prints as
/// the UID of a recorded column, or `~name` for an inherited slot.
type Ask = (
    &'static str,
    &'static str,
    End,
    Option<&'static str>,
    Option<&'static str>,
);
struct Row {
    label: &'static str,
    parent: [Cols; 2],
    children: &'static [(&'static str, [Shape; 2])],
    asks: &'static [Ask],
    /// (child, end, physical column, the parent column that it carries)
    claims: &'static [(&'static str, End, &'static str, Option<&'static str>)],
}

const P: Cols = &[("c_aaaaaa", "k"), ("c_bbbbbb", "d")];
const P_RENAMED: Cols = &[("c_aaaaaa", "k"), ("c_bbbbbb", "e")];
const K: Cols = &[("c_aaaaaa", "k")];
const D: Cols = &[("c_dddddd", "d")];
const C: &str = "t_cccccc";
const B: &str = "c_bbbbbb";
use End::{Base, Goal};

#[rustfmt::skip]
const ROWS: &[Row] = &[
    Row { label: "attach: the candidate holds the parent's name", parent: [P, P],
        children: &[(C, [Alone(D), Member])],
        asks: &[(B, C, Base, Some("c_dddddd"), Some("~d")), (B, C, Goal, Some("~d"), Some("~d"))],
        claims: &[(C, Base, "c_dddddd", Some(B))] },
    Row { label: "attach across a parent rename: the candidate is cut at the baseline", parent: [P, P_RENAMED],
        children: &[(C, [Alone(D), Member])],
        asks: &[(B, C, Base, Some("c_dddddd"), Some("~d")), (B, C, Goal, Some("~e"), Some("~e"))],
        claims: &[(C, Base, "c_dddddd", Some(B))] },
    Row { label: "attach across a parent rename: the candidate is cut at the goal", parent: [P, P_RENAMED],
        children: &[(C, [Alone(&[("c_dddddd", "e")]), Member])],
        asks: &[(B, C, Base, Some("c_dddddd"), Some("~d"))],
        claims: &[(C, Base, "c_dddddd", Some(B))] },
    Row { label: "detach across a parent rename: the retained column keeps its baseline name", parent: [P, P_RENAMED],
        children: &[(C, [Member, Alone(D)])],
        asks: &[(B, C, Base, Some("~d"), Some("~d")), (B, C, Goal, Some("c_dddddd"), Some("~e"))],
        claims: &[(C, Goal, "c_dddddd", Some(B))] },
    Row { label: "detach drops a column of the standalone: a slot without a physical column", parent: [P, P],
        children: &[(C, [Member, Alone(&[])])],
        asks: &[(B, C, Goal, None, Some("~d"))], claims: &[] },
    Row { label: "a column the parent adds has a known-empty slot in a member at the baseline", parent: [K, P],
        children: &[(C, [Member, Alone(&[("c_aaaaaa", "k")])])],
        asks: &[(B, C, Base, None, Some("~d")), (B, C, Goal, None, Some("~d"))], claims: &[] },
    Row { label: "drop of a member leaves nothing at the goal", parent: [P, P],
        children: &[(C, [Member, Gone])],
        asks: &[(B, C, Base, Some("~d"), Some("~d")), (B, C, Goal, None, Some("~d"))], claims: &[] },
    Row { label: "a stationary member follows the parent's rename while another moves", parent: [P, P_RENAMED],
        children: &[(C, [Member, Gone]), ("t_eeeeee", [Member, Member])],
        asks: &[(B, "t_eeeeee", Base, Some("~d"), Some("~d")), (B, "t_eeeeee", Goal, Some("~e"), Some("~e"))],
        claims: &[] },
    Row { label: "a member of another parent belongs to that parent's family", parent: [P, P],
        children: &[(C, [Member, OfOther])],
        asks: &[(B, C, Goal, None, Some("~d"))], claims: &[] },
    Row { label: "two parent columns claiming one standalone column claim none",
        parent: [&[("c_aaaaaa", "d")], &[("c_aaaaaa", "e"), (B, "d")]],
        children: &[(C, [Alone(D), Member])],
        asks: &[("c_aaaaaa", C, Base, Some("c_dddddd"), Some("~d")), (B, C, Base, Some("c_dddddd"), Some("~d"))],
        claims: &[(C, Base, "c_dddddd", None)] },
    Row { label: "an unrelated column of a standalone claims no parent column", parent: [P, P],
        children: &[(C, [Alone(&[("c_dddddd", "z")]), Member])],
        asks: &[(B, C, Base, None, Some("~d"))], claims: &[(C, Base, "c_dddddd", None)] },
    Row { label: "a table the family never held has no physical column in it", parent: [P, P],
        children: &[(C, [Member, Gone]), ("t_ffffff", [Gone, Gone])],
        asks: &[(B, "t_ffffff", Base, None, None)], claims: &[] },
];

fn table(uid: &str) -> TableName {
    TableName::new("app", &uid[2..3])
}
fn build(row: &Row, end: usize) -> (Schema, IdsFile) {
    let (mut schema, mut ids) = (Schema::default(), IdsFile::default());
    let int = || Column::new("integer".parse().unwrap());
    let member_of = |parent: &str| Table {
        partition_of: Some(PartitionOf {
            parent: TableName::new("app", parent),
            bound: PartitionBound::Default,
            columns: Default::default(),
        }),
        ..Default::default()
    };
    let mut parent = Table {
        partition_by: Some(PartitionBy {
            strategy: Default::default(),
            columns: vec!["k".into()],
            external: false,
        }),
        ..Default::default()
    };
    for (uid, name) in row.parent[end] {
        parent.columns.insert((*name).into(), int());
        ids.columns
            .insert(uid.parse().unwrap(), table("t_aaaaaa").column(*name));
    }
    let mut tables = vec![("t_aaaaaa", parent)];
    tables.push(("t_qqqqqq", Table::default()));
    for (uid, shapes) in row.children {
        match shapes[end] {
            Gone => {}
            Member => tables.push((uid, member_of("a"))),
            OfOther => tables.push((uid, member_of("q"))),
            Alone(cols) => {
                let mut alone = Table::default();
                for (id, name) in cols {
                    alone.columns.insert((*name).into(), int());
                    ids.columns
                        .insert(id.parse().unwrap(), table(uid).column(*name));
                }
                tables.push((uid, alone));
            }
        }
    }
    for (uid, shape) in tables {
        ids.tables.insert(uid.parse().unwrap(), table(uid));
        schema.tables.insert(table(uid), shape);
    }
    (schema, ids)
}
fn show(physical: Option<Physical>) -> Option<String> {
    physical.map(|p| match p.key {
        ColumnKey::Recorded(r) => r.uid().to_string(),
        ColumnKey::Partition { column, .. } => format!("~{}", column.shown()),
    })
}
fn recorded(uid: &str) -> Recorded {
    Recorded(uid.parse().unwrap())
}

#[test]
fn a_childs_physical_column_is_found_by_identity_at_both_ends() {
    for row in ROWS {
        let (b, b_ids) = build(row, 0);
        let (g, g_ids) = build(row, 1);
        let lineage = Lineage::of(
            Snapshot {
                schema: &b,
                ids: &b_ids,
            },
            Snapshot {
                schema: &g,
                ids: &g_ids,
            },
        );
        let family = lineage.family(&recorded("t_aaaaaa")).expect(row.label);
        for (column, child, end, physical, slot) in row.asks {
            let (column, child) = (column.parse().unwrap(), recorded(child));
            assert_eq!(
                show(family.physical(&column, &child, *end)).as_deref(),
                *physical,
                "{}: physical {child:?} {end:?}",
                row.label
            );
            assert_eq!(
                show(family.slot(&column, &child, *end)).as_deref(),
                *slot,
                "{}: slot {child:?} {end:?}",
                row.label
            );
        }
        for (child, end, key, parent) in row.claims {
            let key = ColumnKey::Recorded(recorded(key));
            assert_eq!(
                family
                    .parent_column(&key, &recorded(child), *end)
                    .map(|u| u.to_string())
                    .as_deref(),
                *parent,
                "{}: claim of {key:?}",
                row.label
            );
        }
    }
}

fn still((schema, ids): &(Schema, IdsFile)) -> Snapshot<'_> {
    Snapshot { schema, ids }
}
#[test]
fn only_a_parent_that_gains_or_loses_a_member_has_a_family() {
    let row = &ROWS[0];
    let ends: Vec<_> = [0, 0].map(|e| build(row, e)).into();
    let (steady, moving) = (build(&ROWS[7], 0), build(&ROWS[7], 1));
    // The same snapshot on both sides moves nothing, whatever it holds.
    assert!(Lineage::of(still(&ends[0]), still(&ends[1])).is_empty());
    assert!(Lineage::of(still(&steady), still(&steady)).is_empty());
    let lineage = Lineage::of(still(&steady), still(&moving));
    assert_eq!(lineage.parents().count(), 1);
    let family = lineage.family(&recorded("t_aaaaaa")).unwrap();
    assert_eq!(family.children().count(), 2);
    assert!(family.present(End::Base) && family.present(End::Goal));
    assert_eq!(
        lineage.parent_of(&recorded("t_eeeeee"), End::Goal),
        Some(&recorded("t_aaaaaa"))
    );
    assert_eq!(lineage.parent_of(&recorded("t_cccccc"), End::Goal), None);
    assert!(lineage.family(&recorded("t_qqqqqq")).is_none());
}
