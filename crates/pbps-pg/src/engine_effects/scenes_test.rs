//! The scenes the column-meeting oracles share: thirteen kinds of parent
//! change, three motions of the child, and both cuts of the candidate.
use pbps_model::{
    BoundDatum, Column, IdsFile, PartitionBound, PartitionBy, PartitionColumn, PartitionOf, Schema,
    Table, TableName, ValueSource,
};

pub(super) struct Scene {
    pub(super) before: Schema,
    pub(super) after: Schema,
    pub(super) base_ids: IdsFile,
    pub(super) ids: IdsFile,
}
pub(super) fn bound() -> PartitionBound {
    PartitionBound::Range {
        from: vec![BoundDatum::MinValue],
        to: vec![BoundDatum::MaxValue],
    }
}
#[derive(Clone, Copy, Debug)]
pub(super) enum Kind {
    Add,
    Generated,
    Drop,
    Rename,
    Widen,
    Narrow,
    Collation,
    SetNotNull,
    DropNotNull,
    SetDefault,
    DropDefault,
    ParentName,
    ParentSchema,
}
pub(super) const KINDS: [Kind; 13] = [
    Kind::Add,
    Kind::Generated,
    Kind::Drop,
    Kind::Rename,
    Kind::Widen,
    Kind::Narrow,
    Kind::Collation,
    Kind::SetNotNull,
    Kind::DropNotNull,
    Kind::SetDefault,
    Kind::DropDefault,
    Kind::ParentName,
    Kind::ParentSchema,
];
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Motion {
    Attach,
    Detach,
    Drop,
}
pub(super) fn name(s: &str) -> TableName {
    s.parse().unwrap()
}
pub(super) fn uid(s: &str) -> pbps_model::Uid {
    s.parse().unwrap()
}
fn base_columns(kind: Kind) -> Table {
    let mut t = Table::default();
    t.columns.insert(
        "k".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    let mut d = Column::new(
        if matches!(kind, Kind::Narrow) {
            "bigint"
        } else if matches!(kind, Kind::Collation) {
            "text"
        } else {
            "integer"
        }
        .parse()
        .unwrap(),
    );
    d.nullable = !matches!(kind, Kind::DropNotNull);
    if matches!(kind, Kind::DropDefault) {
        d.value_source = ValueSource::Default("1".into());
    }
    if matches!(kind, Kind::Collation) {
        d.collation = Some(pbps_model::Collation::new("pg_catalog.C"));
    }
    t.columns.insert("d".into(), d);
    t
}
fn changed(mut t: Table, kind: Kind) -> Table {
    match kind {
        Kind::Add | Kind::Generated => {
            let mut x = Column::new("integer".parse().unwrap());
            x.value_source = if matches!(kind, Kind::Generated) {
                ValueSource::Generated(pbps_model::Generated {
                    expression: "d*2".into(),
                    stored: true,
                })
            } else {
                ValueSource::Default("7".into())
            };
            t.columns.insert("x".into(), x);
        }
        Kind::Drop => {
            t.columns.shift_remove("d");
        }
        Kind::Rename => {
            let (i, _, c) = t.columns.shift_remove_full("d").unwrap();
            t.columns.shift_insert(i, "e".into(), c);
        }
        Kind::Widen | Kind::Narrow => {
            t.columns.get_mut("d").unwrap().ty = if matches!(kind, Kind::Widen) {
                "bigint"
            } else {
                "integer"
            }
            .parse()
            .unwrap()
        }
        Kind::Collation => {
            t.columns.get_mut("d").unwrap().collation =
                Some(pbps_model::Collation::new("pg_catalog.POSIX"))
        }
        Kind::SetNotNull => t.columns.get_mut("d").unwrap().nullable = false,
        Kind::DropNotNull => t.columns.get_mut("d").unwrap().nullable = true,
        Kind::SetDefault => {
            t.columns.get_mut("d").unwrap().value_source = ValueSource::Default("7".into())
        }
        Kind::DropDefault => t.columns.get_mut("d").unwrap().value_source = ValueSource::None,
        Kind::ParentName | Kind::ParentSchema => {}
    };
    t
}
pub(super) fn build(kind: Kind, motion: Motion, cut_final: bool) -> Scene {
    let p = name("app.p");
    let a = name("app.a");
    let q = match kind {
        Kind::ParentName => name("app.q"),
        Kind::ParentSchema => name("other.p"),
        Kind::Add
        | Kind::Generated
        | Kind::Drop
        | Kind::Rename
        | Kind::Widen
        | Kind::Narrow
        | Kind::Collation
        | Kind::SetNotNull
        | Kind::DropNotNull
        | Kind::SetDefault
        | Kind::DropDefault => p.clone(),
    };
    let mut parent = base_columns(kind);
    parent.partition_by = Some(PartitionBy {
        strategy: Default::default(),
        columns: vec!["k".into()],
        external: false,
    });
    let goal_parent = changed(parent.clone(), kind);
    let member = |parent: TableName| Table {
        partition_of: Some(PartitionOf {
            parent,
            bound: bound(),
            columns: Default::default(),
        }),
        ..Default::default()
    };
    let ordinary = |mut t: Table| {
        t.partition_by = None;
        t
    };
    let child_before = if motion == Motion::Attach {
        ordinary(if cut_final {
            goal_parent.clone()
        } else {
            parent.clone()
        })
    } else {
        member(p.clone())
    };
    let mut before = Schema::default();
    before.tables.insert(p.clone(), parent.clone());
    before.tables.insert(a.clone(), child_before);
    let mut after = Schema::default();
    after.tables.insert(q.clone(), goal_parent.clone());
    if motion != Motion::Drop {
        after.tables.insert(
            a.clone(),
            if motion == Motion::Attach {
                member(q.clone())
            } else {
                ordinary(if cut_final { goal_parent } else { parent })
            },
        );
    }
    let mut base_ids = IdsFile::default();
    let mut ids = IdsFile::default();
    for (schema, ids) in [(&before, &mut base_ids), (&after, &mut ids)] {
        for (at, t) in &schema.tables {
            let is_child = at == &a;
            ids.tables.insert(
                uid(if is_child { "t_bbbbbb" } else { "t_aaaaaa" }),
                at.clone(),
            );
            for column in t.columns.keys() {
                let id = match (is_child, column.raw()) {
                    (false, "k") => "c_aaaaaa",
                    (false, "d" | "e") => "c_bbbbbb",
                    (false, "x") => "c_eeeeee",
                    (true, "k") => "c_cccccc",
                    (true, "d" | "e") => "c_dddddd",
                    (true, "x") => "c_ffffff",
                    _ => panic!(),
                };
                ids.columns.insert(uid(id), at.column(column));
            }
        }
    }
    Scene {
        before,
        after,
        base_ids,
        ids,
    }
}

/// The member of the scene holds a DEFAULT and a NOT NULL of its own (#1578)
/// over the parent's column `column`, at whichever end it is a member.
pub(super) fn with_own_values(mut s: Scene, motion: Motion, column: &str, not_null: bool) -> Scene {
    let member = if motion == Motion::Attach {
        &mut s.after
    } else {
        &mut s.before
    };
    let own = member.tables.get_mut(&name("app.a")).unwrap();
    own.partition_of.as_mut().unwrap().columns.insert(
        column.into(),
        PartitionColumn {
            default: Some("9".into()),
            not_null,
        },
    );
    s
}
